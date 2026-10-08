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

/// DKMS's own default key pair, used when its config names no key.
pub const DKMS_DEFAULT_KEY: &str = "/var/lib/dkms/mok.key";
pub const DKMS_DEFAULT_CERT: &str = "/var/lib/dkms/mok.pub";

/// Module-signing (private key, certificate) pairs in the order the install
/// script looks for them when the DKMS config names no key: Ubuntu's
/// shim-signed MOK (DKMS's default on Ubuntu), DKMS's own default, and
/// Fedora's akmods key. Keep in step with `SIGNING_KEY_PAIRS` in
/// `scripts/privileged-install.sh`.
pub const SIGNING_KEY_PATHS: [(&str, &str); 3] = [
    ("/var/lib/shim-signed/mok/MOK.priv", "/var/lib/shim-signed/mok/MOK.der"),
    (DKMS_DEFAULT_KEY, DKMS_DEFAULT_CERT),
    ("/etc/pki/akmods/private/private_key.priv", "/etc/pki/akmods/certs/public_key.der"),
];

/// `mok_signing_key` / `mok_certificate` from the DKMS config files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DkmsSigningConfig {
    pub key: Option<String>,
    pub cert: Option<String>,
}

/// The value of one shell assignment as the shell would read it: a quoted
/// value up to its closing quote, a bare value up to the first whitespace.
/// `$kernelver` / `${kernelver}` expand to `kernel`, as DKMS allows.
fn conf_value(raw: &str, kernel: &str) -> String {
    let v = raw.trim_start();
    let v = if let Some(rest) = v.strip_prefix('"') {
        rest.split('"').next().unwrap_or("")
    } else if let Some(rest) = v.strip_prefix('\'') {
        rest.split('\'').next().unwrap_or("")
    } else {
        v.split_whitespace().next().unwrap_or("")
    };
    v.replace("${kernelver}", kernel).replace("$kernelver", kernel)
}

/// Parse the DKMS config files (framework.conf first, then
/// framework.conf.d/*.conf in sorted order). The last assignment wins, as
/// when DKMS sources them; an empty value unsets the setting.
pub fn parse_dkms_signing_config(files: &[&str], kernel: &str) -> DkmsSigningConfig {
    let mut cfg = DkmsSigningConfig::default();
    for text in files {
        for line in text.lines() {
            let line = line.trim_start();
            let (slot, raw) = if let Some(raw) = line.strip_prefix("mok_signing_key=") {
                (&mut cfg.key, raw)
            } else if let Some(raw) = line.strip_prefix("mok_certificate=") {
                (&mut cfg.cert, raw)
            } else {
                continue;
            };
            let value = conf_value(raw, kernel);
            *slot = (!value.is_empty()).then_some(value);
        }
    }
    cfg
}

/// Certificates to check for enrollment, in the install script's lookup
/// order. When the DKMS config names a key, DKMS uses only that pair (with
/// its defaults filling a missing half), so only that certificate counts.
pub fn signing_cert_candidates(cfg: &DkmsSigningConfig) -> Vec<String> {
    if cfg.key.is_some() || cfg.cert.is_some() {
        return vec![cfg.cert.clone().unwrap_or_else(|| DKMS_DEFAULT_CERT.to_string())];
    }
    SIGNING_KEY_PATHS.iter().map(|(_, cert)| cert.to_string()).collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SigningKeyState {
    /// No module-signing certificate exists yet.
    NoKey,
    /// The certificate (path) exists but is not enrolled.
    NotEnrolled(String),
    /// The certificate is queued with `mokutil --import`; MOK Manager
    /// enrolls it at the next boot.
    PendingEnrollment(String),
    /// The certificate is enrolled in the MOK list.
    Enrolled(String),
    /// The certificate exists but mokutil could not say whether it is
    /// enrolled (mokutil missing, or no EFI variables).
    Untested(String),
    /// Not checked yet.
    #[default]
    Unknown,
}

impl SigningKeyState {
    /// Path of the machine's signing certificate, when one exists.
    pub fn cert_path(&self) -> Option<&str> {
        match self {
            Self::NotEnrolled(p) | Self::PendingEnrollment(p) | Self::Enrolled(p)
            | Self::Untested(p) => Some(p),
            Self::NoKey | Self::Unknown => None,
        }
    }

    /// Whether Set Up Signing still has something to do.
    pub fn setup_needed(&self) -> bool {
        !matches!(self, Self::Enrolled(_) | Self::PendingEnrollment(_))
    }
}

/// What `mokutil --test-key <file>` said about a certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MokTestResult {
    /// Enrolled, or otherwise already trusted (db, built-in keyring).
    Trusted,
    /// Queued: "is already in the enrollment request".
    Pending,
    NotEnrolled,
}

