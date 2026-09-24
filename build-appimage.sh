#!/usr/bin/env bash
# build-appimage.sh — build the Green Light AppImage.
# Run from the repo root on Ubuntu (CI uses ubuntu-latest). Run as root in CI.
set -euo pipefail

APP="green-light"
# Single source of truth: the version in Cargo.toml
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
ARCH="x86_64"
BUILD_DIR="build-appimage"
APPDIR="$BUILD_DIR/AppDir"

echo "==> Building $APP $VERSION AppImage"

# ── Build dependencies ────────────────────────────────────────────────
# Refresh the package index first: installing from a stale runner index can
# 404 under `set -e`. Tolerate an unrelated third-party repo (e.g. the runner
# image's preinstalled Google Chrome source) failing to refresh -- apt falls
# back to its cached index for that repo and still refreshes everything else;
# only `apt-get install` failing on a package we actually need should be fatal.
apt-get update -qq || true

# zsync is installed unconditionally: the guard below evaluates false in CI
# (a prior workflow step already installs cargo), so the guarded block —
# and zsync along with it — was being silently skipped.
apt-get install -y -qq zsync

if ! command -v cargo >/dev/null 2>&1 || ! pkg-config --exists gtk4 2>/dev/null; then
    echo "==> Installing build dependencies"
    apt-get install -y -qq cargo rustc libgtk-4-dev libadwaita-1-dev \
        pkg-config libssl-dev wget file desktop-file-utils zsync
fi

# ── Release build ─────────────────────────────────────────────────────
echo "==> cargo build --release"
cargo build --release

# ── AppDir layout ─────────────────────────────────────────────────────
rm -rf "$BUILD_DIR"
mkdir -p "$APPDIR/usr/bin" \
         "$APPDIR/usr/lib/$APP" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/256x256/apps" \
         "$APPDIR/usr/share/polkit-1/actions" \
         "$APPDIR/usr/share/metainfo"

cp "target/release/$APP"                    "$APPDIR/usr/bin/"
cp scripts/privileged-install.sh            "$APPDIR/usr/lib/$APP/"
chmod 755 "$APPDIR/usr/lib/$APP/privileged-install.sh"

# Setup helper: bake the SHA256 of the script and policy into it so that,
# once installed root-owned, it can verify whatever AppRun stages for it.
SCRIPT_SHA="$(sha256sum scripts/privileged-install.sh | cut -d' ' -f1)"
POLICY_SHA="$(sha256sum data/io.github.labj1987.GreenLight.policy | cut -d' ' -f1)"
HELPER_TEXT="$(<scripts/green-light-setup.sh)"
HELPER_TEXT="${HELPER_TEXT//@SCRIPT_SHA256@/$SCRIPT_SHA}"
HELPER_TEXT="${HELPER_TEXT//@POLICY_SHA256@/$POLICY_SHA}"
printf '%s\n' "$HELPER_TEXT" > "$APPDIR/usr/lib/$APP/green-light-setup"
chmod 755 "$APPDIR/usr/lib/$APP/green-light-setup"
cp data/$APP.desktop                        "$APPDIR/usr/share/applications/"
cp data/$APP-256.png                        "$APPDIR/usr/share/icons/hicolor/256x256/apps/$APP.png"
cp data/io.github.labj1987.GreenLight.policy       "$APPDIR/usr/share/polkit-1/actions/"
cp data/io.github.labj1987.GreenLight.appdata.xml  "$APPDIR/usr/share/metainfo/"

# Keep the AppStream <releases> list in step with Cargo.toml: if the source
# appdata doesn't already list this version, add an entry for it.
METAINFO="$APPDIR/usr/share/metainfo/io.github.labj1987.GreenLight.appdata.xml"
if ! grep -q "<release version=\"$VERSION\"" "$METAINFO"; then
    echo "==> appdata has no <release> for $VERSION; adding one"
    META_TEXT="$(<"$METAINFO")"
    NEW_RELEASE="<releases>
    <release version=\"$VERSION\" date=\"$(date -u +%F)\"/>"
    META_TEXT="${META_TEXT/<releases>/$NEW_RELEASE}"
    printf '%s\n' "$META_TEXT" > "$METAINFO"
fi

# Top-level AppImage requirements
cp data/$APP.desktop "$APPDIR/"
cp data/$APP-256.png "$APPDIR/$APP.png"

