#!/usr/bin/env bash
# privileged-install.sh — runs as root via pkexec.
#
# SIMPLE, REPO-STYLE INSTALL
# --------------------------
# The new driver is installed to disk while the current one keeps
# running — exactly like a distro package upgrade. No session teardown,
# no module unloading, no black screen. The switch happens at the next
# reboot. The key is nvidia-installer's own
# --allow-installation-with-running-driver flag, which makes it proceed
# with a loaded driver and skip the (impossible) live module tests.
#
# Supports apt-based distros (Ubuntu, Debian, Mint) and dnf-based
# distros (Fedora, RHEL, Nobara). Package manager is detected once at
# startup and every package-related step below branches on it.
#
# Usage: privileged-install.sh <path-to.run> <sha256-of-run-file> [--dkms] [--hold]
#        [--kernel-module-type=open|proprietary]
# Without --kernel-module-type the installer picks the flavor from the
# detected GPUs (open for Turing and newer).
#
#        privileged-install.sh --setup-signing   (one-time password on stdin)
# Setup mode makes sure the machine has a module-signing key and queues its
# certificate for MOK enrollment. It never touches the installed driver.
#
# MODULE SIGNING
# --------------
# Modules are signed with the machine's existing module-signing key, looked
# up in the order DKMS uses: mok_signing_key/mok_certificate from the DKMS
# config, then Ubuntu's shim-signed MOK, then DKMS's own default, then
# Fedora's akmods key. DKMS installs are signed by DKMS itself; other
# installs get the key passed to nvidia-installer. With no key, nothing
# about the install changes. Only --setup-signing ever creates a key.
#
# SECURITY MODEL
# --------------
# The .run file normally lives in a user-writable directory, so it could be
# swapped between the app's checks and this script running it as root. To
# close that window the script (1) refuses files that are symlinks, owned
# by anyone but root or the invoking user, or writable by group/others,
# (2) copies the file into a root-only private directory, (3) verifies the
# SHA256 passed in by the caller against that PRIVATE COPY, and (4) runs
# everything (integrity check + install) from the copy, never the original.

# -e: any unhandled failure aborts, so a partially-applied config can't
# report success. Steps that are allowed to fail are handled explicitly.
set -euo pipefail

LOGFILE="/var/log/green-light.log"
log() {
    local msg="[green-light] $*"
    echo "$msg"
    echo "$(date '+%Y-%m-%d %H:%M:%S') $msg" >> "$LOGFILE" 2>/dev/null || true
}

trap 'log "ERROR: step failed near line $LINENO (exit $?) — install aborted"' ERR

# ── Package manager detection (shared by both modes) ───────────────────
detect_pkg_mgr() {
    if command -v apt-get >/dev/null 2>&1; then
        PKG_MGR="apt"
    elif command -v dnf >/dev/null 2>&1; then
        PKG_MGR="dnf"
    else
        log "ERROR: Neither apt-get nor dnf found. This script supports"
        log "       apt-based (Ubuntu, Debian, Mint) and dnf-based"
        log "       (Fedora, RHEL, Nobara) distros only."
        exit 1
    fi
}

# ── Module-signing key helpers ─────────────────────────────────────────
# Key/certificate pairs in lookup order after the DKMS config.
DKMS_DEFAULT_KEY="/var/lib/dkms/mok.key"
DKMS_DEFAULT_CERT="/var/lib/dkms/mok.pub"
SIGNING_KEY_PAIRS=(
    "/var/lib/shim-signed/mok/MOK.priv|/var/lib/shim-signed/mok/MOK.der"
    "$DKMS_DEFAULT_KEY|$DKMS_DEFAULT_CERT"
    "/etc/pki/akmods/private/private_key.priv|/etc/pki/akmods/certs/public_key.der"
)

