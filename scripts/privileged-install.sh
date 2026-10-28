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
# Usage: privileged-install.sh <absolute-path-to.run> <sha256-of-run-file> [--dkms]
#        [--block-distro-driver] [--kernel-module-type=open|proprietary]
# Without --kernel-module-type the installer picks the flavor from the
# detected GPUs (open for Turing and newer). --block-distro-driver keeps the
# package manager from installing the distro's NVIDIA driver packages over
# this driver later (apt pin / dnf5 exclude); without it any such block from
# an earlier install is removed.
#
#        privileged-install.sh --setup-signing   (one-time password on stdin)
# Setup mode makes sure the machine has a module-signing key and queues its
# certificate for MOK enrollment. It never touches the installed driver.
#
# EXIT CODES
# ----------
# pkexec itself exits 126 when the password prompt is dismissed and 127 when
# authorization fails, so this script never exits with either (the EXIT trap
# maps a stray 126/127, e.g. a missing command, to 1):
#   0  success
#   1  failed before anything was changed
#   2  bad arguments, or a .run file it refuses to use
#   3  the install failed after it had started changing the system (distro
#      driver packages removed, boot config written), and all of that was
#      put back: the next boot uses the driver the machine had before
#   4  as 3, but putting it back failed too: the machine needs attention
#      before a reboot
#   5  the driver is installed, but rebuilding the initramfs failed
#   6  recovery data from an earlier interrupted install no longer matches
#      this computer (its driver was changed by other means since): nothing
#      done; "--discard-recovery" drops that data
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
# by anyone but root or the invoking user, or writable by anyone else
# (group-writable is accepted only for the owner's own private group, the
# umask-002 default on Ubuntu, Debian and Fedora, where nobody else is in
# the group), (2) copies the file into a root-only private directory,
# (3) verifies the SHA256 passed in by the caller against that PRIVATE COPY,
# and (4) runs everything (integrity check + install) from the copy, never
# the original.

# -e: any unhandled failure aborts, so a partially-applied config can't
# report success. Steps that are allowed to fail are handled explicitly.
set -euo pipefail

EXIT_FAILED=1
EXIT_USAGE=2
EXIT_ROLLED_BACK=3
EXIT_ROLLBACK_FAILED=4
EXIT_INITRAMFS=5
EXIT_RECOVERY_REFUSED=6

LOGFILE="/var/log/green-light.log"

# Nothing this script writes may go to a pipe that can vanish (the app that
# started it may be closed mid-install, which would kill it with SIGPIPE):
# without a terminal, stdout goes nowhere and stderr to the log file, and the
# app follows the log file instead. SIGPIPE keeps its default for the
# commands run below. SIGHUP is ignored (a closing terminal must not stop an
# install; the commands inherit that, which they should).
# (Run by systemd, the boot unit, stdout and stderr go to the journal and
# the console, which don't go away; keep them.)
if [[ ! -t 1 && -z "${INVOCATION_ID:-}" ]]; then exec >/dev/null; fi
if [[ ! -t 2 && -z "${INVOCATION_ID:-}" ]]; then exec 2>>"$LOGFILE"; fi
trap '' HUP

# Each log line carries this run's PID: the app shows only the lines of the
# run it started (its pkexec child has this PID).
PLYMOUTH=0
log() {
    local msg="[green-light] $*"
    echo "$(date '+%Y-%m-%d %H:%M:%S') [green-light $$] $*" >> "$LOGFILE" 2>/dev/null || true
    echo "$msg" 2>/dev/null || true
    # At boot, on the splash screen as well.
    if [[ $PLYMOUTH -eq 1 ]]; then plymouth display-message --text="Green Light: $*" 2>/dev/null || true; fi
}

trap 'log "ERROR: step failed near line $LINENO (exit $?) — install aborted"' ERR

# Set once the install starts changing the system (distro packages, boot
# config); cleared once NVIDIA's installer has succeeded. Any failure in
# between restores the previous setup (rollback, defined below).
ROLLBACK_ARMED=0
PRIV_DIR=""
INHIBIT_PID=""
on_exit() {
    local rc=$1
    set +e
    trap - ERR
    if [[ $ROLLBACK_ARMED -eq 1 && $rc -ne 0 ]]; then
        ROLLBACK_ARMED=0
        if rollback; then
            journal_clear
            rc=$EXIT_ROLLED_BACK
        else
            # Kept: the next install, Restore Previous Driver or the boot
            # unit tries again.
            recovery_note "failed: the previous driver could not be fully restored; see $LOGFILE"
            rc=$EXIT_ROLLBACK_FAILED
        fi
    elif [[ ${JOURNAL_OURS:-0} -eq 1 ]]; then
        # Ended before changing anything: nothing to put back.
        journal_clear
    fi
    if [[ -n "$INHIBIT_PID" ]]; then kill "$INHIBIT_PID" 2>/dev/null; fi
    if [[ -n "$PRIV_DIR" ]]; then rm -rf "$PRIV_DIR"; fi
    if [[ $rc -eq 126 || $rc -eq 127 ]]; then rc=$EXIT_FAILED; fi
    exit "$rc"
}
# Signals are ignored first thing, so nothing can cut the rollback short.
# shellcheck disable=SC2154  # assigned inside the trap string
trap 'on_exit_rc=$?; trap "" TERM INT; on_exit "$on_exit_rc"' EXIT
# SIGTERM/SIGINT ask the run to stop; it does so at the next point where
# that is safe (see STOP_REQUESTED below), never in the middle of a step.
STOP_REQUESTED=0
trap 'STOP_REQUESTED=1; log "Stop requested (SIGTERM); stopping at the next safe point…"' TERM
trap 'STOP_REQUESTED=1; log "Stop requested (SIGINT); stopping at the next safe point…"' INT
# Stub for setup mode; the install part defines the real one.
journal_clear() { :; }

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
        exit "$EXIT_FAILED"
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

# Run a command without the lock's file descriptor, so nothing it leaves
# running in the background (akmods from an rpm scriptlet, a DKMS build)
# keeps the lock, and with it the "install running" state, after this run;
# and with SIGTERM/SIGINT at their defaults, whatever this script does with
# them (it ignores them while putting things back). Used by the signing
# setup below too, which holds the same lock.
# (env resets dispositions bash can't: a signal already ignored when the
# script started, like SIGPIPE under some service managers, stays ignored
# for bash itself.)
ENV_DEFAULT_SIGNALS=()
if env --default-signal=PIPE true 2>/dev/null; then ENV_DEFAULT_SIGNALS=(env "--default-signal=PIPE,TERM,INT"); fi
nolock() { ( trap - TERM INT; exec "${ENV_DEFAULT_SIGNALS[@]}" "$@" ) 9>&-; }

# ── Setup mode: --setup-signing ────────────────────────────────────────
# The list of waiting keys the apps read (MokNew itself is readable by root
# only). It is shared: another app's signing setup rebuilds the same request
# and writes the same list. First line "moknew-size <bytes>", MokNew's size
# right after the import, so an app can tell when the request has changed
# since (enrolled at boot, revoked, replaced by other means) and ignore the
# list; then one SHA1 fingerprint (lowercase hex) per line, every key in the
# request.
MOK_PENDING_LIST="/var/lib/mok-pending/list"
MOK_GUID="605dab50-e046-4300-abb6-3dd810dd8b23"
EFI_DIR="/sys/firmware/efi"
EFIVARS_DIR="$EFI_DIR/efivars"
MOK_NEW="$EFIVARS_DIR/MokNew-$MOK_GUID"

# MOK Manager takes 8 to 16 characters typed on a bare (US layout) keyboard
# at boot: printable ASCII without spaces, whatever the locale.
valid_mok_password() {
    local LC_ALL=C
    [[ "$1" =~ ^[!-~]{8,16}$ ]]
}

# Write the shared list for the keys just imported (<sha1>...), atomically.
write_mok_pending_list() {
    local dir size tmp
    dir="${MOK_PENDING_LIST%/*}"
    if ! size="$(stat -c %s "$MOK_NEW" 2>/dev/null)"; then
        rm -f "$MOK_PENDING_LIST"
        log "WARNING: mokutil reported success, but no enrollment request can be seen, so the keys may not be waiting. Set up signing again."
        return 1
    fi
    mkdir -p "$dir" && chmod 755 "$dir" || return 1
    tmp="$(mktemp "$dir/.list.XXXXXX")" || return 1
    if { printf 'moknew-size %s\n' "$size"; printf '%s\n' "$@"; } > "$tmp" \
            && chmod 644 "$tmp" && mv -f "$tmp" "$MOK_PENDING_LIST"; then
        return 0
    fi
    rm -f "$tmp"
    return 1
}

# Before anything is decided about MokNew: on an EFI machine the EFI
# variables must be mounted, or MokNew looks absent and mokutil sees
# nothing (a chroot teardown can unmount them, for example). Mounted means
# another filesystem than the EFI firmware directory's (a mount that has been
# covered by another one is still listed in mountinfo, so that is not asked).
efivars_mounted() {
    local d e
    d="$(stat -c %d "$EFIVARS_DIR" 2>/dev/null)" || return 1
    e="$(stat -c %d "$EFI_DIR" 2>/dev/null)" || return 1
    [[ "$d" != "$e" ]]
}
ensure_efivarfs() {
    [[ -d "$EFI_DIR" ]] || return 0
    efivars_mounted && return 0
    log "The EFI variables are not mounted; mounting them on $EFIVARS_DIR"
    if ! mount -t efivarfs -o nosuid,nodev,noexec efivarfs "$EFIVARS_DIR" >>"$LOGFILE" 2>&1; then
        log "ERROR: The EFI variables are not mounted and could not be mounted on $EFIVARS_DIR, so Green Light cannot see the waiting enrollment request or check the key. Nothing was changed. Mount them (mount -t efivarfs efivarfs $EFIVARS_DIR) or reboot, then set up signing again."
        return 1
    fi
}

