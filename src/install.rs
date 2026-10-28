use crate::preflight::{acl_writers, group_is_owners_private_group, run_file_problem, FileFacts, ModuleChoice};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};

const PRIVILEGED_SCRIPT: &str = "/usr/lib/green-light/privileged-install.sh";
/// Held (flock) by the install script for as long as it runs; keep in step
/// with INSTALL_LOCK in the script (tested below).
pub const INSTALL_LOCK: &str = "/run/green-light-install.lock";

/// Whether an install is running right now, e.g. one started before the app
/// was closed and opened again.
pub fn install_running() -> bool {
    lock_held(Path::new(INSTALL_LOCK))
}

fn lock_held(path: &Path) -> bool {
    match std::fs::File::open(path) {
        Ok(f) => matches!(f.try_lock_shared(), Err(std::fs::TryLockError::WouldBlock)),
        Err(_) => false,
    }
}
const INSTALL_LOG: &str = "/var/log/green-light.log";

pub struct InstallOptions {
    pub use_dkms: bool,
    /// Keep the package manager from installing the distro's driver over
    /// this one (`--block-distro-driver`); off removes an earlier block.
    pub block_distro_driver: bool,
    pub module: ModuleChoice,
    pub run_file: String,
    /// SHA256 (hex) of the run file. When the checksum came from NVIDIA it is
    /// passed through; otherwise (local file, or a user-approved unverified
    /// download) it is computed here so the privileged script can still pin
    /// the exact bytes the user chose against a swap.
    pub sha256: Option<String>,
    /// Drop recovery data from an earlier interrupted install that no longer
    /// matches the computer (`--discard-recovery`), as the user chose.
    pub discard_recovery: bool,
}

/// Hash a file with SHA256, streaming so a ~300 MB .run isn't held in memory.
fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("Could not open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf).context("Read error while hashing run file")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn command_stdout(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Refuse, before the password prompt, a .run file the install script would
/// refuse: a symlink, not a regular file, or changeable by anyone but its
/// owner (see `run_file_problem`).
pub fn check_run_file(path: &str) -> Result<()> {
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("Run file not found: {path}"))?;
    if meta.file_type().is_symlink() {
        bail!("{path} is a symbolic link; open the file it points to instead.");
    }
    if !meta.is_file() {
        bail!("{path} is not a regular file.");
    }
    // The owner of /proc/self is this process's effective uid.
    let caller = std::fs::metadata("/proc/self").map(|m| m.uid()).context("Could not read /proc/self")?;
    let facts = FileFacts { mode: meta.mode(), uid: meta.uid(), gid: meta.gid() };
    let passwd = command_stdout("getent", &["passwd"]).unwrap_or_default();
    let group = command_stdout("getent", &["group", &facts.gid.to_string()]);
    if let Some(problem) = run_file_problem(path, facts, caller, &passwd, group.as_deref()) {
        bail!("{problem}");
    }
    // An ACL can let others write the file whatever its mode says ("ls"
    // marks a file with one by a "+" after the mode).
    let mode = command_stdout("ls", &["-ldn", "--", path]).unwrap_or_default();
    if !mode.split_whitespace().next().unwrap_or("").ends_with('+') {
        return Ok(());
    }
    let Some(acl) = command_stdout("getfacl", &["-cpn", "--", path]) else {
        bail!("{path} has an access control list that can't be checked (getfacl is missing). Remove it (setfacl -b) and try again.");
    };
    for who in acl_writers(&acl, facts.uid) {
        if let Some(gid) = who.strip_prefix("group:").and_then(|g| g.parse::<u32>().ok()) {
            let group = command_stdout("getent", &["group", &gid.to_string()]);
            if group_is_owners_private_group(gid, facts.uid, &passwd, group.as_deref()) {
                continue;
            }
        }
        bail!("{path} has an access control list letting {who} change it. Remove it (setfacl -b) and try again.");
    }
    Ok(())
}

/// What a privileged run printed, and how it ended.
struct ScriptRun {
    code: Option<i32>,
    lines: Vec<String>,
}

