//! instance.rs — one running copy of the app per user login.
//!
//! The first copy listens on a socket in the user's runtime directory
//! (`XDG_RUNTIME_DIR`, one per login, shared by every display of that login).
//! A second launch finds it, hands over its activation token so the running
//! window can be raised, and exits, so there is never a second window that
//! could start a download or an install next to the first one's.
//!
//! Without `XDG_RUNTIME_DIR` there is no directory that is known to be the
//! user's alone, so the app runs without the single-instance check rather
//! than trust a socket in a shared place.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

/// Requests from later launches to show the running window, each with the
/// activation token of that launch when it had one (see `launch_token`).
pub type Activations = UnboundedReceiver<Option<String>>;

fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()).map(PathBuf::from)
}

/// What the desktop gave this launch to raise a window with: the Wayland
/// activation token, or the startup id (which desktops also fill with that
/// token).
fn launch_token() -> Option<String> {
    ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"].into_iter().filter_map(|name| std::env::var(name).ok()).find(|token| valid_token(token))
}

/// The longest token passed on. Real ones are a few dozen bytes; a Wayland
/// message holds at most 4096, and a request larger than that makes the
/// compositor drop the app's whole connection.
const MAX_TOKEN: usize = 256;

/// A token is a short run of printable ASCII: it travels as one line here
/// and as a string argument to the compositor, and comes from the
/// environment, which anything may have set.
fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= MAX_TOKEN && token.bytes().all(|b| b.is_ascii_graphic())
}

/// `None` when another copy is already running (it has been asked to show
/// its window); otherwise this is the primary copy, and the receiver yields
/// once per later launch.
pub fn claim() -> Option<Activations> {
    match runtime_dir() {
        Some(dir) => claim_at(&dir.join("green-light.sock"), launch_token()),
        None => {
            eprintln!("green-light: XDG_RUNTIME_DIR is not set, so a second launch can't find this one");
            let (tx, rx) = unbounded_channel();
            // Kept open for the life of the process: the receiver never ends.
            std::mem::forget(tx);
            Some(rx)
        }
    }
}

/// Holds `flock` on the lock file next to the socket, so that of two copies
/// started together only one looks at, removes and binds the socket at a time.
struct Lock(#[allow(dead_code)] std::fs::File);

fn lock(socket: &Path) -> Option<Lock> {
    let mut name = socket.as_os_str().to_owned();
    name.push(".lock");
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(PathBuf::from(name)).ok()?;
    // SAFETY: flock on a file descriptor this function owns; released when the file closes.
    (unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0).then_some(Lock(file))
}

/// Whether a failed connect means "nobody is listening" (a socket file left
/// by a copy that is gone, or no file at all), as opposed to a running copy
/// that could not be reached just now.
fn nobody_listening(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound)
}

fn claim_at(path: &Path, token: Option<String>) -> Option<Activations> {
    let _lock = lock(path);
    match UnixStream::connect(path) {
        Ok(mut stream) => {
            let _ = writeln!(stream, "activate {}", token.as_deref().unwrap_or(""));
            return None;
        }
        Err(e) if nobody_listening(&e) => {
            let _ = std::fs::remove_file(path);
        }
        Err(e) => {
            // Something answers to that name but not to us: leave it alone and
            // run without the check.
            eprintln!("green-light: single-instance socket {}: {e}", path.display());
        }
    }
    let (tx, rx) = unbounded_channel();
    match UnixListener::bind(path) {
        Ok(listener) => {
            std::thread::spawn(move || {
                for connection in listener.incoming().flatten() {
                    if tx.send(read_request(connection)).is_err() {
                        break;
                    }
                }
            });
        }
        Err(e) => {
            // Without the socket the app still runs; it just can't be found
            // by a second launch.
            eprintln!("green-light: single-instance socket {}: {e}", path.display());
            std::mem::forget(tx);
        }
    }
    Some(rx)
}

/// The token a second launch sent with its request, if any. A launch that
/// sends nothing readable still counts as a request.
fn read_request(connection: UnixStream) -> Option<String> {
    let _ = connection.set_read_timeout(Some(Duration::from_secs(2)));
    let mut line = String::new();
    let _ = BufReader::new(connection).take(8192).read_line(&mut line);
    parse_request(&line)
}

fn parse_request(line: &str) -> Option<String> {
    let token = line.trim_end_matches('\n').strip_prefix("activate ")?;
    valid_token(token).then(|| token.to_string())
}

#[cfg(test)]
mod tests {
    use super::{claim_at, nobody_listening, parse_request, valid_token, MAX_TOKEN};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("green-light-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn second_launch_reaches_the_first_with_its_token() {
        let dir = scratch("instance");
        let path = dir.join("test.sock");
        let mut first = claim_at(&path, None).expect("the first copy is the primary");
        assert!(claim_at(&path, Some("token-1".into())).is_none(), "a second copy finds the first and stands down");
        assert_eq!(first.blocking_recv(), Some(Some("token-1".to_string())), "and the first is told, with the token to raise its window with");
        assert!(claim_at(&path, None).is_none());
        assert_eq!(first.blocking_recv(), Some(None), "no token: still a request");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_socket_file_is_replaced() {
        let dir = scratch("stale");
        let path = dir.join("test.sock");
        // A socket nobody listens on any more.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let mut first = claim_at(&path, None).expect("a dead socket does not block startup");
        assert!(claim_at(&path, None).is_none(), "and the new one answers");
        assert_eq!(first.blocking_recv(), Some(None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_dead_socket_is_removed() {
        use std::io::{Error, ErrorKind};
        assert!(nobody_listening(&Error::from(ErrorKind::ConnectionRefused)));
        assert!(nobody_listening(&Error::from(ErrorKind::NotFound)));
        assert!(!nobody_listening(&Error::from(ErrorKind::PermissionDenied)));
        assert!(!nobody_listening(&Error::from(ErrorKind::WouldBlock)));
        // A path that is not a socket at all is not ours to delete.
        let dir = scratch("notsock");
        let path = dir.join("test.sock");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), b"x").unwrap();
        let _ = claim_at(&path, None);
        assert!(path.join("keep").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_copies_started_together_leave_one_primary() {
        let dir = scratch("race");
        let path = dir.join("test.sock");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || claim_at(&path, None))
            })
            .collect();
        let claims: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(claims.iter().filter(|c| c.is_some()).count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn requests_and_tokens_are_one_clean_line() {
        assert_eq!(parse_request("activate abc-123_x\n"), Some("abc-123_x".into()));
        assert_eq!(parse_request("activate \n"), None);
        assert_eq!(parse_request("activate\n"), None);
        assert_eq!(parse_request(""), None);
        assert_eq!(parse_request("something else\n"), None);
        assert!(!valid_token("a\nb") && !valid_token("") && !valid_token("a\u{7}"));
        assert!(!valid_token("a b") && !valid_token("tok\u{e9}n"), "printable ASCII only");
        assert!(valid_token(&"x".repeat(MAX_TOKEN)));
        assert!(!valid_token(&"x".repeat(MAX_TOKEN + 1)), "an oversized token must never reach the compositor");
        let long = format!("activate {}\n", "x".repeat(MAX_TOKEN + 1));
        assert_eq!(parse_request(&long), None);
    }
}