# Creates the key if needed and queues its certificate for MOK enrollment.
# The one-time password arrives on stdin and never appears in argv or a log.
if [[ "${1:-}" == "--setup-signing" ]]; then
    trap 'log "ERROR: step failed near line $LINENO (exit $?) — signing setup aborted"' ERR
    # Not alongside an install (same lock as the install part below).
    exec 9>>/run/green-light-install.lock
    if ! flock -n 9; then
        log "ERROR: A Green Light install is running; set up signing once it has finished."
        exit 1
    fi
    MOK_PASSWORD=""
    IFS= read -r MOK_PASSWORD || true
    log "==== Module signing setup started ===="
    if ! valid_mok_password "$MOK_PASSWORD"; then
        log "ERROR: The one-time password must be 8 to 16 printable ASCII characters without spaces."
        exit 1
    fi
    ensure_efivarfs || exit 1
    detect_pkg_mgr

    MISSING_PKGS=()
    command -v mokutil >/dev/null 2>&1 || MISSING_PKGS+=(mokutil)
    command -v openssl >/dev/null 2>&1 || MISSING_PKGS+=(openssl)
    if [[ ${#MISSING_PKGS[@]} -gt 0 ]]; then
        log "Installing ${MISSING_PKGS[*]}…"
        if [[ "$PKG_MGR" == "apt" ]]; then
            nolock apt-get install -y "${MISSING_PKGS[@]}" >>"$LOGFILE" 2>&1 \
                || { log "ERROR: apt-get could not install ${MISSING_PKGS[*]}"; exit 1; }
        else
            nolock dnf install -y "${MISSING_PKGS[@]}" >>"$LOGFILE" 2>&1 \
                || { log "ERROR: dnf could not install ${MISSING_PKGS[*]}"; exit 1; }
        fi
    fi

    ensure_signing_key || exit 1

    PRIV_DIR="$(mktemp -d /var/tmp/green-light-signing.XXXXXX)"
    chmod 700 "$PRIV_DIR"   # removed by on_exit
    if ! CERT_DER="$(cert_as_der "$SIGN_CERT" "$PRIV_DIR")"; then
        log "ERROR: Could not read the certificate $SIGN_CERT"
        exit 1
    fi
    SIGNER_CN="$(cert_subject_cn "$CERT_DER")"
    log "Signing certificate: $SIGN_CERT${SIGNER_CN:+ (CN: $SIGNER_CN)}"

    # shim keeps ONE pending enrollment request (MokNew) with ONE password
    # (MokAuth). `mokutil --import` adds to a waiting request but replaces its
    # password, and skips a key that is already waiting without touching the
    # password. So another tool's earlier request (a kernel-signing key, say)
    # and this one would need different passwords, and only the last one
    # works at MOK Manager. Instead the whole request is rebuilt here: the
    # certificates already waiting (read from MokNew) plus this one, under
    # the password chosen now, and the log says so.
    mkdir -p "$PRIV_DIR/pending"
    PENDING_RC=0
    if [[ -e "$MOK_NEW" ]]; then
        perl -e '
            use strict; use warnings;
            my ($file, $dir) = @ARGV;
            open(my $fh, "<:raw", $file) or exit 2;
            local $/; my $raw = <$fh>; close $fh;
            my $data = substr($raw, 4);   # efivarfs: 4 attribute bytes first
            my $x509 = pack("H*", "a159c0a5e494a74a87b5ab155c2bf072");
            my ($off, $n, $other) = (0, 0, 0);
            while ($off < length $data) {
                exit 2 if length($data) - $off < 28;
                my $type = substr($data, $off, 16);
                my ($lsize, $hsize, $ssize) = unpack("V3", substr($data, $off + 16, 12));
                exit 2 if $lsize < 28 || $off + $lsize > length $data || $ssize < 16;
                # The header and whole entries must fill the list exactly.
                exit 2 if 28 + $hsize > $lsize || ($lsize - 28 - $hsize) % $ssize != 0;
                my $e = $off + 28 + $hsize;
                while ($e + $ssize <= $off + $lsize) {
                    if ($type eq $x509) {
                        $n++;
                        open(my $out, ">:raw", "$dir/pending-$n.der") or exit 2;
                        print {$out} substr($data, $e + 16, $ssize - 16);
                        close $out or exit 2;
                    } else { $other++ }
                    $e += $ssize;
                }
                $off += $lsize;
            }
            exit($other ? 3 : 0);
        ' "$MOK_NEW" "$PRIV_DIR/pending" || PENDING_RC=$?
    fi
    case $PENDING_RC in
        0) ;;
        3) log "ERROR: Another enrollment request is waiting that holds more than certificates, which Green Light cannot carry over. Finish it at the next reboot, then set up signing again."
           exit 1 ;;
        *) log "ERROR: The waiting enrollment request could not be read, so Green Light will not replace it. Reboot to finish it first, then set up signing again."
           exit 1 ;;
    esac

    cert_sha1() { openssl x509 -inform DER -in "$1" -noout -fingerprint -sha1 2>/dev/null | sed 's/.*=//; s/://g' | tr 'A-F' 'a-f'; }
    QUEUE_DERS=(); QUEUE_FPS=(); QUEUE_NAMES=()
    queue_add() { # <der> <why> <waiting|own>
        local der="$1" why="$2" kind="$3" fp c cn
        fp="$(cert_sha1 "$der")" || fp=""
        if [[ -z "$fp" ]]; then
            # Nothing has been changed yet. A waiting entry that cannot be
            # read would be lost by rebuilding the request: leave it alone.
            if [[ "$kind" == waiting ]]; then
                log "ERROR: The waiting enrollment request holds an entry that is not a readable certificate, so Green Light will not replace it. Reboot to finish it first, then set up signing again."
            else
                log "ERROR: Could not read the certificate $SIGN_CERT ($why). Nothing was queued."
            fi
            exit 1
        fi
        for c in "${QUEUE_FPS[@]}"; do [[ "$c" == "$fp" ]] && return 0; done
        cn="$(cert_subject_cn "$der")"
        # Enrolled ("already enrolled") or trusted another way ("already in
        # db", "already in the built-in trusted keyring"): nothing to queue.
        if mokutil --test-key "$der" 2>&1 | grep -qiE 'already enrolled|already in db|built-in'; then
            log "Already enrolled: ${cn:-$der}"
            return 0
        fi
        local copy="$PRIV_DIR/key-$(( ${#QUEUE_DERS[@]} + 1 )).der"
        cp "$der" "$copy"
        QUEUE_DERS+=("$copy"); QUEUE_FPS+=("$fp"); QUEUE_NAMES+=("${cn:-$der}")
        log "To enroll: ${cn:-$der} ($why)"
    }
    for f in "$PRIV_DIR"/pending/*.der; do
        [[ -e "$f" ]] || continue
        queue_add "$f" "already waiting for enrollment" waiting
    done
    queue_add "$CERT_DER" "this machine's module-signing key" own

    if [[ ${#QUEUE_DERS[@]} -eq 0 ]]; then
        if [[ -e "$MOK_NEW" ]]; then mokutil --revoke-import >>"$LOGFILE" 2>&1 || true; fi
        rm -f "$MOK_PENDING_LIST"
        log "The certificate is already enrolled; nothing to do."
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
    # Replace the waiting request (its certificates are in the list above).
    if [[ -e "$MOK_NEW" ]] && ! mokutil --revoke-import >>"$LOGFILE" 2>&1; then
        log "ERROR: Could not replace the waiting enrollment request (mokutil --revoke-import failed)."
        exit 1
    fi
    if ! mokutil --import "${QUEUE_DERS[@]}" --hash-file "$HASH_FILE" >>"$LOGFILE" 2>&1; then
        log "mokutil --import failed; trying once more"
        if ! mokutil --import "${QUEUE_DERS[@]}" --hash-file "$HASH_FILE" >>"$LOGFILE" 2>&1; then
            # The earlier request is gone. Drop whatever part of the new one
            # mokutil may have written, so that nothing at all waits and no
            # list claims otherwise.
            if [[ -e "$MOK_NEW" ]]; then mokutil --revoke-import >>"$LOGFILE" 2>&1 || true; fi
            rm -f "$MOK_PENDING_LIST"
            log "ERROR: mokutil --import failed twice (see $LOGFILE). None of these keys is waiting for enrollment now:"
            for i in "${!QUEUE_DERS[@]}"; do
                log "  ${QUEUE_NAMES[$i]} (SHA1 ${QUEUE_FPS[$i]})"
            done
            log "Set up signing again to queue them all."
            exit 1
        fi
    fi
    rm -f "$HASH_FILE"
    write_mok_pending_list "${QUEUE_FPS[@]}" || log "WARNING: could not record the queued keys for the apps"
    NAMES_JOINED="$(printf '%s, ' "${QUEUE_NAMES[@]}")"
    log "Enrollment queued for ${#QUEUE_DERS[@]} key(s): ${NAMES_JOINED%, }."
    log "On the next reboot, choose Enroll MOK at the blue MOK Manager screen and type the one-time password you chose here. That one password enrolls all of these keys; a password chosen earlier, here or in another app, no longer works."
    log "==== Module signing setup done ===="
    exit 0
fi

# ── Install and recover modes ──────────────────────────────────────────
#   privileged-install.sh <run-file> <sha256> [options]   install
#   privileged-install.sh --recover                       roll back an install
#     that was cut off (power loss, SIGKILL) from its journal; also what the
#     green-light-recover.service boot unit runs.
#   privileged-install.sh --discard-recovery              drop recovery data
#     that no longer matches the computer. As an install option it does the
#     same before installing.
MODE="install"
if [[ "${1:-}" == "--recover" ]]; then MODE="recover"; fi
if [[ "${1:-}" == "--discard-recovery" ]]; then MODE="discard"; fi
DISCARD_RECOVERY=0
for arg in "$@"; do
    if [[ "$arg" == "--discard-recovery" ]]; then DISCARD_RECOVERY=1; fi
done

# Out of the user's session: logging out (or the session being torn down)
# stops every process in the session's scope, which would cut the install
# off mid-way. A transient scope of its own avoids that; systemd-run execs
# the command in the same process, so the exit code still reaches pkexec.
# (Not when systemd started it as a service, the boot unit: that is already
# its own unit, and its process must stay the service's.)
# A trial scope first: when systemd-run can't make one, the run goes on in
# the session's scope rather than failing.
if [[ -z "${GREEN_LIGHT_SCOPED:-}" && -z "${INVOCATION_ID:-}" && -d /run/systemd/system ]] \
    && command -v systemd-run >/dev/null 2>&1; then
    export GREEN_LIGHT_SCOPED=1
    if systemd-run --scope --quiet --collect true >/dev/null 2>&1; then
        exec systemd-run --scope --quiet --collect --unit="green-light-install-$$" \
            --description="Green Light driver install" -- "$BASH" "$0" "$@"
    fi
    log "WARNING: systemd-run could not give the install a scope of its own; logging out during it would stop it"
fi

# Its own session: a hangup or Ctrl-C delivered to the app's process group
# (the terminal it was started from) must not reach the install. Under
# pkexec this keeps the PID (not a group leader, so setsid doesn't fork);
# --wait passes the exit code on when it does fork.
read -r PROC_STAT < /proc/$$/stat
read -r _ _ _ OWN_SID _ <<<"${PROC_STAT##*) }"
if [[ "$OWN_SID" != "$$" ]] && command -v setsid >/dev/null 2>&1; then
    exec setsid --wait "$BASH" "$0" "$@"
fi

usage_error() { log "ERROR: $*"; exit "$EXIT_USAGE"; }

# One install at a time. The app also checks this lock to tell that an
# install is still running (for example after it was closed and reopened).
# Keep the path in step with INSTALL_LOCK in install.rs.
INSTALL_LOCK="/run/green-light-install.lock"
exec 9>>"$INSTALL_LOCK"
if ! flock -n 9; then
    usage_error "Another Green Light install is still running; wait for it to finish."
fi
echo "$$" > "$INSTALL_LOCK"

# Logged once this is the only run, so every message of this run (argument
# and file errors included) comes after it in the log.
case "$MODE" in
    recover) log "==== Green Light recovery started ====" ;;
    discard) log "==== Green Light: discarding recovery data ====" ;;
    *)       log "==== NVIDIA driver install started ====" ;;
esac

# ── Shared helpers ─────────────────────────────────────────────────────

# Distro driver packages, removed before the install. Keep in step with
# DEB_DRIVER_GLOBS / RPM_DRIVER_GLOBS / is_driver_package in preflight.rs
# (a unit test there compares them). Docker's GPU plumbing
# (nvidia-container-toolkit, libnvidia-container*) matches the globs but is
# not the display driver; see Step 3.
DEB_DRIVER_GLOBS=('nvidia-*' 'libnvidia-*' 'xserver-xorg-video-nvidia*' 'system76-driver-nvidia*')
RPM_DRIVER_GLOBS=('akmod-nvidia*' 'xorg-x11-drv-nvidia*' 'kmod-nvidia*' 'nvidia-driver*' 'nvidia-settings*')
NOT_DRIVER_RE='^(nvidia-container|libnvidia-container)'
# Kernel module packages akmods builds on this machine (kmod-nvidia-<kernel>);
# no repository has them.
LOCAL_KMOD_RE='^kmod-nvidia-[0-9]'

is_driver_pkg() { # <name[:arch]> <glob>...
    local name="${1%%:*}" glob
    shift
    if [[ "$name" =~ $NOT_DRIVER_RE ]]; then return 1; fi
    for glob in "$@"; do
        # shellcheck disable=SC2053  # the right-hand side is a glob on purpose
        if [[ "$name" == $glob ]]; then return 0; fi
    done
    return 1
}

# Where --block-distro-driver keeps the distro driver out (keep in step with
# APT_BLOCK_FILE / DNF_BLOCK_FILE in preflight.rs, which shows the state).
APT_BLOCK_FILE="/etc/apt/preferences.d/green-light-nvidia"
DNF_BLOCK_DIR="/etc/dnf/libdnf5.conf.d"
DNF_BLOCK_FILE="$DNF_BLOCK_DIR/90-green-light-nvidia.conf"

distro_driver_blocked() { [[ -f "$APT_BLOCK_FILE" || -f "$DNF_BLOCK_FILE" ]]; }

# apt: a Pin-Priority of -1 means "never install". The driver comes from the
# nvidia-graphics-drivers* source packages (Ubuntu's numbered and -server
# branches, Debian's), so pinning those sources catches every binary package
# of every branch, present or future, and nothing else (the container
# toolkit is a different source). Ubuntu's prebuilt module packages and
# Pop!_OS's metapackage are named directly.
# dnf5: an excludepkgs drop-in, added to dnf.conf's own excludes. dnf4 has no
# drop-in for it, so there the block isn't available.
block_distro_driver() {
    local tmp
    tmp="$(mktemp "$PRIV_DIR/block.XXXXXX")" || return 1
    if [[ "$PKG_MGR" == "apt" ]]; then
        cat > "$tmp" <<'PIN' || return 1
Explanation: Written by Green Light. The NVIDIA driver on this computer comes from
Explanation: NVIDIA's .run installer; never install the distribution's driver over it.
Explanation: Turn off "Block Distro Driver Packages" in Green Light and reinstall to remove this.
Package: src:nvidia-graphics-drivers* src:nvidia-settings linux-modules-nvidia-* linux-objects-nvidia-* linux-signatures-nvidia-* system76-driver-nvidia*
Pin: version *
Pin-Priority: -1
PIN
        install -m 0644 "$tmp" "$APT_BLOCK_FILE" || return 1
    elif command -v dnf5 >/dev/null 2>&1; then
        {
            echo "# Written by Green Light. The NVIDIA driver on this computer comes from"
            echo "# NVIDIA's .run installer; never install the distribution's driver over it."
            echo "# Turn off \"Block Distro Driver Packages\" in Green Light and reinstall to remove this."
            echo "[main]"
            echo "excludepkgs=$(IFS=,; echo "${RPM_DRIVER_GLOBS[*]}")"
        } > "$tmp" || return 1
        install -D -m 0644 "$tmp" "$DNF_BLOCK_FILE" || return 1
    else
        log "WARNING: Blocking the distro driver packages needs apt or dnf5; this dnf has no way to do it"
        return 1
    fi
    return 0
}

unblock_distro_driver() { rm -f "$APT_BLOCK_FILE" "$DNF_BLOCK_FILE"; }

# Log the last lines of a build log.
log_tail() { # <file> <label>
    log "Build log: last lines of $2"
    tail -n 15 "$1" 2>/dev/null | while IFS= read -r line; do
        log "Build log: $line"
    done
    return 0
}

# Log the tail of the newest DKMS build log for the nvidia module, so a failed
# kernel module build is visible in the log and the app without digging.
log_dkms_build_tail() {
    local newest="" f
    for f in /var/lib/dkms/nvidia/*/build/make.log; do
        [[ -f "$f" ]] || continue
        if [[ -z "$newest" || "$f" -nt "$newest" ]]; then newest="$f"; fi
    done
    if [[ -z "$newest" ]]; then return 0; fi
    log_tail "$newest" "$newest"
}

