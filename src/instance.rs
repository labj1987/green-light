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
//! socket for as long as it answers there: AppRun checks it to skip its launch-time
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
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

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
    ui_up: UiUp,
    listening: Option<Listening>,
}

impl Activations {
    fn new(requests: UnboundedReceiver<Activation>, ui_up: UiUp, listening: Option<Listening>) -> Self {
        Activations { requests, ui_up, listening }
    }

    /// Says the UI is up and handles requests: until then, a later launch is
    /// told this copy is still starting, and waits longer for its answer.
    #[cfg(test)]
    fn ui_up(&self) {
        self.ui_up.set();
    }

    /// The handle that says the UI is up, for the window's first frame.
    pub fn up_signal(&self) -> UiUp {
        self.ui_up.clone()
    }

    /// The next request.
    pub async fn recv(&mut self) -> Option<Activation> {
        self.requests.recv().await
    }

    #[cfg(test)]
    fn blocking_recv(&mut self) -> Option<Activation> {
        self.requests.blocking_recv()
    }

    #[cfg(test)]
    fn close(&self) {
        if let Some(listening) = &self.listening {
            listening.close();
        }
    }
}

/// Says the primary copy's UI is up (see `Activations::up_signal`). Set once
/// the window's first frame has been drawn and presented: a window that has
/// been created but never shown is not up yet, and a later launch is told to
/// wait for it.
#[derive(Clone, Default)]
pub struct UiUp(Arc<AtomicBool>);

