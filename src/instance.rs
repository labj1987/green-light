//! instance.rs — one running copy of the app per user login.
//!
//! The first copy listens on a socket in the user's runtime directory
//! (`XDG_RUNTIME_DIR`, one per login, shared by every display of that login).
//! A second launch finds it, hands over its activation token so the running
//! window can be raised, waits for the running copy to say it did, and exits,
//! so there is never a second window that could start a download or an
//! install next to the first one's. The wire format is one line each way:
//! `activate <token>\n` from the second launch, `ok\n` back once the running
//! copy's UI has handled it. A copy that is still starting (its socket is up
//! before its window: a cold start on a software renderer takes seconds)
//! first answers `starting\n` at once, and `ok\n` once its UI is up. A
//! running copy that does not answer (its UI is stuck) is named, with its
//! PID, instead of the second launch exiting with nothing to show for it.
//!
//! The primary copy also holds a lock on `green-light.running` next to the
//! socket for as long as it runs: AppRun checks it to skip its launch-time
//! setup (and its password prompt) when a copy is already running.
//!
//! Without `XDG_RUNTIME_DIR` there is no directory that is known to be the
//! user's alone, so the app runs without the single-instance check rather
//! than trust a socket in a shared place.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

/// A later launch's request to show the running window, with the activation
/// token of that launch when it had one (see `launch_token`).
pub struct Activation {
    pub token: Option<String>,
    reply: Option<UnixStream>,
}

impl Activation {
    /// Tells the launch that asked that its request was handled, so it exits
    /// quietly. A request that is never answered makes that launch report the
    /// running copy as not responding.
    pub fn done(self) {
        if let Some(mut stream) = self.reply {
            let _ = stream.write_all(ACK);
        }
    }
}

/// Requests from later launches to show the running window.
pub struct Activations {
    requests: UnboundedReceiver<Activation>,
    ui_up: Arc<AtomicBool>,
}

impl Activations {
    fn new(requests: UnboundedReceiver<Activation>, ui_up: Arc<AtomicBool>) -> Self {
        Activations { requests, ui_up }
    }

    /// Says the UI is up and handles requests: until then, a later launch is
    /// told this copy is still starting, and waits longer for its answer.
    pub fn ui_up(&self) {
        self.ui_up.store(true, Ordering::Release);
    }

    /// The next request.
    pub async fn recv(&mut self) -> Option<Activation> {
        self.requests.recv().await
    }

    #[cfg(test)]
    fn blocking_recv(&mut self) -> Option<Activation> {
        self.requests.blocking_recv()
    }
}

/// What `claim` found.
pub enum Claim {
    /// This is the primary copy; the receiver yields once per later launch.
    Primary(Activations),
    /// Another copy runs and has shown its window.
    HandedOver,
    /// Another copy runs but did not answer in time; its PID when known.
    NotResponding(Option<u32>),
}

/// The running copy's answer once it has handled a request.
const ACK: &[u8] = b"ok\n";

/// Its answer, at once, while its UI is not up yet.
const STARTING: &[u8] = b"starting\n";

/// How long a second launch waits for an answer.
#[derive(Clone, Copy)]
struct Waits {
    /// For any answer at all.
    answer: Duration,
    /// For "ok" from a copy that said it is starting, counted from the request.
    start: Duration,
}

const WAITS: Waits = Waits { answer: Duration::from_secs(2), start: Duration::from_secs(15) };

/// What a second launch says when the running copy did not answer.
pub fn not_responding_message(pid: Option<u32>) -> String {
    match pid {
        Some(pid) => format!("Green Light is already running but not responding; close it (PID {pid}) and start again."),
        None => "Green Light is already running but not responding; close it and start again.".to_string(),
    }
}

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

/// Whether this is the primary copy, or another copy runs (and was asked to
/// show its window).
pub fn claim() -> Claim {
    match runtime_dir() {
        Some(dir) => claim_at(&dir.join("green-light.sock"), launch_token(), WAITS),
        None => {
            eprintln!("green-light: XDG_RUNTIME_DIR is not set, so a second launch can't find this one");
            let (tx, rx) = unbounded_channel();
            // Kept open for the life of the process: the receiver never ends.
            std::mem::forget(tx);
            Claim::Primary(Activations::new(rx, Arc::default()))
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

/// The file the primary copy keeps locked while it runs: `green-light.running`
/// for `green-light.sock`.
fn running_file(socket: &Path) -> PathBuf {
    socket.with_extension("running")
}

/// Takes the lock on the running file and keeps it until the process ends.
fn mark_running(socket: &Path) {
    let Ok(file) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(running_file(socket)) else {
        return;
    };
    // SAFETY: flock on a file descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        // Held for the life of the process: the kernel drops it when the process ends.
        std::mem::forget(file);
    }
}

/// The PID of the process at the other end of `stream`: for a connection to
/// the socket, the copy that listens on it.
fn peer_pid(stream: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED fills a ucred of the size given, on a socket this borrows.
    let got = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len) };
    (got == 0 && cred.pid > 0).then(|| cred.pid.unsigned_abs())
}