# Version of a driver installed from NVIDIA's .run (its installer stays
# behind as nvidia-uninstall), or nothing.
installed_run_version() {
    command -v nvidia-uninstall >/dev/null 2>&1 || return 0
    local out
    out="$(nolock nvidia-installer --version 2>/dev/null || true)"
    if [[ "$out" =~ version[[:space:]]+([0-9]+\.[0-9.]+) ]]; then printf '%s' "${BASH_REMATCH[1]}"; fi
    return 0
}

module_version() { modinfo -k "${1:-$KVER}" -F version nvidia 2>/dev/null || true; }
module_signer() { modinfo -k "${1:-$KVER}" -F signer nvidia 2>/dev/null || true; }
secure_boot_on() {
    command -v mokutil >/dev/null 2>&1 && mokutil --sb-state 2>/dev/null | grep -qi 'SecureBoot enabled'
}

set_kernel() { # <kernel release>
    KVER="$1"
    if [[ "$PKG_MGR" == "apt" ]]; then
        INITRAMFS_CMD=(update-initramfs -u -k "$KVER")
    else
        INITRAMFS_CMD=(dracut --force --kver "$KVER")
    fi
}

# Options that let dnf install what it can when some of the packages asked
# for exist nowhere any more; the result is checked afterwards anyway.
dnf_tolerant() {
    if command -v dnf5 >/dev/null 2>&1; then echo "--skip-unavailable"; else echo "--setopt=strict=0"; fi
}

# ── The journal ────────────────────────────────────────────────────────
# Everything needed to put the previous driver setup back, written before
# the first change and kept until NVIDIA's installer has succeeded (or a
# rollback has been verified), so a run cut off by a power loss or SIGKILL
# can be rolled back later: by the next install, by "--recover" (the app's
# Restore Previous Driver), or at boot by green-light-recover.service. It
# also holds copies of the packages the install removes, so that works
# offline. Every file is written to a temporary name, synced and renamed.
# The directory is root-only; its parent is readable so the app can see
# whether a journal exists (keep in step with JOURNAL_DIR in install.rs).
STATE_DIR="/var/lib/green-light"
JOURNAL="$STATE_DIR/journal"

durable() { sync -- "$@" 2>/dev/null || sync; }

journal_set_phase() { # armed | installer-started | installer-done
    local tmp
    tmp="$(mktemp "$JOURNAL/.phase.XXXXXX")" || return 1
    printf '%s\n' "$1" > "$tmp" || return 1
    durable "$tmp"
    mv -f "$tmp" "$JOURNAL/phase" || return 1
    durable "$JOURNAL"
}

journal_clear() {
    if [[ -d "$JOURNAL" ]]; then
        rm -rf "$JOURNAL"
        durable "$STATE_DIR"
    fi
    rm -f "$STATE_DIR/recovery-status"
    return 0
}

# Every installed package as "<key>|<exact spec>": apt
# "name[:arch]|name[:arch]=version", rpm "name.arch|name-version-release.arch".
package_snapshot() {
    if [[ "$PKG_MGR" == "apt" ]]; then
        { nolock dpkg-query -W -f '${db:Status-Abbrev}|${binary:Package}|${Version}\n' 2>/dev/null || true; } \
            | while IFS='|' read -r st name ver; do
                if [[ "${st:1:1}" == "i" ]]; then printf '%s|%s=%s\n' "$name" "$name" "$ver"; fi
            done
    else
        nolock rpm -qa --qf '%{NAME}.%{ARCH}|%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}\n' 2>/dev/null || true
    fi
    return 0
}

