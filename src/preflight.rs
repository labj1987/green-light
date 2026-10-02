//! preflight.rs — Pure decision logic for pre-install checks: kernel module
//! flavor (open vs proprietary), conflicting distro packages, and Secure Boot
//! signing advice. Nothing here touches the system; `system.rs` gathers the
//! raw facts and `ui.rs` shows the results.

use crate::system::SecureBootStatus;

// ─────────────────────────────────────────────────────────────────────────────
//  Kernel module flavor (open vs proprietary)
// ─────────────────────────────────────────────────────────────────────────────

/// What the user asked the installer to build. `Auto` passes no flag and lets
/// nvidia-installer decide from the detected GPUs (open for Turing and newer,
/// proprietary when pre-Turing hardware is present).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModuleChoice {
    #[default]
    Auto,
    Open,
    Proprietary,
}

impl ModuleChoice {
    /// The nvidia-installer argument for this choice (`--kernel-module-type`,
    /// short form `-M`), or `None` for `Auto`.
    pub fn installer_arg(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Open => Some("--kernel-module-type=open"),
            Self::Proprietary => Some("--kernel-module-type=proprietary"),
        }
    }
}

/// First driver major that ships the open kernel modules.
const MIN_OPEN_DRIVER_MAJOR: u32 = 515;

/// CUDA compute capability as (major, minor), from nvidia-smi.
pub type ComputeCap = (u32, u32);

/// Parse one line of `nvidia-smi --query-gpu=compute_cap` output ("8.9").
pub fn parse_compute_cap(line: &str) -> Option<ComputeCap> {
    let (maj, min) = line.trim().split_once('.')?;
    Some((maj.parse().ok()?, min.parse().ok()?))
}

/// Parse the multi-line nvidia-smi output, skipping lines that don't parse
/// (older drivers print "[N/A]" or reject the field entirely).
pub fn parse_compute_caps(text: &str) -> Vec<ComputeCap> {
    text.lines().filter_map(parse_compute_cap).collect()
}

/// GPU architecture name from compute capability.
pub fn generation_name(cc: ComputeCap) -> &'static str {
    match cc {
        (maj, _) if maj >= 10 => "Blackwell or newer",
        (9, _) => "Hopper",
        (8, 9) => "Ada",
        (8, _) => "Ampere",
        (7, 5) => "Turing",
        (7, _) => "Volta",
        (6, _) => "Pascal",
        (5, _) => "Maxwell",
        _ => "Pre-Maxwell",
    }
}

fn is_pre_turing(cc: ComputeCap) -> bool {
    cc < (7, 5)
}

fn requires_open(cc: ComputeCap) -> bool {
    cc.0 >= 10
}

/// One-line description of what the installer will do for the detected GPUs.
pub fn module_hint(caps: &[ComputeCap]) -> String {
    if caps.is_empty() {
        return "Automatic: the installer picks from the detected GPUs".to_string();
    }
    let mut names: Vec<&str> = caps.iter().map(|c| generation_name(*c)).collect();
    names.dedup();
    let gens = names.join(", ");
    if caps.iter().any(|c| requires_open(*c)) {
        format!("{gens}: open kernel modules are required")
    } else if caps.iter().any(|c| is_pre_turing(*c)) {
        format!("{gens}: proprietary kernel modules only (open needs Turing or newer)")
    } else {
        format!("{gens}: open modules recommended, proprietary also works")
    }
}

/// Reject choices that cannot work, before asking for a password.
pub fn validate_module_choice(
    choice: ModuleChoice,
    caps: &[ComputeCap],
    driver_major: Option<u32>,
) -> Result<(), String> {
    match choice {
        ModuleChoice::Auto => Ok(()),
        ModuleChoice::Open => {
            if driver_major.is_some_and(|m| m < MIN_OPEN_DRIVER_MAJOR) {
                return Err(format!(
                    "Open kernel modules need driver {MIN_OPEN_DRIVER_MAJOR} or newer."
                ));
            }
            match caps.iter().find(|c| is_pre_turing(**c)) {
                Some(cc) => Err(format!(
                    "Open kernel modules need a Turing or newer GPU; this system has a {} GPU.",
                    generation_name(*cc)
                )),
                None => Ok(()),
            }
        }
        ModuleChoice::Proprietary => match caps.iter().find(|c| requires_open(**c)) {
            Some(cc) => Err(format!(
                "{} GPUs only work with the open kernel modules.",
                generation_name(*cc)
            )),
            None => Ok(()),
        },
    }
}

