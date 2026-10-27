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

/// The package globs the install script removes (`DEB_DRIVER_GLOBS` /
/// `RPM_DRIVER_GLOBS` in `scripts/privileged-install.sh`; a test below keeps
/// the two in step). Pop!_OS's system76-driver-nvidia metapackages are
/// included because they pin the distro driver.
pub const DEB_DRIVER_GLOBS: [&str; 4] =
    ["nvidia-*", "libnvidia-*", "xserver-xorg-video-nvidia*", "system76-driver-nvidia*"];
pub const RPM_DRIVER_GLOBS: [&str; 5] =
    ["akmod-nvidia*", "xorg-x11-drv-nvidia*", "kmod-nvidia*", "nvidia-driver*", "nvidia-settings*"];

/// Driver packages from the distro (apt or rpm) that the install script
/// removes before running the .run installer: what the globs above match,
/// minus Docker's GPU plumbing (nvidia-container-*, libnvidia-container*,
/// the script's `NOT_DRIVER_RE`), which is not the display driver.
pub fn is_driver_package(name: &str) -> bool {
    if name.starts_with("nvidia-container") || name.starts_with("libnvidia-container") {
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
/// installed driver package names: second status letter "i", whatever the
/// selection ("ii", or "hi" for a held package), as the script reads it.
pub fn parse_dpkg_installed(text: &str) -> Vec<String> {
    let mut names: Vec<String> = text
        .lines()
        .filter_map(|l| l.split_once('|'))
        .filter(|(status, _)| status.as_bytes().get(1) == Some(&b'i'))
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
//  Blocking the distro driver
// ─────────────────────────────────────────────────────────────────────────────

/// Where the install script's `--block-distro-driver` keeps the distro's
/// driver packages out: an apt pin, or a dnf5 excludepkgs drop-in. Keep in
/// step with `APT_BLOCK_FILE` / `DNF_BLOCK_FILE` in the script (tested below).
pub const APT_BLOCK_FILE: &str = "/etc/apt/preferences.d/green-light-nvidia";
pub const DNF_BLOCK_FILE: &str = "/etc/dnf/libdnf5.conf.d/90-green-light-nvidia.conf";

// ─────────────────────────────────────────────────────────────────────────────
//  The .run file the install script will accept
// ─────────────────────────────────────────────────────────────────────────────

/// Owner and permissions of a .run file, as `stat` reports them.
#[derive(Debug, Clone, Copy)]
pub struct FileFacts {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// One `getent passwd` line: (name, uid, primary gid).
fn passwd_entry(line: &str) -> Option<(&str, u32, u32)> {
    let mut f = line.split(':');
    let name = f.next()?;
    let uid = f.nth(1)?.parse().ok()?;
    let gid = f.next()?.parse().ok()?;
    Some((name, uid, gid))
}

/// Whether `gid` is the private group of account `uid`: named after it, its
/// primary group, and with no other member, supplementary or primary.
/// `passwd` is `getent passwd` output, `group` the `getent group <gid>` line.
/// Mirrors `group_is_owners_private_group` in the install script.
pub fn group_is_owners_private_group(gid: u32, uid: u32, passwd: &str, group: Option<&str>) -> bool {
    let entries: Vec<_> = passwd.lines().filter_map(passwd_entry).collect();
    let Some(&(owner, _, owner_gid)) = entries.iter().find(|e| e.1 == uid) else {
        return false;
    };
    let Some(group) = group else { return false };
    let mut g = group.trim_end().split(':');
    let gname = g.next().unwrap_or("");
    let members = g.nth(2).unwrap_or("");
    owner_gid == gid
        && gname == owner
        && (members.is_empty() || members == owner)
        && !entries.iter().any(|e| e.2 == gid && e.0 != owner)
}

/// Who besides the owner (`owner_uid`) an ACL lets write a file, from
/// `getfacl -cpn` output: "user:<uid>" and "group:<gid>" entries with write
/// permission that the mask doesn't take away. Mirrors `acl_writers` in the
/// install script.
pub fn acl_writers(getfacl: &str, owner_uid: u32) -> Vec<String> {
    let mask_w = getfacl
        .lines()
        .find_map(|l| l.strip_prefix("mask::"))
        .is_none_or(|m| m.as_bytes().get(1) == Some(&b'w'));
    getfacl
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, ':');
            let (kind, who, perms) = (f.next()?, f.next()?, f.next()?);
            let other = match kind {
                "user" => !who.is_empty() && who != owner_uid.to_string(),
                "group" => !who.is_empty(),
                _ => false,
            };
            (other && perms.as_bytes().get(1) == Some(&b'w') && mask_w)
                .then(|| format!("{kind}:{who}"))
        })
        .collect()
}

/// Why the install script would refuse this .run file, checked the way the
/// script checks it, so the reason shows before the password prompt: an
/// absolute `.run` path, owned by root or the caller, and writable by nobody
/// else.
pub fn run_file_problem(
    path: &str,
    facts: FileFacts,
    caller_uid: u32,
    passwd: &str,
    group: Option<&str>,
) -> Option<String> {
    if !path.starts_with('/') || !path.ends_with(".run") {
        return Some(format!("{path} is not an absolute path to a .run file."));
    }
    if facts.uid != 0 && facts.uid != caller_uid {
        return Some(format!(
            "{path} belongs to another user (uid {}); use a copy of your own.",
            facts.uid
        ));
    }
    if facts.mode & 0o002 != 0 {
        return Some(format!(
            "{path} can be changed by every user (mode {:o}). Remove that permission \
             (chmod go-w) and try again.",
            facts.mode & 0o7777
        ));
    }
    if facts.mode & 0o020 != 0
        && !group_is_owners_private_group(facts.gid, facts.uid, passwd, group)
    {
        return Some(format!(
            "{path} can be changed by other members of its group (mode {:o}). Remove that \
             permission (chmod go-w) and try again.",
            facts.mode & 0o7777
        ));
    }
    None
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

/// The module-signing key pair the install script would sign with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningPair {
    /// Private key path, or a `pkcs11:` URI.
    pub key: String,
    pub cert: String,
    /// False when the DKMS config names a pair that is (partly) missing; the
    /// script then signs nothing.
    pub complete: bool,
    /// The private key sits where only root can look (Fedora's
    /// /etc/pki/akmods/private), so whether it exists is not known here; the
    /// install checks it.
    pub key_hidden: bool,
}

impl SigningPair {
    /// A key on a PKCS#11 token: DKMS can sign with it, NVIDIA's installer can't.
    pub fn is_pkcs11(&self) -> bool {
        self.key.starts_with("pkcs11:")
    }
}

/// The signing pair, resolved like `resolve_signing_key` in the install
/// script: a pair named by the DKMS config is used as named (DKMS's defaults
/// fill a missing half, and it counts even when its files are missing);
/// otherwise the first default pair whose key AND certificate both exist.
/// `exists` says whether a file exists: `Some(true/false)`, or `None` when
/// that can't be told without root (a directory only root may search). A
/// key that can't be seen beside a certificate that exists counts, marked
/// `key_hidden`, since the script (as root) will find it if it is there.
pub fn resolve_signing_pair(
    cfg: &DkmsSigningConfig,
    exists: impl Fn(&str) -> Option<bool>,
) -> Option<SigningPair> {
    let key_state = |k: &str| if k.starts_with("pkcs11:") { Some(true) } else { exists(k) };
    if cfg.key.is_some() || cfg.cert.is_some() {
        let key = cfg.key.clone().unwrap_or_else(|| DKMS_DEFAULT_KEY.to_string());
        let cert = cfg.cert.clone().unwrap_or_else(|| DKMS_DEFAULT_CERT.to_string());
        let k = key_state(&key);
        let complete = k != Some(false) && exists(&cert) == Some(true);
        return Some(SigningPair { key, cert, complete, key_hidden: k.is_none() });
    }
    SIGNING_KEY_PATHS
        .iter()
        .find(|(key, cert)| exists(cert) == Some(true) && key_state(key) != Some(false))
        .map(|(key, cert)| SigningPair {
            key: key.to_string(),
            cert: cert.to_string(),
            complete: true,
            key_hidden: key_state(key).is_none(),
        })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SigningKeyState {
    /// No module-signing key pair exists yet.
    NoKey,
    /// The DKMS config names a key pair that is (partly) missing: (key, cert).
    Incomplete(String, String),
    /// The certificate (path) exists but is not enrolled.
    NotEnrolled(String),
    /// The certificate is queued with `mokutil --import`; MOK Manager
    /// enrolls it at the next boot.
    PendingEnrollment(String),
    /// The certificate is enrolled in the MOK list.
    Enrolled(String),
    /// The certificate exists, but this EFI machine's EFI variables are not
    /// mounted, so its enrollment can't be read.
    EfiVarsNotMounted(String),
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
            | Self::EfiVarsNotMounted(p) | Self::Untested(p) => Some(p),
            Self::NoKey | Self::Incomplete(..) | Self::Unknown => None,
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
    /// "EFI variables are not supported" on a machine that booted with EFI
    /// (/sys/firmware/efi exists): efivarfs is not mounted.
    EfiVarsNotMounted,
}

/// Interpret `mokutil --test-key <file>` output (stdout and stderr).
/// `efi_firmware` is whether /sys/firmware/efi exists. `None` for anything
/// else, such as a non-DER certificate or a machine without EFI.
pub fn parse_mok_test_key(output: &str, efi_firmware: bool) -> Option<MokTestResult> {
    let o = output.to_lowercase();
    if o.contains("efi variables are not supported") {
        efi_firmware.then_some(MokTestResult::EfiVarsNotMounted)
    } else if o.contains("already in the enrollment request") {
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

/// SHA1 fingerprints (lowercase hex, no colons) from the shared list of
/// waiting keys that the signing setup of this app or another one writes
/// (`mokutil --list-new` cannot tell: the waiting request, MokNew, is
/// readable by root only). The first line is `moknew-size <bytes>`, MokNew's
/// size right after that setup queued the keys; `moknew_size` is its size
/// now, `None` when nothing is waiting. Any other size means the request
/// changed since (enrolled at boot, revoked, replaced by other means): the
/// list is stale and gives nothing.
pub fn parse_pending_list(text: &str, moknew_size: Option<u64>) -> Vec<String> {
    let mut lines = text.lines();
    let recorded = lines
        .next()
        .and_then(|l| l.trim().strip_prefix("moknew-size "))
        .and_then(|n| n.trim().parse::<u64>().ok());
    if recorded.is_none() || recorded != moknew_size {
        return vec![];
    }
    lines
        .map(|l| l.chars().filter(char::is_ascii_hexdigit).collect::<String>().to_lowercase())
        .filter(|f| f.len() == 40)
        .collect()
}

/// State of the certificate at `path` from the mokutil results. Enrolled
/// (or otherwise trusted) per `--test-key` wins; then its SHA1 among the
/// pending fingerprints means queued; otherwise the `--test-key` answer
/// decides. `cert_sha1` is the SHA1 of the certificate file, which equals
/// mokutil's fingerprint for a DER certificate. `None` when mokutil gave no
/// usable answer and the key isn't listed.
pub fn classify_signing_key(
    path: &str,
    test_key_output: &str,
    efi_firmware: bool,
    pending: &[String],
    cert_sha1: Option<&str>,
) -> Option<SigningKeyState> {
    let path = path.to_string();
    let test = parse_mok_test_key(test_key_output, efi_firmware);
    if test == Some(MokTestResult::Trusted) {
        return Some(SigningKeyState::Enrolled(path));
    }
    if cert_sha1.is_some_and(|f| pending.iter().any(|p| p.eq_ignore_ascii_case(f))) {
        return Some(SigningKeyState::PendingEnrollment(path));
    }
    Some(match test? {
        MokTestResult::Trusted => SigningKeyState::Enrolled(path),
        MokTestResult::Pending => SigningKeyState::PendingEnrollment(path),
        MokTestResult::NotEnrolled => SigningKeyState::NotEnrolled(path),
        MokTestResult::EfiVarsNotMounted => SigningKeyState::EfiVarsNotMounted(path),
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
        SigningKeyState::Incomplete(k, c) => {
            format!("The DKMS config names key {k} and certificate {c}, but they don't both exist")
        }
        SigningKeyState::NotEnrolled(p) => format!("Key {p} is not enrolled"),
        SigningKeyState::PendingEnrollment(p) => {
            format!("Key {p} is queued; enroll it at the MOK Manager screen on the next reboot")
        }
        SigningKeyState::Enrolled(p) => format!("Key {p} is enrolled"),
        SigningKeyState::EfiVarsNotMounted(p) => format!(
            "Key {p}: the EFI variables are not mounted, so its enrollment can't be read. \
             Mount them (sudo mount -t efivarfs efivarfs /sys/firmware/efi/efivars) or reboot"
        ),
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

/// The password rule as the Set Up Signing dialog first shows it.
pub const MOK_PASSWORD_HINT: &str =
    "Use 8 to 16 characters: letters, digits and standard symbols, without spaces.";

/// Check the one-time MOK password typed twice. MOK Manager takes 8-16
/// characters typed on a bare (US layout) keyboard, so only printable ASCII
/// without spaces is allowed (the install script checks the same).
pub fn validate_mok_password(first: &str, second: &str) -> Result<(), String> {
    let len = first.chars().count();
    if !(8..=16).contains(&len) {
        return Err(MOK_PASSWORD_HINT.to_string());
    }
    if !first.chars().all(|c| c.is_ascii_graphic()) {
        return Err("Use only letters, digits and standard symbols, without spaces.".to_string());
    }
    if first != second {
        return Err("The passwords do not match.".to_string());
    }
    Ok(())
}

/// The Secure Boot row's text: the firmware state, and with Secure Boot on,
/// whether modules signed with this machine's key will load.
pub fn secure_boot_summary(sb: &crate::system::SecureBootStatus, key: &SigningKeyState) -> String {
    use crate::system::SecureBootStatus as S;
    match (sb, key) {
        (S::Enabled, SigningKeyState::Enrolled(_)) => {
            "Enabled — the module-signing key is enrolled, so signed modules load".to_string()
        }
        (S::Enabled, SigningKeyState::PendingEnrollment(_)) => {
            "Enabled — the module-signing key is waiting for enrollment at the next reboot".to_string()
        }
        (S::Enabled, SigningKeyState::EfiVarsNotMounted(_)) => {
            "Enabled — the EFI variables are not mounted, so the module-signing key's enrollment \
             can't be checked"
                .to_string()
        }
        (S::Enabled, SigningKeyState::Untested(_) | SigningKeyState::Unknown) => {
            "Enabled — could not check whether the module-signing key is enrolled".to_string()
        }
        (S::Enabled, _) => {
            "Enabled — the module-signing key is not enrolled; use Set Up Signing".to_string()
        }
        (other, _) => other.to_string(),
    }
}

/// Warning text for the pre-install checks. `None` means nothing to warn
/// about. `pair` is the key the install would sign with and `use_dkms` the
/// DKMS option: DKMS signs with any key, NVIDIA's installer (no DKMS) can't
/// use a PKCS#11 one, so then the module stays unsigned.
pub fn secure_boot_advice(
    sb: &SecureBootStatus,
    key: &SigningKeyState,
    pair: Option<&SigningPair>,
    use_dkms: bool,
) -> Option<String> {
    if *sb != SecureBootStatus::Enabled {
        return None;
    }
    let unsignable = !use_dkms && pair.is_some_and(SigningPair::is_pkcs11);
    if unsignable && key.cert_path().is_some() {
        return Some(
            "Enabled. The module-signing key is on a PKCS#11 token, which NVIDIA's installer \
             can't sign with, so without DKMS the new module will be unsigned and won't load. \
             Turn on DKMS to have it signed."
                .to_string(),
        );
    }
    Some(match key {
        SigningKeyState::Enrolled(_) if use_dkms => {
            "Enabled. The module-signing key is enrolled; DKMS signs the new module with it."
                .to_string()
        }
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
        SigningKeyState::EfiVarsNotMounted(path) => format!(
            "Enabled, but the EFI variables are not mounted, so Green Light can't check \
             whether the module-signing key {path} is enrolled. Mount them (sudo mount -t \
             efivarfs efivarfs /sys/firmware/efi/efivars) or reboot."
        ),
        SigningKeyState::Untested(path) => format!(
            "Enabled. Could not check whether the module-signing key {path} is enrolled. \
             Set Up Signing on the System tab enrolls it."
        ),
        SigningKeyState::Incomplete(k, c) => format!(
            "Enabled, and the DKMS config names signing key {k} and certificate {c}, which \
             don't both exist, so the new module will be unsigned. Fix the DKMS config, or use \
             Set Up Signing on the System tab to create the key there."
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
        assert!(!is_driver_package("nvidia-container-runtime"));
        assert!(!is_driver_package("libcuda1"));
        assert!(!is_driver_package("green-light"));
    }

    #[test]
    fn dpkg_output_parsing() {
        let text = "ii |nvidia-driver-595\nrc |nvidia-dkms-570\nii |nvidia-container-toolkit\n\
                    ii |libnvidia-gl-595\nii |nvidia-driver-595\nii |bash\nhi |nvidia-utils-595\n\
                    un |nvidia-kernel-dkms\n";
        assert_eq!(
            parse_dpkg_installed(text),
            vec![
                "libnvidia-gl-595".to_string(),
                "nvidia-driver-595".to_string(),
                "nvidia-utils-595".to_string(),
            ]
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
        assert_eq!(parse_mok_test_key("/var/lib/dkms/mok.pub is already enrolled\n", true), Some(Trusted));
        assert_eq!(parse_mok_test_key("/c.der is already in the built-in trusted keyring\n", true), Some(Trusted));
        assert_eq!(parse_mok_test_key("/var/lib/dkms/mok.pub is not enrolled\n", true), Some(NotEnrolled));
        assert_eq!(
            parse_mok_test_key("/c.der is already in the enrollment request\n", true),
            Some(Pending)
        );
        // On an EFI machine this means efivarfs is not mounted; without EFI
        // firmware there is nothing to read.
        assert_eq!(parse_mok_test_key("EFI variables are not supported on this system\n", true), Some(EfiVarsNotMounted));
        assert_eq!(parse_mok_test_key("EFI variables are not supported on this system\n", false), None);
        assert_eq!(
            parse_mok_test_key("Abort!!! /c.pem is not a valid x509 certificate in DER format", true),
            None
        );
    }

    #[test]
    fn secure_boot_summary_follows_the_key() {
        use crate::system::SecureBootStatus as S;
        let on = S::Enabled;
        assert!(secure_boot_summary(&on, &SigningKeyState::Enrolled("/c".into())).contains("is enrolled"));
        assert!(secure_boot_summary(&on, &SigningKeyState::PendingEnrollment("/c".into())).contains("waiting"));
        assert!(secure_boot_summary(&on, &SigningKeyState::NotEnrolled("/c".into())).contains("Set Up Signing"));
        assert!(secure_boot_summary(&on, &SigningKeyState::NoKey).contains("Set Up Signing"));
        assert!(!secure_boot_summary(&on, &SigningKeyState::Enrolled("/c".into())).contains("may be required"));
        assert_eq!(secure_boot_summary(&S::Disabled, &SigningKeyState::NoKey), "Disabled");
    }

    #[test]
    fn secure_boot_advice_cases() {
        let on = SecureBootStatus::Enabled;
        let none = SigningKeyState::NoKey;
        let path = "/var/lib/shim-signed/mok/MOK.der".to_string();
        assert_eq!(secure_boot_advice(&SecureBootStatus::Disabled, &none, None, true), None);
        assert_eq!(secure_boot_advice(&SecureBootStatus::Unknown, &none, None, false), None);
        let adv = |sb: &SecureBootStatus, key: &SigningKeyState| secure_boot_advice(sb, key, None, true);
        assert_eq!(adv(&SecureBootStatus::Disabled, &none), None);
        let enrolled = adv(&on, &SigningKeyState::Enrolled(path.clone())).unwrap();
        assert!(enrolled.contains("is enrolled"));
        let pending = adv(&on, &SigningKeyState::PendingEnrollment(path.clone())).unwrap();
        assert!(pending.contains("MOK Manager"));
        let not = adv(&on, &SigningKeyState::NotEnrolled(path.clone())).unwrap();
        assert!(not.contains(&path) && not.contains("Set Up Signing"));
        let no_key = adv(&on, &none).unwrap();
        assert!(no_key.contains("Set Up Signing"));
        let half = adv(&on, &SigningKeyState::Incomplete("/k".into(), "/c".into())).unwrap();
        assert!(half.contains("unsigned") && half.contains("/k"));
        // Never send the user to run mokutil by hand, or into firmware settings.
        let all = [
            SigningKeyState::NoKey,
            SigningKeyState::Unknown,
            SigningKeyState::NotEnrolled(path.clone()),
            SigningKeyState::PendingEnrollment(path.clone()),
            SigningKeyState::Enrolled(path.clone()),
            SigningKeyState::Untested(path),
            SigningKeyState::Incomplete("/k".into(), "/c".into()),
        ];
        for key in all {
            for dkms in [true, false] {
                let text = secure_boot_advice(&on, &key, None, dkms).unwrap();
                assert!(!text.contains("mokutil"), "{text}");
                assert!(!text.to_lowercase().contains("disable"), "{text}");
            }
        }
    }

    #[test]
    fn pkcs11_key_needs_dkms() {
        let on = SecureBootStatus::Enabled;
        let enrolled = SigningKeyState::Enrolled("/c.der".into());
        let token = SigningPair {
            key: "pkcs11:token=mok".into(),
            cert: "/c.der".into(),
            complete: true,
            key_hidden: false,
        };
        let file = SigningPair { key: "/k.priv".into(), ..token.clone() };
        let without = secure_boot_advice(&on, &enrolled, Some(&token), false).unwrap();
        assert!(without.contains("PKCS#11") && without.contains("Turn on DKMS"), "{without}");
        assert!(!without.contains("signed with it"));
        let with = secure_boot_advice(&on, &enrolled, Some(&token), true).unwrap();
        assert!(with.contains("DKMS signs"), "{with}");
        let plain = secure_boot_advice(&on, &enrolled, Some(&file), false).unwrap();
        assert!(plain.contains("signed with it"), "{plain}");
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
    fn signing_pair_resolution() {
        let none = DkmsSigningConfig::default();
        let files = |set: &'static [&'static str]| move |p: &str| Some(set.contains(&p));
        assert_eq!(resolve_signing_pair(&none, files(&[])), None);
        // A certificate without its key is not a pair: the next complete pair wins.
        let p = resolve_signing_pair(
            &none,
            files(&["/var/lib/shim-signed/mok/MOK.der", DKMS_DEFAULT_KEY, DKMS_DEFAULT_CERT]),
        )
        .unwrap();
        assert_eq!((p.key.as_str(), p.cert.as_str(), p.complete), (DKMS_DEFAULT_KEY, DKMS_DEFAULT_CERT, true));
        assert_eq!(
            resolve_signing_pair(&none, files(&["/var/lib/shim-signed/mok/MOK.der"])),
            None
        );
        let p = resolve_signing_pair(
            &none,
            files(&["/var/lib/shim-signed/mok/MOK.priv", "/var/lib/shim-signed/mok/MOK.der"]),
        )
        .unwrap();
        assert_eq!(p.cert, "/var/lib/shim-signed/mok/MOK.der");

        // The DKMS config's pair is used as named, defaults filling a missing half.
        let cfg = DkmsSigningConfig { key: None, cert: Some("/c.der".into()) };
        let p = resolve_signing_pair(&cfg, files(&["/c.der", DKMS_DEFAULT_KEY])).unwrap();
        assert_eq!((p.key.as_str(), p.cert.as_str(), p.complete), (DKMS_DEFAULT_KEY, "/c.der", true));
        let p = resolve_signing_pair(&cfg, files(&["/c.der"])).unwrap();
        assert!(!p.complete);
        let token = DkmsSigningConfig { key: Some("pkcs11:token=x".into()), cert: Some("/c.der".into()) };
        let p = resolve_signing_pair(&token, files(&["/c.der"])).unwrap();
        assert!(p.complete && p.is_pkcs11() && !p.key_hidden);

        // A key in a root-only directory: counted beside its certificate, but
        // marked as not seen; a missing certificate still rules the pair out.
        let akmods_key = "/etc/pki/akmods/private/private_key.priv";
        let akmods_cert = "/etc/pki/akmods/certs/public_key.der";
        let hidden = move |p: &str| if p == akmods_key { None } else { Some(p == akmods_cert) };
        let p = resolve_signing_pair(&none, hidden).unwrap();
        assert_eq!(p.cert, akmods_cert);
        assert!(p.complete && p.key_hidden);
        let no_cert = move |p: &str| if p == akmods_key { None } else { Some(false) };
        assert_eq!(resolve_signing_pair(&none, no_cert), None);
        let cfg_hidden = DkmsSigningConfig { key: Some("/root/k".into()), cert: Some("/c.der".into()) };
        let p = resolve_signing_pair(&cfg_hidden, |p| if p == "/root/k" { None } else { Some(p == "/c.der") }).unwrap();
        assert!(p.complete && p.key_hidden);
    }

    #[test]
    fn private_group_rules() {
        let passwd = "root:x:0:0::/root:/bin/bash\nalex:x:1000:1000::/home/alex:/bin/bash\n\
                      bob:x:1001:1001::/home/bob:/bin/bash\n";
        assert!(group_is_owners_private_group(1000, 1000, passwd, Some("alex:x:1000:")));
        assert!(group_is_owners_private_group(1000, 1000, passwd, Some("alex:x:1000:alex\n")));
        assert!(!group_is_owners_private_group(1000, 1000, passwd, Some("alex:x:1000:bob")));
        assert!(!group_is_owners_private_group(100, 1000, passwd, Some("users:x:100:")));
        assert!(!group_is_owners_private_group(1000, 1000, passwd, None));
        let shared = format!("{passwd}carol:x:1002:1000::/home/carol:/bin/bash\n");
        assert!(!group_is_owners_private_group(1000, 1000, &shared, Some("alex:x:1000:")));
    }

    #[test]
    fn acl_entries_that_allow_writing() {
        let acl = "user::rw-\nuser:1001:rw-\nuser:1000:rw-\ngroup::r--\ngroup:27:rw-\n\
                   group:44:r--\nmask::rw-\nother::r--\n";
        assert_eq!(acl_writers(acl, 1000), ["user:1001", "group:27"]);
        // The mask takes write away from named entries.
        let masked = acl.replace("mask::rw-", "mask::r--");
        assert!(acl_writers(&masked, 1000).is_empty());
        assert!(acl_writers("user::rw-\ngroup::r--\nother::r--\n", 1000).is_empty());
    }

    #[test]
    fn run_file_rules() {
        let passwd = "alex:x:1000:1000::/home/alex:/bin/bash\n";
        let group = Some("alex:x:1000:");
        let f = |mode, uid, gid| FileFacts { mode, uid, gid };
        let p = "/home/alex/Downloads/NVIDIA-Linux-x86_64-595.84.run";
        assert_eq!(run_file_problem(p, f(0o100644, 1000, 1000), 1000, passwd, group), None);
        assert_eq!(run_file_problem(p, f(0o100664, 1000, 1000), 1000, passwd, group), None);
        assert_eq!(run_file_problem(p, f(0o100644, 0, 0), 1000, passwd, None), None);
        assert!(run_file_problem(p, f(0o100664, 1000, 1000), 1000, passwd, Some("alex:x:1000:bob"))
            .unwrap()
            .contains("other members of its group"));
        assert!(run_file_problem(p, f(0o100646, 1000, 1000), 1000, passwd, group)
            .unwrap()
            .contains("every user (mode 646)"));
        assert!(run_file_problem(p, f(0o100644, 1001, 1001), 1000, passwd, group)
            .unwrap()
            .contains("another user"));
        assert!(run_file_problem("relative.run", f(0o644, 1000, 1000), 1000, passwd, group).is_some());
        assert!(run_file_problem("/x/driver.bin", f(0o644, 1000, 1000), 1000, passwd, group).is_some());
    }

    /// The bash array `NAME=('a' 'b' ...)` from the install script.
    fn script_array(name: &str) -> Vec<String> {
        let script = include_str!("../scripts/privileged-install.sh");
        let head = format!("\n{name}=(");
        let start = script
            .find(&head)
            .unwrap_or_else(|| panic!("{name} not found in the install script"))
            + head.len();
        let inner = &script[start..start + script[start..].find(')').unwrap()];
        inner
            .split_whitespace()
            .map(|w| w.trim_matches(|c| c == '\'' || c == '"').to_string())
            .collect()
    }

    fn script_value(name: &str) -> String {
        let script = include_str!("../scripts/privileged-install.sh");
        let line = script
            .lines()
            .find(|l| l.starts_with(&format!("{name}=")))
            .unwrap_or_else(|| panic!("{name} not found in the install script"));
        line.split_once('=').unwrap().1.trim_matches(|c| c == '\'' || c == '"').to_string()
    }

    #[test]
    fn install_script_agrees() {
        assert_eq!(script_array("DEB_DRIVER_GLOBS"), DEB_DRIVER_GLOBS);
        assert_eq!(script_array("RPM_DRIVER_GLOBS"), RPM_DRIVER_GLOBS);
        assert_eq!(script_value("NOT_DRIVER_RE"), "^(nvidia-container|libnvidia-container)");
        assert_eq!(script_value("APT_BLOCK_FILE"), APT_BLOCK_FILE);
        let dnf = format!("{}/{}", script_value("DNF_BLOCK_DIR"), "90-green-light-nvidia.conf");
        assert_eq!(dnf, DNF_BLOCK_FILE);
        assert_eq!(script_value("DNF_BLOCK_FILE"), "$DNF_BLOCK_DIR/90-green-light-nvidia.conf");
        let pairs: Vec<(String, String)> = script_array("SIGNING_KEY_PAIRS")
            .iter()
            .map(|p| {
                let p = p.replace("$DKMS_DEFAULT_KEY", DKMS_DEFAULT_KEY)
                    .replace("$DKMS_DEFAULT_CERT", DKMS_DEFAULT_CERT);
                let (k, c) = p.split_once('|').unwrap();
                (k.to_string(), c.to_string())
            })
            .collect();
        let ours: Vec<(String, String)> =
            SIGNING_KEY_PATHS.iter().map(|(k, c)| (k.to_string(), c.to_string())).collect();
        assert_eq!(pairs, ours);
        assert_eq!(script_value("DKMS_DEFAULT_KEY"), DKMS_DEFAULT_KEY);
        assert_eq!(script_value("DKMS_DEFAULT_CERT"), DKMS_DEFAULT_CERT);
    }

    #[test]
    fn mok_pending_fingerprints() {
        let out = "moknew-size 2052\n5c9cab0123456789abcdef0123456789abcdef01\n43:E4:F8:24:5B:73:F1:34:BF:13:C1:DB:E6:30:01:52:7C:7B:09:05\nnot a fingerprint\n";
        let fprs = parse_pending_list(out, Some(2052));
        assert_eq!(
            fprs,
            vec!["5c9cab0123456789abcdef0123456789abcdef01".to_string(), "43e4f8245b73f134bf13c1dbe63001527c7b0905".to_string()]
        );
        // Valid only while MokNew exists with the size recorded at the import.
        assert!(parse_pending_list(out, Some(2053)).is_empty(), "request changed since: stale");
        assert!(parse_pending_list(out, None).is_empty(), "no request waiting: stale");
        // A list without the size line, or with a damaged one, is never trusted.
        assert!(parse_pending_list("5c9cab0123456789abcdef0123456789abcdef01\n", Some(2052)).is_empty());
        assert!(parse_pending_list("moknew-size\n5c9cab0123456789abcdef0123456789abcdef01\n", Some(2052)).is_empty());
        assert!(parse_pending_list("", Some(0)).is_empty());
        assert!(parse_pending_list("moknew-size 7\n", Some(7)).is_empty());

        let path = "/var/lib/dkms/mok.pub";
        let sha = "5c9cab0123456789abcdef0123456789abcdef01";
        let enrolled_out = "/var/lib/dkms/mok.pub is already enrolled\n";
        // Enrolled wins over a list entry: the key was enrolled at boot.
        assert_eq!(
            classify_signing_key(path, enrolled_out, true, &fprs, Some(sha)),
            Some(SigningKeyState::Enrolled(path.into()))
        );
        assert_eq!(
            classify_signing_key(path, enrolled_out, true, &[], Some(sha)),
            Some(SigningKeyState::Enrolled(path.into()))
        );
        // Listed and not enrolled: queued (a plain user's --test-key can't see MokNew).
        assert_eq!(
            classify_signing_key(path, "/var/lib/dkms/mok.pub is not enrolled", true, &fprs, Some(sha)),
            Some(SigningKeyState::PendingEnrollment(path.into()))
        );
        assert_eq!(
            classify_signing_key(path, "/var/lib/dkms/mok.pub is not enrolled", true, &fprs, None),
            Some(SigningKeyState::NotEnrolled(path.into()))
        );
        assert_eq!(
            classify_signing_key(path, "/var/lib/dkms/mok.pub is already in the enrollment request", true, &[], None),
            Some(SigningKeyState::PendingEnrollment(path.into()))
        );
        assert_eq!(
            classify_signing_key(path, "EFI variables are not supported on this system", true, &[], Some(sha)),
            Some(SigningKeyState::EfiVarsNotMounted(path.into()))
        );
        assert_eq!(classify_signing_key(path, "EFI variables are not supported", false, &[], None), None);
        assert_eq!(classify_signing_key(path, "mokutil: command not found", true, &fprs, Some(sha)),
            Some(SigningKeyState::PendingEnrollment(path.into())));
    }

    #[test]
    fn signing_key_state_helpers() {
        assert!(SigningKeyState::NoKey.setup_needed());
        assert!(SigningKeyState::NotEnrolled("/c".into()).setup_needed());
        assert!(!SigningKeyState::Enrolled("/c".into()).setup_needed());
        assert!(!SigningKeyState::PendingEnrollment("/c".into()).setup_needed());
        assert_eq!(SigningKeyState::NoKey.cert_path(), None);
        assert_eq!(SigningKeyState::Untested("/c".into()).cert_path(), Some("/c"));
        assert_eq!(SigningKeyState::EfiVarsNotMounted("/c".into()).cert_path(), Some("/c"));
        assert!(SigningKeyState::EfiVarsNotMounted("/c".into()).setup_needed());
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
        // Same rule as the other apps and the install script: no spaces.
        assert!(validate_mok_password("has space1", "has space1").is_err());
        assert!(validate_mok_password("tab\there12", "tab\there12").is_err());
        assert!(validate_mok_password("Abc123!@#$%^&*()", "Abc123!@#$%^&*()").is_ok());
        assert!(validate_mok_password("p4ssw0rd~", "p4ssw0rd~").is_ok());
    }

    #[test]
    fn mok_password_rule_matches_the_script() {
        let script = include_str!("../scripts/privileged-install.sh");
        assert!(script.contains("local LC_ALL=C\n    [[ \"$1\" =~ ^[!-~]{8,16}$ ]]"));
        assert!(MOK_PASSWORD_HINT.contains("without spaces"));
    }

    #[test]
    fn efi_variables_not_mounted_is_explained() {
        let key = SigningKeyState::EfiVarsNotMounted("/c.der".into());
        assert!(signing_summary(&key, &ModuleSignature::Missing).contains("EFI variables are not mounted"));
        let on = SecureBootStatus::Enabled;
        assert!(secure_boot_summary(&on, &key).contains("not mounted"));
        assert!(secure_boot_advice(&on, &key, None, true).unwrap().contains("mount -t efivarfs"));
    }
}