# What this run's removal takes away, as keys: apt's simulated purge
# ("Purg"/"Remv" lines), or dnf's transaction listing (every "Removing ..."
# section). Only these are ever reinstalled: something else removed in the
# meantime is not this run's to put back.
planned_removals() { # <package>...
    if [[ "$PKG_MGR" == "apt" ]]; then
        { nolock apt-get -s purge "$@" 2>/dev/null || true; } | while read -r op name _; do
            if [[ "$op" == Purg || "$op" == Remv ]]; then printf '%s\n' "$name"; fi
        done
    else
        { nolock dnf remove --assumeno "$@" 2>/dev/null || true; } | {
            local section=0 line name arch
            while IFS= read -r line; do
                if [[ "$line" == Removing* ]]; then section=1; continue; fi
                if [[ $section -eq 1 && "$line" =~ ^[[:space:]]+([^[:space:]]+)[[:space:]]+([^[:space:]]+)[[:space:]] ]]; then
                    name="${BASH_REMATCH[1]}"; arch="${BASH_REMATCH[2]}"
                    printf '%s.%s\n' "$name" "$arch"
                elif [[ $section -eq 1 && ! "$line" =~ ^[[:space:]] ]]; then
                    section=0
                fi
            done
        }
    fi
    return 0
}

# Packages of the plan that are no longer installed, as "<key>|<spec>".
packages_removed() {
    local -A now=() plan=()
    local key spec
    while IFS='|' read -r key spec; do now["$key"]=1; done < <(package_snapshot)
    while IFS= read -r key; do [[ -n "$key" ]] && plan["$key"]=1; done < "$JOURNAL/planned"
    while IFS='|' read -r key spec; do
        if [[ -n "$key" && -n "${plan[$key]:-}" && -z "${now[$key]:-}" ]]; then
            printf '%s|%s\n' "$key" "$spec"
        fi
    done < "$JOURNAL/packages-before"
    return 0
}

