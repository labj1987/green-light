#!/usr/bin/env bash
# build-appimage.sh — build the Green Light AppImage.
# Run from the repo root on Ubuntu (the GitHub Actions runner), as an ordinary user: it writes
# only inside the checkout, and never changes tracked files. Only its from-scratch dependency
# install below needs root, which it gets through sudo.
set -euo pipefail

APP="green-light"

# >>> libcheck: before any setup, make sure the app's shared libraries resolve.
# LIBCHECK_LDD, LIBCHECK_OS_RELEASE and LIBCHECK_DIALOG=none exist for the tests.
libcheck_missing() {
    local out
    if [[ -n "${LIBCHECK_LDD:-}" ]]; then
        out="$("$LIBCHECK_LDD" "$1" 2>/dev/null)"
    elif command -v ldd >/dev/null 2>&1; then
        out="$(ldd "$1" 2>/dev/null)"
    else
        out="$(LD_TRACE_LOADED_OBJECTS=1 "$1" 2>/dev/null)"
    fi
    sed -n 's/^[[:space:]]*\([^[:space:]]*\) => not found.*/\1/p' <<<"$out"
}

libcheck_install_hint() {
    local f="${LIBCHECK_OS_RELEASE:-/etc/os-release}" ids id
    [[ -r "$f" ]] || return 0
    # shellcheck disable=SC1090
    ids="$(. "$f" 2>/dev/null; echo "${ID:-} ${ID_LIKE:-}")"
    for id in $ids; do
        case "$id" in
            debian|ubuntu|linuxmint|pop) echo "sudo apt install libgtk-4-1 libadwaita-1-0"; return 0 ;;
            fedora|rhel|centos|rocky|almalinux) echo "sudo dnf install gtk4 libadwaita"; return 0 ;;
            arch|cachyos|manjaro|endeavouros) echo "sudo pacman -S gtk4 libadwaita"; return 0 ;;
            opensuse*|suse|sles) echo "sudo zypper install libgtk-4-1 libadwaita-1-0"; return 0 ;;
        esac
    done
}

libcheck_show() {
    printf '%s: %s\n' "$1" "$2" >&2
    [[ "${LIBCHECK_DIALOG:-}" == none ]] && return 0
    [[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]] || return 0
    if command -v zenity >/dev/null 2>&1; then
        zenity --error --title="$1" --text="$2" --width=460 >/dev/null 2>&1
    elif command -v kdialog >/dev/null 2>&1; then
        kdialog --error "$2" --title "$1" >/dev/null 2>&1
    elif command -v notify-send >/dev/null 2>&1; then
        notify-send -u critical "$1" "$2" >/dev/null 2>&1
    fi
    return 0
}

# libcheck_run NAME BINARY: returns 1 (after showing why) when libraries are missing.
libcheck_run() {
    local missing list hint msg
    missing="$(libcheck_missing "$2")"
    [[ -n "$missing" ]] || return 0
    list="$(paste -sd, - <<<"$missing" | sed 's/,/, /g')"
    hint="$(libcheck_install_hint)"
    msg="$1 can't start because these system libraries are missing: $list."
    if [[ -n "$hint" ]]; then
        msg+=" They come with GTK 4 and libadwaita. Install them with: $hint, then start $1 again."
    else
        msg+=" They come with GTK 4 (4.12 or newer) and libadwaita (1.5 or newer): install both with your package manager, then start $1 again."
    fi
    libcheck_show "$1" "$msg"
    return 1
}
# <<< libcheck

libcheck_run "Green Light" "$HERE/usr/bin/$APP" || exit 1
# Single source of truth: the version in Cargo.toml
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
ARCH="x86_64"
BUILD_DIR="build-appimage"
APPDIR="$BUILD_DIR/AppDir"

echo "==> Building $APP $VERSION AppImage"