/// Which flavor of the NVIDIA kernel module is loaded right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModuleFlavor {
    Open,
    Proprietary,
    #[default]
    Unknown,
}

impl std::fmt::Display for ModuleFlavor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open => write!(f, "Open"),
            Self::Proprietary => write!(f, "Proprietary"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Classify /proc/driver/nvidia/version. The open module reports
/// "NVIDIA UNIX Open Kernel Module for x86_64 <ver>", the proprietary one
/// "NVIDIA UNIX x86_64 Kernel Module <ver>".
pub fn parse_module_flavor(proc_version: &str) -> ModuleFlavor {
    if proc_version.contains("Open Kernel Module") {
        ModuleFlavor::Open
    } else if proc_version.contains("Kernel Module") {
        ModuleFlavor::Proprietary
    } else {
        ModuleFlavor::Unknown
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Conflicting distro packages
// ─────────────────────────────────────────────────────────────────────────────

/// Driver packages from the distro (apt or rpm) that the install script
/// removes before running the .run installer. Mirrors the script's package
/// set: Docker's GPU plumbing (nvidia-container-toolkit, libnvidia-container)
/// is excluded, and Pop!_OS's system76-driver-nvidia metapackages are
/// included because they pin the distro driver.
pub fn is_driver_package(name: &str) -> bool {
    if name.starts_with("nvidia-container-toolkit") || name.starts_with("libnvidia-container") {
        return false;
    }
    name.starts_with("nvidia-")
        || name.starts_with("libnvidia-")
        || name.starts_with("xserver-xorg-video-nvidia")
        || name.starts_with("system76-driver-nvidia")
        || name.starts_with("akmod-nvidia")
        || name.starts_with("xorg-x11-drv-nvidia")
        || name.starts_with("kmod-nvidia")
}

/// Parse `dpkg-query -W -f '${db:Status-Abbrev}|${Package}\n'` output into the
/// installed driver package names (status "ii").
pub fn parse_dpkg_installed(text: &str) -> Vec<String> {
    let mut names: Vec<String> = text
        .lines()
        .filter_map(|l| l.split_once('|'))
        .filter(|(status, _)| status.trim() == "ii")
        .map(|(_, name)| name.trim().to_string())
        .filter(|n| is_driver_package(n))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Parse `rpm -qa --qf '%{NAME}\n'` output into driver package names.
pub fn parse_rpm_names(text: &str) -> Vec<String> {
    let mut names: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|n| is_driver_package(n))
        .map(str::to_string)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Human-readable warning for the detected distro packages, or `None`.
pub fn conflict_summary(packages: &[String]) -> Option<String> {
    if packages.is_empty() {
        return None;
    }
    let mut text = format!(
        "Will be removed before install: {}",
        packages.join(", ")
    );
    if packages.iter().any(|p| p.starts_with("system76-driver-nvidia")) {
        text.push_str(" (Pop!_OS driver packages)");
    }
    Some(text)
}

// ─────────────────────────────────────────────────────────────────────────────
//  Secure Boot signing key
// ─────────────────────────────────────────────────────────────────────────────

/// Public keys DKMS commonly signs modules with, in the DER form mokutil
/// takes: DKMS 3.x, Ubuntu's shim-signed, and Fedora's akmods.
pub const SIGNING_KEY_PATHS: [&str; 3] = [
    "/var/lib/dkms/mok.pub",
    "/var/lib/shim-signed/mok/MOK.der",
    "/etc/pki/akmods/certs/public_key.der",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SigningKeyState {
    /// A key was found and is enrolled in the firmware (MOK).
    Enrolled,
    /// A key was found but is not enrolled yet; holds its path.
    NotEnrolled(String),
    /// No readable key (some of these directories are root-only).
    #[default]
    Unknown,
}

/// Interpret `mokutil --test-key <file>` output.
pub fn parse_mok_test_key(output: &str) -> Option<bool> {
    let o = output.to_lowercase();
    if o.contains("already enrolled") {
        Some(true)
    } else if o.contains("not enrolled") {
        Some(false)
    } else {
        None
    }
}

/// Warning text for the pre-install checks. `None` means nothing to warn about.
pub fn secure_boot_advice(sb: &SecureBootStatus, key: &SigningKeyState) -> Option<String> {
    if *sb != SecureBootStatus::Enabled {
        return None;
    }
    Some(match key {
        SigningKeyState::Enrolled => {
            "Enabled. A DKMS signing key is already enrolled, so the new module should load."
                .to_string()
        }
        SigningKeyState::NotEnrolled(path) => format!(
            "Enabled, and the signing key {path} is not enrolled. Unsigned modules will not \
             load. Before rebooting, run as root: mokutil --import {path} (set a one-time \
             password), then reboot and choose Enroll MOK at the blue screen."
        ),
        SigningKeyState::Unknown => {
            "Enabled, and no readable DKMS signing key was found. Unsigned modules will not \
             load. Install with DKMS enabled, then run as root: mokutil --import \
             /var/lib/dkms/mok.pub (Ubuntu: /var/lib/shim-signed/mok/MOK.der), reboot, and \
             choose Enroll MOK. Or disable Secure Boot in firmware."
                .to_string()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installer_args() {
        assert_eq!(ModuleChoice::Auto.installer_arg(), None);
        assert_eq!(ModuleChoice::Open.installer_arg(), Some("--kernel-module-type=open"));
        assert_eq!(
            ModuleChoice::Proprietary.installer_arg(),
            Some("--kernel-module-type=proprietary")
        );
    }

    #[test]
    fn compute_cap_parsing() {
        assert_eq!(parse_compute_cap("8.9\n"), Some((8, 9)));
        assert_eq!(parse_compute_cap("12.0"), Some((12, 0)));
        assert_eq!(parse_compute_cap("[N/A]"), None);
        assert_eq!(parse_compute_caps("7.5\n8.6\n\nbad\n"), vec![(7, 5), (8, 6)]);
    }

    #[test]
    fn generation_mapping() {
        assert_eq!(generation_name((5, 2)), "Maxwell");
        assert_eq!(generation_name((6, 1)), "Pascal");
        assert_eq!(generation_name((7, 0)), "Volta");
        assert_eq!(generation_name((7, 5)), "Turing");
        assert_eq!(generation_name((8, 6)), "Ampere");
        assert_eq!(generation_name((8, 9)), "Ada");
        assert_eq!(generation_name((9, 0)), "Hopper");
        assert_eq!(generation_name((10, 0)), "Blackwell or newer");
        assert_eq!(generation_name((12, 0)), "Blackwell or newer");
    }

    #[test]
    fn module_choice_validation() {
        let pascal = [(6, 1)];
        let ada = [(8, 9)];
        let blackwell = [(12, 0)];
        assert!(validate_module_choice(ModuleChoice::Auto, &pascal, Some(580)).is_ok());
        assert!(validate_module_choice(ModuleChoice::Open, &pascal, Some(580)).is_err());
        assert!(validate_module_choice(ModuleChoice::Proprietary, &pascal, Some(580)).is_ok());
        assert!(validate_module_choice(ModuleChoice::Open, &ada, Some(595)).is_ok());
        assert!(validate_module_choice(ModuleChoice::Proprietary, &ada, Some(595)).is_ok());
        assert!(validate_module_choice(ModuleChoice::Proprietary, &blackwell, Some(595)).is_err());
        assert!(validate_module_choice(ModuleChoice::Open, &blackwell, Some(595)).is_ok());
        assert!(validate_module_choice(ModuleChoice::Open, &ada, Some(470)).is_err());
        // Unknown GPU or driver version: let the installer decide.
        assert!(validate_module_choice(ModuleChoice::Open, &[], None).is_ok());
        assert!(validate_module_choice(ModuleChoice::Proprietary, &[], None).is_ok());
    }

    #[test]
    fn mixed_gpus_block_open() {
        let mixed = [(8, 9), (6, 1)];
        assert!(validate_module_choice(ModuleChoice::Open, &mixed, Some(580)).is_err());
    }

    #[test]
    fn module_hint_text() {
        assert!(module_hint(&[(12, 0)]).contains("required"));
        assert!(module_hint(&[(6, 1)]).contains("proprietary kernel modules only"));
        assert!(module_hint(&[(8, 9)]).contains("recommended"));
        assert!(module_hint(&[]).starts_with("Automatic"));
    }

    #[test]
    fn flavor_from_proc_version() {
        let prop = "NVRM version: NVIDIA UNIX x86_64 Kernel Module  595.84  Tue Sep  1 2026";
        let open = "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  595.84  Release Build";
        assert_eq!(parse_module_flavor(prop), ModuleFlavor::Proprietary);
        assert_eq!(parse_module_flavor(open), ModuleFlavor::Open);
        assert_eq!(parse_module_flavor(""), ModuleFlavor::Unknown);
    }

    #[test]
    fn driver_package_filter() {
        assert!(is_driver_package("nvidia-driver-595"));
        assert!(is_driver_package("nvidia-dkms-595-open"));
        assert!(is_driver_package("libnvidia-gl-595"));
        assert!(is_driver_package("xserver-xorg-video-nvidia-595"));
        assert!(is_driver_package("system76-driver-nvidia"));
        assert!(is_driver_package("system76-driver-nvidia-open"));
        assert!(is_driver_package("akmod-nvidia"));
        assert!(!is_driver_package("nvidia-container-toolkit"));
        assert!(!is_driver_package("nvidia-container-toolkit-base"));
        assert!(!is_driver_package("libnvidia-container1"));
        assert!(!is_driver_package("libnvidia-container-tools"));
        assert!(!is_driver_package("libcuda1"));
        assert!(!is_driver_package("green-light"));
    }

    #[test]
    fn dpkg_output_parsing() {
        let text = "ii |nvidia-driver-595\nrc |nvidia-dkms-570\nii |nvidia-container-toolkit\n\
                    ii |libnvidia-gl-595\nii |nvidia-driver-595\nii |bash\n";
        assert_eq!(
            parse_dpkg_installed(text),
            vec!["libnvidia-gl-595".to_string(), "nvidia-driver-595".to_string()]
        );
        assert!(parse_dpkg_installed("").is_empty());
    }

    #[test]
    fn rpm_output_parsing() {
        let text = "akmod-nvidia\nxorg-x11-drv-nvidia-cuda\nbash\nakmod-nvidia\n";
        assert_eq!(
            parse_rpm_names(text),
            vec!["akmod-nvidia".to_string(), "xorg-x11-drv-nvidia-cuda".to_string()]
        );
    }

    #[test]
    fn conflict_text() {
        assert_eq!(conflict_summary(&[]), None);
        let s = conflict_summary(&["nvidia-driver-595".to_string()]).unwrap();
        assert!(s.contains("nvidia-driver-595"));
        assert!(!s.contains("Pop!_OS"));
        let p = conflict_summary(&["system76-driver-nvidia".to_string()]).unwrap();
        assert!(p.contains("Pop!_OS"));
    }

    #[test]
    fn mokutil_output() {
        assert_eq!(parse_mok_test_key("/var/lib/dkms/mok.pub is already enrolled\n"), Some(true));
        assert_eq!(parse_mok_test_key("/var/lib/dkms/mok.pub is not enrolled\n"), Some(false));
        assert_eq!(parse_mok_test_key("EFI variables are not supported"), None);
    }

    #[test]
    fn secure_boot_advice_cases() {
        let none = SigningKeyState::Unknown;
        assert_eq!(secure_boot_advice(&SecureBootStatus::Disabled, &none), None);
        assert_eq!(secure_boot_advice(&SecureBootStatus::Unknown, &none), None);
        let enrolled = secure_boot_advice(&SecureBootStatus::Enabled, &SigningKeyState::Enrolled);
        assert!(enrolled.unwrap().contains("already enrolled"));
        let pending = secure_boot_advice(
            &SecureBootStatus::Enabled,
            &SigningKeyState::NotEnrolled("/var/lib/dkms/mok.pub".into()),
        )
        .unwrap();
        assert!(pending.contains("mokutil --import /var/lib/dkms/mok.pub"));
        let unknown = secure_boot_advice(&SecureBootStatus::Enabled, &none).unwrap();
        assert!(unknown.contains("mokutil --import"));
    }
}
