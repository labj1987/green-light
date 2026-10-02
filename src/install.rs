use crate::preflight::ModuleChoice;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::process::Command;

const INSTALL_LOG: &str = "/var/log/green-light.log";
const RUN_START_MARKER: &str = "==== NVIDIA driver install started";

pub struct InstallOptions {
    pub use_dkms: bool,
    pub hold_packages: bool,
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

/// Invoke the privileged install script via pkexec and wait for it.
///
/// The install is repo-style: the new driver goes on disk while the
/// current one keeps running, and the switch happens at the next
/// reboot. Nothing touches the live session, so a plain blocking call
/// is safe — the GUI stays up the whole time.
pub fn run_privileged_install(opts: &InstallOptions) -> Result<()> {
    let script = "/usr/lib/green-light/privileged-install.sh";

    if !Path::new(script).exists() {
        bail!("Privileged install script not found at {}", script);
    }
    if !Path::new(&opts.run_file).exists() {
        bail!("Run file not found: {}", opts.run_file);
    }

    let sha256 = match &opts.sha256 {
        Some(h) => h.clone(),
        None => sha256_file(Path::new(&opts.run_file))?,
    };

    let mut args = vec![script.to_string(), opts.run_file.clone(), sha256];
    if opts.use_dkms {
        args.push("--dkms".to_string());
    }
    if opts.hold_packages {
        args.push("--hold".to_string());
    }
    if let Some(arg) = opts.module.installer_arg() {
        args.push(arg.to_string());
    }

    let status = Command::new("pkexec")
        .args(&args)
        .status()
        .context("Failed to launch pkexec — is polkit installed?")?;

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        if code == 126 || code == 127 {
            bail!("Authentication was cancelled.");
        }
        bail!(
            "Install script exited with code {} (see /var/log/green-light.log)",
            code
        );
    }

    Ok(())
}

/// Lines of the most recent install run in the log that carry its post-install
/// verification results, warnings, errors, and DKMS build-log excerpts.
pub fn install_report() -> Vec<String> {
    std::fs::read_to_string(INSTALL_LOG)
        .map(|t| extract_report_lines(&t))
        .unwrap_or_default()
}

fn extract_report_lines(log: &str) -> Vec<String> {
    let lines: Vec<&str> = log.lines().collect();
    let Some(start) = lines.iter().rposition(|l| l.contains(RUN_START_MARKER)) else {
        return vec![];
    };
    lines[start..]
        .iter()
        .filter(|l| {
            l.contains("Verify:") || l.contains("WARNING") || l.contains("ERROR")
                || l.contains("Build log:")
        })
        .map(|l| match l.split_once("[green-light] ") {
            Some((_, msg)) => msg.to_string(),
            None => l.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_uses_only_the_last_run() {
        let log = "2026-10-01 10:00:00 [green-light] ==== NVIDIA driver install started ====\n\
                   2026-10-01 10:00:01 [green-light] Verify: old run\n\
                   2026-10-02 09:00:00 [green-light] ==== NVIDIA driver install started ====\n\
                   2026-10-02 09:00:01 [green-light] Running the NVIDIA installer\n\
                   2026-10-02 09:00:09 [green-light] Verify: nouveau blacklist present\n\
                   2026-10-02 09:00:10 [green-light] Verify: WARNING initramfs check failed\n\
                   2026-10-02 09:00:11 [green-light] ==== Done. ====\n";
        assert_eq!(
            extract_report_lines(log),
            vec![
                "Verify: nouveau blacklist present".to_string(),
                "Verify: WARNING initramfs check failed".to_string(),
            ]
        );
    }

    #[test]
    fn report_empty_without_marker() {
        assert!(extract_report_lines("nothing here\n").is_empty());
        assert!(extract_report_lines("").is_empty());
    }
}