/// The messages of the run with PID `pid` in some lines of the script's log
/// ("<date> <time> [green-light <pid>] <message>"). Other runs' lines (a
/// second install refused by the lock, for one) and other programs' output,
/// which also goes to the log, are left out.
fn script_lines(complete_lines: &[u8], pid: u32) -> Vec<String> {
    let tag = format!("[green-light {pid}] ");
    complete_lines
        .split(|&b| b == b'\n')
        .filter_map(|line| {
            let line = String::from_utf8_lossy(line);
            let (_, msg) = line.split_once(&tag)?;
            (!msg.trim().is_empty()).then(|| msg.to_string())
        })
        .collect()
}

/// Follows a log file from a given offset, a whole line at a time (so a
/// character split across two reads is never cut), across rotation.
struct LogTail {
    path: std::path::PathBuf,
    pos: u64,
    inode: Option<u64>,
    partial: Vec<u8>,
}

impl LogTail {
    /// From the current end of `path`.
    fn at_end(path: impl Into<std::path::PathBuf>) -> Self {
        let path = path.into();
        let meta = std::fs::metadata(&path).ok();
        LogTail { pos: meta.as_ref().map_or(0, |m| m.len()), inode: meta.as_ref().map(|m| m.ino()), partial: Vec::new(), path }
    }

    /// Complete new lines (raw bytes, newline-separated).
    fn read_new_lines(&mut self) -> Vec<u8> {
        use std::io::{Seek, SeekFrom};
        let Ok(mut f) = std::fs::File::open(&self.path) else { return vec![] };
        let Ok(meta) = f.metadata() else { return vec![] };
        // A new file under the name (rotated), or the old one cut short.
        if self.inode != Some(meta.ino()) || meta.len() < self.pos {
            self.inode = Some(meta.ino());
            self.pos = 0;
            self.partial.clear();
        }
        let mut buf = Vec::new();
        if f.seek(SeekFrom::Start(self.pos)).is_err() || f.read_to_end(&mut buf).is_err() {
            return vec![];
        }
        self.pos += buf.len() as u64;
        self.partial.extend_from_slice(&buf);
        match self.partial.iter().rposition(|&b| b == b'\n') {
            Some(end) => self.partial.drain(..=end).collect(),
            None => vec![],
        }
    }

    fn read_new(&mut self, pid: u32) -> Vec<String> {
        script_lines(&self.read_new_lines(), pid)
    }
}

