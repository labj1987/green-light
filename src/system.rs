//! system.rs — Query the local system for GPU, driver, kernel, disk, and boot info.

use crate::preflight::{
    classify_signing_key, parse_compute_caps, parse_dkms_signing_config, parse_dpkg_installed, parse_module_flavor, parse_module_signer, parse_pending_list,
    parse_rpm_names, resolve_signing_pair, ComputeCap, ModuleFlavor, ModuleSignature, SigningKeyState, SigningPair, APT_BLOCK_FILE, DEB_DRIVER_GLOBS,
    DNF_BLOCK_FILE, RPM_DRIVER_GLOBS,
};
use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct SystemInfo {
    pub installed_driver: Option<String>,
    pub gpu_name: Option<String>,
    /// CUDA compute capability per GPU, used to tell GPU generations apart.
    pub gpu_compute_caps: Vec<ComputeCap>,
    /// Flavor of the kernel module that is loaded right now.
    pub module_flavor: ModuleFlavor,
    pub kernel_version: String,
    pub dkms_status: Vec<DkmsEntry>,
    pub secure_boot: SecureBootStatus,
    pub signing_key: SigningKeyState,
    /// The key pair the install script would sign with.
    pub signing_pair: Option<SigningPair>,
    /// Signature on the nvidia module on disk for the running kernel.
    pub module_signature: ModuleSignature,
    /// Distro-packaged NVIDIA driver packages the install will remove.
    pub distro_packages: Vec<String>,
    /// The install script's block on the distro driver packages is in place.
    pub distro_driver_blocked: bool,
    /// This package manager can block them (apt, or dnf5; not dnf4).
    pub block_supported: bool,
    pub free_disk_bytes: Option<u64>,
    pub reboot_required: bool,
}