# Copy the packages the removal will take into the journal, so they can be
# reinstalled without the network: from apt's or akmods' cache when there,
# else downloaded at the exact version. "cache-index" maps keys to files.
cache_planned_packages() {
    local key spec name ver file got index="$JOURNAL/.cache-index.new"
    mkdir -p "$JOURNAL/cache"
    cp -- "$JOURNAL/cache-index" "$index" 2>/dev/null || : > "$index"
    while IFS='|' read -r key spec; do
        grep -qxF -- "$key" "$JOURNAL/planned" || continue
        # Already kept (carried over from an earlier install's journal).
        grep -q "^${key//./\\.}|" "$index" && continue
        got=""
        if [[ "$PKG_MGR" == "apt" ]]; then
            name="${key%%:*}"; ver="${spec#*=}"
            for file in /var/cache/apt/archives/"${name}_${ver//:/%3a}"_*.deb; do
                [[ -f "$file" ]] && { cp -- "$file" "$JOURNAL/cache/" && got="$JOURNAL/cache/${file##*/}"; break; }
            done
            if [[ -z "$got" ]] && (cd "$JOURNAL/cache" && nolock apt-get download "$spec" >>"$LOGFILE" 2>&1); then
                for file in "$JOURNAL/cache/${name}_${ver//:/%3a}"_*.deb; do [[ -f "$file" ]] && got="$file"; done
            fi
        else
            if [[ "${key%.*}" =~ $LOCAL_KMOD_RE ]]; then
                for file in /var/cache/akmods/*/"$spec.rpm"; do
                    [[ -f "$file" ]] && { cp -- "$file" "$JOURNAL/cache/" && got="$JOURNAL/cache/${file##*/}"; break; }
                done
            elif nolock dnf download --destdir "$JOURNAL/cache" "$spec" >>"$LOGFILE" 2>&1 \
                && [[ -f "$JOURNAL/cache/$spec.rpm" ]]; then
                got="$JOURNAL/cache/$spec.rpm"
            fi
        fi
        if [[ -n "$got" ]]; then
            printf '%s|%s\n' "$key" "$got" >> "$index"
        elif [[ "$PKG_MGR" == "dnf" && "${key%.*}" =~ $LOCAL_KMOD_RE ]]; then
            log "  $key was built here by akmods; it is rebuilt if needed"
        else
            log "WARNING: Could not keep a copy of $spec; putting it back would need the package source"
        fi
    done < "$JOURNAL/packages-before"
    durable "$JOURNAL/cache"/* "$JOURNAL/cache" "$index"
    mv -f "$index" "$JOURNAL/cache-index"
    durable "$JOURNAL"
}

backup_file() { # <path>
    local saved=""
    if [[ -e "$1" ]]; then
        saved="${#BACKUPS[@]}"
        cp -a -- "$1" "$JOURNAL.new/backup/$saved"
    fi
    BACKUPS+=("$1|$saved")
}

restore_backups() {
    local entry path saved rc=0
    for entry in "${BACKUPS[@]}"; do
        path="${entry%%|*}"
        saved="${entry#*|}"
        if [[ -n "$saved" ]]; then
            cp -a -- "$JOURNAL/backup/$saved" "$path" || { log "ERROR: Could not restore $path"; rc=1; }
        else
            rm -f -- "$path" || { log "ERROR: Could not remove $path"; rc=1; }
        fi
    done
    return "$rc"
}

# Whether the module signer is one Secure Boot will accept: the one the
# module had before, or the machine's own enrolled signing key.
signer_trusted() { # <signer>
    [[ -n "$1" ]] || return 1
    [[ "$1" == "$MOD_SIGNER_BEFORE" ]] && return 0
    resolve_signing_key
    [[ -n "$SIGN_KEY" && $SIGN_MISSING -eq 0 ]] || return 1
    [[ "$(cert_subject_cn "$SIGN_CERT")" == "$1" ]] || return 1
    local der
    der="$(cert_as_der "$SIGN_CERT" "$PRIV_DIR")" || return 1
    mokutil --test-key "$der" 2>&1 | grep -qiE 'already (enrolled|in )'
}

# Kernels whose module matters after a rollback: the one the journal was
# written for, those of the earlier installs a carried journal was built on
# (their baseline is the setup being restored), and the one running now if
# that is another (a recovery at boot after a newer kernel was installed in
# the meantime). Each once, and only while its module tree exists.
restore_kernels() {
    local boot k
    boot="$(uname -r)"
    printf '%s\n' "$KVER"
    for k in "${EARLIER_KERNELS[@]}" "$boot"; do
        if [[ "$k" != "$KVER" && -d "/lib/modules/$k" ]]; then printf '%s\n' "$k"; fi
    done | sort -u
}

initramfs_for() { # <kernel>
    if [[ "$PKG_MGR" == "apt" ]]; then
        nolock update-initramfs -u -k "$1"
    else
        nolock dracut --force --kver "$1"
    fi
}

# Make sure the restored driver has its module for <kernel> when the
# packages alone don't bring it: akmods for akmod-nvidia, DKMS for DKMS
# packages, Ubuntu's prebuilt package for that kernel when the setup used
# prebuilt ones. Sets nothing; verify_restored checks the result.
ensure_module() { # <kernel> <restored keys...>
    local k="$1" key branch=""
    shift
    [[ -n "$(module_version "$k")" ]] && return 0
    for key in "$@"; do
        if [[ "$key" =~ ^linux-modules-nvidia-([0-9]+(-server)?(-open)?)- ]]; then branch="${BASH_REMATCH[1]}"; fi
    done
    if [[ "$PKG_MGR" == "dnf" ]] && command -v akmods >/dev/null 2>&1 \
        && printf '%s\n' "$@" | grep -q '^akmod-nvidia\.'; then
        log "Building the restored driver's kernel module for $k with akmods…"
        nolock akmods --force --kernels "$k" >>"$LOGFILE" 2>&1 || log "WARNING: akmods could not build the kernel module for $k"
    elif [[ -n "$branch" ]]; then
        log "Installing Ubuntu's prebuilt NVIDIA module package for $k…"
        nolock apt-get install -y "linux-modules-nvidia-$branch-$k" >>"$LOGFILE" 2>&1 \
            || log "WARNING: Could not install linux-modules-nvidia-$branch-$k"
    elif command -v dkms >/dev/null 2>&1; then
        log "Building the restored driver's kernel module for $k with DKMS…"
        nolock dkms autoinstall -k "$k" >>"$LOGFILE" 2>&1 || log "WARNING: DKMS could not build the kernel module for $k"
    fi
    return 0
}

# After a rollback: is the driver the machine had before back? Only then may
# the run claim to have restored it.
verify_restored() {
    local ok=1 run_now mod_now signer_now entry k
    run_now="$(installed_run_version)"
    if [[ -n "$RUN_VER_BEFORE" && "$run_now" != "$RUN_VER_BEFORE" ]]; then
        log "ERROR: The driver $RUN_VER_BEFORE installed earlier from NVIDIA's .run file was removed by NVIDIA's installer and can't be put back from here."
        log "       Install a driver (that version or another) with Green Light before rebooting."
        ok=0
    fi
    while IFS='|' read -r entry _; do
        [[ -n "$entry" ]] || continue
        # akmods' local builds are checked through the module itself.
        if [[ "$PKG_MGR" == "dnf" && "${entry%.*}" =~ $LOCAL_KMOD_RE ]]; then continue; fi
        log "ERROR: The removed package $entry is not installed again."
        ok=0
    done < <(packages_removed)
    while IFS= read -r k; do
        mod_now="$(module_version "$k")"
        if [[ -n "$MOD_VER_BEFORE" ]]; then
            if [[ -z "$mod_now" ]]; then
                # Another kernel that the previous driver had no module for
                # either (a .run driver without DKMS never builds one; a
                # prebuilt package for it may not be reachable): not this
                # restore's failure, but worth knowing.
                if [[ "$k" != "$KVER" ]] && ! printf '%s\n' "${MOD_KERNELS_BEFORE[@]}" | grep -qxF -- "$k"; then
                    log "WARNING: There is no nvidia kernel module for $k, which the previous driver didn't have one for either; start $KVER, or install a driver for $k."
                    continue
                fi
                log "ERROR: There is no nvidia kernel module for $k (there was $MOD_VER_BEFORE for $KVER)."
                ok=0
                continue
            fi
            if [[ "$mod_now" != "$MOD_VER_BEFORE" ]]; then
                log "The nvidia kernel module for $k is now $mod_now (it was $MOD_VER_BEFORE)."
            fi
            signer_now="$(module_signer "$k")"
            # Unchanged (unsigned before and after included) is restored as
            # it was; a changed signer must be one Secure Boot trusts.
            if secure_boot_on && [[ "$signer_now" != "$MOD_SIGNER_BEFORE" ]] && ! signer_trusted "$signer_now"; then
                log "ERROR: Secure Boot is on, and the nvidia module for $k is ${signer_now:+signed by \"$signer_now\", }not signed by a key it trusts (it was ${MOD_SIGNER_BEFORE:-unsigned})."
                ok=0
            fi
        elif [[ -n "$mod_now" ]]; then
            log "ERROR: An nvidia kernel module ($mod_now) for $k is left behind from the failed install."
            ok=0
        fi
    done < <(restore_kernels)
    return $(( ok ? 0 : 1 ))
}

rollback() {
    local ok=1 key spec file run_now k kmod_cached=0 local_failed=0
    local -A cached=()
    local files=() specs=() names=() auto=() keys=()
    log "Restoring the previous driver setup…"
    # A partial install of the new driver: let its own uninstaller take it
    # out, so the restored packages don't sit on top of half of it.
    if [[ $INSTALLER_STARTED -eq 1 || ${EARLIER_INSTALLER_STARTED:-0} -eq 1 ]]; then
        run_now="$(installed_run_version)"
        if [[ -n "$run_now" && "$run_now" != "$RUN_VER_BEFORE" ]]; then
            log "Removing what NVIDIA's installer had already put in place ($run_now)…"
            nolock nvidia-uninstall --silent >>"$LOGFILE" 2>&1 \
                || { log "ERROR: nvidia-uninstall could not remove the partial install"; ok=0; }
        fi
    fi
    restore_backups || ok=0
    while IFS='|' read -r key file; do [[ -n "$key" ]] && cached["$key"]="$file"; done < "$JOURNAL/cache-index"
    while IFS='|' read -r key spec; do
        [[ -n "$key" ]] || continue
        keys+=("$key")
        if [[ -n "${cached[$key]:-}" ]]; then
            files+=("${cached[$key]}")
            if [[ "$PKG_MGR" == "dnf" && "${key%.*}" == "kmod-nvidia-$KVER" ]]; then kmod_cached=1; fi
        elif [[ "$PKG_MGR" == "dnf" && "${key%.*}" =~ $LOCAL_KMOD_RE ]]; then
            continue   # exists in no repository; akmods rebuilds it below
        else
            specs+=("$spec")
        fi
        if [[ "$PKG_MGR" == "apt" ]]; then names+=("$key"); else names+=("${key%.*}"); fi
        if grep -qxF -- "$key" "$JOURNAL/auto" 2>/dev/null; then auto+=("$key"); fi
    done < <(packages_removed)
    if [[ ${#names[@]} -gt 0 ]]; then
        log "Reinstalling the packages that were removed: ${names[*]}"
        [[ ${#files[@]} -gt 0 ]] && log "  from the copies kept before the install: ${#files[@]} of them"
        if [[ "$PKG_MGR" == "apt" ]]; then
            # Local .deb files need no network; apt only reads its lists.
            if ! nolock apt-get install -y --allow-downgrades "${files[@]}" "${specs[@]}" >>"$LOGFILE" 2>&1; then
                record_reinstalled
                log "WARNING: The removed versions can't all be reinstalled; installing the distro's current ones"
                nolock apt-get install -y "${names[@]}" >>"$LOGFILE" 2>&1 \
                    || { log "ERROR: apt-get could not reinstall the removed packages"; ok=0; }
            fi
            record_reinstalled
            if [[ ${#auto[@]} -gt 0 ]]; then
                nolock apt-mark auto "${auto[@]}" >>"$LOGFILE" 2>&1 || log "WARNING: Could not mark ${auto[*]} as automatically installed"
            fi
        else
            local tolerant
            tolerant="$(dnf_tolerant)"
            # The kept copies alone, with every repository off: that works
            # offline, where loading repository metadata would fail. Only
            # what has no copy needs a repository.
            if [[ ${#files[@]} -gt 0 ]] \
                && ! nolock dnf install -y --disablerepo='*' "${files[@]}" >>"$LOGFILE" 2>&1; then
                local_failed=1
            fi
            record_reinstalled
            if [[ ${#specs[@]} -gt 0 || $local_failed -eq 1 ]]; then
                local again=("${specs[@]}")
                if [[ $local_failed -eq 1 ]]; then again+=("${files[@]}"); fi
                if ! nolock dnf install -y "$tolerant" "${again[@]}" >>"$LOGFILE" 2>&1; then
                    record_reinstalled
                    log "WARNING: The removed versions can't all be reinstalled; installing the distro's current ones"
                    nolock dnf install -y "$tolerant" "${names[@]}" >>"$LOGFILE" 2>&1 \
                        || { log "ERROR: dnf could not reinstall the removed packages"; ok=0; }
                fi
                record_reinstalled
            fi
            if [[ ${#auto[@]} -gt 0 ]]; then
                local mark=(mark remove)
                if command -v dnf5 >/dev/null 2>&1; then mark=(mark dependency); fi
                nolock dnf "${mark[@]}" "${auto[@]%.*}" >>"$LOGFILE" 2>&1 || log "WARNING: Could not mark ${auto[*]} as installed as dependencies"
            fi
        fi
        # akmod-nvidia builds its module in the background from its rpm
        # scriptlet, DKMS packages for the running kernel only: build what
        # is missing now, for every kernel that matters, so it can be checked.
        while IFS= read -r k; do
            if [[ "$k" == "$KVER" && $kmod_cached -eq 1 ]]; then continue; fi
            ensure_module "$k" "${keys[@]}"
            record_reinstalled
        done < <(restore_kernels)
    fi
    if [[ ${#HELD[@]} -gt 0 ]]; then
        nolock apt-mark hold "${HELD[@]}" >>"$LOGFILE" 2>&1 \
            || { log "ERROR: Could not restore the apt holds on ${HELD[*]}"; ok=0; }
    fi
    if [[ $WAS_BLOCKED -eq 1 ]]; then
        block_distro_driver || { log "ERROR: Could not restore the block on the distro driver packages"; ok=0; }
    fi
    while IFS= read -r k; do
        if ! initramfs_for "$k" >>"$LOGFILE" 2>&1; then
            log "ERROR: Rebuilding the initramfs for $k failed."
            ok=0
        fi
    done < <(restore_kernels)
    verify_restored || ok=0
    record_reinstalled
    if [[ $ok -eq 1 ]]; then
        log "Restored and checked the previous driver setup; the next boot uses the driver this computer had before."
        return 0
    fi
    log "ERROR: The previous driver setup could not be fully restored (see above and $LOGFILE)."
    log "       Don't reboot until a working driver is installed, or the next boot may have no NVIDIA driver."
    return 1
}

# Note what a rollback step installed, at once and durably: an interrupted
# recovery must still recognise its own work as its own (see
# journal_stale_reason).
record_reinstalled() {
    [[ -d "$JOURNAL" ]] || return 0
    { cat "$JOURNAL/reinstalled" 2>/dev/null; driver_package_specs; } | sort -u > "$JOURNAL/.reinstalled.new" \
        && durable "$JOURNAL/.reinstalled.new" && mv -f "$JOURNAL/.reinstalled.new" "$JOURNAL/reinstalled" \
        && durable "$JOURNAL"
    return 0
}

# Exact specs of the driver packages installed now.
driver_package_specs() {
    local key spec
    while IFS='|' read -r key spec; do
        if [[ "$PKG_MGR" == "apt" ]]; then
            is_driver_pkg "$key" "${DEB_DRIVER_GLOBS[@]}" && printf '%s\n' "$spec"
        else
            is_driver_pkg "${key%.*}" "${RPM_DRIVER_GLOBS[@]}" && printf '%s\n' "$spec"
        fi
    done < <(package_snapshot)
    return 0
}

# Why a journal no longer describes this computer, or nothing when it does.
# It is stale when the NVIDIA driver was changed by something else after the
# run that wrote it: another driver from NVIDIA's installer than the one
# from before or the one that run was putting in, or distribution driver
# packages that were neither there before nor put back by a rollback.
journal_stale_reason() {
    local run_now spec
    local -A known=()
    run_now="$(installed_run_version)"
    local t known_target=0
    for t in "${TARGET_RUN_VER:-}" "${KNOWN_TARGETS[@]}"; do
        if [[ -n "$t" && "$run_now" == "$t" ]]; then known_target=1; fi
    done
    if [[ -n "$run_now" && "$run_now" != "$RUN_VER_BEFORE" && $known_target -eq 0 ]]; then
        printf '%s' "the driver installed now from NVIDIA's installer ($run_now) is neither the one from before (${RUN_VER_BEFORE:-none}) nor the one that install was putting in (${TARGET_RUN_VER:-unknown})"
        return 0
    fi
    while IFS='|' read -r _ spec; do known["$spec"]=1; done < "$JOURNAL/packages-before"
    while IFS= read -r spec; do [[ -n "$spec" ]] && known["$spec"]=1; done < <(cat "$JOURNAL/reinstalled" 2>/dev/null)
    while IFS= read -r spec; do
        if [[ -z "${known[$spec]:-}" ]]; then
            printf '%s' "the distribution driver package $spec was installed after it"
            return 0
        fi
    done < <(driver_package_specs)
    return 0
}

# A note for the app (the journal itself is root-only): why recovery data is
# waiting, or nothing. Keep in step with RECOVERY_STATUS in install.rs.
RECOVERY_STATUS="$STATE_DIR/recovery-status"
recovery_note() { # <text>, or nothing to remove the note
    if [[ -z "${1:-}" ]]; then rm -f "$RECOVERY_STATUS"; return 0; fi
    mkdir -p "$STATE_DIR"
    printf '%s\n' "$1" > "$RECOVERY_STATUS.new" && chmod 644 "$RECOVERY_STATUS.new" \
        && mv -f "$RECOVERY_STATUS.new" "$RECOVERY_STATUS"
}

# Hold off shutdown and suspend while the system is being changed. The held
# command follows this process (tail --pid), so the inhibitor also ends if
# the run is killed.
start_inhibitor() {
    [[ -z "$INHIBIT_PID" && -d /run/systemd/system ]] || return 0
    command -v systemd-inhibit >/dev/null 2>&1 || return 0
    ( trap - TERM INT; exec systemd-inhibit --what=shutdown:sleep --mode=block --who="Green Light" \
        --why="Changing the NVIDIA driver; stopping now could leave the computer without one" \
        tail --pid=$$ -f /dev/null ) 9>&- &
    INHIBIT_PID=$!
}

# ── Recovery from a journal ────────────────────────────────────────────
# A journal left behind means an install was cut off, or its rollback
# failed. Before NVIDIA's installer had succeeded, its changes are rolled
# back; after, it only has to be cleared. Runs as "--recover", from the
# boot unit, and at the start of every install. A journal that no longer
# describes this computer (the driver was changed by other means since) is
# left alone until "--discard-recovery" drops it.
# (Top level, not a function: sourcing the state must set globals.)
RECOVER_RC=0
CARRIED=0
EARLIER_KERNELS=()
# shellcheck source=/dev/null  # the state is written by this script, root-only
if [[ -d "$JOURNAL" ]]; then
    PHASE="$(cat "$JOURNAL/phase" 2>/dev/null || echo armed)"
    TARGET_RUN_VER=""
    KNOWN_TARGETS=()
    EARLIER_INSTALLER_STARTED=0
    EARLIER_KERNELS=()
    MOD_KERNELS_BEFORE=()
    if [[ "$PHASE" == "installer-done" ]]; then
        log "An earlier install had finished NVIDIA's installer before it was cut off; nothing to put back."
        journal_clear
        recovery_note ""
    elif [[ $DISCARD_RECOVERY -eq 1 ]]; then
        log "Discarding the recovery data of an earlier interrupted install, as asked."
        journal_clear
        recovery_note ""
    elif ! source "$JOURNAL/state" 2>/dev/null; then
        log "ERROR: The recovery data of an earlier interrupted install can't be read."
        recovery_note "refused: its recovery data can't be read"
        RECOVER_RC=$EXIT_RECOVERY_REFUSED
    else
        set_kernel "$KVER"
        STALE="$(journal_stale_reason)"
        if [[ -n "$STALE" ]]; then
            log "ERROR: Not restoring from the recovery data of an earlier interrupted install (boot ${BOOT_ID:-unknown}): $STALE."
            log "       The driver was changed by other means since; discard that recovery data if the driver now is the one you want."
            recovery_note "refused: $STALE"
            RECOVER_RC=$EXIT_RECOVERY_REFUSED
        else
            log "An earlier install was cut off ($PHASE); putting the driver setup it changed back first…"
            if [[ -n "${INVOCATION_ID:-}" ]] && command -v plymouth >/dev/null 2>&1 && plymouth --ping 2>/dev/null; then
                PLYMOUTH=1
            fi
            INSTALLER_STARTED=0
            if [[ "$PHASE" == "installer-started" ]]; then INSTALLER_STARTED=1; fi
            PRIV_DIR="$(mktemp -d /var/tmp/green-light-recover.XXXXXX)"
            chmod 700 "$PRIV_DIR"
            start_inhibitor
            if [[ "$PKG_MGR" == "apt" ]]; then
                nolock dpkg --configure -a >>"$LOGFILE" 2>&1 || log "WARNING: dpkg --configure -a reported a problem"
            fi
            if rollback; then
                journal_clear
                recovery_note ""
                RECOVER_RC=$EXIT_ROLLED_BACK
            else
                recovery_note "failed: the previous driver could not be fully restored; see $LOGFILE"
                RECOVER_RC=$EXIT_ROLLBACK_FAILED
            fi
            rm -rf "$PRIV_DIR"
            PRIV_DIR=""
            PLYMOUTH=0
        fi
    fi
fi
if [[ "$MODE" != "install" ]]; then
    if [[ "$MODE" == "discard" ]]; then
        log "Recovery data discarded."
    elif [[ ! -d "$JOURNAL" && $RECOVER_RC -eq 0 ]]; then
        log "Nothing to recover."
    fi
    log "==== Recovery done ===="
    exit "$RECOVER_RC"
fi
if [[ $RECOVER_RC -eq $EXIT_RECOVERY_REFUSED ]]; then
    log "ERROR: Recovery data from an earlier interrupted install is waiting and doesn't match this computer any more; discard it to install. No changes made."
    exit "$EXIT_RECOVERY_REFUSED"
fi
if [[ $RECOVER_RC -eq $EXIT_ROLLBACK_FAILED ]]; then
    # Its packages-before, plan and kept copies still describe the good
    # setup from before that install: they become this install's baseline,
    # so a rollback of this install puts that setup back.
    # The journal stays where it is (the boot unit and the app still see it)
    # until this install replaces it with one built on it.
    log "WARNING: The earlier setup could not be fully restored; this install keeps its recovery data as the starting point to restore."
    CARRIED=1
    # That install's driver, and whether its installer had run, stay known.
    if [[ -n "${TARGET_RUN_VER:-}" ]]; then KNOWN_TARGETS+=("$TARGET_RUN_VER"); fi
    if [[ "$PHASE" == "installer-started" ]]; then EARLIER_INSTALLER_STARTED=1; fi
    # So does the kernel it was written for: this install's journal is
    # written for the running one, and a rollback rebuilds and checks both.
    if [[ -n "${KVER:-}" ]] && ! printf '%s\n' "${EARLIER_KERNELS[@]}" | grep -qxF -- "$KVER"; then
        EARLIER_KERNELS+=("$KVER")
    fi
fi

# ── Install mode ───────────────────────────────────────────────────────
ORIG_RUN_FILE="${1:-}"
EXPECT_SHA256="${2:-}"
USE_DKMS=0
BLOCK_DISTRO=0
MODULE_TYPE=""

if [[ -z "$ORIG_RUN_FILE" ]]; then usage_error "No .run file specified"; fi
if [[ ! "$ORIG_RUN_FILE" =~ ^/.*\.run$ ]]; then usage_error "Invalid run file path (must be absolute and end in .run): $ORIG_RUN_FILE"; fi
if [[ -L "$ORIG_RUN_FILE" ]]; then usage_error "Refusing symlink: $ORIG_RUN_FILE"; fi
if [[ ! -f "$ORIG_RUN_FILE" ]]; then usage_error "File not found: $ORIG_RUN_FILE"; fi
if [[ ! "$EXPECT_SHA256" =~ ^[0-9a-fA-F]{64}$ ]]; then
    usage_error "A 64-character SHA256 of the run file is required as the second argument"
fi
EXPECT_SHA256="${EXPECT_SHA256,,}"

shift 2
for arg in "$@"; do
    case "$arg" in
        --dkms)                USE_DKMS=1 ;;
        --block-distro-driver) BLOCK_DISTRO=1 ;;
        --discard-recovery)    ;;  # handled before the recovery above
        --kernel-module-type=open|--kernel-module-type=proprietary)
            MODULE_TYPE="${arg#--kernel-module-type=}" ;;
        *) usage_error "Unknown argument: $arg" ;;
    esac
done

# Whether <gid> is the private group of the account <uid>: named after it,
# its primary group, and with no other member. Supplementary members come
# from the group entry itself; accounts with it as their primary group are
# looked for in the account list NSS can enumerate (all local ones).
group_is_owners_private_group() { # <gid> <uid>
    local entry owner owner_gid gname members name pgid
    entry="$(getent passwd "$2")" || return 1
    IFS=: read -r owner _ _ owner_gid _ <<<"$entry"
    [[ "$owner_gid" == "$1" ]] || return 1
    entry="$(getent group "$1")" || return 1
    IFS=: read -r gname _ _ members <<<"$entry"
    [[ "$gname" == "$owner" ]] || return 1
    [[ -z "$members" || "$members" == "$owner" ]] || return 1
    while IFS=: read -r name _ _ pgid _; do
        if [[ "$pgid" == "$1" && "$name" != "$owner" ]]; then return 1; fi
    done < <(getent passwd || true)
    return 0
}

# Who besides the owner an ACL lets write the file, as "user:<uid>" /
# "group:<gid>" lines; empty when nobody, "?" when the file has an ACL that
# can't be read (no getfacl).
acl_writers() { # <file> <owner-uid>
    local line kind who perms mask="rwx"
    # ls shows "+" after the mode for a file with an ACL.
    local mode_str
    mode_str="$(ls -ldn -- "$1" | cut -d' ' -f1)"
    [[ "$mode_str" == *+ ]] || return 0
    if ! command -v getfacl >/dev/null 2>&1; then echo "?"; return 0; fi
    while IFS= read -r line; do
        if [[ "$line" == mask::* ]]; then mask="${line#mask::}"; fi
    done < <(getfacl -cpn -- "$1" 2>/dev/null)
    while IFS=: read -r kind who perms; do
        [[ "$kind" == user && -n "$who" && "$who" != "$2" ]] || [[ "$kind" == group && -n "$who" ]] || continue
        if [[ "${perms:1:1}" == w && "${mask:1:1}" == w ]]; then echo "$kind:$who"; fi
    done < <(getfacl -cpn -- "$1" 2>/dev/null)
    return 0
}

# Ownership / permission check on the original file: only its owner (root
# or the invoking user) may have been able to write it.
FILE_UID="$(stat -c '%u' -- "$ORIG_RUN_FILE")"
FILE_GID="$(stat -c '%g' -- "$ORIG_RUN_FILE")"
FILE_MODE="$(stat -c '%a' -- "$ORIG_RUN_FILE")"
CALLER_UID="${PKEXEC_UID:-${SUDO_UID:-0}}"
if [[ "$FILE_UID" != "0" && "$FILE_UID" != "$CALLER_UID" ]]; then
    usage_error "$ORIG_RUN_FILE is owned by uid $FILE_UID, not root or the invoking user ($CALLER_UID)"
fi
if (( (8#$FILE_MODE) & 8#002 )); then
    usage_error "$ORIG_RUN_FILE can be changed by every user (mode $FILE_MODE) — refusing. Remove that permission (chmod go-w) and retry."
fi
if (( (8#$FILE_MODE) & 8#020 )) && ! group_is_owners_private_group "$FILE_GID" "$FILE_UID"; then
    usage_error "$ORIG_RUN_FILE can be changed by other members of group $FILE_GID (mode $FILE_MODE) — refusing. Remove that permission (chmod go-w) and retry."
fi
ACL_WRITERS="$(acl_writers "$ORIG_RUN_FILE" "$FILE_UID")"
if [[ "$ACL_WRITERS" == "?" ]]; then
    usage_error "$ORIG_RUN_FILE has an access control list that can't be checked (getfacl is missing) — refusing. Remove it (setfacl -b) and retry."
fi
while IFS= read -r who; do
    [[ -n "$who" ]] || continue
    if [[ "$who" == group:* ]] && group_is_owners_private_group "${who#group:}" "$FILE_UID"; then continue; fi
    usage_error "$ORIG_RUN_FILE has an access control list letting $who change it — refusing. Remove it (setfacl -b) and retry."
done <<<"$ACL_WRITERS"

# ── Detect package manager ─────────────────────────────────────────────
detect_pkg_mgr
set_kernel "$(uname -r)"

log "Run file: $ORIG_RUN_FILE (dkms=$USE_DKMS block_distro_driver=$BLOCK_DISTRO module=${MODULE_TYPE:-auto} pkg_mgr=$PKG_MGR)"

# A requested stop, honoured before anything has been changed.
stop_point_unchanged() {
    if [[ $STOP_REQUESTED -eq 1 ]]; then
        log "Stopped as requested. No changes made."
        exit "$EXIT_FAILED"
    fi
}

# ── Step 0: Copy into a root-only directory and verify the copy ───────
# /var/tmp rather than /tmp: the installer self-extracts and executes, and
# /tmp is often mounted noexec.
PRIV_DIR="$(mktemp -d /var/tmp/green-light-install.XXXXXX)"
chmod 700 "$PRIV_DIR"
RUN_FILE="$PRIV_DIR/installer.run"
log "Copying installer to private directory…"
if ! cp --no-preserve=mode,ownership -- "$ORIG_RUN_FILE" "$RUN_FILE"; then
    log "ERROR: Could not copy the installer (disk full?). No changes made."
    exit "$EXIT_FAILED"
fi
chmod 700 "$RUN_FILE"
ACTUAL_SHA256="$(sha256sum "$RUN_FILE" | cut -d' ' -f1)"
if [[ "$ACTUAL_SHA256" != "$EXPECT_SHA256" ]]; then
    log "ERROR: SHA256 mismatch on the private copy (expected $EXPECT_SHA256, got $ACTUAL_SHA256). No changes made."
    exit "$EXIT_FAILED"
fi
log "SHA256 verified on private copy"

# ── Step 1: Verify archive integrity before touching anything ────────
log "Verifying installer archive integrity…"
if ! nolock "$RUN_FILE" --check >>"$LOGFILE" 2>&1; then
    log "ERROR: Installer failed its integrity self-check. No changes made."
    exit "$EXIT_FAILED"
fi
log "Integrity OK"

# ── Step 2: Build prerequisites ───────────────────────────────────────
# Installing them may fail for reasons that don't matter (an unrelated
# repository being down, packages already present under other names), so
# that alone is not fatal. What is checked afterwards is fatal: everything
# below removes the distro driver, and a module that can't build would leave
# the next boot without any NVIDIA driver.
log "Ensuring kernel headers and build tools for $KVER…"
if [[ "$PKG_MGR" == "apt" ]]; then
    nolock apt-get install -y "linux-headers-${KVER}" build-essential dkms >>"$LOGFILE" 2>&1 \
        || log "WARNING: apt could not install all of the prerequisites; checking what is there"
else
    nolock dnf install -y "kernel-devel-${KVER}" "kernel-headers-${KVER}" \
        gcc make dkms >>"$LOGFILE" 2>&1 \
        || log "WARNING: dnf could not install all of the prerequisites; checking what is there"
fi

# The compiler the kernel was built with, chosen the way DKMS does: clang
# kernels build with LLVM=1; for gcc, the compiler named in
# CONFIG_CC_VERSION_TEXT (e.g. x86_64-linux-gnu-gcc-15) when installed, else
# gcc. Sets BUILD_VARS (make variables) and BUILD_CC (for the prerequisites).
choose_compiler() {
    local autoconf="/lib/modules/$KVER/build/include/generated/autoconf.h" text cc
    BUILD_VARS=()
    text="$(grep -s '^#define CONFIG_CC_VERSION_TEXT ' "$autoconf" | cut -d'"' -f2 || true)"
    cc="${text%% *}"
    if grep -qs '^#define CONFIG_CC_IS_CLANG 1' "$autoconf"; then
        BUILD_CC="clang"
        BUILD_VARS=(LLVM=1)
    elif [[ -n "$cc" ]] && command -v "$cc" >/dev/null 2>&1; then
        BUILD_CC="$cc"
        BUILD_VARS=("CC=$cc")
    else
        BUILD_CC="gcc"
    fi
}

check_build_prereqs() {
    local build="/lib/modules/$KVER/build" autoconf missing=() list
    autoconf="$build/include/generated/autoconf.h"
    if [[ ! -f "$build/Makefile" || ! -f "$autoconf" ]]; then
        missing+=("the kernel headers for $KVER ($build)")
    fi
    # Modules must be built with the compiler (and linker) the kernel was.
    choose_compiler
    command -v "$BUILD_CC" >/dev/null 2>&1 || missing+=("$BUILD_CC")
    if grep -qs '^#define CONFIG_LD_IS_LLD 1' "$autoconf"; then
        command -v ld.lld >/dev/null 2>&1 || missing+=("ld.lld")
    fi
    command -v make >/dev/null 2>&1 || missing+=("make")
    if [[ $USE_DKMS -eq 1 ]]; then
        command -v dkms >/dev/null 2>&1 || missing+=("dkms")
    fi
    if [[ ${#missing[@]} -gt 0 ]]; then
        printf -v list '%s, ' "${missing[@]}"
        log "ERROR: The NVIDIA kernel module can't be built for kernel $KVER: missing ${list%, }."
        log "       Install them and retry (the package manager's output is in $LOGFILE). No changes made."
        exit "$EXIT_FAILED"
    fi
    log "Build prerequisites for $KVER are present (compiler: $BUILD_CC)"
}
check_build_prereqs

# ── Step 2b: Test-build the kernel module before changing anything ────
# The most likely way for the install to fail is the kernel module not
# building for this kernel (a kernel newer than the driver supports). Build
# it the way DKMS does, from the unpacked installer in the private dir: if
# that fails, nothing has been changed yet. With the module flavor left to
# the installer, both flavors are built, since which one it picks depends on
# the GPUs.
test_build_modules() {
    local dir="$PRIV_DIR/unpacked" flavors=() f out="$PRIV_DIR/test-build.log"
    log "Test-building the NVIDIA kernel module for $KVER before changing anything…"
    if ! nolock "$RUN_FILE" --extract-only --target "$dir" >>"$LOGFILE" 2>&1; then
        log "ERROR: Could not unpack the installer to test-build its kernel module. No changes made."
        exit "$EXIT_FAILED"
    fi
    case "$MODULE_TYPE" in
        open)        flavors=(kernel-open) ;;
        proprietary) flavors=(kernel) ;;
        *)           flavors=(kernel-open kernel) ;;
    esac
    for f in "${flavors[@]}"; do
        # Drivers older than 515 have no open flavor.
        [[ -d "$dir/$f" ]] || continue
        if ! nolock make -C "$dir/$f" -j"$(nproc)" "${BUILD_VARS[@]}" KERNEL_UNAME="$KVER" modules >"$out" 2>&1; then
            cat "$out" >> "$LOGFILE" 2>/dev/null || true
            log "ERROR: The NVIDIA kernel module ($f) doesn't build for kernel $KVER. No changes made."
            log_tail "$out" "the test build"
            exit "$EXIT_FAILED"
        fi
        log "Test build OK: $f"
    done
    rm -rf "$dir" "$out"
}
test_build_modules
stop_point_unchanged

# ── Step 3: Record everything, then clear conflicting distro packages ─
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
#     makes dpkg refuse to remove nvidia-support-<ver>. The half-removed
#     state that results leaves nvidia-support-<ver>'s
#     /usr/lib/nvidia/alternate-install-present marker file in place, which
#     makes the .run installer itself abort with "please use the Debian
#     packages instead" — on a machine that's mid-purge of those exact
#     packages. Confirmed end to end against a real install.
#
# Everything this step and the next ones change goes into the journal first
# (see above), so a failure, a stop or a cut-off run can put it all back,
# checked against the recorded state.
PKGS=()       # installed driver packages to remove (name[:arch] / rpm name)
HELD=()       # apt holds on them (in the baseline: what the rollback restores)
UNHOLD=()     # apt holds on them now (what this run lifts)
BACKUPS=()    # "<path>|<saved copy number, or empty when it didn't exist>"
WAS_BLOCKED=0
INSTALLER_STARTED=0

log "Removing distro-managed NVIDIA packages (if any)…"
if [[ "$PKG_MGR" == "apt" ]]; then
    # Second status letter "i" = installed, whatever the selection (a held
    # package reads "hi"). binary:Package keeps the :i386 of multiarch copies.
    while IFS='|' read -r st name _; do
        if [[ "${st:1:1}" == "i" ]] && is_driver_pkg "$name" "${DEB_DRIVER_GLOBS[@]}"; then
            PKGS+=("$name")
        fi
    done < <(nolock dpkg-query -W -f '${db:Status-Abbrev}|${binary:Package}|${Version}\n' \
                 "${DEB_DRIVER_GLOBS[@]}" 2>/dev/null || true)
    while IFS= read -r name; do
        if is_driver_pkg "$name" "${DEB_DRIVER_GLOBS[@]}"; then HELD+=("$name"); fi
    done < <(nolock apt-mark showhold 2>/dev/null || true)
    UNHOLD=("${HELD[@]}")
else
    while IFS= read -r name; do
        if is_driver_pkg "$name" "${RPM_DRIVER_GLOBS[@]}"; then PKGS+=("$name"); fi
    done < <(nolock rpm -qa --qf '%{NAME}\n' "${RPM_DRIVER_GLOBS[@]}" 2>/dev/null || true)
fi
if distro_driver_blocked; then WAS_BLOCKED=1; fi
RUN_VER_BEFORE="$(installed_run_version)"
MOD_VER_BEFORE="$(module_version)"
MOD_SIGNER_BEFORE="$(module_signer)"
# Every kernel that had an nvidia module under the previous setup.
MOD_KERNELS_BEFORE=()
for kdir in /lib/modules/*/; do
    kdir="${kdir%/}"
    if [[ -n "$(module_version "${kdir##*/}")" ]]; then MOD_KERNELS_BEFORE+=("${kdir##*/}"); fi
done

# Write the journal: built under a temporary name and renamed into place.
# What the run is putting in and in which boot, so recovery can tell a
# journal that no longer describes the computer.
# The version comes from the verified copy itself (its makeself header's
# label, "NVIDIA Accelerated Graphics Driver for Linux-x86_64 595.84"), not
# the file name, which a download may have changed.
TARGET_RUN_VER=""
RUN_LABEL="$(head -c 65536 "$RUN_FILE" | grep -a -m1 '^label=' || true)"
if [[ "$RUN_LABEL" =~ [[:space:]]([0-9]+\.[0-9]+(\.[0-9]+)?)\"[[:space:]]*$ ]]; then TARGET_RUN_VER="${BASH_REMATCH[1]}"; fi
if [[ -z "$TARGET_RUN_VER" ]]; then
    log "WARNING: The installer's version can't be read from it; if this install is cut off, recovering it may need Discard Recovery Data"
fi
BOOT_ID="$(cat /proc/sys/kernel/random/boot_id 2>/dev/null || true)"
mkdir -p "$STATE_DIR"
chmod 755 "$STATE_DIR"
rm -rf "$JOURNAL.new"
if [[ $CARRIED -eq 1 ]]; then
    # An earlier install's rollback failed: its record of the setup before
    # it (packages, plan, copies, holds, block, files, driver and module)
    # stays the baseline; this install's own removals are added to it.
    cp -a "$JOURNAL" "$JOURNAL.new"
    if [[ ${#PKGS[@]} -gt 0 ]]; then
        { cat "$JOURNAL.new/planned"; planned_removals "${PKGS[@]}"; } | sort -u > "$PRIV_DIR/planned"
        cp -- "$PRIV_DIR/planned" "$JOURNAL.new/planned"
    fi
    declare -p KVER TARGET_RUN_VER BOOT_ID KNOWN_TARGETS EARLIER_INSTALLER_STARTED EARLIER_KERNELS >> "$JOURNAL.new/state"
else
    mkdir -m 700 "$JOURNAL.new" "$JOURNAL.new/backup"
    package_snapshot > "$JOURNAL.new/packages-before"
    if [[ ${#PKGS[@]} -gt 0 ]]; then
        planned_removals "${PKGS[@]}" > "$JOURNAL.new/planned"
    else
        : > "$JOURNAL.new/planned"
    fi
    # Packages of the plan that were installed only as dependencies, so they
    # are marked that way again when put back.
    if [[ "$PKG_MGR" == "apt" ]]; then
        { nolock apt-mark showauto 2>/dev/null || true; } | grep -xF -f "$JOURNAL.new/planned" > "$JOURNAL.new/auto" || true
    else
        { nolock dnf repoquery --installed --qf '%{name}.%{arch}\n' 2>/dev/null || true; } > "$PRIV_DIR/all-installed"
        { nolock dnf repoquery --installed --userinstalled --qf '%{name}.%{arch}\n' 2>/dev/null || true; } > "$PRIV_DIR/user-installed"
        grep -xF -f "$JOURNAL.new/planned" "$PRIV_DIR/all-installed" | grep -vxF -f "$PRIV_DIR/user-installed" > "$JOURNAL.new/auto" || true
    fi
    backup_file /etc/modprobe.d/blacklist-nouveau.conf
    backup_file /etc/modprobe.d/nvidia-drm-modeset.conf
    backup_file /usr/lib/nvidia/alternate-install-present
    KNOWN_TARGETS=()
    EARLIER_INSTALLER_STARTED=0
    EARLIER_KERNELS=()
    declare -p PKG_MGR KVER HELD BACKUPS WAS_BLOCKED RUN_VER_BEFORE MOD_VER_BEFORE MOD_SIGNER_BEFORE \
        MOD_KERNELS_BEFORE TARGET_RUN_VER BOOT_ID KNOWN_TARGETS EARLIER_INSTALLER_STARTED EARLIER_KERNELS \
        > "$JOURNAL.new/state"
    : > "$JOURNAL.new/cache-index"
fi
printf 'armed\n' > "$JOURNAL.new/phase"
shopt -s nullglob
durable "$JOURNAL.new"/* "$JOURNAL.new"/backup/* "$JOURNAL.new"/cache/* "$JOURNAL.new/backup" "$JOURNAL.new"
shopt -u nullglob
rm -rf "$JOURNAL"
mv "$JOURNAL.new" "$JOURNAL"
durable "$STATE_DIR"
# A journal made by this run may go when it ends before changing anything;
# a carried one still describes an unrestored setup and stays.
if [[ $CARRIED -eq 0 ]]; then JOURNAL_OURS=1; fi
# What a rollback restores: the baseline (the carried one, if any).
# shellcheck source=/dev/null  # written by this script, root-only
source "$JOURNAL/state"
# From here a leftover journal means "roll this back" (phase: armed).
if [[ -s "$JOURNAL/planned" ]]; then
    log "Keeping copies of the packages the install removes, so they can be put back offline…"
    cache_planned_packages
fi
if [[ $STOP_REQUESTED -eq 1 ]]; then
    if [[ $CARRIED -eq 0 ]]; then journal_clear; fi
    stop_point_unchanged
fi

# Keep the machine from shutting down or suspending until the driver is in
# place or put back.
start_inhibitor

ROLLBACK_ARMED=1

# The block (if any) would stop the package manager from touching these
# packages; it is set again once the driver is installed, as asked.
unblock_distro_driver
if [[ "$PKG_MGR" == "apt" ]]; then
    # apt refuses to remove held packages.
    if [[ ${#UNHOLD[@]} -gt 0 ]]; then
        nolock apt-mark unhold "${UNHOLD[@]}" >>"$LOGFILE" 2>&1
    fi
    # Driver packages only. libcuda*/libcudnn* are deliberately NOT matched:
    # they can belong to a user-installed CUDA toolkit unrelated to the
    # display driver (the driver's own libcuda is in libnvidia-compute-*).
    # apt also removes what depends on them (Ubuntu's prebuilt module
    # packages, cuda-drivers, ...); those are in the journal's plan.
    if [[ ${#PKGS[@]} -gt 0 ]]; then
        log "  purging: ${PKGS[*]}"
        # No dpkg --force-all fallback: it can leave dpkg in a broken
        # dependency state. Fail and restore instead.
        if ! nolock apt-get purge -y "${PKGS[@]}" >>"$LOGFILE" 2>&1; then
            log "ERROR: apt-get purge of the distro NVIDIA packages failed (see $LOGFILE)."
            exit "$EXIT_FAILED"
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
    if [[ ${#PKGS[@]} -gt 0 ]]; then
        log "  removing: ${PKGS[*]}"
        if ! nolock dnf remove -y "${PKGS[@]}" >>"$LOGFILE" 2>&1; then
            log "ERROR: dnf remove of the distro NVIDIA packages failed (see $LOGFILE)."
            exit "$EXIT_FAILED"
        fi
    fi
fi
ALSO_REMOVED=()
while IFS='|' read -r key _; do
    [[ -n "$key" ]] || continue
    if [[ " ${PKGS[*]} " != *" ${key} "* && " ${PKGS[*]} " != *" ${key%.*} "* ]]; then ALSO_REMOVED+=("$key"); fi
done < <(packages_removed)
if [[ ${#ALSO_REMOVED[@]} -gt 0 ]]; then
    log "  also removed, as they depend on the driver: ${ALSO_REMOVED[*]}"
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

if [[ $STOP_REQUESTED -eq 1 ]]; then
    log "Stopping as requested, before NVIDIA's installer: putting everything back."
    exit "$EXIT_FAILED"
fi

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
            log "WARNING: A PKCS#11 signing key can't be passed to the NVIDIA installer; the module will not be signed (turn on DKMS to have it signed)"
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
# resolve_signing_key may be called again by the rollback's checks.
INSTALL_SIGN_KEY="$SIGN_KEY"
INSTALL_SIGN_CERT="$SIGN_CERT"

RC=0
INSTALLER_STARTED=1
journal_set_phase installer-started
nolock "$RUN_FILE" "${INSTALLER_ARGS[@]}" >>"$LOGFILE" 2>&1 || RC=$?
if [[ $RC -ne 0 ]]; then
    log "ERROR: NVIDIA installer exited with code $RC"
    log "See /var/log/nvidia-installer.log for details."
    log_dkms_build_tail
    exit "$EXIT_FAILED"
fi
journal_set_phase installer-done
ROLLBACK_ARMED=0
journal_clear
log "NVIDIA installer finished successfully"
if [[ $STOP_REQUESTED -eq 1 ]]; then
    log "A stop was requested while NVIDIA's installer ran; it had already succeeded, so the install is finished rather than undone."
fi

# ── Step 6: Keep the distro driver from coming back (optional) ────────
# Right after the install, so every later exit (initramfs failure included)
# leaves the block as asked.
if [[ $BLOCK_DISTRO -eq 1 ]]; then
    if block_distro_driver; then
        log "Blocked the distro's NVIDIA driver packages: the package manager won't install them over this driver"
    else
        log "WARNING: Could not block the distro's NVIDIA driver packages"
    fi
elif [[ $WAS_BLOCKED -eq 1 ]]; then
    log "Removed the block on the distro's NVIDIA driver packages"
fi

# ── Step 7: Rebuild initramfs so the blacklist applies at boot ────────
log "Rebuilding initramfs…"
if ! nolock "${INITRAMFS_CMD[@]}" >>"$LOGFILE" 2>&1; then
    log "ERROR: initramfs rebuild failed. The driver is installed but the nouveau"
    log "       blacklist may not apply at boot. Run: ${INITRAMFS_CMD[*]}"
    exit "$EXIT_INITRAMFS"
fi
if [[ -n "$INHIBIT_PID" ]]; then kill "$INHIBIT_PID" 2>/dev/null || true; INHIBIT_PID=""; fi

# ── Step 8: Verify the result (report only; never fails the install) ──
# Everything here is a read-only check whose outcome goes to the log; the app
# shows the "Verify:" lines after the install finishes.
log "Verifying the installation…"
SIGN_KEY="$INSTALL_SIGN_KEY"
SIGN_CERT="$INSTALL_SIGN_CERT"
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
    MODULE_SIGNER="$(module_signer)"
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
    MODULE_SIGNER="$(module_signer)"
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

log "==== Done. Reboot to switch to the new driver. ===="
exit 0