/// Run `pkexec <args>` and pass the script's messages to `on_line` as they
/// come. Without a terminal the script writes nothing to its stdout or
/// stderr (a pipe to this app, which may be closed mid-install), only to its
/// log file, so the log is what is followed here: the lines tagged with the
/// script's PID, which is pkexec's (pkexec execs the script). pkexec's own
/// messages (from before the script starts) come on stderr.
fn run_pkexec(args: &[String], stdin: Option<&str>, on_line: &mut dyn FnMut(&str)) -> Result<ScriptRun> {
    let mut tail = LogTail::at_end(INSTALL_LOG);
    let mut child = Command::new("pkexec")
        .args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to launch pkexec — is polkit installed?")?;
    let pid = child.id();
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A write error means the script already exited (e.g. cancelled
        // authentication); its exit code says why.
        let _ = writeln!(pipe, "{text}");
    }
    let stderr = child.stderr.take().map(|pipe| std::thread::spawn(move || BufReader::new(pipe).lines().map_while(Result::ok).collect::<Vec<String>>()));

    let mut lines = Vec::new();
    let status = loop {
        let exited = child.try_wait().context("Waiting for pkexec failed")?;
        for line in tail.read_new(pid) {
            on_line(&line);
            lines.push(line);
        }
        if let Some(status) = exited {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    if let Some(Ok(err_lines)) = stderr.map(|t| t.join()) {
        for line in err_lines.into_iter().filter(|l| !l.trim().is_empty()) {
            on_line(&line);
            lines.push(line);
        }
    }
    Ok(ScriptRun { code: status.code(), lines })
}

/// Recovery data from an earlier interrupted install no longer matches the
/// computer (the script's exit code 6): nothing is done until the user
/// chooses to discard it.
#[derive(Debug)]
pub struct StaleRecovery(pub String);

impl std::fmt::Display for StaleRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for StaleRecovery {}

fn stale_recovery(lines: &[String]) -> anyhow::Error {
    let why = first_error(lines).unwrap_or("its recovery data doesn't match this computer any more");
    anyhow::Error::new(StaleRecovery(format!(
        "An earlier install was interrupted, and its recovery data no longer matches this \
         computer: {why}. Discard that data if the driver this computer has now is the one you \
         want."
    )))
}

/// An interrupted install's journal (keep in step with STATE_DIR/JOURNAL in
/// the script; tested below). Its parent is world-readable, so this can be
/// seen without root.
pub const JOURNAL_DIR: &str = "/var/lib/green-light/journal";

/// An install was cut off (power loss, crash) and its changes are waiting to
/// be rolled back, with no install running now.
pub fn recovery_pending() -> bool {
    Path::new(JOURNAL_DIR).exists() && !install_running()
}

/// How `--recover` ended, when it didn't fail.
#[derive(Debug, PartialEq, Eq)]
pub enum Recovered {
    /// Nothing to put back (the cut-off install had already finished).
    Nothing,
    /// Rolled back and checked.
    Restored,
}

fn recover_result(code: Option<i32>, lines: &[String]) -> Result<Recovered> {
    let detail = first_error(lines).map(|e| format!(": {e}")).unwrap_or_default();
    match code {
        Some(0) => Ok(Recovered::Nothing),
        Some(3) => Ok(Recovered::Restored),
        Some(126) => bail!("Authentication was cancelled; nothing was changed."),
        Some(127) => bail!("Not authorized, or pkexec could not start the script; nothing was changed."),
        Some(4) => bail!(
            "The previous driver could not be fully restored{detail}. Don't reboot until a \
             working driver is installed; the Log tab has the details."
        ),
        Some(6) => Err(stale_recovery(lines)),
        Some(c) => bail!("Restoring exited with code {c}{detail} (see {INSTALL_LOG})"),
        None => bail!("Restoring was killed before it finished."),
    }
}

/// Why recovery data is waiting, from the script's note
/// (`/var/lib/green-light/recovery-status`; keep in step with
/// RECOVERY_STATUS in the script): "refused: <why>" or "failed: <why>".
pub const RECOVERY_STATUS: &str = "/var/lib/green-light/recovery-status";

#[derive(Debug, PartialEq, Eq)]
pub enum RecoveryNote {
    /// The data no longer matches the computer; only discarding it helps.
    Refused(String),
    /// A restore was tried and failed; trying again may help.
    Failed(String),
}

pub fn parse_recovery_note(text: &str) -> Option<RecoveryNote> {
    let text = text.trim();
    if let Some(why) = text.strip_prefix("refused: ") {
        Some(RecoveryNote::Refused(why.to_string()))
    } else {
        text.strip_prefix("failed: ").map(|why| RecoveryNote::Failed(why.to_string()))
    }
}

pub fn recovery_note() -> Option<RecoveryNote> {
    std::fs::read_to_string(RECOVERY_STATUS).ok().and_then(|t| parse_recovery_note(&t))
}

/// Drop recovery data that no longer matches the computer (the script's
/// `--discard-recovery`).
pub fn run_privileged_discard_recovery(mut on_line: impl FnMut(&str)) -> Result<()> {
    if !Path::new(PRIVILEGED_SCRIPT).exists() {
        bail!("Privileged install script not found at {}", PRIVILEGED_SCRIPT);
    }
    let args = [PRIVILEGED_SCRIPT.to_string(), "--discard-recovery".to_string()];
    let run = run_pkexec(&args, None, &mut on_line)?;
    discard_result(run.code, &run.lines)
}

fn discard_result(code: Option<i32>, lines: &[String]) -> Result<()> {
    match code {
        Some(0) => Ok(()),
        Some(126) => bail!("Authentication was cancelled; nothing was changed."),
        Some(127) => bail!("Not authorized, or pkexec could not start the script; nothing was changed."),
        code => bail!("Discarding the recovery data failed (exit code {code:?}){}", first_error(lines).map(|e| format!(": {e}")).unwrap_or_default()),
    }
}

/// Roll back an install that was cut off, from its journal (the script's
/// `--recover`, which the boot unit also runs).
pub fn run_privileged_recover(mut on_line: impl FnMut(&str)) -> Result<Recovered> {
    if !Path::new(PRIVILEGED_SCRIPT).exists() {
        bail!("Privileged install script not found at {}", PRIVILEGED_SCRIPT);
    }
    let args = [PRIVILEGED_SCRIPT.to_string(), "--recover".to_string()];
    let run = run_pkexec(&args, None, &mut on_line)?;
    recover_result(run.code, &run.lines)
}

/// The first "ERROR:" message of a run, if any: the cause (later ones are
/// about putting things back).
fn first_error(lines: &[String]) -> Option<&str> {
    lines.iter().find_map(|l| l.strip_prefix("ERROR: ")).map(str::trim)
}

/// What a failed install run means, from the script's exit code (see EXIT
/// CODES in privileged-install.sh) and pkexec's own 126/127, which the
/// script never uses.
fn install_failure(code: Option<i32>, lines: &[String]) -> String {
    let detail = first_error(lines).map(|e| format!(": {e}")).unwrap_or_default();
    match code {
        Some(126) => "Authentication was cancelled; nothing was changed.".to_string(),
        Some(127) => "Not authorized, or pkexec could not start the install script; nothing was changed.".to_string(),
        Some(1) => format!("The install stopped before changing anything{detail}"),
        Some(2) => format!("The install script refused to start{detail}"),
        Some(3) => format!(
            "The install failed{detail}. Everything it had changed was put back, and the \
             driver this computer had before was checked to be in place again: the next boot \
             uses it."
        ),
        Some(4) => format!(
            "The install failed{detail}, and putting the previous driver back failed too. \
             Don't reboot until a working driver is installed; the Log tab has the details."
        ),
        Some(5) => format!(
            "The driver is installed, but rebuilding the initramfs failed{detail}. Rebuild it \
             before rebooting; the Log tab has the command."
        ),
        Some(c) => format!("The install script exited with code {c}{detail} (see {INSTALL_LOG})"),
        None => "The install script was killed before it finished.".to_string(),
    }
}

/// Invoke the privileged install script via pkexec and wait for it,
/// passing every line it prints to `on_line` as it runs.
///
/// The install is repo-style: the new driver goes on disk while the
/// current one keeps running, and the switch happens at the next
/// reboot. Nothing touches the live session, so a plain blocking call
/// is safe — the GUI stays up the whole time.
pub fn run_privileged_install(opts: &InstallOptions, mut on_line: impl FnMut(&str)) -> Result<()> {
    let script = PRIVILEGED_SCRIPT;

    if !Path::new(script).exists() {
        bail!("Privileged install script not found at {}", script);
    }
    check_run_file(&opts.run_file)?;

    let sha256 = match &opts.sha256 {
        Some(h) => h.clone(),
        None => sha256_file(Path::new(&opts.run_file))?,
    };

    let mut args = vec![script.to_string(), opts.run_file.clone(), sha256];
    if opts.use_dkms {
        args.push("--dkms".to_string());
    }
    if opts.block_distro_driver {
        args.push("--block-distro-driver".to_string());
    }
    if opts.discard_recovery {
        args.push("--discard-recovery".to_string());
    }
    if let Some(arg) = opts.module.installer_arg() {
        args.push(arg.to_string());
    }

    let run = run_pkexec(&args, None, &mut on_line)?;
    if run.code == Some(0) {
        return Ok(());
    }
    if run.code == Some(6) {
        return Err(stale_recovery(&run.lines));
    }
    bail!("{}", install_failure(run.code, &run.lines))
}

/// Result of `--setup-signing`: whether it succeeded and the script's
/// output lines for the Log tab.
pub struct SetupOutcome {
    pub result: Result<()>,
    pub lines: Vec<String>,
}

/// Run the privileged script's one-time signing setup: make sure the
/// machine has a module-signing key and queue its certificate for MOK
/// enrollment. The one-time password goes to the script on stdin, never on
/// the command line.
pub fn run_privileged_setup_signing(password: &str) -> SetupOutcome {
    let fail = |e: anyhow::Error| SetupOutcome { result: Err(e), lines: vec![] };
    if !Path::new(PRIVILEGED_SCRIPT).exists() {
        return fail(anyhow::anyhow!("Privileged install script not found at {}", PRIVILEGED_SCRIPT));
    }
    let args = [PRIVILEGED_SCRIPT.to_string(), "--setup-signing".to_string()];
    let run = match run_pkexec(&args, Some(password), &mut |_| {}) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let result = match run.code {
        Some(0) => Ok(()),
        Some(126) => Err(anyhow::anyhow!("Authentication was cancelled.")),
        Some(127) => Err(anyhow::anyhow!("Not authorized, or pkexec could not start the script.")),
        code => Err(anyhow::anyhow!(
            "Signing setup failed{}",
            first_error(&run.lines).map(|e| format!(": {e}")).unwrap_or_else(|| format!(" (exit code {code:?}; see {INSTALL_LOG})"))
        )),
    };
    SetupOutcome { result, lines: run.lines }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pkexec_codes_are_not_script_failures() {
        let out = lines(&["ERROR: something"]);
        assert!(install_failure(Some(126), &out).contains("cancelled"));
        assert!(install_failure(Some(127), &out).contains("Not authorized"));
        for code in [1, 2, 3, 4, 5, 9] {
            let msg = install_failure(Some(code), &out);
            assert!(!msg.contains("cancelled") && !msg.contains("Not authorized"), "{code}: {msg}");
        }
    }

    #[test]
    fn failure_messages_carry_the_reason() {
        let out =
            lines(&["==== NVIDIA driver install started ====", "ERROR: /home/a/x.run can be changed by other members of group 100 (mode 664) — refusing."]);
        let msg = install_failure(Some(2), &out);
        assert!(msg.contains("refused") && msg.contains("other members of group 100"), "{msg}");
        let out =
            lines(&["ERROR: NVIDIA installer exited with code 1", "See /var/log/nvidia-installer.log for details.", "Restoring the previous driver setup…"]);
        let msg = install_failure(Some(3), &out);
        assert!(msg.contains("NVIDIA installer exited with code 1"), "{msg}");
        assert!(msg.contains("driver this computer had before"), "{msg}");
        assert!(install_failure(Some(4), &out).contains("Don't reboot"));
        assert!(install_failure(Some(1), &[]).ends_with("before changing anything"));
        assert!(install_failure(None, &[]).contains("killed"));
    }

    #[test]
    fn script_lines_are_this_runs_only() {
        let log = "2026-10-12 10:00:00 [green-light 42] ==== NVIDIA driver install started ====\n\
                   Reading package lists...\n\
                   2026-10-12 10:00:00 [green-light 77] ERROR: Another Green Light install is still running\n\
                   2026-10-12 10:00:01 [green-light 42] Copying installer\n\
                   2026-10-12 10:00:02 [green-light 42] \n";
        assert_eq!(script_lines(log.as_bytes(), 42), ["==== NVIDIA driver install started ====", "Copying installer"]);
        assert_eq!(script_lines(log.as_bytes(), 77), ["ERROR: Another Green Light install is still running"]);
    }

    #[test]
    fn log_tail_keeps_characters_and_follows_rotation() {
        let dir = std::env::temp_dir().join(format!("green-light-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log");
        std::fs::write(&path, "t [green-light 5] before\n").unwrap();
        let mut tail = LogTail::at_end(&path);
        assert!(tail.read_new(5).is_empty());
        let append = |bytes: &[u8]| std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(bytes).unwrap();
        // "…" is three bytes; the first read ends inside it.
        let line = "t [green-light 5] Copying…done\n".as_bytes();
        let cut = line.iter().position(|&b| b == 0xE2).unwrap() + 1;
        append(&line[..cut]);
        assert!(tail.read_new(5).is_empty());
        append(&line[cut..]);
        assert_eq!(tail.read_new(5), ["Copying…done"]);
        // Rotated: a new file, already longer than where the old one was read to.
        std::fs::rename(&path, dir.join("log.1")).unwrap();
        std::fs::write(&path, "t [green-light 5] after rotation, a long first line here\n").unwrap();
        assert_eq!(tail.read_new(5), ["after rotation, a long first line here"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_recovery_is_its_own_error() {
        let e = install_failure_or_stale(Some(6), &lines(&["ERROR: Not restoring: the driver changed"]));
        assert!(e.downcast_ref::<StaleRecovery>().is_some(), "{e}");
        assert!(e.to_string().contains("the driver changed"));
        let e = recover_result(Some(6), &[]).unwrap_err();
        assert!(e.downcast_ref::<StaleRecovery>().is_some());
        assert_eq!(parse_recovery_note("refused: the driver changed\n"), Some(RecoveryNote::Refused("the driver changed".into())));
        assert_eq!(parse_recovery_note("failed: x"), Some(RecoveryNote::Failed("x".into())));
        assert_eq!(parse_recovery_note(""), None);
    }

    fn install_failure_or_stale(code: Option<i32>, lines: &[String]) -> anyhow::Error {
        if code == Some(6) {
            stale_recovery(lines)
        } else {
            anyhow::anyhow!(install_failure(code, lines))
        }
    }

    #[test]
    fn recovery_status_matches_the_script() {
        let script = include_str!("../scripts/privileged-install.sh");
        assert!(script.contains("\nRECOVERY_STATUS=\"$STATE_DIR/recovery-status\"\n"));
        assert_eq!(RECOVERY_STATUS, "/var/lib/green-light/recovery-status");
    }

    #[test]
    fn recover_codes() {
        assert_eq!(recover_result(Some(0), &[]).unwrap(), Recovered::Nothing);
        assert_eq!(recover_result(Some(3), &[]).unwrap(), Recovered::Restored);
        let failed = recover_result(Some(4), &lines(&["ERROR: no module"])).unwrap_err().to_string();
        assert!(failed.contains("Don't reboot") && failed.contains("no module"), "{failed}");
        assert!(recover_result(Some(126), &[]).unwrap_err().to_string().contains("cancelled"));
    }

    #[test]
    fn discard_codes() {
        assert!(discard_result(Some(0), &[]).is_ok());
        assert!(discard_result(Some(126), &[]).unwrap_err().to_string().contains("cancelled"));
        let refused = discard_result(Some(127), &[]).unwrap_err().to_string();
        assert!(refused.contains("Not authorized") && !refused.contains("exit code"), "{refused}");
        let failed = discard_result(Some(1), &lines(&["ERROR: busy"])).unwrap_err().to_string();
        assert!(failed.contains("exit code Some(1)") && failed.contains("busy"), "{failed}");
    }

    #[test]
    fn journal_matches_the_script() {
        let script = include_str!("../scripts/privileged-install.sh");
        assert!(script.contains("\nSTATE_DIR=\"/var/lib/green-light\"\n"));
        assert!(script.contains("\nJOURNAL=\"$STATE_DIR/journal\"\n"));
        assert_eq!(JOURNAL_DIR, "/var/lib/green-light/journal");
        let unit = include_str!("../data/green-light-recover.service");
        assert!(unit.contains(&format!("ConditionPathExists={JOURNAL_DIR}\n")));
        assert!(unit.contains(&format!("ExecStart={PRIVILEGED_SCRIPT} --recover\n")));
    }

    #[test]
    fn lock_matches_the_script_and_is_detected() {
        let script = include_str!("../scripts/privileged-install.sh");
        assert!(script.contains(&format!("\nINSTALL_LOCK=\"{INSTALL_LOCK}\"\n")));
        let path = std::env::temp_dir().join(format!("green-light-lock-{}", std::process::id()));
        let holder = std::fs::File::create(&path).unwrap();
        assert!(!lock_held(&path));
        holder.lock().unwrap();
        assert!(lock_held(&path));
        holder.unlock().unwrap();
        assert!(!lock_held(&path));
        assert!(!lock_held(Path::new("/nonexistent/green-light.lock")));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn first_error_is_the_cause() {
        let out = lines(&["ok", "ERROR: first  ", "ERROR: second", "Verify: fine"]);
        assert_eq!(first_error(&out), Some("first"));
        assert_eq!(first_error(&lines(&["WARNING: x"])), None);
    }

    #[test]
    fn run_file_checks_on_real_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("green-light-runfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("NVIDIA-Linux-x86_64-1.0.run");
        std::fs::write(&file, b"x").unwrap();
        let path = file.to_str().unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        check_run_file(path).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o646)).unwrap();
        assert!(check_run_file(path).unwrap_err().to_string().contains("every user"));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let link = dir.join("link.run");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(check_run_file(link.to_str().unwrap()).unwrap_err().to_string().contains("symbolic link"));
        assert!(check_run_file(dir.join("missing.run").to_str().unwrap()).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