# ── Build dependencies ────────────────────────────────────────────────
# Only on a machine with no toolchain (this block needs root, through sudo when the
# script runs as an ordinary user). In CI the workflow installs the GTK headers and the
# packaging tools itself, so the guard is false there and nothing below is installed.
if ! command -v cargo >/dev/null 2>&1 || ! pkg-config --exists gtk4 2>/dev/null; then
    echo "==> Installing build dependencies"
    AS_ROOT=()
    if [[ $EUID -ne 0 ]]; then AS_ROOT=(sudo); fi
    # Tolerate an unrelated third-party repo failing to refresh: apt falls back
    # to its cached index for it. Only the install failing should be fatal.
    "${AS_ROOT[@]}" apt-get update -qq || true
    "${AS_ROOT[@]}" apt-get install -y -qq cargo rustc libgtk-4-dev libadwaita-1-dev \
        pkg-config zsync wget file desktop-file-utils
fi

for tool in wget file desktop-file-validate; do
    command -v "$tool" >/dev/null 2>&1 || { echo "==> ERROR: $tool is not installed" >&2; exit 1; }
done

# ── Release build ─────────────────────────────────────────────────────
echo "==> cargo build --release --locked"
cargo build --release --locked

# ── AppDir layout ─────────────────────────────────────────────────────
rm -rf "$BUILD_DIR"
mkdir -p "$APPDIR/usr/bin" \
         "$APPDIR/usr/lib/$APP" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/256x256/apps" \
         "$APPDIR/usr/share/polkit-1/actions" \
         "$APPDIR/usr/share/metainfo"

cp "${CARGO_TARGET_DIR:-target}/release/$APP" "$APPDIR/usr/bin/"
cp scripts/privileged-install.sh            "$APPDIR/usr/lib/$APP/"
cp data/green-light-recover.service         "$APPDIR/usr/lib/$APP/"
chmod 755 "$APPDIR/usr/lib/$APP/privileged-install.sh"

# Setup helper: bake the SHA256 of the script and policy into it so that,
# once installed root-owned, it can verify whatever AppRun stages for it.
SCRIPT_SHA="$(sha256sum scripts/privileged-install.sh | cut -d' ' -f1)"
POLICY_SHA="$(sha256sum data/io.github.labj1987.GreenLight.policy | cut -d' ' -f1)"
UNIT_SHA="$(sha256sum data/green-light-recover.service | cut -d' ' -f1)"
HELPER_TEXT="$(<scripts/green-light-setup.sh)"
HELPER_TEXT="${HELPER_TEXT//@SCRIPT_SHA256@/$SCRIPT_SHA}"
HELPER_TEXT="${HELPER_TEXT//@POLICY_SHA256@/$POLICY_SHA}"
HELPER_TEXT="${HELPER_TEXT//@UNIT_SHA256@/$UNIT_SHA}"
printf '%s\n' "$HELPER_TEXT" > "$APPDIR/usr/lib/$APP/green-light-setup"
chmod 755 "$APPDIR/usr/lib/$APP/green-light-setup"
cp data/$APP.desktop                        "$APPDIR/usr/share/applications/"
cp data/$APP-256.png                        "$APPDIR/usr/share/icons/hicolor/256x256/apps/$APP.png"
cp data/io.github.labj1987.GreenLight.policy       "$APPDIR/usr/share/polkit-1/actions/"
# The <releases> list is generated from CHANGELOG.md's version headings straight into the
# AppDir (the tracked copy is left alone; CI checks that it is in step), and fails the build
# if the newest one is not this Cargo.toml version.
python3 scripts/sync_appdata_releases.py \
    --output "$APPDIR/usr/share/metainfo/io.github.labj1987.GreenLight.appdata.xml"

# Top-level AppImage requirements
cp data/$APP.desktop "$APPDIR/"
cp data/$APP-256.png "$APPDIR/$APP.png"

desktop-file-validate "$APPDIR/$APP.desktop"

