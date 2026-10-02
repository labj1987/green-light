//! system.rs — Query the local system for GPU, driver, kernel, disk, and boot info.


use crate::preflight::{
    parse_compute_caps, parse_dpkg_installed, parse_module_flavor, parse_mok_test_key,
    parse_rpm_names, ComputeCap, ModuleFlavor, SigningKeyState, SIGNING_KEY_PATHS,
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
    /// Distro-packaged NVIDIA driver packages the install will remove.
    pub distro_packages: Vec<String>,
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
            Self::Enabled => write!(f, "Enabled (MOK enrollment may be required)"),
            Self::Disabled => write!(f, "Disabled"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

pub fn query_system() -> SystemInfo {
    let secure_boot = get_secure_boot();
    let signing_key = if secure_boot == SecureBootStatus::Enabled {
        get_signing_key_state()
    } else {
        SigningKeyState::Unknown
    };
    SystemInfo {
        installed_driver: get_installed_driver(),
        gpu_name: get_gpu_name(),
        gpu_compute_caps: get_gpu_compute_caps(),
        module_flavor: get_module_flavor(),
        kernel_version: get_kernel_version(),
        dkms_status: get_dkms_status(),
        secure_boot,
        signing_key,
        distro_packages: get_distro_packages(),
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
    std::fs::read_to_string("/proc/driver/nvidia/version")
        .map(|t| parse_module_flavor(&t))
        .unwrap_or_default()
}

/// Installed distro driver packages (apt or rpm). Empty when none, or when
/// neither package manager can be queried.
fn get_distro_packages() -> Vec<String> {
    const DEB_GLOBS: [&str; 4] =
        ["nvidia-*", "libnvidia-*", "xserver-xorg-video-nvidia*", "system76-driver-nvidia*"];
    const RPM_GLOBS: [&str; 5] = [
        "akmod-nvidia*", "xorg-x11-drv-nvidia*", "kmod-nvidia*", "nvidia-driver*",
        "nvidia-settings*",
    ];
    // dpkg-query exits non-zero when a glob matches nothing but still prints
    // the matches of the others, so only the output is used.
    if let Ok(out) = Command::new("dpkg-query")
        .args(["-W", "-f", "${db:Status-Abbrev}|${Package}\\n"])
        .args(DEB_GLOBS)
        .output()
    {
        return parse_dpkg_installed(&String::from_utf8_lossy(&out.stdout));
    }
    if let Ok(out) = Command::new("rpm")
        .args(["-qa", "--qf", "%{NAME}\\n"])
        .args(RPM_GLOBS)
        .output()
    {
        return parse_rpm_names(&String::from_utf8_lossy(&out.stdout));
    }
    vec![]
}

/// Whether a readable DKMS signing key is enrolled. Only meaningful with
/// Secure Boot on; some key directories are root-only, which shows up as
/// `Unknown`.
fn get_signing_key_state() -> SigningKeyState {
    for path in SIGNING_KEY_PATHS {
        if !std::path::Path::new(path).is_file() {
            continue;
        }
        if let Ok(out) = Command::new("mokutil").args(["--test-key", path]).output() {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            match parse_mok_test_key(&text) {
                Some(true) => return SigningKeyState::Enrolled,
                Some(false) => return SigningKeyState::NotEnrolled(path.to_string()),
                None => {}
            }
        }
    }
    SigningKeyState::Unknown
}

/// Read the running driver version from nvidia-smi or /proc
fn get_installed_driver() -> Option<String> {
    // Try nvidia-smi first
    if let Ok(out) = Command::new("nvidia-smi")
        .args(["--query-gpu=driver_version", "--format=csv,noheader"])
        .output()
    {
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
            if let Some(ver) = parts[i + 1..]
                .iter()
                .find(|p| p.starts_with(|c: char| c.is_ascii_digit()))
            {
                return Some(ver.to_string());
            }
        }
    }
    None
}

/// Get the GPU name from nvidia-smi
fn get_gpu_name() -> Option<String> {
    let out = Command::new("nvidia-smi")
        .args(["--query-gpu=name", "--format=csv,noheader"])
        .output()
        .ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() { return Some(s); }
    }
    None
}

/// uname -r
fn get_kernel_version() -> String {
    Command::new("uname")
        .arg("-r")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
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
        let fields: Vec<&str> = spec
            .split(['/', ','])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();

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
        if s.contains("secureboot enabled") { return SecureBootStatus::Enabled; }
        if s.contains("secureboot disabled") { return SecureBootStatus::Disabled; }
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
    let out = Command::new("df")
        .args(["-B1", "--output=avail", "/"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .nth(1)
        .and_then(|l| l.trim().parse::<u64>().ok())
}

/// Version of the nvidia kernel module ON DISK — i.e. what will load at
/// the next boot. This is how a pending repo-style install is detected.
fn get_disk_module_version() -> Option<String> {
    let out = Command::new("modinfo")
        .args(["-F", "version", "nvidia"])
        .output()
        .ok()?;
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