/// Interpret `mokutil --test-key <file>` output. `None` for anything else,
/// such as "EFI variables are not supported" or a non-DER certificate.
pub fn parse_mok_test_key(output: &str) -> Option<MokTestResult> {
    let o = output.to_lowercase();
    if o.contains("already in the enrollment request") {
        Some(MokTestResult::Pending)
    } else if o.contains("already enrolled")
        || o.contains("already in db")
        || o.contains("already in the built-in trusted keyring")
    {
        Some(MokTestResult::Trusted)
    } else if o.contains("not enrolled") {
        Some(MokTestResult::NotEnrolled)
    } else {
        None
    }
}

/// SHA1 fingerprints (lowercase hex, no colons) of the keys listed by
/// `mokutil --list-new`.
pub fn parse_mok_fingerprints(list_new: &str) -> Vec<String> {
    list_new
        .lines()
        .filter_map(|l| {
            let (label, value) = l.split_once(':')?;
            label.trim().eq_ignore_ascii_case("SHA1 Fingerprint").then(|| {
                value.chars().filter(char::is_ascii_hexdigit).collect::<String>()
                    .to_lowercase()
            })
        })
        .filter(|f| f.len() == 40)
        .collect()
}

/// State of the certificate at `path` from the mokutil results: its SHA1
/// among the `mokutil --list-new` fingerprints means queued, otherwise the
/// `--test-key` answer decides. `cert_sha1` is the SHA1 of the certificate
/// file, which equals mokutil's fingerprint for a DER certificate. `None`
/// when mokutil gave no usable answer.
pub fn classify_signing_key(
    path: &str,
    test_key_output: &str,
    pending: &[String],
    cert_sha1: Option<&str>,
) -> Option<SigningKeyState> {
    if cert_sha1.is_some_and(|f| pending.iter().any(|p| p.eq_ignore_ascii_case(f))) {
        return Some(SigningKeyState::PendingEnrollment(path.to_string()));
    }
    let path = path.to_string();
    Some(match parse_mok_test_key(test_key_output)? {
        MokTestResult::Trusted => SigningKeyState::Enrolled(path),
        MokTestResult::Pending => SigningKeyState::PendingEnrollment(path),
        MokTestResult::NotEnrolled => SigningKeyState::NotEnrolled(path),
    })
}

/// Signature on the nvidia module on disk for the running kernel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ModuleSignature {
    /// No nvidia module installed (or modinfo unavailable).
    #[default]
    Missing,
    Unsigned,
    /// Signed; holds the signer modinfo reports.
    Signed(String),
}

/// Interpret `modinfo -F signer nvidia`: it fails when there is no module
/// and prints nothing for an unsigned one.
pub fn parse_module_signer(modinfo_succeeded: bool, output: &str) -> ModuleSignature {
    if !modinfo_succeeded {
        return ModuleSignature::Missing;
    }
    match output.trim() {
        "" => ModuleSignature::Unsigned,
        s => ModuleSignature::Signed(s.to_string()),
    }
}