/// Sends the request to the running copy and waits for its answer: "ok"
/// within `waits.answer`, or "starting" within that and then "ok" within
/// `waits.start`.
fn hand_over(mut stream: UnixStream, token: Option<&str>, waits: Waits) -> Claim {
    let pid = peer_pid(&stream);
    let asked = Instant::now();
    let _ = stream.set_write_timeout(Some(waits.answer));
    if writeln!(stream, "activate {}", token.unwrap_or("")).is_err() {
        return Claim::NotResponding(pid);
    }
    let mut reader = BufReader::new(&stream).take(64);
    let mut line = Vec::new();
    let _ = stream.set_read_timeout(Some(waits.answer));
    if reader.read_until(b'\n', &mut line).is_ok() && line == STARTING {
        line.clear();
        let left = waits.start.saturating_sub(asked.elapsed()).max(Duration::from_millis(1));
        let _ = stream.set_read_timeout(Some(left));
        let _ = reader.read_until(b'\n', &mut line);
    }
    if line == ACK {
        Claim::HandedOver
    } else {
        Claim::NotResponding(pid)
    }
}

/// Whether a failed connect means "nobody is listening" (a socket file left
/// by a copy that is gone, or no file at all), as opposed to a running copy
/// that could not be reached just now.
fn nobody_listening(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound)
}

