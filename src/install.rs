use crate::preflight::{run_file_problem, FileFacts, ModuleChoice};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};

const PRIVILEGED_SCRIPT: &str = "/usr/lib/green-light/privileged-install.sh";
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
}

/// Hash a file with SHA256, streaming so a ~300 MB .run isn't held in memory.
fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("Could not open {} for hashing", path.display()))?;
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
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("Run file not found: {path}"))?;
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
    match run_file_problem(path, facts, caller, &passwd, group.as_deref()) {
        Some(problem) => bail!("{problem}"),
        None => Ok(()),
    }
}

/// What a privileged run printed, and how it ended.
struct ScriptRun {
    code: Option<i32>,
    lines: Vec<String>,
}

/// Run `pkexec <args>`, handing each output line (stdout and stderr, with the
/// script's "[green-light] " prefix removed) to `on_line` as it arrives.
fn run_pkexec(args: &[String], stdin: Option<&str>, on_line: &mut dyn FnMut(&str)) -> Result<ScriptRun> {
    let mut child = Command::new("pkexec")
        .args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to launch pkexec — is polkit installed?")?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A write error means the script already exited (e.g. cancelled
        // authentication); its exit code says why.
        let _ = writeln!(pipe, "{text}");
    }

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let readers: Vec<_> = [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|pipe| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        })
    })
    .collect();
    drop(tx);

    let mut lines = Vec::new();
    for line in rx {
        let line = line.strip_prefix("[green-light] ").unwrap_or(&line).to_string();
        if line.trim().is_empty() {
            continue;
        }
        on_line(&line);
        lines.push(line);
    }
    for r in readers {
        let _ = r.join();
    }
    let status = child.wait().context("Waiting for pkexec failed")?;
    Ok(ScriptRun { code: status.code(), lines })
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
        Some(127) => {
            "Not authorized, or pkexec could not start the install script; nothing was changed."
                .to_string()
        }
        Some(1) => format!("The install stopped before changing anything{detail}"),
        Some(2) => format!("The install script refused to start{detail}"),
        Some(3) => format!(
            "The install failed{detail}. Everything it had changed was put back, so the next \
             boot uses the driver this computer had before."
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
    if let Some(arg) = opts.module.installer_arg() {
        args.push(arg.to_string());
    }

    let run = run_pkexec(&args, None, &mut on_line)?;
    if run.code == Some(0) {
        return Ok(());
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
        return fail(anyhow::anyhow!(
            "Privileged install script not found at {}",
            PRIVILEGED_SCRIPT
        ));
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
            first_error(&run.lines)
                .map(|e| format!(": {e}"))
                .unwrap_or_else(|| format!(" (exit code {code:?}; see {INSTALL_LOG})"))
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
        let out = lines(&[
            "==== NVIDIA driver install started ====",
            "ERROR: /home/a/x.run can be changed by other members of group 100 (mode 664) — refusing.",
        ]);
        let msg = install_failure(Some(2), &out);
        assert!(msg.contains("refused") && msg.contains("other members of group 100"), "{msg}");
        let out = lines(&[
            "ERROR: NVIDIA installer exited with code 1",
            "See /var/log/nvidia-installer.log for details.",
            "Restoring the previous driver setup…",
        ]);
        let msg = install_failure(Some(3), &out);
        assert!(msg.contains("NVIDIA installer exited with code 1"), "{msg}");
        assert!(msg.contains("driver this computer had before"), "{msg}");
        assert!(install_failure(Some(4), &out).contains("Don't reboot"));
        assert!(install_failure(Some(1), &[]).ends_with("before changing anything"));
        assert!(install_failure(None, &[]).contains("killed"));
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