# ── AppRun ────────────────────────────────────────────────────────────
# On first launch the privileged script and polkit policy must exist at
# fixed system paths (polkit refuses relative/user paths), so AppRun
# installs them via the dedicated green-light-setup helper when missing or
# outdated, then execs the app. The helper verifies the staged files against
# hashes baked into it at build time, so an installed helper can only
# reinstall the files of its own build (restoring deleted or altered ones,
# under its own polkit action). For a new build, or the very first run,
# pkexec runs the staged helper through bash (the stage directory is in /tmp,
# which may be mounted noexec); the prompt names the helper's path.
cat > "$APPDIR/AppRun" << 'APPRUN'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "$0")")"
APP="green-light"

SRC_SCRIPT="$HERE/usr/lib/$APP/privileged-install.sh"
SRC_HELPER="$HERE/usr/lib/$APP/green-light-setup"
SRC_POLICY="$HERE/usr/share/polkit-1/actions/io.github.labj1987.GreenLight.policy"
SRC_UNIT="$HERE/usr/lib/$APP/green-light-recover.service"
DST_SCRIPT="/usr/lib/$APP/privileged-install.sh"
DST_HELPER="/usr/lib/$APP/green-light-setup"
DST_POLICY="/usr/share/polkit-1/actions/io.github.labj1987.GreenLight.policy"
DST_UNIT="/etc/systemd/system/green-light-recover.service"
SETUP_ACTION="io.github.labj1987.GreenLight.setup"

needs_install=0
for pair in "$SRC_SCRIPT:$DST_SCRIPT" "$SRC_POLICY:$DST_POLICY" "$SRC_HELPER:$DST_HELPER" "$SRC_UNIT:$DST_UNIT"; do
    src="${pair%%:*}"; dst="${pair#*:}"
    if [[ ! -f "$dst" ]] || ! cmp -s "$src" "$dst"; then
        needs_install=1
    fi
done
# The boot-time recovery unit must also be enabled (the setup helper does it).
if [[ -d /run/systemd/system ]] && command -v systemctl >/dev/null 2>&1 \
    && ! systemctl is-enabled --quiet green-light-recover.service 2>/dev/null; then
    needs_install=1
fi

if [[ $needs_install -eq 1 ]]; then
    STAGE="$(mktemp -d)"
    cp "$SRC_SCRIPT" "$STAGE/privileged-install.sh"
    cp "$SRC_POLICY" "$STAGE/policy"
    cp "$SRC_UNIT" "$STAGE/recover-service"
    cp "$SRC_HELPER" "$STAGE/green-light-setup"
    chmod 755 "$STAGE/green-light-setup"

    rc=0
    ERRF="$(mktemp)"
    if [[ -x "$DST_HELPER" ]] && cmp -s "$SRC_HELPER" "$DST_HELPER" \
        && pkaction --action-id "$SETUP_ACTION" >/dev/null 2>&1; then
        pkexec "$DST_HELPER" "$STAGE" 2>"$ERRF" || rc=$?
    else
        # $BASH: this script's own interpreter, as the absolute path pkexec needs.
        pkexec "$BASH" "$STAGE/green-light-setup" "$STAGE" 2>"$ERRF" || rc=$?
    fi
    rm -rf "$STAGE"
    cat "$ERRF" >&2

    # pkexec: 126 = password prompt dismissed, 127 = not authorized or it could
    # not run the helper. The helper itself never exits with either. The reason
    # is handed to the app (GREEN_LIGHT_SETUP_FAILURE), which shows it in a
    # banner and in the messages of the actions that need the setup.
    if [[ $rc -ne 0 ]]; then
        case $rc in
            126) reason="authorization was cancelled" ;;
            127) if grep -q -e "No authentication agent found" -e "Error creating textual authentication agent" "$ERRF"; then
                     reason="no polkit authentication agent is running (the program that shows the admin password dialog), so nothing was changed; start one (hyprpolkitagent on Hyprland, polkit-gnome (policykit-1-gnome on Debian/Ubuntu) or lxqt-policykit on sway or i3)"
                 else
                     reason="authorization failed, or pkexec could not run the setup helper"
                 fi ;;
            *)   reason="installing system components failed (exit $rc)" ;;
        esac
        echo "Green Light: setup did not complete: $reason." >&2
        export GREEN_LIGHT_SETUP_FAILURE="$reason"
    fi
    rm -f "$ERRF"