impl UiUp {
    pub fn set(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn is_set(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// The primary copy's socket, closed (and its file removed) when the app is
/// done with it: when its last window closes, or when it can't open one.
/// Without that, a later launch would connect to a copy that no longer reads
/// its requests, wait out the whole start wait, and call it stuck. The lock
/// on the running file goes with it, so AppRun runs its setup for that launch.
#[derive(Clone)]
struct Listening {
    listener: Arc<UnixListener>,
    path: PathBuf,
    running: Option<Arc<std::fs::File>>,
    closed: Arc<AtomicBool>,
}

impl Listening {
    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        // The running lock first: AppRun must not see a copy running once
        // its socket is gone, or it would skip its setup for a launch that
        // then becomes the primary copy itself.
        if let Some(running) = &self.running {
            // SAFETY: flock on a file this struct keeps open.
            unsafe { libc::flock(running.as_raw_fd(), libc::LOCK_UN) };
        }
        // Then the file: a launch from now on finds nobody and binds its own
        // socket, instead of connecting to this one's backlog.
        let _ = std::fs::remove_file(&self.path);
        // Wakes the listening thread's accept with an error once the backlog
        // is empty, and refuses connections from here on; the thread then ends.
        // SAFETY: shutdown on a socket this struct keeps open.
        unsafe { libc::shutdown(self.listener.as_raw_fd(), libc::SHUT_RDWR) };
    }
}

/// The primary copy's socket, for `release`.
static PRIMARY: std::sync::OnceLock<Listening> = std::sync::OnceLock::new();

/// Stops answering later launches and removes the socket: call it when the
/// app is about to end (its last window closed, or none could be opened), so
/// the next launch becomes the primary copy at once.
pub fn release() {
    if let Some(listening) = PRIMARY.get() {
        listening.close();
    }
}

/// What `claim` found.
pub enum Claim {
    /// This is the primary copy; the receiver yields once per later launch.
    Primary(Activations),
    /// Another copy runs and has shown its window (or went away while asked).
    HandedOver,
    /// Another copy runs and is still starting: it has the request and will
    /// show its window when it is up. Its PID when known.
    Starting(Option<u32>),
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
    /// For any answer at all; also how long a copy that closes the
    /// connection unanswered (it is exiting) is retried.
    answer: Duration,
    /// For "ok" from a copy that said it is starting, counted from the request.
    start: Duration,
}

const WAITS: Waits = Waits { answer: Duration::from_secs(2), start: Duration::from_secs(15) };

/// How often a copy that closed the connection unanswered is asked again.
const RETRY_STEP: Duration = Duration::from_millis(75);

/// What a second launch says when the running copy did not answer (an error).
pub fn not_responding_message(pid: Option<u32>) -> String {
    match pid {
        Some(pid) => format!("Green Light is already running but not responding; close it (PID {pid}) and start again."),
        None => "Green Light is already running but not responding; close it and start again.".to_string(),
    }
}

/// What a second launch says when the running copy is still starting (not an
/// error: its window comes up on its own, and the request is queued).
pub fn starting_message(pid: Option<u32>) -> String {
    match pid {
        Some(pid) => format!("Green Light is still starting (process {pid}); its window will appear when it is ready."),
        None => "Green Light is still starting; its window will appear when it is ready.".to_string(),
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
        Some(dir) => {
            let claim = claim_at(&dir.join("green-light.sock"), launch_token(), WAITS);
            if let Claim::Primary(Activations { listening: Some(listening), .. }) = &claim {
                let _ = PRIMARY.set(listening.clone());
            }
            claim
        }
        None => {
            eprintln!("green-light: XDG_RUNTIME_DIR is not set, so a second launch can't find this one");
            let (tx, rx) = unbounded_channel();
            // Kept open for the life of the process: the receiver never ends.
            std::mem::forget(tx);
            Claim::Primary(Activations::new(rx, UiUp::default(), None))
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

/// Takes the lock on the running file, held until `release` or the end of
/// the process (the kernel drops it then). The open file, while it holds it.
fn mark_running(socket: &Path) -> Option<std::fs::File> {
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(running_file(socket)).ok()?;
    // AppRun's check takes the lock for a moment; wait that out (briefly:
    // without the lock the app still runs, AppRun just doesn't skip its setup).
    for _ in 0..50 {
        // SAFETY: flock on a file descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Some(file);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
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
/// `Ok(claim)`, or `Err(Closed)` when the running copy closed the connection
/// without answering: it was exiting, so the claim is worth another try.
fn hand_over(mut stream: UnixStream, token: Option<&str>, waits: Waits) -> Result<Claim, Closed> {
    let pid = peer_pid(&stream);
    let asked = Instant::now();
    let _ = stream.set_write_timeout(Some(waits.answer));
    if writeln!(stream, "activate {}", token.unwrap_or("")).is_err() {
        return Err(Closed);
    }
    let mut reader = BufReader::new(&stream).take(64);
    let mut line = Vec::new();
    let _ = stream.set_read_timeout(Some(waits.answer));
    let mut read = reader.read_until(b'\n', &mut line);
    let starting = read.is_ok() && line == STARTING;
    if starting {
        line.clear();
        let left = waits.start.saturating_sub(asked.elapsed()).max(Duration::from_millis(1));
        let _ = stream.set_read_timeout(Some(left));
        read = reader.read_until(b'\n', &mut line);
    }
    match read {
        _ if line == ACK => Ok(Claim::HandedOver),
        // End of file with nothing, or a reset (it closed before reading the
        // request): the copy went away. A timeout is another error.
        Ok(0) => Err(Closed),
        Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe) => Err(Closed),
        _ if starting => Ok(Claim::Starting(pid)),
        _ => Ok(Claim::NotResponding(pid)),
    }
}

/// The running copy closed the connection without an answer.
struct Closed;

/// Whether a failed connect means "nobody is listening" (a socket file left
/// by a copy that is gone, or no file at all), as opposed to a running copy
/// that could not be reached just now.
fn nobody_listening(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound)
}

fn claim_at(path: &Path, token: Option<String>, waits: Waits) -> Claim {
    // A copy that exits while it is being asked closes the connection
    // unanswered. Its listener may take a moment more to go away, so ask
    // again in short steps until nobody listens (this launch becomes the
    // primary) or a copy answers; if copies keep going away unanswered for
    // the whole answer wait, leave quietly rather than call one hung. That
    // wait counts from the first closed connection: a copy may have said
    // "starting" for seconds before it went away.
    let mut deadline = None;
    loop {
        if let Ok(claim) = try_claim(path, token.as_deref(), waits) {
            return claim;
        }
        let deadline = *deadline.get_or_insert_with(|| Instant::now() + waits.answer);
        if Instant::now() >= deadline {
            return Claim::HandedOver;
        }
        std::thread::sleep(RETRY_STEP);
    }
}

fn try_claim(path: &Path, token: Option<&str>, waits: Waits) -> Result<Claim, Closed> {
    let lock = lock(path);
    match UnixStream::connect(path) {
        Ok(stream) => {
            // The wait for the answer needs no lock: other launches may look meanwhile.
            drop(lock);
            return hand_over(stream, token, waits);
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
    let ui_up = UiUp::default();
    let activations = match UnixListener::bind(path) {
        Ok(listener) => {
            let listening =
                Listening { listener: Arc::new(listener), path: path.to_path_buf(), running: mark_running(path).map(Arc::new), closed: Arc::default() };
            let activations = Activations::new(rx, ui_up.clone(), Some(listening.clone()));
            std::thread::spawn(move || serve(&listening, &tx, &ui_up));
            activations
        }
        Err(e) => {
            // Without the socket the app still runs; it just can't be found
            // by a second launch.
            eprintln!("green-light: single-instance socket {}: {e}", path.display());
            std::mem::forget(tx);
            Activations::new(rx, ui_up, None)
        }
    };
    Ok(Claim::Primary(activations))
}

/// The listening thread: hands each later launch's request to the UI. A
/// connection accepted after `release`, or one the UI no longer takes, is
/// dropped unanswered, so that launch hears the end of the connection and
/// asks again. The loop goes on after one: every launch already waiting in
/// the backlog is taken and dropped the same way, and only the accept error
/// that `release`'s shutdown causes once the backlog is empty ends it.
fn serve(listening: &Listening, tx: &UnboundedSender<Activation>, ui_up: &UiUp) {
    for connection in listening.listener.incoming() {
        let mut connection = match connection {
            Ok(connection) => connection,
            // Closed by `release`, and the backlog is empty: no more requests.
            Err(_) if listening.closed.load(Ordering::Acquire) => break,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        // Released between the accept and now: dropped unanswered, so that
        // launch asks again and finds nobody.
        if listening.closed.load(Ordering::Acquire) {
            continue;
        }
        let token = read_request(&connection);
        if !ui_up.is_set() {
            let _ = connection.write_all(STARTING);
        }
        // A UI that no longer takes requests drops this one, unanswered.
        let _ = tx.send(Activation { token, reply: Some(connection) });
    }
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
    use super::{
        claim_at, nobody_listening, not_responding_message, parse_request, running_file, serve, starting_message, valid_token, Activations, Claim, Listening,
        UiUp, Waits, MAX_TOKEN,
    };
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
    fn a_copy_that_stays_starting_is_reported_as_starting_after_the_long_wait() {
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
        let claim = launch.join().unwrap();
        assert!(matches!(claim, Claim::Starting(Some(pid)) if pid == std::process::id()), "reported as still starting, not as stuck");
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(900) && waited < Duration::from_secs(3), "{waited:?}");
        drop(request);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_copy_that_exits_while_asked_is_not_called_hung() {
        let dir = scratch("exiting");
        let path = dir.join("test.sock");
        // A copy that takes the request and goes away without answering, in
        // an app's order: the connection closes first, the listener and its
        // socket file a moment later. Asking again at once still reaches that
        // listener (its backlog); the launch keeps asking until it is gone.
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let exiting = {
            let path = path.clone();
            std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                drop(stream);
                std::thread::sleep(Duration::from_millis(300));
                let _ = std::fs::remove_file(&path);
                drop(listener);
            })
        };
        // Nobody being there any more, the second launch becomes the primary.
        let claim = claim_at(&path, None, waits(2000, 5000));
        exiting.join().unwrap();
        assert!(matches!(claim, Claim::Primary(_)), "not \"not responding\"");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_copy_that_dies_after_saying_starting_still_gets_the_retries() {
        let dir = scratch("starting-dies");
        let path = dir.join("test.sock");
        // A copy says "starting", stays silent longer than the answer wait,
        // then goes away (connection first, socket a moment later).
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let dying = {
            let path = path.clone();
            std::thread::spawn(move || {
                use std::io::Write;
                let (mut stream, _) = listener.accept().unwrap();
                stream.write_all(b"starting\n").unwrap();
                std::thread::sleep(Duration::from_millis(600));
                drop(stream);
                std::thread::sleep(Duration::from_millis(150));
                let _ = std::fs::remove_file(&path);
                drop(listener);
            })
        };
        // Any answer within 300 ms, "ok" within 5 s: the copy dies 600 ms in.
        let claim = claim_at(&path, None, waits(300, 5000));
        dying.join().unwrap();
        assert!(matches!(claim, Claim::Primary(_)), "retried after the copy went away, and became the primary");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launches_queued_at_a_copy_that_gives_up_start_at_once() {
        let dir = scratch("gives-up");
        let path = dir.join("test.sock");
        // The primary's UI never comes up (no window could be opened).
        let Claim::Primary(first) = claim_at(&path, None, WAIT) else { panic!("primary") };
        let started = Instant::now();
        let launch = {
            let path = path.clone();
            std::thread::spawn(move || claim_at(&path, None, waits(2000, 15000)))
        };
        // Its request is queued (told "starting"); then the copy gives up as
        // ui::no_window does: release, and drop the requests.
        std::thread::sleep(Duration::from_millis(300));
        first.close();
        drop(first);
        let claim = launch.join().unwrap();
        assert!(matches!(claim, Claim::Primary(_)), "not \"still starting\" beside the other copy's error");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copies_that_keep_exiting_while_asked_end_the_launch_quietly() {
        let dir = scratch("exiting-always");
        let path = dir.join("test.sock");
        // Every attempt finds a copy that takes the request and closes without
        // answering, and keeps its socket: after the answer wait, leave quietly.
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let closer = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut taken = 0;
                for stream in listener.incoming() {
                    drop(stream);
                    taken += 1;
                    if stop.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                }
                taken
            })
        };
        let started = Instant::now();
        let claim = claim_at(&path, None, waits(500, 5000));
        let waited = started.elapsed();
        stop.store(true, std::sync::atomic::Ordering::Release);
        drop(std::os::unix::net::UnixStream::connect(&path));
        let taken = closer.join().unwrap();
        assert!(matches!(claim, Claim::HandedOver), "no hung-copy message");
        assert!(taken > 2, "asked again in short steps: {taken}");
        assert!(waited >= Duration::from_millis(500) && waited < Duration::from_secs(2), "{waited:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A primary copy's socket with nobody serving it yet.
    fn bound(path: &Path) -> Listening {
        Listening {
            listener: std::sync::Arc::new(std::os::unix::net::UnixListener::bind(path).unwrap()),
            path: path.to_path_buf(),
            running: None,
            closed: Default::default(),
        }
    }

    /// What a launch heard before its connection ended; `None` when it is
    /// still open a second later (nobody took the connection).
    fn heard(mut stream: std::os::unix::net::UnixStream) -> Option<Vec<u8>> {
        use std::io::Read;
        stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).ok().map(|_| bytes)
    }

    #[test]
    fn every_launch_queued_at_a_released_socket_hears_the_end_at_once() {
        let dir = scratch("backlog");
        let path = dir.join("test.sock");
        let listening = bound(&path);
        // Three launches wait in the backlog when the copy releases its socket.
        let waiting: Vec<_> = (0..3).map(|_| std::os::unix::net::UnixStream::connect(&path).unwrap()).collect();
        listening.close();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        serve(&listening, &tx, &UiUp::default());
        for stream in waiting {
            assert_eq!(heard(stream), Some(Vec::new()), "dropped unanswered, so that launch asks again");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_request_the_ui_no_longer_takes_is_dropped_unanswered() {
        use std::io::Write;
        let dir = scratch("no-ui");
        let path = dir.join("test.sock");
        let listening = bound(&path);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let serving = {
            let listening = listening.clone();
            std::thread::spawn(move || serve(&listening, &tx, &UiUp::default()))
        };
        for _ in 0..3 {
            let stream = std::os::unix::net::UnixStream::connect(&path).unwrap();
            writeln!(&stream, "activate").unwrap();
            assert_eq!(heard(stream).as_deref(), Some(&b"starting\n"[..]));
        }
        listening.close();
        serving.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_released_copy_lets_the_next_launch_start_at_once() {
        let dir = scratch("released");
        let path = dir.join("test.sock");
        // The primary's UI never comes up (as when it can't open a window),
        // and it releases the socket before it exits.
        let Claim::Primary(first) = claim_at(&path, None, WAIT) else { panic!("primary") };
        first.close();
        assert!(!path.exists(), "the socket file is removed");
        // AppRun no longer takes this copy for a running one.
        let file = std::fs::File::open(running_file(&path)).unwrap();
        // SAFETY: flock on a descriptor this test owns.
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0, "the running file is unlocked");
        drop(file);
        let started = Instant::now();
        let claim = claim_at(&path, None, waits(2000, 15000));
        assert!(matches!(claim, Claim::Primary(_)), "the next launch is the primary");
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
        drop(first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn releasing_twice_is_harmless_and_nobody_listens_after() {
        let dir = scratch("released-refuse");
        let path = dir.join("test.sock");
        let Claim::Primary(first) = claim_at(&path, None, WAIT) else { panic!("primary") };
        first.close();
        first.close();
        let error = std::os::unix::net::UnixStream::connect(&path).unwrap_err();
        assert!(nobody_listening(&error), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_running_lock_waits_out_a_brief_check() {
        let dir = scratch("probe");
        let path = dir.join("test.sock");
        // AppRun's check holds the lock for a moment while the primary starts.
        let probe = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(running_file(&path)).unwrap();
        // SAFETY: flock on a descriptor this test owns.
        assert_eq!(unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX) }, 0);
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(probe);
        });
        let _first = primary(&path);
        release.join().unwrap();
        let file = std::fs::File::open(running_file(&path)).unwrap();
        // SAFETY: as above.
        let free = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        assert!(!free, "the primary took the lock once the check let go");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_messages_name_the_pid() {
        assert_eq!(not_responding_message(Some(42)), "Green Light is already running but not responding; close it (PID 42) and start again.");
        assert!(!not_responding_message(None).contains("PID"));
        assert_eq!(starting_message(Some(42)), "Green Light is still starting (process 42); its window will appear when it is ready.");
        assert!(!starting_message(None).contains("process"));
        assert!(!starting_message(Some(1)).contains("start again"));
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