/// One-line state of module signing for the System tab.
pub fn signing_summary(key: &SigningKeyState, module: &ModuleSignature) -> String {
    let key_text = match key {
        SigningKeyState::NoKey => "No signing key yet".to_string(),
        SigningKeyState::NotEnrolled(p) => format!("Key {p} is not enrolled"),
        SigningKeyState::PendingEnrollment(p) => {
            format!("Key {p} is queued; enroll it at the MOK Manager screen on the next reboot")
        }
        SigningKeyState::Enrolled(p) => format!("Key {p} is enrolled"),
        SigningKeyState::Untested(p) => format!("Key {p} (enrollment unknown)"),
        SigningKeyState::Unknown => "Unknown".to_string(),
    };
    let module_text = match module {
        ModuleSignature::Missing => String::new(),
        ModuleSignature::Signed(s) => format!("\nInstalled module signed by: {s}"),
        ModuleSignature::Unsigned if key.cert_path().is_some() => {
            "\nThe installed module is unsigned. Reinstall the driver to sign it.".to_string()
        }
        ModuleSignature::Unsigned => "\nThe installed module is unsigned.".to_string(),
    };
    key_text + &module_text
}

/// Check the one-time MOK password typed twice. MOK Manager takes 8-16
/// characters typed on a bare keyboard, so only printable ASCII is allowed.
pub fn validate_mok_password(first: &str, second: &str) -> Result<(), String> {
    let len = first.chars().count();
    if !(8..=16).contains(&len) {
        return Err("Use 8 to 16 characters.".to_string());
    }
    if !first.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        return Err("Use only letters, digits, spaces and standard symbols.".to_string());
    }
    if first != second {
        return Err("The passwords do not match.".to_string());
    }
    Ok(())
}