# One value from a DKMS config assignment, as the shell would read it:
# quoted values up to the closing quote, bare values up to whitespace.
# $kernelver expands to the running kernel, as DKMS allows.
conf_value() {
    local v="$1"
    v="${v#"${v%%[![:space:]]*}"}"
    if [[ "$v" == \"* ]]; then
        v="${v#\"}"; v="${v%%\"*}"
    elif [[ "$v" == \'* ]]; then
        v="${v#\'}"; v="${v%%\'*}"
    else
        v="${v%%[[:space:]]*}"
    fi
    v="${v//\$\{kernelver\}/$(uname -r)}"
    v="${v//\$kernelver/$(uname -r)}"
    printf '%s' "$v"
}

# mok_signing_key / mok_certificate from framework.conf, then
# framework.conf.d/*.conf; the last assignment wins, as DKMS sources them.
read_dkms_signing_config() {
    CFG_KEY=""
    CFG_CERT=""
    local f line
    for f in /etc/dkms/framework.conf /etc/dkms/framework.conf.d/*.conf; do
        [[ -f "$f" ]] || continue
        while IFS= read -r line || [[ -n "$line" ]]; do
            if [[ "$line" =~ ^[[:space:]]*mok_signing_key=(.*)$ ]]; then
                CFG_KEY="$(conf_value "${BASH_REMATCH[1]}")"
            elif [[ "$line" =~ ^[[:space:]]*mok_certificate=(.*)$ ]]; then
                CFG_CERT="$(conf_value "${BASH_REMATCH[1]}")"
            fi
        done < "$f"
    done
}

key_present() { [[ "$1" == pkcs11:* || -f "$1" ]]; }

# Sets SIGN_KEY, SIGN_CERT and SIGN_SOURCE (config|default). SIGN_KEY stays
# empty when no key exists. When the DKMS config names a key, those paths
# are used even if missing (SIGN_MISSING=1), since that is what DKMS uses.
resolve_signing_key() {
    SIGN_KEY=""
    SIGN_CERT=""
    SIGN_SOURCE=""
    SIGN_MISSING=0
    read_dkms_signing_config
    if [[ -n "$CFG_KEY" || -n "$CFG_CERT" ]]; then
        SIGN_KEY="${CFG_KEY:-$DKMS_DEFAULT_KEY}"
        SIGN_CERT="${CFG_CERT:-$DKMS_DEFAULT_CERT}"
        SIGN_SOURCE="config"
        if ! key_present "$SIGN_KEY" || [[ ! -f "$SIGN_CERT" ]]; then SIGN_MISSING=1; fi
        return 0
    fi
    local pair
    for pair in "${SIGNING_KEY_PAIRS[@]}"; do
        if [[ -f "${pair%%|*}" && -f "${pair#*|}" ]]; then
            SIGN_KEY="${pair%%|*}"
            SIGN_CERT="${pair#*|}"
            SIGN_SOURCE="default"
            return 0
        fi
    done
    return 0
}

# Setup mode only: make sure a key exists, creating one the standard way
# when none does. Never overwrites an existing key or certificate.
ensure_signing_key() {
    resolve_signing_key
    if [[ -n "$SIGN_KEY" && $SIGN_MISSING -eq 0 ]]; then
        log "Using the existing module-signing key $SIGN_KEY (certificate $SIGN_CERT)"
        return 0
    fi
    if [[ -n "$SIGN_KEY" ]] && { key_present "$SIGN_KEY" || [[ -f "$SIGN_CERT" ]]; }; then
        log "ERROR: Only half of the signing key pair exists ($SIGN_KEY, $SIGN_CERT)."
        log "       Refusing to replace it; restore the missing file or fix the DKMS config."
        return 1
    fi
    if [[ "$SIGN_SOURCE" != "config" ]] && command -v update-secureboot-policy >/dev/null 2>&1; then
        log "Creating a module-signing key with update-secureboot-policy…"
        # SHIM_NOTRIGGER: create the key only; enrollment is queued below.
        SHIM_NOTRIGGER=y update-secureboot-policy --new-key >>"$LOGFILE" 2>&1 \
            || log "WARNING: update-secureboot-policy --new-key failed"
        resolve_signing_key
    fi
    if [[ -z "$SIGN_KEY" || $SIGN_MISSING -eq 1 ]] && command -v dkms >/dev/null 2>&1; then
        log "Letting DKMS create its module-signing key…"
        dkms generate_mok >>"$LOGFILE" 2>&1 || log "WARNING: dkms generate_mok failed"
        resolve_signing_key
    fi
    if [[ -z "$SIGN_KEY" || $SIGN_MISSING -eq 1 ]]; then
        log "ERROR: Could not create a module-signing key (see $LOGFILE)."
        return 1
    fi
    log "Created module-signing key $SIGN_KEY (certificate $SIGN_CERT)"
    return 0
}

# Path of the certificate in DER form, converting a PEM certificate into
# <dir> when needed. Without openssl the certificate is assumed to be DER
# (every default location stores DER).
cert_as_der() { # <cert> <dir>
    if ! command -v openssl >/dev/null 2>&1 \
        || openssl x509 -inform DER -in "$1" -noout >/dev/null 2>&1; then
        printf '%s' "$1"
        return 0
    fi
    openssl x509 -inform PEM -in "$1" -outform DER -out "$2/signing-cert.der" >/dev/null 2>&1 \
        || return 1
    printf '%s' "$2/signing-cert.der"
}

# Subject CN of a certificate (what modinfo reports as the signer), or
# nothing when it can't be read.
cert_subject_cn() { # <cert>
    command -v openssl >/dev/null 2>&1 || return 0
    local form subject line
    for form in DER PEM; do
        if subject="$(openssl x509 -inform "$form" -in "$1" -noout -subject \
                -nameopt multiline 2>/dev/null)"; then
            while IFS= read -r line; do
                if [[ "$line" =~ ^[[:space:]]*commonName[[:space:]]*=[[:space:]]*(.*)$ ]]; then
                    printf '%s' "${BASH_REMATCH[1]}"
                    return 0
                fi
            done <<<"$subject"
            return 0
        fi
    done
    return 0
}

# ── Setup mode: --setup-signing ────────────────────────────────────────
# Creates the key if needed and queues its certificate for MOK enrollment.
# The one-time password arrives on stdin and never appears in argv or a log.
if [[ "${1:-}" == "--setup-signing" ]]; then
    trap 'log "ERROR: step failed near line $LINENO (exit $?) — signing setup aborted"' ERR
    MOK_PASSWORD=""
    IFS= read -r MOK_PASSWORD || true
    log "==== Module signing setup started ===="
    PASSWORD_RE='^[[:print:]]{8,16}$'
    if [[ ! "$MOK_PASSWORD" =~ $PASSWORD_RE ]]; then
        log "ERROR: The one-time password must be 8 to 16 printable characters."
        exit 1
    fi
    detect_pkg_mgr

    MISSING_PKGS=()
    command -v mokutil >/dev/null 2>&1 || MISSING_PKGS+=(mokutil)
    command -v openssl >/dev/null 2>&1 || MISSING_PKGS+=(openssl)
    if [[ ${#MISSING_PKGS[@]} -gt 0 ]]; then
        log "Installing ${MISSING_PKGS[*]}…"
        if [[ "$PKG_MGR" == "apt" ]]; then
            apt-get install -y "${MISSING_PKGS[@]}" >>"$LOGFILE" 2>&1 \
                || { log "ERROR: apt-get could not install ${MISSING_PKGS[*]}"; exit 1; }
        else
            dnf install -y "${MISSING_PKGS[@]}" >>"$LOGFILE" 2>&1 \
                || { log "ERROR: dnf could not install ${MISSING_PKGS[*]}"; exit 1; }
        fi
    fi

    ensure_signing_key || exit 1

    PRIV_DIR="$(mktemp -d /var/tmp/green-light-signing.XXXXXX)"
    chmod 700 "$PRIV_DIR"
    trap 'rm -rf "$PRIV_DIR"' EXIT
    if ! CERT_DER="$(cert_as_der "$SIGN_CERT" "$PRIV_DIR")"; then
        log "ERROR: Could not read the certificate $SIGN_CERT"
        exit 1
    fi
    SIGNER_CN="$(cert_subject_cn "$CERT_DER")"
    log "Signing certificate: $SIGN_CERT${SIGNER_CN:+ (CN: $SIGNER_CN)}"

    # Enrolled ("already enrolled"), trusted another way ("already in db",
    # "already in the built-in trusted keyring") or already queued ("already
    # in the enrollment request"): nothing to queue.
    TEST_OUT="$(mokutil --test-key "$CERT_DER" 2>&1 || true)"
    if grep -qiE 'already (enrolled|in )' <<<"$TEST_OUT"; then
        log "The certificate is already enrolled or queued for enrollment; nothing to do."
        log "==== Module signing setup done ===="
        exit 0
    fi

    # mokutil --generate-hash reads the password twice from stdin when there
    # is no tty and prints a SHA-512 crypt line; keep only that line.
    HASH="$(printf '%s\n%s\n' "$MOK_PASSWORD" "$MOK_PASSWORD" \
        | mokutil --generate-hash 2>/dev/null | grep -oE '[$]6[$][^[:space:]]+' | head -n 1 || true)"
    MOK_PASSWORD=""
    if [[ -z "$HASH" ]]; then
        log "ERROR: mokutil --generate-hash produced no password hash."
        exit 1
    fi
    HASH_FILE="$(umask 077; mktemp "$PRIV_DIR/hash.XXXXXX")"
    printf '%s\n' "$HASH" > "$HASH_FILE"
    HASH=""
    if ! mokutil --import "$CERT_DER" --hash-file "$HASH_FILE" >>"$LOGFILE" 2>&1; then
        log "ERROR: mokutil --import failed (see $LOGFILE)."
        exit 1
    fi
    rm -f "$HASH_FILE"
    log "Enrollment queued. On the next reboot, choose Enroll MOK at the blue MOK Manager screen and type the one-time password."
    log "==== Module signing setup done ===="
    exit 0
fi

ORIG_RUN_FILE="${1:-}"
EXPECT_SHA256="${2:-}"
USE_DKMS=0
HOLD_PKG=0
MODULE_TYPE=""

if [[ -z "$ORIG_RUN_FILE" ]]; then log "ERROR: No .run file specified"; exit 1; fi
if [[ ! "$ORIG_RUN_FILE" =~ ^/.*\.run$ ]]; then log "ERROR: Invalid run file path: $ORIG_RUN_FILE"; exit 1; fi
if [[ -L "$ORIG_RUN_FILE" ]]; then log "ERROR: Refusing symlink: $ORIG_RUN_FILE"; exit 1; fi
if [[ ! -f "$ORIG_RUN_FILE" ]]; then log "ERROR: File not found: $ORIG_RUN_FILE"; exit 1; fi
if [[ ! "$EXPECT_SHA256" =~ ^[0-9a-fA-F]{64}$ ]]; then
    log "ERROR: A 64-character SHA256 of the run file is required as the second argument"
    exit 1
fi
EXPECT_SHA256="${EXPECT_SHA256,,}"

# Ownership / permission check on the original file.
FILE_UID="$(stat -c '%u' "$ORIG_RUN_FILE")"
FILE_MODE="$(stat -c '%a' "$ORIG_RUN_FILE")"
CALLER_UID="${PKEXEC_UID:-${SUDO_UID:-0}}"
if [[ "$FILE_UID" != "0" && "$FILE_UID" != "$CALLER_UID" ]]; then
    log "ERROR: $ORIG_RUN_FILE is owned by uid $FILE_UID, not root or the invoking user ($CALLER_UID)"
    exit 1
fi
if (( (8#$FILE_MODE) & 8#022 )); then
    log "ERROR: $ORIG_RUN_FILE is writable by group/others (mode $FILE_MODE) — refusing"
    exit 1
fi

shift 2
for arg in "$@"; do
    case "$arg" in
        --dkms)       USE_DKMS=1 ;;
        --hold)       HOLD_PKG=1 ;;
        --kernel-module-type=open|--kernel-module-type=proprietary)
            MODULE_TYPE="${arg#--kernel-module-type=}" ;;
        *) log "WARNING: Unknown argument: $arg" ;;
    esac
done

# ── Detect package manager ─────────────────────────────────────────────
detect_pkg_mgr

log "==== NVIDIA driver install started ===="
log "Run file: $ORIG_RUN_FILE (dkms=$USE_DKMS hold=$HOLD_PKG module=${MODULE_TYPE:-auto} pkg_mgr=$PKG_MGR)"

# Log the tail of the newest DKMS build log for the nvidia module, so a failed
# kernel module build is visible in the log and the app without digging.
log_dkms_build_tail() {
    local newest="" f
    for f in /var/lib/dkms/nvidia/*/build/make.log; do
        [[ -f "$f" ]] || continue
        if [[ -z "$newest" || "$f" -nt "$newest" ]]; then newest="$f"; fi
    done
    if [[ -z "$newest" ]]; then return 0; fi
    log "Build log: last lines of $newest"
    tail -n 15 "$newest" 2>/dev/null | while IFS= read -r line; do
        log "Build log: $line"
    done
    return 0
}

# ── Step 0: Copy into a root-only directory and verify the copy ───────
# /var/tmp rather than /tmp: the installer self-extracts and executes, and
# /tmp is often mounted noexec.
PRIV_DIR="$(mktemp -d /var/tmp/green-light-install.XXXXXX)"
chmod 700 "$PRIV_DIR"
trap 'rm -rf "$PRIV_DIR"' EXIT
RUN_FILE="$PRIV_DIR/installer.run"
log "Copying installer to private directory…"
if ! cp --no-preserve=mode,ownership -- "$ORIG_RUN_FILE" "$RUN_FILE"; then
    log "ERROR: Could not copy the installer (disk full?). No changes made."
    exit 1
fi
chmod 700 "$RUN_FILE"
ACTUAL_SHA256="$(sha256sum "$RUN_FILE" | cut -d' ' -f1)"
if [[ "$ACTUAL_SHA256" != "$EXPECT_SHA256" ]]; then
    log "ERROR: SHA256 mismatch on the private copy (expected $EXPECT_SHA256, got $ACTUAL_SHA256). No changes made."
    exit 1
fi
log "SHA256 verified on private copy"

# ── Step 1: Verify archive integrity before touching anything ────────
log "Verifying installer archive integrity…"
if ! "$RUN_FILE" --check >>"$LOGFILE" 2>&1; then
    log "ERROR: Installer failed its integrity self-check. No changes made."
    exit 1
fi
log "Integrity OK"

# ── Step 2: Build prerequisites (non-fatal if the package manager balks)
KVER="$(uname -r)"
log "Ensuring kernel headers and build tools for $KVER…"
if [[ "$PKG_MGR" == "apt" ]]; then
    apt-get install -y "linux-headers-${KVER}" build-essential dkms >>"$LOGFILE" 2>&1 \
        || log "WARNING: apt could not confirm prerequisites — continuing"
else
    dnf install -y "kernel-devel-${KVER}" "kernel-headers-${KVER}" \
        gcc make dkms >>"$LOGFILE" 2>&1 \
        || log "WARNING: dnf could not confirm prerequisites — continuing"
fi

# ── Step 3: Clear conflicting distro packages (non-fatal)
# Removing package files does not affect the running driver — the loaded
# kernel module and already-mapped libraries keep working, same as
# during a normal package-manager driver upgrade.
#
# The package set here has caused two real failures in practice:
#   - nvidia-container-toolkit / libnvidia-container* match nvidia-*/
#     libnvidia-* but are Docker's GPU-passthrough plumbing, not the
#     display driver. Purging them doesn't touch the running driver, but
#     it breaks every GPU container the moment its runtime next restarts
#     (confirmed: took down a running Frigate NVR container this way).
#   - xserver-xorg-video-nvidia-<ver> does NOT match nvidia-*/libnvidia-*
#     (wrong prefix) but is part of the same apt-managed driver flavor and
#     is a reverse-dependency of nvidia-support-<ver>. Leaving it installed
#     makes dpkg refuse to remove nvidia-support-<ver> — silently, since
#     every removal below used to be `|| true`. The half-removed state that
#     resulted left nvidia-support-<ver>'s
#     /usr/lib/nvidia/alternate-install-present marker file in place, which
#     makes the .run installer itself abort with "please use the Debian
#     packages instead" — on a machine that's mid-purge of those exact
#     packages. Confirmed end to end against a real install.
log "Removing distro-managed NVIDIA packages (if any)…"
if [[ "$PKG_MGR" == "apt" ]]; then
    apt-mark unhold 'nvidia*' 'libnvidia*' 'xserver-xorg-video-nvidia*' 2>/dev/null || true
    # Driver packages only. libcuda*/libcudnn* are deliberately NOT matched:
    # they can belong to a user-installed CUDA toolkit unrelated to the
    # display driver (the driver's own libcuda is in libnvidia-compute-*).
    mapfile -t PKGS < <(dpkg -l 'nvidia-*' 'libnvidia-*' \
                 'xserver-xorg-video-nvidia*' 2>/dev/null \
        | awk '/^ii/{print $2}' | grep -v '^green-light' \
        | grep -vE '^(nvidia-container-toolkit|libnvidia-container)' || true)
    if [[ ${#PKGS[@]} -gt 0 ]]; then
        log "  purging: ${PKGS[*]}"
        # No dpkg --force-all fallback: it can leave dpkg in a broken
        # dependency state. Fail cleanly instead — nothing has been
        # installed yet and the running driver is untouched.
        if ! apt-get purge -y "${PKGS[@]}" >>"$LOGFILE" 2>&1; then
            log "ERROR: apt-get purge of the distro NVIDIA packages failed. No driver changes made."
            log "       Resolve the dependency conflict (see $LOGFILE) and retry."
            exit 1
        fi
    fi
    update-alternatives --remove-all nvidia 2>/dev/null || true   # absent alternative is fine
    update-alternatives --remove-all nvidia-ld.so.conf 2>/dev/null || true

    # The .run installer refuses to proceed if this marker is present,
    # regardless of whether the purge above actually removed the distro
    # package that left it there.
    if [[ -e /usr/lib/nvidia/alternate-install-present ]]; then
        log "Removing stale alternate-install marker left by the distro packages…"
        rm -f /usr/lib/nvidia/alternate-install-present
    fi
else
    # Fedora driver packages typically come from RPM Fusion: akmod-nvidia,
    # xorg-x11-drv-nvidia*, kmod-nvidia*, nvidia-driver* if present.
    if dnf versionlock --help >/dev/null 2>&1; then
        dnf versionlock delete 'nvidia*' 'akmod-nvidia*' 'xorg-x11-drv-nvidia*' \
            'kmod-nvidia*' 2>/dev/null || true
    fi
    mapfile -t PKGS < <(rpm -qa 'akmod-nvidia*' 'xorg-x11-drv-nvidia*' 'kmod-nvidia*' \
        'nvidia-driver*' 'nvidia-settings*' 2>/dev/null || true)
    if [[ ${#PKGS[@]} -gt 0 ]]; then
        log "  removing: ${PKGS[*]}"
        if ! dnf remove -y "${PKGS[@]}" >>"$LOGFILE" 2>&1; then
            log "ERROR: dnf remove of the distro NVIDIA packages failed. No driver changes made."
            exit 1
        fi
    fi
fi

# ── Step 4: On-disk boot config (takes effect at next boot) ───────────
log "Writing nouveau blacklist and nvidia modeset config…"
cat > /etc/modprobe.d/blacklist-nouveau.conf << 'BLACKLIST'
blacklist nouveau
options nouveau modeset=0
BLACKLIST
cat > /etc/modprobe.d/nvidia-drm-modeset.conf << 'MODESET'
options nvidia_drm modeset=1
MODESET

# ── Step 5: Run the installer — repo-style, old driver keeps running ──
log "Running the NVIDIA installer (a few minutes; desktop stays up)…"
INSTALLER_ARGS=(
    --silent
    --accept-license
    --ui=none
    --no-x-check
    --allow-installation-with-running-driver
    --log-file-name=/var/log/nvidia-installer.log
)
if [[ $USE_DKMS -eq 1 ]]; then INSTALLER_ARGS+=(--dkms); fi
if [[ -n "$MODULE_TYPE" ]]; then INSTALLER_ARGS+=("--kernel-module-type=$MODULE_TYPE"); fi

# Module signing: use the machine's existing key; with none, change nothing.
# DKMS signs with this key itself at build time; without DKMS the installer
# gets the key (PEM private key, DER certificate).
resolve_signing_key
if [[ -n "$SIGN_KEY" && $SIGN_MISSING -eq 1 ]]; then
    log "WARNING: The DKMS config names signing key $SIGN_KEY / $SIGN_CERT, but it is missing"
    SIGN_KEY=""
    SIGN_CERT=""
fi
if [[ -n "$SIGN_KEY" ]]; then
    log "Module signing key: $SIGN_KEY (certificate $SIGN_CERT)"
    if [[ $USE_DKMS -eq 0 ]]; then
        if [[ "$SIGN_KEY" == pkcs11:* ]]; then
            log "WARNING: A PKCS#11 signing key can't be passed to the NVIDIA installer; the module will not be signed"
        elif SIGN_CERT_DER="$(cert_as_der "$SIGN_CERT" "$PRIV_DIR")"; then
            INSTALLER_ARGS+=(
                "--module-signing-secret-key=$SIGN_KEY"
                "--module-signing-public-key=$SIGN_CERT_DER"
            )
        else
            log "WARNING: Could not read certificate $SIGN_CERT; the module will not be signed"
        fi
    fi
fi

RC=0
"$RUN_FILE" "${INSTALLER_ARGS[@]}" >>"$LOGFILE" 2>&1 || RC=$?
if [[ $RC -ne 0 ]]; then
    log "ERROR: NVIDIA installer exited with code $RC"
    log "See /var/log/nvidia-installer.log for details."
    log_dkms_build_tail
    exit $RC
fi
log "NVIDIA installer finished successfully"

# ── Step 6: Rebuild initramfs so the blacklist applies at boot ────────
log "Rebuilding initramfs…"
if [[ "$PKG_MGR" == "apt" ]]; then
    INITRAMFS_CMD=(update-initramfs -u -k "$KVER")
else
    INITRAMFS_CMD=(dracut --force --kver "$KVER")
fi
if ! "${INITRAMFS_CMD[@]}" >>"$LOGFILE" 2>&1; then
    log "ERROR: initramfs rebuild failed. The driver is installed but the nouveau"
    log "       blacklist may not apply at boot. Run: ${INITRAMFS_CMD[*]}"
    exit 1
fi

# ── Step 6b: Verify the result (report only; never fails the install) ─
# Everything here is a read-only check whose outcome goes to the log; the app
# shows the "Verify:" lines after the install finishes.
log "Verifying the installation…"
VERIFY_WARNINGS=0
verify_warn() { log "Verify: WARNING $*"; VERIFY_WARNINGS=$((VERIFY_WARNINGS + 1)); }

if grep -qs '^blacklist nouveau' /etc/modprobe.d/blacklist-nouveau.conf; then
    log "Verify: nouveau blacklist present"
else
    verify_warn "nouveau blacklist file is missing or empty"
fi

# initramfs: confirm it was built and carries the blacklist.
if [[ "$PKG_MGR" == "apt" ]]; then
    INITRAMFS_IMG="/boot/initrd.img-$KVER"
    INITRAMFS_LIST_CMD=(lsinitramfs)
else
    INITRAMFS_IMG="/boot/initramfs-$KVER.img"
    INITRAMFS_LIST_CMD=(lsinitrd)
fi
if [[ ! -f "$INITRAMFS_IMG" ]]; then
    verify_warn "initramfs image not found at $INITRAMFS_IMG"
elif command -v "${INITRAMFS_LIST_CMD[0]}" >/dev/null 2>&1; then
    # Capture first: grep -q on a pipe would SIGPIPE the lister under pipefail.
    INITRAMFS_FILES="$("${INITRAMFS_LIST_CMD[@]}" "$INITRAMFS_IMG" 2>/dev/null || true)"
    if grep -q 'blacklist-nouveau' <<<"$INITRAMFS_FILES"; then
        log "Verify: initramfs rebuilt and includes the nouveau blacklist ($INITRAMFS_IMG)"
    else
        verify_warn "initramfs $INITRAMFS_IMG does not list blacklist-nouveau.conf"
    fi
else
    log "Verify: initramfs rebuilt ($INITRAMFS_IMG); contents not checked (${INITRAMFS_LIST_CMD[0]} not found)"
fi

# DKMS: the nvidia module should be built and installed for the running kernel.
if [[ $USE_DKMS -eq 1 ]]; then
    if command -v dkms >/dev/null 2>&1; then
        DKMS_NVIDIA="$(dkms status 2>/dev/null | grep -i '^nvidia' || true)"
        DKMS_FOR_KERNEL="$(grep -F "$KVER" <<<"$DKMS_NVIDIA" || true)"
        if grep -q ': installed' <<<"$DKMS_FOR_KERNEL"; then
            log "Verify: DKMS module installed for $KVER"
        else
            verify_warn "DKMS shows no installed nvidia module for $KVER (dkms status: ${DKMS_NVIDIA:-none})"
            log_dkms_build_tail
        fi
    else
        verify_warn "--dkms was requested but the dkms command is not available"
    fi
fi

# Module signature: with a signing key, the module should be signed by it.
# Without one, only Secure Boot makes the signature matter.
SB_STATE=""
if command -v mokutil >/dev/null 2>&1; then SB_STATE="$(mokutil --sb-state 2>/dev/null || true)"; fi
if [[ -n "$SIGN_KEY" ]]; then
    MODULE_SIGNER="$(modinfo -k "$KVER" -F signer nvidia 2>/dev/null || true)"
    EXPECTED_SIGNER="$(cert_subject_cn "$SIGN_CERT")"
    if [[ -z "$MODULE_SIGNER" ]]; then
        verify_warn "the nvidia module for $KVER is unsigned; expected a signature from ${EXPECTED_SIGNER:-$SIGN_CERT}"
    elif [[ -n "$EXPECTED_SIGNER" && "$MODULE_SIGNER" != "$EXPECTED_SIGNER" ]]; then
        verify_warn "the nvidia module for $KVER is signed by \"$MODULE_SIGNER\", not by the signing key \"$EXPECTED_SIGNER\" ($SIGN_CERT)"
    else
        log "Verify: nvidia module is signed by: $MODULE_SIGNER"
    fi
    if command -v mokutil >/dev/null 2>&1; then
        MOK_CERT="$(cert_as_der "$SIGN_CERT" "$PRIV_DIR" || printf '%s' "$SIGN_CERT")"
        MOK_TEST="$(mokutil --test-key "$MOK_CERT" 2>&1 | head -n 1 || true)"
        log "Verify: signing certificate enrollment: ${MOK_TEST:-unknown}"
    fi
elif grep -qi 'SecureBoot enabled' <<<"$SB_STATE"; then
    MODULE_SIGNER="$(modinfo -k "$KVER" -F signer nvidia 2>/dev/null || true)"
    if [[ -n "$MODULE_SIGNER" ]]; then
        log "Verify: Secure Boot is on; nvidia module is signed by: $MODULE_SIGNER"
    else
        verify_warn "Secure Boot is on but the nvidia module for $KVER is unsigned; use Set Up Signing on Green Light's System tab, then reinstall the driver"
    fi
fi

if [[ $VERIFY_WARNINGS -eq 0 ]]; then
    log "Verify: all checks passed"
else
    log "Verify: $VERIFY_WARNINGS warning(s); see above"
fi

# ── Step 7: Optional package hold ──────────────────────────────────────
if [[ $HOLD_PKG -eq 1 ]]; then
    if [[ "$PKG_MGR" == "apt" ]]; then
        HELD=$(dpkg -l 'nvidia-*' 'libnvidia-*' 'xserver-xorg-video-nvidia*' 2>/dev/null \
            | awk '/^ii/{print $2}' \
            | grep -vE '^(nvidia-container-toolkit|libnvidia-container)' || true)
        if [[ -n "$HELD" ]]; then
            # $HELD is newline-separated package names; word-splitting intended.
            # shellcheck disable=SC2086
            if apt-mark hold $HELD >>"$LOGFILE" 2>&1; then
                log "Held packages: $HELD"
            else
                log "WARNING: apt-mark hold failed — packages not held"
            fi
        fi
    else
        if dnf versionlock --help >/dev/null 2>&1; then
            if dnf versionlock add 'akmod-nvidia*' 'xorg-x11-drv-nvidia*' \
                'kmod-nvidia*' >>"$LOGFILE" 2>&1; then
                log "Versionlock applied to nvidia packages"
            else
                log "WARNING: dnf versionlock add failed — packages not locked"
            fi
        else
            log "WARNING: --hold requested but dnf versionlock plugin is not"
            log "         installed. Run: dnf install python3-dnf-plugin-versionlock"
        fi
    fi
fi

log "==== Done. Reboot to switch to the new driver. ===="
exit 0