fn claim_at(path: &Path, token: Option<String>, waits: Waits) -> Claim {
    let lock = lock(path);
    match UnixStream::connect(path) {
        Ok(stream) => {
            // The wait for the answer needs no lock: other launches may look meanwhile.
            drop(lock);
            return hand_over(stream, token.as_deref(), waits);
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
    let ui_up = Arc::new(AtomicBool::new(false));
    let activations = Activations::new(rx, ui_up.clone());
    match UnixListener::bind(path) {
        Ok(listener) => {
            mark_running(path);
            std::thread::spawn(move || {
                for mut connection in listener.incoming().flatten() {
                    let token = read_request(&connection);
                    if !ui_up.load(Ordering::Acquire) {
                        let _ = connection.write_all(STARTING);
                    }
                    if tx.send(Activation { token, reply: Some(connection) }).is_err() {
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
    Claim::Primary(activations)
}

/// The token a second launch sent with its request, if any. A launch that
/// sends nothing readable still counts as a request.
fn read_request(connection: &UnixStream) -> Option<String> {
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
    use super::{claim_at, nobody_listening, not_responding_message, parse_request, running_file, valid_token, Activations, Claim, Waits, MAX_TOKEN};
    use std::os::fd::AsRawFd;
    use std::path::Path;
    use std::time::{Duration, Instant};

    const WAIT: Waits = Waits { answer: Duration::from_secs(2), start: Duration::from_secs(15) };

    /// Short waits: `answer` for any answer, `start` for "ok" after "starting".
    fn waits(answer: u64, start: u64) -> Waits {
        Waits { answer: Duration::from_millis(answer), start: Duration::from_millis(start) }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("green-light-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The primary copy, with its UI up.
    fn primary(path: &Path) -> Activations {
        match claim_at(path, None, WAIT) {
            Claim::Primary(rx) => {
                rx.ui_up();
                rx
            }
            _ => panic!("expected to be the primary copy"),
        }
    }

    /// A second launch, on a thread (it waits for the answer); the primary's
    /// side handles its request and answers, and the second launch's outcome
    /// comes back with the token the primary saw.
    fn second_launch(path: &Path, first: &mut Activations, token: Option<&str>) -> (Option<String>, Claim) {
        let (path, token) = (path.to_path_buf(), token.map(String::from));
        let launch = std::thread::spawn(move || claim_at(&path, token, WAIT));
        let request = first.blocking_recv().expect("the first copy is told");
        let seen = request.token.clone();
        request.done();
        (seen, launch.join().unwrap())
    }

    #[test]
    fn second_launch_reaches_the_first_with_its_token() {
        let dir = scratch("instance");
        let path = dir.join("test.sock");
        let mut first = primary(&path);
        let (seen, claim) = second_launch(&path, &mut first, Some("token-1"));
        assert_eq!(seen.as_deref(), Some("token-1"), "the first is told, with the token to raise its window with");
        assert!(matches!(claim, Claim::HandedOver), "a second copy finds the first and stands down");
        let (seen, claim) = second_launch(&path, &mut first, None);
        assert_eq!(seen, None, "no token: still a request");
        assert!(matches!(claim, Claim::HandedOver));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_running_copy_that_never_answers_is_reported_with_its_pid() {
        let dir = scratch("silent");
        let path = dir.join("test.sock");
        // A listener that takes the connection and then says nothing, as a copy whose UI hangs.
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let held = std::thread::spawn(move || listener.accept().map(|(stream, _)| stream));
        let started = Instant::now();
        let claim = claim_at(&path, Some("tok".into()), waits(300, 5000));
        let waited = started.elapsed();
        match claim {
            Claim::NotResponding(pid) => assert_eq!(pid, Some(std::process::id()), "the listener's PID (this process)"),
            _ => panic!("a silent copy must be reported, not taken as handed over"),
        }
        assert!(waited >= Duration::from_millis(300) && waited < Duration::from_secs(2), "{waited:?}");
        drop(held.join());
        // An answer that is not "ok" is no answer either.
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let wrong = std::thread::spawn(move || {
            use std::io::Write;
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"busy\n").unwrap();
            stream
        });
        assert!(matches!(claim_at(&path, None, waits(300, 5000)), Claim::NotResponding(_)));
        drop(wrong.join());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_copy_still_starting_gets_longer_to_answer() {
        let dir = scratch("starting");
        let path = dir.join("test.sock");
        // The primary's UI is not up yet: the listener says "starting" at once.
        let Claim::Primary(mut first) = claim_at(&path, None, WAIT) else { panic!("primary") };
        let launch = {
            let path = path.clone();
            // Any answer within 300 ms, "ok" within 5 s.
            std::thread::spawn(move || claim_at(&path, None, waits(300, 5000)))
        };
        let request = first.blocking_recv().unwrap();
        // The UI comes up well after the 300 ms a silent copy gets, then answers.
        std::thread::sleep(Duration::from_millis(800));
        first.ui_up();
        request.done();
        assert!(matches!(launch.join().unwrap(), Claim::HandedOver), "a slow start is not \"not responding\"");
        // Once the UI is up, nothing says "starting": a stuck UI gets only the short wait.
        let started = Instant::now();
        let launch = {
            let path = path.clone();
            std::thread::spawn(move || claim_at(&path, None, waits(300, 5000)))
        };
        let request = first.blocking_recv().unwrap();
        assert!(matches!(launch.join().unwrap(), Claim::NotResponding(Some(_))));
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        drop(request);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_copy_that_stays_starting_is_reported_after_the_long_wait() {
        let dir = scratch("stuck-start");
        let path = dir.join("test.sock");
        let Claim::Primary(mut first) = claim_at(&path, None, WAIT) else { panic!("primary") };
        let started = Instant::now();
        let launch = {
            let path = path.clone();
            std::thread::spawn(move || claim_at(&path, None, waits(200, 900)))
        };
        let request = first.blocking_recv().unwrap();
        // Never answered: the UI never comes up.
        assert!(matches!(launch.join().unwrap(), Claim::NotResponding(Some(_))));
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(900) && waited < Duration::from_secs(3), "{waited:?}");
        drop(request);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_not_responding_message_names_the_pid() {
        assert_eq!(not_responding_message(Some(42)), "Green Light is already running but not responding; close it (PID 42) and start again.");
        assert!(!not_responding_message(None).contains("PID"));
    }

    #[test]
    fn the_primary_copy_keeps_the_running_file_locked() {
        let dir = scratch("running");
        let path = dir.join("test.sock");
        assert_eq!(running_file(&path), dir.join("test.running"));
        let _first = primary(&path);
        let file = std::fs::File::open(running_file(&path)).expect("the running file is there");
        // SAFETY: flock on a descriptor this test owns.
        let free = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        assert!(!free, "AppRun must find the running file locked while the primary copy runs");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_socket_file_is_replaced() {
        let dir = scratch("stale");
        let path = dir.join("test.sock");
        // A socket nobody listens on any more.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let mut first = primary(&path);
        let (_, claim) = second_launch(&path, &mut first, None);
        assert!(matches!(claim, Claim::HandedOver), "and the new one answers");
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
        let _ = claim_at(&path, None, WAIT);
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
                // Nobody answers here, so the others report the primary as not responding,
                // quickly: what matters is that only one became the primary.
                std::thread::spawn(move || claim_at(&path, None, waits(100, 100)))
            })
            .collect();
        // All kept until counted: a dropped primary would stop listening.
        let claims: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(claims.iter().filter(|claim| matches!(claim, Claim::Primary(_))).count(), 1);
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