/// Warning text for the pre-install checks. `None` means nothing to warn about.
pub fn secure_boot_advice(sb: &SecureBootStatus, key: &SigningKeyState) -> Option<String> {
    if *sb != SecureBootStatus::Enabled {
        return None;
    }
    Some(match key {
        SigningKeyState::Enrolled(_) => {
            "Enabled. The module-signing key is enrolled; the new module is signed with it."
                .to_string()
        }
        SigningKeyState::PendingEnrollment(_) => {
            "Enabled. The module-signing key is queued for enrollment: on the next reboot, \
             choose Enroll MOK at the blue MOK Manager screen and type the one-time password."
                .to_string()
        }
        SigningKeyState::NotEnrolled(path) => format!(
            "Enabled, and the module-signing key {path} is not enrolled, so modules signed \
             with it are not trusted yet. Use Set Up Signing on the System tab."
        ),
        SigningKeyState::Untested(path) => format!(
            "Enabled. Could not check whether the module-signing key {path} is enrolled. \
             Set Up Signing on the System tab enrolls it."
        ),
        SigningKeyState::NoKey | SigningKeyState::Unknown => {
            "Enabled, and this machine has no module-signing key, so the new module will be \
             unsigned. Use Set Up Signing on the System tab before installing."
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
        use MokTestResult::*;
        assert_eq!(parse_mok_test_key("/var/lib/dkms/mok.pub is already enrolled\n"), Some(Trusted));
        assert_eq!(parse_mok_test_key("/c.der is already in the built-in trusted keyring\n"), Some(Trusted));
        assert_eq!(parse_mok_test_key("/var/lib/dkms/mok.pub is not enrolled\n"), Some(NotEnrolled));
        assert_eq!(
            parse_mok_test_key("/c.der is already in the enrollment request\n"),
            Some(Pending)
        );
        assert_eq!(parse_mok_test_key("EFI variables are not supported"), None);
        assert_eq!(
            parse_mok_test_key("Abort!!! /c.pem is not a valid x509 certificate in DER format"),
            None
        );
    }

    #[test]
    fn secure_boot_advice_cases() {
        let on = SecureBootStatus::Enabled;
        let none = SigningKeyState::NoKey;
        let path = "/var/lib/shim-signed/mok/MOK.der".to_string();
        assert_eq!(secure_boot_advice(&SecureBootStatus::Disabled, &none), None);
        assert_eq!(secure_boot_advice(&SecureBootStatus::Unknown, &none), None);
        let enrolled = secure_boot_advice(&on, &SigningKeyState::Enrolled(path.clone())).unwrap();
        assert!(enrolled.contains("is enrolled"));
        let pending =
            secure_boot_advice(&on, &SigningKeyState::PendingEnrollment(path.clone())).unwrap();
        assert!(pending.contains("MOK Manager"));
        let not = secure_boot_advice(&on, &SigningKeyState::NotEnrolled(path.clone())).unwrap();
        assert!(not.contains(&path) && not.contains("Set Up Signing"));
        let no_key = secure_boot_advice(&on, &none).unwrap();
        assert!(no_key.contains("Set Up Signing"));
        // Never send the user to run mokutil by hand, or into firmware settings.
        let all = [
            SigningKeyState::NoKey,
            SigningKeyState::Unknown,
            SigningKeyState::NotEnrolled(path.clone()),
            SigningKeyState::PendingEnrollment(path.clone()),
            SigningKeyState::Enrolled(path.clone()),
            SigningKeyState::Untested(path),
        ];
        for key in all {
            let text = secure_boot_advice(&on, &key).unwrap();
            assert!(!text.contains("mokutil"), "{text}");
            assert!(!text.to_lowercase().contains("disable"), "{text}");
        }
    }

    #[test]
    fn dkms_config_parsing() {
        let k = "7.2.6-generic";
        assert_eq!(parse_dkms_signing_config(&[], k), DkmsSigningConfig::default());
        // Upstream framework.conf ships the settings commented out.
        let stock = "# mok_signing_key=/var/lib/dkms/mok.key\n# mok_certificate=/var/lib/dkms/mok.pub\n";
        assert_eq!(parse_dkms_signing_config(&[stock], k), DkmsSigningConfig::default());
        let set = "mok_signing_key=/root/k.pem\n  mok_certificate=\"/root/c.der\" # mine\n";
        assert_eq!(
            parse_dkms_signing_config(&[stock, set], k),
            DkmsSigningConfig {
                key: Some("/root/k.pem".into()),
                cert: Some("/root/c.der".into()),
            }
        );
        // Later files win; an empty value unsets; $kernelver expands.
        let later = "mok_signing_key=''\nmok_certificate='/certs/${kernelver}/c.der'\n";
        assert_eq!(
            parse_dkms_signing_config(&[set, later], k),
            DkmsSigningConfig { key: None, cert: Some("/certs/7.2.6-generic/c.der".into()) }
        );
        let bare = "mok_signing_key=/keys/$kernelver.key\n";
        assert_eq!(
            parse_dkms_signing_config(&[bare], k).key.as_deref(),
            Some("/keys/7.2.6-generic.key")
        );
        // Similar names are not the setting.
        let other = "my_mok_signing_key=/x\nmok_signing_key_extra=/y\n";
        assert_eq!(parse_dkms_signing_config(&[other], k), DkmsSigningConfig::default());
    }

    #[test]
    fn cert_candidate_order() {
        let defaults = signing_cert_candidates(&DkmsSigningConfig::default());
        assert_eq!(
            defaults,
            vec![
                "/var/lib/shim-signed/mok/MOK.der".to_string(),
                "/var/lib/dkms/mok.pub".to_string(),
                "/etc/pki/akmods/certs/public_key.der".to_string(),
            ]
        );
        let cfg = DkmsSigningConfig { key: None, cert: Some("/c.der".into()) };
        assert_eq!(signing_cert_candidates(&cfg), vec!["/c.der".to_string()]);
        let key_only = DkmsSigningConfig { key: Some("/k".into()), cert: None };
        assert_eq!(signing_cert_candidates(&key_only), vec![DKMS_DEFAULT_CERT.to_string()]);
    }

    #[test]
    fn mok_pending_fingerprints() {
        let out = "[key 1]\nSHA1 Fingerprint: 5C:9C:AB:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01\n\
                   Certificate:\n    Data:\n        Subject: CN = test\n";
        let fprs = parse_mok_fingerprints(out);
        assert_eq!(fprs, vec!["5c9cab0123456789abcdef0123456789abcdef01".to_string()]);
        assert!(parse_mok_fingerprints("MokNew is empty\n").is_empty());

        let path = "/var/lib/dkms/mok.pub";
        let sha = "5c9cab0123456789abcdef0123456789abcdef01";
        let enrolled_out = "/var/lib/dkms/mok.pub is already enrolled\n";
        // Queued keys also test as "already enrolled"; the pending list decides.
        assert_eq!(
            classify_signing_key(path, enrolled_out, &fprs, Some(sha)),
            Some(SigningKeyState::PendingEnrollment(path.into()))
        );
        assert_eq!(
            classify_signing_key(path, enrolled_out, &[], Some(sha)),
            Some(SigningKeyState::Enrolled(path.into()))
        );
        assert_eq!(
            classify_signing_key(path, "/var/lib/dkms/mok.pub is not enrolled", &fprs, None),
            Some(SigningKeyState::NotEnrolled(path.into()))
        );
        assert_eq!(
            classify_signing_key(path, "/var/lib/dkms/mok.pub is already in the enrollment request", &[], None),
            Some(SigningKeyState::PendingEnrollment(path.into()))
        );
        assert_eq!(classify_signing_key(path, "EFI variables are not supported", &[], None), None);
    }

    #[test]
    fn signing_key_state_helpers() {
        assert!(SigningKeyState::NoKey.setup_needed());
        assert!(SigningKeyState::NotEnrolled("/c".into()).setup_needed());
        assert!(!SigningKeyState::Enrolled("/c".into()).setup_needed());
        assert!(!SigningKeyState::PendingEnrollment("/c".into()).setup_needed());
        assert_eq!(SigningKeyState::NoKey.cert_path(), None);
        assert_eq!(SigningKeyState::Untested("/c".into()).cert_path(), Some("/c"));
    }

    #[test]
    fn module_signer_parsing() {
        assert_eq!(parse_module_signer(false, ""), ModuleSignature::Missing);
        assert_eq!(parse_module_signer(true, "\n"), ModuleSignature::Unsigned);
        assert_eq!(
            parse_module_signer(true, "host Secure Boot Module Signature key\n"),
            ModuleSignature::Signed("host Secure Boot Module Signature key".into())
        );
    }

    #[test]
    fn signing_summary_text() {
        let key = SigningKeyState::Enrolled("/c.der".into());
        assert!(signing_summary(&key, &ModuleSignature::Unsigned).contains("Reinstall the driver"));
        assert!(!signing_summary(&SigningKeyState::NoKey, &ModuleSignature::Unsigned)
            .contains("Reinstall"));
        assert!(signing_summary(&key, &ModuleSignature::Signed("me".into())).contains("signed by: me"));
        assert_eq!(signing_summary(&SigningKeyState::NoKey, &ModuleSignature::Missing), "No signing key yet");
    }

    #[test]
    fn mok_password_rules() {
        assert!(validate_mok_password("abcd1234", "abcd1234").is_ok());
        assert!(validate_mok_password("sixteen-chars-ok", "sixteen-chars-ok").is_ok());
        assert!(validate_mok_password("short", "short").is_err());
        assert!(validate_mok_password("seventeen-chars-x", "seventeen-chars-x").is_err());
        assert!(validate_mok_password("abcd1234", "abcd1235").is_err());
        assert!(validate_mok_password("pässwörd1", "pässwörd1").is_err());
        assert!(validate_mok_password("abcd\n1234", "abcd\n1234").is_err());
    }
}