#[derive(Debug, Clone)]
pub struct DkmsEntry {
    pub module: String,
    pub version: String,
    pub kernel: String,
    pub status: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum SecureBootStatus {
    Enabled,
    Disabled,
    #[default]
    Unknown,
}

impl std::fmt::Display for SecureBootStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Enabled => write!(f, "Enabled"),
            Self::Disabled => write!(f, "Disabled"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

pub fn query_system() -> SystemInfo {
    let secure_boot = get_secure_boot();
    let (signing_key, signing_pair) = get_signing_key_state();
    SystemInfo {
        installed_driver: get_installed_driver(),
        gpu_name: get_gpu_name(),
        gpu_compute_caps: get_gpu_compute_caps(),
        module_flavor: get_module_flavor(),
        kernel_version: get_kernel_version(),
        dkms_status: get_dkms_status(),
        secure_boot,
        signing_key,
        signing_pair,
        module_signature: get_module_signature(),
        distro_packages: get_distro_packages(),
        distro_driver_blocked: std::path::Path::new(APT_BLOCK_FILE).is_file() || std::path::Path::new(DNF_BLOCK_FILE).is_file(),
        block_supported: on_path("apt-get") || on_path("dnf5"),
        free_disk_bytes: get_free_disk(),
        reboot_required: check_reboot_required(),
    }
}

/// Compute capability of each GPU (`nvidia-smi`, driver 510+). Empty when no
/// driver is running or the field is unsupported.
fn get_gpu_compute_caps() -> Vec<ComputeCap> {
    Command::new("nvidia-smi")
        .args(["--query-gpu=compute_cap", "--format=csv,noheader"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_compute_caps(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// Open or proprietary flavor of the loaded kernel module.
fn get_module_flavor() -> ModuleFlavor {
    std::fs::read_to_string("/proc/driver/nvidia/version").map(|t| parse_module_flavor(&t)).unwrap_or_default()
}

/// Whether an executable `name` is on PATH.
fn on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| std::fs::metadata(dir.join(name)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
    })
}

/// Installed distro driver packages (apt or rpm), with the install script's
/// package globs. Empty when none, or when neither package manager can be
/// queried.
fn get_distro_packages() -> Vec<String> {
    // dpkg-query exits non-zero when a glob matches nothing but still prints
    // the matches of the others, so only the output is used.
    if let Ok(out) = Command::new("dpkg-query").args(["-W", "-f", "${db:Status-Abbrev}|${Package}\\n"]).args(DEB_DRIVER_GLOBS).output() {
        return parse_dpkg_installed(&String::from_utf8_lossy(&out.stdout));
    }
    if let Ok(out) = Command::new("rpm").args(["-qa", "--qf", "%{NAME}\\n"]).args(RPM_DRIVER_GLOBS).output() {
        return parse_rpm_names(&String::from_utf8_lossy(&out.stdout));
    }
    vec![]
}

/// The DKMS signing settings, read the way DKMS reads them.
fn get_dkms_signing_config() -> crate::preflight::DkmsSigningConfig {
    let mut paths = vec![std::path::PathBuf::from("/etc/dkms/framework.conf")];
    if let Ok(dir) = std::fs::read_dir("/etc/dkms/framework.conf.d") {
        let mut extra: Vec<_> = dir.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "conf")).collect();
        extra.sort();
        paths.extend(extra);
    }
    let texts: Vec<String> = paths.iter().filter_map(|p| std::fs::read_to_string(p).ok()).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    parse_dkms_signing_config(&refs, &get_kernel_version())
}

fn command_text(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    Some(format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

/// The machine's module-signing key pair, chosen as the install script
/// chooses it, and whether its certificate is enrolled, queued for
/// enrollment, or neither. Checked whatever the Secure Boot state. (The
/// private keys are root-only to read, but their existence can be seen.)
fn get_signing_key_state() -> (SigningKeyState, Option<SigningPair>) {
    let pair = resolve_signing_pair(&get_dkms_signing_config(), file_exists);
    let state = match &pair {
        None => SigningKeyState::NoKey,
        Some(p) if !p.complete => SigningKeyState::Incomplete(p.key.clone(), p.cert.clone()),
        Some(p) => cert_enrollment(&p.cert),
    };
    (state, pair)
}

/// Whether `path` is a file: `None` when an ordinary user can't tell, as for
/// a private key in a directory only root may search (Fedora's
/// /etc/pki/akmods/private).
fn file_exists(path: &str) -> Option<bool> {
    match std::fs::metadata(path) {
        Ok(m) => Some(m.is_file()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => None,
        Err(_) => Some(false),
    }
}

const MOK_NEW: &str = "/sys/firmware/efi/efivars/MokNew-605dab50-e046-4300-abb6-3dd810dd8b23";
/// The shared list of waiting keys, written by this app's signing setup or
/// another app's (they rebuild the same request).
const MOK_PENDING_LIST: &str = "/var/lib/mok-pending/list";

/// Enrollment state of the certificate at `path`.
fn cert_enrollment(path: &str) -> SigningKeyState {
    // Pending: an enrollment request exists, unchanged since the last
    // signing setup, and that setup listed this key in it (MokNew itself is
    // readable by root only, but a plain user can stat it).
    let moknew_size = std::fs::metadata(MOK_NEW).ok().map(|m| m.len());
    let pending = std::fs::read_to_string(MOK_PENDING_LIST).map(|t| parse_pending_list(&t, moknew_size)).unwrap_or_default();
    let efi_firmware = std::path::Path::new("/sys/firmware/efi").exists();
    let sha1 = Command::new("sha1sum")
        .arg(path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().next().map(str::to_string));
    command_text("mokutil", &["--test-key", path])
        .and_then(|t| classify_signing_key(path, &t, efi_firmware, &pending, sha1.as_deref()))
        .unwrap_or_else(|| SigningKeyState::Untested(path.to_string()))
}

/// Who signed the nvidia module on disk for the running kernel.
fn get_module_signature() -> ModuleSignature {
    match Command::new("modinfo").args(["-F", "signer", "nvidia"]).output() {
        Ok(out) => parse_module_signer(out.status.success(), &String::from_utf8_lossy(&out.stdout)),
        Err(_) => ModuleSignature::Missing,
    }
}

/// Read the running driver version from nvidia-smi or /proc
fn get_installed_driver() -> Option<String> {
    // Try nvidia-smi first
    if let Ok(out) = Command::new("nvidia-smi").args(["--query-gpu=driver_version", "--format=csv,noheader"]).output() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() && out.status.success() {
            return Some(s);
        }
    }

    // Fallback: /proc/driver/nvidia/version
    if let Ok(content) = std::fs::read_to_string("/proc/driver/nvidia/version") {
        return parse_proc_driver_version(&content);
    }

    None
}

/// Extract the version from /proc/driver/nvidia/version. Proprietary format:
///   "NVRM version: NVIDIA UNIX x86_64 Kernel Module  595.84  ..."
/// Open-module format puts "for x86_64" after "Module":
///   "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  595.84  ..."
/// so the version is the first token after "Module" that starts with a digit.
fn parse_proc_driver_version(content: &str) -> Option<String> {
    for line in content.lines().filter(|l| l.contains("NVRM version")) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let Some(i) = parts.iter().position(|p| *p == "Module") {
            if let Some(ver) = parts[i + 1..].iter().find(|p| p.starts_with(|c: char| c.is_ascii_digit())) {
                return Some(ver.to_string());
            }
        }
    }
    None
}

/// Get the GPU name from nvidia-smi
fn get_gpu_name() -> Option<String> {
    let out = Command::new("nvidia-smi").args(["--query-gpu=name", "--format=csv,noheader"]).output().ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// uname -r
fn get_kernel_version() -> String {
    Command::new("uname").arg("-r").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_else(|_| "unknown".to_string())
}

/// Parse `dkms status` output for nvidia entries. Handles all formats:
///   old:     "nvidia/595.84, 7.0.0-27-generic, x86_64: installed"
///   new:     "nvidia/595.84/7.0.0-27-generic/x86_64: installed"
///   partial: "nvidia/595.84: added"
fn get_dkms_status() -> Vec<DkmsEntry> {
    let out = match Command::new("dkms").arg("status").output() {
        Ok(o) => o,
        Err(_) => return vec![],
    };
    parse_dkms_status(&String::from_utf8_lossy(&out.stdout))
}

/// Parse the text of `dkms status` (see `get_dkms_status` for the formats).
fn parse_dkms_status(text: &str) -> Vec<DkmsEntry> {
    let mut entries = vec![];

    for raw in text.lines() {
        let line = raw.trim();
        if !line.to_lowercase().starts_with("nvidia") {
            continue;
        }

        // Status is everything after the LAST colon; the module spec is
        // everything before it. rsplit_once keeps this correct even if a
        // future kernel string ever contains a colon.
        let (spec, status) = match line.rsplit_once(':') {
            Some((l, r)) => (l.trim(), r.trim().to_string()),
            None => (line, "unknown".to_string()),
        };

        // The spec fields are separated by '/' (new) or ', ' (old).
        let fields: Vec<&str> = spec.split(['/', ',']).map(str::trim).filter(|s| !s.is_empty()).collect();

        let module = fields.first().unwrap_or(&"nvidia").to_string();
        let version = fields.get(1).unwrap_or(&"?").to_string();
        // "added" lines have no kernel yet — show a dash instead of garbage
        let kernel = fields.get(2).unwrap_or(&"\u{2014}").to_string();

        entries.push(DkmsEntry { module, version, kernel, status });
    }
    entries
}

/// Check secure boot via mokutil
fn get_secure_boot() -> SecureBootStatus {
    if let Ok(out) = Command::new("mokutil").arg("--sb-state").output() {
        let s = String::from_utf8_lossy(&out.stdout).to_lowercase();
        if s.contains("secureboot enabled") {
            return SecureBootStatus::Enabled;
        }
        if s.contains("secureboot disabled") {
            return SecureBootStatus::Disabled;
        }
    }
    // Fallback: read EFI variable directly
    if let Ok(content) = std::fs::read("/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c") {
        // Byte 4 is the value: 1 = enabled
        if content.get(4).copied() == Some(1) {
            return SecureBootStatus::Enabled;
        } else {
            return SecureBootStatus::Disabled;
        }
    }
    SecureBootStatus::Unknown
}

/// Free disk space on /
fn get_free_disk() -> Option<u64> {
    // Use statvfs via df -B1 for simplicity
    let out = Command::new("df").args(["-B1", "--output=avail", "/"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().nth(1).and_then(|l| l.trim().parse::<u64>().ok())
}

/// Version of the nvidia kernel module ON DISK — i.e. what will load at
/// the next boot. This is how a pending repo-style install is detected.
fn get_disk_module_version() -> Option<String> {
    let out = Command::new("modinfo").args(["-F", "version", "nvidia"]).output().ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// A reboot is required when the driver on disk differs from the one
/// currently running — i.e. a new driver was installed and is waiting
/// for the next boot to take over.
pub fn check_reboot_required() -> bool {
    let running = get_installed_driver(); // reports the RUNNING driver
    let on_disk = get_disk_module_version();

    match (running, on_disk) {
        (Some(r), Some(d)) => r != d,
        // Module present on disk but nothing running — needs a boot
        (None, Some(_)) => true,
        _ => false,
    }
}

/// Minimum disk space required for a driver download + install (bytes)
pub const MIN_DISK_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GB

pub fn format_bytes(b: u64) -> String {
    if b >= 1_073_741_824 {
        format!("{:.1} GB", b as f64 / 1_073_741_824.0)
    } else if b >= 1_048_576 {
        format!("{:.1} MB", b as f64 / 1_048_576.0)
    } else {
        format!("{} KB", b / 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 KB");
        assert_eq!(format_bytes(2048), "2 KB");
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
        assert_eq!(format_bytes(1_572_864), "1.5 MB");
        assert_eq!(format_bytes(1_073_741_824), "1.0 GB");
        assert_eq!(format_bytes(5 * 1_073_741_824 / 2), "2.5 GB");
    }

    #[test]
    fn proc_version_both_flavors() {
        let prop = "NVRM version: NVIDIA UNIX x86_64 Kernel Module  595.84  Tue Sep  1 2026\n";
        let open = "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  595.84  Release Build\n";
        assert_eq!(parse_proc_driver_version(prop).as_deref(), Some("595.84"));
        assert_eq!(parse_proc_driver_version(open).as_deref(), Some("595.84"));
        assert_eq!(parse_proc_driver_version("GCC version: gcc 15"), None);
    }

    #[test]
    fn dkms_old_format() {
        let e = parse_dkms_status("nvidia/595.84, 7.0.0-27-generic, x86_64: installed\n");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].module, "nvidia");
        assert_eq!(e[0].version, "595.84");
        assert_eq!(e[0].kernel, "7.0.0-27-generic");
        assert_eq!(e[0].status, "installed");
    }

    #[test]
    fn dkms_new_format() {
        let e = parse_dkms_status("nvidia/595.84/7.0.0-27-generic/x86_64: installed");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].version, "595.84");
        assert_eq!(e[0].kernel, "7.0.0-27-generic");
        assert_eq!(e[0].status, "installed");
    }

    #[test]
    fn dkms_partial_added_has_dash_kernel() {
        let e = parse_dkms_status("nvidia/595.84: added");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kernel, "\u{2014}");
        assert_eq!(e[0].status, "added");
    }

    #[test]
    fn dkms_ignores_other_modules_and_blank_lines() {
        let e = parse_dkms_status("\nvirtualbox/7.0, 6.8.0, x86_64: installed\n  \nNVIDIA/1.0: added\n");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].module, "NVIDIA");
    }
}