fi

export PATH="$HERE/usr/bin:$PATH"
exec "$HERE/usr/bin/$APP" "$@"
APPRUN
chmod 755 "$APPDIR/AppRun"

# ── appimagetool ──────────────────────────────────────────────────────
# Pinned release + checksum (from the release's published asset digest) so the
# release build doesn't depend on a moving, unverified "continuous" artifact.
APPIMAGETOOL_VERSION="1.9.1"
APPIMAGETOOL_SHA256="ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0"
TOOL_DIR=".cache"
TOOL="$TOOL_DIR/appimagetool-$APPIMAGETOOL_VERSION"
if [[ ! -f "$TOOL" ]]; then
    mkdir -p "$TOOL_DIR"
    wget -q --no-hsts -O "$TOOL.part" \
        "https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_VERSION/appimagetool-x86_64.AppImage"
    mv "$TOOL.part" "$TOOL"
fi
if ! echo "$APPIMAGETOOL_SHA256  $TOOL" | sha256sum -c --status -; then
    echo "==> ERROR: appimagetool checksum mismatch" >&2
    rm -f "$TOOL"
    exit 1
fi
chmod +x "$TOOL"

# The runtime appimagetool puts in front of the squashfs. Without --runtime-file it downloads
# the moving `continuous` build at pack time, so it is pinned and checked the same way.
# To bump: pick a release at https://github.com/AppImage/type2-runtime/releases and take the
# sha256 of its runtime-x86_64 asset (download it and run sha256sum).
RUNTIME_VERSION="20251108"
RUNTIME_SHA256="2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d"
RUNTIME="$TOOL_DIR/runtime-x86_64-$RUNTIME_VERSION"
if [[ ! -f "$RUNTIME" ]]; then
    mkdir -p "$TOOL_DIR"
    wget -q --no-hsts -O "$RUNTIME.part" \
        "https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_VERSION/runtime-x86_64"
    mv "$RUNTIME.part" "$RUNTIME"
fi
if ! echo "$RUNTIME_SHA256  $RUNTIME" | sha256sum -c --status -; then
    echo "==> ERROR: AppImage runtime checksum mismatch" >&2
    rm -f "$RUNTIME"
    exit 1
fi

echo "==> Packing AppImage"
OUT="$APP-$VERSION-$ARCH.AppImage"

UPDATE_INFORMATION="gh-releases-zsync|labj1987|green-light|latest|green-light-*-x86_64.AppImage.zsync"
VERSION="$VERSION" ARCH="$ARCH" "$TOOL" --appimage-extract-and-run \
    --runtime-file "$RUNTIME" -u "$UPDATE_INFORMATION" "$APPDIR" "$OUT"

echo "==> Done: $OUT"
ls -lh "$OUT"

# appimagetool's built-in zsync generation silently no-ops on GitHub Actions
# runners, so build the .zsync sidecar directly. Fatal in CI (CI is set): the
# AppImage's UPDATE_INFORMATION points at a .zsync, so a release without one
# cannot update. A local build only warns.
echo "==> Generating .zsync sidecar"
if ! command -v zsyncmake >/dev/null 2>&1; then
    if [[ -n "${CI:-}" ]]; then
        echo "==> ERROR: zsyncmake not found (install the zsync package)" >&2
        exit 1
    fi
    echo "==> WARNING: zsyncmake not found — continuing without .zsync"
elif zsyncmake "$OUT"; then
    echo "==> .zsync generated: $OUT.zsync"
elif [[ -n "${CI:-}" ]]; then
    echo "==> ERROR: zsyncmake failed" >&2
    exit 1
else
    echo "==> WARNING: zsyncmake failed — continuing without .zsync"
fi