# ── AppRun ────────────────────────────────────────────────────────────
# On first launch the privileged script and polkit policy must exist at
# fixed system paths (polkit refuses relative/user paths), so AppRun
# installs them via the dedicated green-light-setup helper when missing or
# outdated, then execs the app. Once the helper is installed, updates use
# its own polkit action (a specific prompt); the very first run has no such
# action yet, so pkexec runs the staged helper directly (its prompt names
# the program, and the helper verifies everything against baked-in hashes).
cat > "$APPDIR/AppRun" << 'APPRUN'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "$0")")"
APP="green-light"

SRC_SCRIPT="$HERE/usr/lib/$APP/privileged-install.sh"
SRC_HELPER="$HERE/usr/lib/$APP/green-light-setup"
SRC_POLICY="$HERE/usr/share/polkit-1/actions/io.github.labj1987.GreenLight.policy"
DST_SCRIPT="/usr/lib/$APP/privileged-install.sh"
DST_HELPER="/usr/lib/$APP/green-light-setup"
DST_POLICY="/usr/share/polkit-1/actions/io.github.labj1987.GreenLight.policy"
SETUP_ACTION="io.github.labj1987.GreenLight.setup"

needs_install=0
for pair in "$SRC_SCRIPT:$DST_SCRIPT" "$SRC_POLICY:$DST_POLICY" "$SRC_HELPER:$DST_HELPER"; do
    src="${pair%%:*}"; dst="${pair#*:}"
    if [[ ! -f "$dst" ]] || ! cmp -s "$src" "$dst"; then
        needs_install=1
    fi
done

if [[ $needs_install -eq 1 ]]; then
    STAGE="$(mktemp -d)"
    cp "$SRC_SCRIPT" "$STAGE/privileged-install.sh"
    cp "$SRC_POLICY" "$STAGE/policy"
    cp "$SRC_HELPER" "$STAGE/green-light-setup"
    chmod 755 "$STAGE/green-light-setup"

    rc=0
    if [[ -x "$DST_HELPER" ]] && pkaction --action-id "$SETUP_ACTION" >/dev/null 2>&1; then
        pkexec "$DST_HELPER" "$STAGE" || rc=$?
    else
        pkexec "$STAGE/green-light-setup" "$STAGE" || rc=$?
    fi
    rm -rf "$STAGE"

    if [[ $rc -ne 0 ]]; then
        if [[ $rc -eq 126 || $rc -eq 127 ]]; then
            echo "Green Light: authorization was cancelled; system components were not updated." >&2
        else
            echo "Green Light: installing system components failed (exit $rc)." >&2
        fi
        if [[ ! -f "$DST_SCRIPT" ]]; then
            echo "Green Light: the install feature will not work until setup succeeds — relaunch to retry." >&2
        fi
    fi
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
TOOL="$BUILD_DIR/appimagetool"
if [[ ! -f "$TOOL" ]]; then
    echo "==> Downloading appimagetool $APPIMAGETOOL_VERSION"
    wget -q -O "$TOOL" \
        "https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_VERSION/appimagetool-x86_64.AppImage"
fi
echo "$APPIMAGETOOL_SHA256  $TOOL" | sha256sum -c - \
    || { echo "ERROR: appimagetool checksum mismatch" >&2; rm -f "$TOOL"; exit 1; }
chmod +x "$TOOL"

echo "==> Packing AppImage"
OUT="$APP-$VERSION-$ARCH.AppImage"

UPDATE_INFORMATION="gh-releases-zsync|labj1987|green-light|latest|green-light-*-x86_64.AppImage.zsync"
VERSION="$VERSION" ARCH="$ARCH" "$TOOL" --appimage-extract-and-run \
    -u "$UPDATE_INFORMATION" "$APPDIR" "$OUT"

echo "==> Done: $OUT"
ls -lh "$OUT"

# appimagetool's built-in zsync generation silently no-ops on this runner,
# so build the .zsync sidecar directly. Non-fatal: the AppImage itself is
# already valid without it.
echo "==> Generating .zsync sidecar"
if zsyncmake "$OUT"; then
    echo "==> .zsync generated: $OUT.zsync"
else
    echo "==> WARNING: zsyncmake failed — continuing without .zsync"
fi
