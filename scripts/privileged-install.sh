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
#        [--no-x-check] [--kernel-module-type=open|proprietary]
# Without --kernel-module-type the installer picks the flavor from the
# detected GPUs (open for Turing and newer).
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
        --no-x-check) : ;;   # always passed to the installer now; kept for compatibility
        --kernel-module-type=open|--kernel-module-type=proprietary)
            MODULE_TYPE="${arg#--kernel-module-type=}" ;;
        *) log "WARNING: Unknown argument: $arg" ;;
    esac
done

# ── Detect package manager ─────────────────────────────────────────────
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

# Secure Boot: the module must carry a signature the firmware trusts.
SB_STATE=""
if command -v mokutil >/dev/null 2>&1; then SB_STATE="$(mokutil --sb-state 2>/dev/null || true)"; fi
if grep -qi 'SecureBoot enabled' <<<"$SB_STATE"; then
    MODULE_SIGNER="$(modinfo -k "$KVER" -F signer nvidia 2>/dev/null || true)"
    if [[ -n "$MODULE_SIGNER" ]]; then
        log "Verify: Secure Boot is on; nvidia module is signed by: $MODULE_SIGNER"
    else
        verify_warn "Secure Boot is on but the nvidia module for $KVER is unsigned; enroll a signing key with: mokutil --import <key.der>"
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
