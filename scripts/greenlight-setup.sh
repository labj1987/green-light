#!/usr/bin/env bash
# greenlight-setup — runs as root via pkexec (polkit action
# io.github.labj1987.GreenLight.setup once installed). Installs or updates
# GreenLight's privileged install script and polkit policy at their fixed
# system paths.
#
# Usage: greenlight-setup <stage-dir>
#   <stage-dir> holds `privileged-install.sh` and `policy`, staged by the
#   AppImage's AppRun (the AppImage's FUSE mount isn't readable by root).
#
# The stage directory is user-writable, so nothing in it is trusted: the
# files are copied into a root-only directory and verified against the
# SHA256 values baked into this helper at build time before installation.
# (The @...@ placeholders below are substituted by build-appimage.sh; an
# unsubstituted copy refuses to run.)

set -euo pipefail

BAKED_SCRIPT_SHA256="@SCRIPT_SHA256@"
BAKED_POLICY_SHA256="@POLICY_SHA256@"

DST_DIR="/usr/lib/greenlight"
DST_SCRIPT="$DST_DIR/privileged-install.sh"
DST_HELPER="$DST_DIR/greenlight-setup"
DST_POLICY="/usr/share/polkit-1/actions/io.github.labj1987.GreenLight.policy"

die() { echo "greenlight-setup: $*" >&2; exit 1; }

STAGE="${1:-}"
[[ -n "$STAGE" && -d "$STAGE" ]] || die "usage: greenlight-setup <stage-dir>"
[[ "$BAKED_SCRIPT_SHA256" =~ ^[0-9a-f]{64}$ && "$BAKED_POLICY_SHA256" =~ ^[0-9a-f]{64}$ ]] \
    || die "build-time hashes missing; refusing to install"

WORK="$(mktemp -d /var/tmp/greenlight-setup.XXXXXX)"
chmod 700 "$WORK"
trap 'rm -rf "$WORK"' EXIT

for f in privileged-install.sh policy; do
    [[ -f "$STAGE/$f" && ! -L "$STAGE/$f" ]] || die "missing or invalid staged file: $f"
    cp --no-preserve=mode,ownership -- "$STAGE/$f" "$WORK/$f"
done
cp --no-preserve=mode,ownership -- "$0" "$WORK/helper"

check() { # <file> <expected-sha256> <label>
    local got
    got="$(sha256sum "$1" | cut -d' ' -f1)"
    [[ "$got" == "$2" ]] || die "$3 failed verification (sha256 mismatch)"
}
check "$WORK/privileged-install.sh" "$BAKED_SCRIPT_SHA256" "install script"
check "$WORK/policy"                "$BAKED_POLICY_SHA256" "polkit policy"

install -D -m 755 "$WORK/privileged-install.sh" "$DST_SCRIPT"
install -D -m 755 "$WORK/helper"                "$DST_HELPER"
install -D -m 644 "$WORK/policy"                "$DST_POLICY"
echo "greenlight-setup: installed"
