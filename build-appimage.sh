#!/usr/bin/env bash
# build-appimage.sh — build the Green Light AppImage.
# Run from the repo root on Ubuntu (the GitHub Actions runner), as an ordinary user: it writes
# only inside the checkout, and never changes tracked files. Only its from-scratch dependency
# install below needs root, which it gets through sudo.
set -euo pipefail

APP="green-light"

# Single source of truth: the version in Cargo.toml
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
ARCH="x86_64"
BUILD_DIR="build-appimage"
APPDIR="$BUILD_DIR/AppDir"

echo "==> Building $APP $VERSION AppImage"

# ── Build dependencies ────────────────────────────────────────────────
# The UI toolkit (GPUI) links libxcb, libxkbcommon and libxkbcommon-x11 and one of its crates
# probes fontconfig at build time (the binary does not load it), so their headers are the sign
# of a machine that can build it.
if ! command -v cargo >/dev/null 2>&1 || ! pkg-config --exists xcb xkbcommon xkbcommon-x11 fontconfig 2>/dev/null; then
    echo "==> Installing build dependencies"
    # Tolerate an unrelated third-party repo (e.g. the runner image's preinstalled
    # Google Chrome source) failing to refresh -- apt falls back to its cached index
    # for that repo and still refreshes everything else; only `apt-get install`
    # failing on a package we actually need should be fatal.
    SUDO=()
    if [[ $EUID -ne 0 ]]; then
        SUDO=(sudo)
    fi
    "${SUDO[@]}" apt-get update -qq || true
    "${SUDO[@]}" apt-get install -y -qq cargo rustc build-essential pkg-config \
        libxcb1-dev libxkbcommon-dev libxkbcommon-x11-dev libfontconfig1-dev \
        zsync wget file desktop-file-utils
fi
# On a machine that already has the toolchain (CI: the workflow installs the packaging
# tools) nothing above runs, so a missing tool is reported here instead of mid-build.
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

# The binary goes in as cargo built it (CARGO_TARGET_DIR when it is set, else target/). It brings
# no libraries: the UI toolkit draws everything itself and takes only the windowing and graphics
# libraries every desktop has from the system (AppRun's libcheck names them).
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

# The app's own licence, and the notices of everything built into the binary: every Rust crate
# of the release build with its licence text (the UI toolkit's assets crate carries the Lucide
# icons' ISC licence).
mkdir -p "$APPDIR/usr/share/licenses/$APP"
cp LICENSE "$APPDIR/usr/share/licenses/$APP/"
python3 scripts/third_party_licenses.py "$APPDIR/usr/share/licenses/$APP/THIRD-PARTY-LICENSES.txt"

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

# >>> libcheck: before anything else, make sure the app can start. Green Light brings no libraries
# of its own. It is linked against glibc, libgcc_s, libxcb, libxkbcommon and libxkbcommon-x11:
# `ldd -r` on the binary must report no missing library, no missing symbol version (a glibc older
# than the build's 2.39, for one) and no undefined symbol of the binary. And it loads by name at
# run time, where ldd cannot see them, libwayland-client on a Wayland session and a graphics
# loader, Vulkan's or EGL's (libcheck_runtime). A system with a loader but no working driver is
# the app's own business: it says so in a dialog of its own.
# LIBCHECK_LDD, LIBCHECK_LDCONFIG, LIBCHECK_OS_RELEASE, LIBCHECK_GLIBC_HAVE and LIBCHECK_DIALOG=none
# exist for the tests.

# libcheck_scan BINARY: sorts what `ldd -r BINARY` reports into LIBCHECK_LIBS (libraries not
# found), LIBCHECK_GLIBC (the newest GLIBC_x.y version that is needed but missing),
# LIBCHECK_OTHER (other "not found" lines), LIBCHECK_SYMBOLS (undefined symbols) and LIBCHECK_RC
# (ldd's exit status). Without ldd, or with "loader" as the second argument, the dynamic loader
# is asked directly, as ldd -r does (LIBCHECK_LOADER replaces it in the tests).
libcheck_scan() {
    local out line rc=0 re='version .GLIBC_([0-9]+(\.[0-9]+)*). not found'
    LIBCHECK_LIBS=(); LIBCHECK_GLIBC=""; LIBCHECK_OTHER=(); LIBCHECK_SYMBOLS=()
    if [[ "${2:-}" == loader ]]; then
        if [[ -n "${LIBCHECK_LOADER:-}" ]]; then
            out="$("$LIBCHECK_LOADER" "$1" 2>&1)" || rc=$?
        else
            out="$(LD_TRACE_LOADED_OBJECTS=1 LD_WARN=yes LD_BIND_NOW=yes "$1" 2>&1)" || rc=$?
        fi
    elif [[ -n "${LIBCHECK_LDD:-}" ]]; then
        out="$("$LIBCHECK_LDD" -r "$1" 2>&1)" || rc=$?
    elif command -v ldd >/dev/null 2>&1; then
        out="$(ldd -r "$1" 2>&1)" || rc=$?
    else
        out="$(LD_TRACE_LOADED_OBJECTS=1 LD_WARN=yes LD_BIND_NOW=yes "$1" 2>&1)" || rc=$?
    fi
    while IFS= read -r line; do
        line="${line#"${line%%[![:space:]]*}"}"
        if [[ "$line" == *"=> not found"* ]]; then
            LIBCHECK_LIBS+=("${line%% *}")
        elif [[ "$line" =~ $re ]]; then
            if [[ -z "$LIBCHECK_GLIBC" ]] || [[ "$(printf '%s\n' "$LIBCHECK_GLIBC" "${BASH_REMATCH[1]}" | sort -V | tail -n1)" != "$LIBCHECK_GLIBC" ]]; then
                LIBCHECK_GLIBC="${BASH_REMATCH[1]}"
            fi
        elif [[ "$line" == *"not found"* ]]; then
            LIBCHECK_OTHER+=("$line")
        elif [[ "$line" == "undefined symbol: "* ]] && libcheck_ours "$line" "$1"; then
            line="${line#undefined symbol: }"
            LIBCHECK_SYMBOLS+=("${line%%[[:space:]]*}")
        elif [[ "$line" == "symbol "*" version "*" not defined in file "* ]] && libcheck_ours "$line" "$1"; then
            line="${line#symbol }"
            LIBCHECK_SYMBOLS+=("${line%% *}")
        fi
    done <<<"$out"
    LIBCHECK_RC=$rc
}

# libcheck_ours LINE BINARY: whether the object an unresolved-symbol line ends with, "(OBJECT)", is
# BINARY. A system library's own unresolved (lazy) symbol is its business.
libcheck_ours() {
    local obj="${1##*(}"
    [[ "${obj%)}" == "$2" ]]
}

# libcheck_cache: puts the dynamic loader's cache listing (`ldconfig -p`) in LIBCHECK_CACHE, or
# nothing when there is no usable one: no ldconfig, or a cache that does not even list glibc (a
# system that keeps its libraries elsewhere, like NixOS). Without it nothing is said about the
# run-time libraries.
libcheck_cache() {
    local tool out
    LIBCHECK_CACHE=""
    for tool in "${LIBCHECK_LDCONFIG:-}" ldconfig /sbin/ldconfig /usr/sbin/ldconfig; do
        [[ -n "$tool" ]] || continue
        command -v "$tool" >/dev/null 2>&1 || continue
        out="$("$tool" -p 2>/dev/null)" || continue
        [[ "$out" == *$'\t'"libc.so.6 (libc6,x86-64"* ]] || return 0
        LIBCHECK_CACHE="$out"
        return 0
    done
}

# libcheck_has LIBRARY: whether the loader would find the 64-bit LIBRARY: it is in the cache, or
# in a directory of LD_LIBRARY_PATH.
libcheck_has() {
    local dir
    [[ "$LIBCHECK_CACHE" == *$'\t'"$1 (libc6,x86-64"* ]] && return 0
    local IFS=:
    for dir in ${LD_LIBRARY_PATH:-}; do
        [[ -n "$dir" && -e "$dir/$1" ]] && return 0
    done
    return 1
}

# libcheck_runtime: adds to LIBCHECK_LIBS the missing libraries among those the app loads by name:
# libwayland-client.so.0 on a Wayland session, and libvulkan.so.1 with libEGL.so.1 when both are
# missing (the window is drawn with either). LIBCHECK_NOTE gets the sentence that says so.
libcheck_runtime() {
    LIBCHECK_NOTE=""
    libcheck_cache
    [[ -n "$LIBCHECK_CACHE" ]] || return 0
    if [[ -n "${WAYLAND_DISPLAY:-}" ]] && ! libcheck_has libwayland-client.so.0; then
        LIBCHECK_LIBS+=(libwayland-client.so.0)
    fi
    if ! libcheck_has libvulkan.so.1 && ! libcheck_has libEGL.so.1; then
        LIBCHECK_LIBS+=(libvulkan.so.1 libEGL.so.1)
        LIBCHECK_NOTE=" The window is drawn with Vulkan or OpenGL ES, so one of libvulkan.so.1 and libEGL.so.1 is enough."
    fi
}

# libcheck_glibc_version: the system's glibc version, or nothing when it can't be told.
libcheck_glibc_version() {
    local v
    if [[ -n "${LIBCHECK_GLIBC_HAVE:-}" ]]; then
        echo "$LIBCHECK_GLIBC_HAVE"
        return 0
    fi
    v="$(getconf GNU_LIBC_VERSION 2>/dev/null)"
    v="${v#glibc }"
    if [[ -z "$v" ]] && command -v ldd >/dev/null 2>&1; then
        v="$(ldd --version 2>/dev/null | head -n1)"
        v="${v##* }"
    fi
    [[ "$v" =~ ^[0-9]+(\.[0-9]+)+$ ]] && echo "$v"
    return 0
}

# libcheck_package MANAGER LIBRARY: the apt or pacman package that provides LIBRARY, for the
# libraries the AppImage takes from the system; nothing for any other library.
libcheck_package() {
    local so apt pacman
    while read -r so apt pacman; do
        [[ "$so" == "$2" ]] || continue
        if [[ "$1" == apt ]]; then echo "$apt"; else echo "$pacman"; fi
        return 0
    done <<'TABLE'
libxcb.so.1 libxcb1 libxcb
libxkbcommon.so.0 libxkbcommon0 libxkbcommon
libxkbcommon-x11.so.0 libxkbcommon-x11-0 libxkbcommon-x11
libwayland-client.so.0 libwayland-client0 wayland
libvulkan.so.1 libvulkan1 vulkan-icd-loader
libEGL.so.1 libegl1 libglvnd
libgcc_s.so.1 libgcc-s1 gcc-libs
TABLE
}

# libcheck_manager: the system's package manager (apt, dnf, zypper or pacman), from os-release;
# nothing for an unknown distribution.
libcheck_manager() {
    local f="${LIBCHECK_OS_RELEASE:-/etc/os-release}" ids id
    [[ -r "$f" ]] || return 0
    # shellcheck disable=SC1090
    ids="$(. "$f" 2>/dev/null; echo "${ID:-} ${ID_LIKE:-}")"
    for id in $ids; do
        case "$id" in
            fedora|rhel|centos|rocky|almalinux) echo dnf ;;
            opensuse*|suse|sles) echo zypper ;;
            debian|ubuntu) echo apt ;;
            arch) echo pacman ;;
            *) continue ;;
        esac
        return 0
    done
}

# libcheck_install_hint LIBRARY...: how to install the packages that provide LIBRARY..., as one
# or two sentences, for the system's package manager; nothing for an unknown distribution.
# dnf and zypper install by library name (rpm's provides); apt and pacman get the package names
# of the usual host libraries, and the command that finds the package for any other one.
libcheck_install_hint() {
    local mgr cmd lib pkg out=""
    local pkgs=() unknown=()
    mgr="$(libcheck_manager)"
    case "$mgr" in
        dnf|zypper)
            cmd="sudo $mgr install"
            for lib in "$@"; do cmd+=" '$lib()(64bit)'"; done
            echo "Install them with: $cmd."
            ;;
        apt|pacman)
            for lib in "$@"; do
                pkg="$(libcheck_package "$mgr" "$lib")"
                if [[ -z "$pkg" ]]; then
                    unknown+=("$lib")
                elif [[ " ${pkgs[*]} " != *" $pkg "* ]]; then
                    pkgs+=("$pkg")
                fi
            done
            if ((${#pkgs[@]})); then
                if [[ "$mgr" == apt ]]; then cmd="sudo apt install"; else cmd="sudo pacman -S"; fi
                out="Install them with: $cmd ${pkgs[*]}."
            fi
            if ((${#unknown[@]})); then
                if [[ "$mgr" == apt ]]; then cmd="apt-file search"; else cmd="pacman -F"; fi
                if ((${#pkgs[@]})); then lib="each of the others"; else lib="each of them"; fi
                out+="${out:+ }Find the package that provides $lib with:"
                for lib in "${unknown[@]}"; do out+=" $cmd $lib;"; done
                out="${out%;}."
            fi
            echo "$out"
            ;;
    esac
}

# libcheck_list ITEM...: "a, b, c".
libcheck_list() {
    local IFS=,
    local s="$*"
    echo "${s//,/, }"
}

# libcheck_plain TEXT: TEXT with nothing kdialog (Qt rich text) or a notification server (body
# markup) would take as markup: no &, < or >.
libcheck_plain() {
    local t="${1//&/and}"
    t="${t//</‹}"
    echo "${t//>/›}"
}

# libcheck_gvariant TEXT: TEXT as a GVariant text-format string, for gdbus.
libcheck_gvariant() {
    local t="${1//\\/\\\\}"
    t="${t//\'/\\\'}"
    t="${t//$'\n'/\\n}"
    printf "'%s'" "$t"
}

# libcheck_seen RC ERR: whether a dialog that ended with RC, having printed ERR, was seen: 0 or 1
# (closed with its button or otherwise), unless it could not open the display at all (zenity says so
# and exits 1).
libcheck_seen() {
    (($1 <= 1)) || return 1
    [[ "$2" != *"open display"* && "$2" != *"connect to display"* && "$2" != *"connect to X server"* ]]
}

# libcheck_dialog PROGRAM ARG...: runs a dialog program; 0 when it showed its message (see
# libcheck_seen). Its stderr goes to a temporary file read once it has ended: reading a pipe to its
# end would also wait for anything it left running.
libcheck_dialog() {
    local rc=0 errf err=""
    errf="$(mktemp 2>/dev/null)" || errf=""
    "$@" </dev/null >/dev/null 2>"${errf:-/dev/null}" || rc=$?
    if [[ -n "$errf" ]]; then
        err="$(<"$errf")"
        rm -f "$errf"
    fi
    libcheck_seen "$rc" "$err"
}

# libcheck_show TITLE TEXT: TEXT on stderr and, in a desktop session, in the first of these that
# shows it: zenity, kdialog (see libcheck_seen), a notification through notify-send or
# straight over D-Bus with gdbus (0: the desktop took it; Pop!_OS COSMIC has gdbus but no
# notify-send; expiry 0 keeps either up until it is closed, COSMIC drops others after seconds),
# and xmessage (its text folded, it does not wrap).
libcheck_show() {
    local plain
    printf '%s: %s\n' "$1" "$2" >&2
    [[ "${LIBCHECK_DIALOG:-}" == none ]] && return 0
    [[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]] || return 0
    plain="$(libcheck_plain "$2")"
    if command -v zenity >/dev/null 2>&1; then
        libcheck_dialog zenity --error --no-markup --title="$1" --text="$2" --width=460 && return 0
    fi
    if command -v kdialog >/dev/null 2>&1; then
        libcheck_dialog kdialog --error "$plain" --title "$1" && return 0
    fi
    if command -v notify-send >/dev/null 2>&1; then
        notify-send -u critical -t 0 "$1" "$plain" >/dev/null 2>&1 && return 0
    fi
    if command -v gdbus >/dev/null 2>&1; then
        gdbus call --session --dest org.freedesktop.Notifications --object-path /org/freedesktop/Notifications \
            --method org.freedesktop.Notifications.Notify "$(libcheck_gvariant "$1")" 'uint32 0' "''" \
            "$(libcheck_gvariant "$1")" "$(libcheck_gvariant "$plain")" '@as []' "{'urgency': <byte 2>}" 'int32 0' \
            >/dev/null 2>&1 && return 0
    fi
    if command -v xmessage >/dev/null 2>&1; then
        plain="$2"
        command -v fold >/dev/null 2>&1 && plain="$(printf '%s\n' "$2" | fold -s -w 80)"
        xmessage -center -title "$1" "$plain" >/dev/null 2>&1
    fi
    return 0
}

# libcheck_run NAME BINARY: returns 1 (after showing why) when BINARY can't start here.
libcheck_run() {
    local msg have hint more=""
    libcheck_scan "$2"
    if ((LIBCHECK_RC != 0)) && [[ -z "$LIBCHECK_GLIBC" ]] && ((${#LIBCHECK_LIBS[@]} + ${#LIBCHECK_OTHER[@]} + ${#LIBCHECK_SYMBOLS[@]} == 0)); then
        # ldd failed without saying why: ask the loader itself, and stop only if it names a problem.
        libcheck_scan "$2" loader
    fi
    LIBCHECK_NOTE=""
    [[ -n "$LIBCHECK_GLIBC" ]] || libcheck_runtime
    if [[ -n "$LIBCHECK_GLIBC" ]]; then
        have="$(libcheck_glibc_version)"
        msg="$1 can't start: it needs glibc $LIBCHECK_GLIBC or newer; this system has ${have:-an older one}."
        msg+=" This system is too old for this version of $1."
    elif ((${#LIBCHECK_LIBS[@]})); then
        msg="$1 can't start because these system libraries are missing: $(libcheck_list "${LIBCHECK_LIBS[@]}")."
        msg+=" $1 takes them from the system (windowing and graphics libraries every desktop normally has).$LIBCHECK_NOTE"
        hint="$(libcheck_install_hint "${LIBCHECK_LIBS[@]}")"
        if [[ -n "$hint" ]]; then
            msg+=" $hint Then start $1 again."
        else
            msg+=" Install the packages that provide them with your package manager, then start $1 again."
        fi
    elif ((${#LIBCHECK_SYMBOLS[@]} + ${#LIBCHECK_OTHER[@]})); then
        if ((${#LIBCHECK_SYMBOLS[@]} > 3)); then more=" and $((${#LIBCHECK_SYMBOLS[@]} - 3)) more"; fi
        if ((${#LIBCHECK_SYMBOLS[@]})); then
            msg="$1 can't start because the system's libraries are too old: they lack $(libcheck_list "${LIBCHECK_SYMBOLS[@]:0:3}")$more."
        else
            msg="$1 can't start because the system's libraries are too old: ${LIBCHECK_OTHER[0]}."
        fi
        msg+=" Update the system, then start $1 again."
    else
        return 0
    fi
    libcheck_show "$1" "$msg"
    return 1
}
# <<< libcheck

libcheck_run "Green Light" "$HERE/usr/bin/$APP" || exit 1

# >>> setup: the launch-time setup's helpers (tests/libcheck.rs runs them with the libcheck block).

# setup_running FILE: whether another copy of the app runs for this login. The copy that owns the
# single-instance socket holds an exclusive lock on FILE for as long as it runs (src/instance.rs).
# The probe takes a shared one, so two launches probing at once don't see each other as running;
# only flock's "conflict" (1) counts.
setup_running() {
    local rc=0
    [[ -e "$1" ]] && command -v flock >/dev/null 2>&1 || return 1
    flock -n -s "$1" true 2>/dev/null || rc=$?
    ((rc == 1))
}

# setup_pkexec ARG...: pkexec ARG..., in a session of its own (no controlling terminal) and with
# stdin from /dev/null. A desktop started from a text console (Hyprland from tty1) leaves its
# apps that console as controlling terminal; with no polkit agent running, pkexec would then ask
# for the password there, on a console nobody sees, and wait for ever. Without a terminal it
# fails at once instead ("Error creating textual authentication agent"), and a running agent
# shows its dialog as before (polkit goes by the login session, not the terminal).
setup_pkexec() {
    if command -v setsid >/dev/null 2>&1; then
        setsid -w pkexec "$@" </dev/null
    else
        pkexec "$@" </dev/null
    fi
}

# setup_reason RC ERRFILE: why the setup did not complete, from pkexec's exit status RC and what
# it printed (in ERRFILE). 126 is a dismissed prompt (COSMIC's agent, GNOME's); polkit-kde-agent
# reports its Cancel as "Not authorized", 127. The helper itself never exits with either.
setup_reason() {
    case $1 in
        126) echo "authorization was cancelled or refused" ;;
        127) if grep -q -e "No authentication agent found" -e "Error creating textual authentication agent" "$2"; then
                 echo "no polkit authentication agent is running (the program that shows the admin password dialog), so nothing was changed; start one (hyprpolkitagent on Hyprland, polkit-gnome (policykit-1-gnome on Debian/Ubuntu) or lxqt-policykit on sway or i3)"
             elif grep -q "Not authorized" "$2"; then
                 echo "authorization was cancelled or refused"
             else
                 echo "authorization was cancelled or refused, or pkexec could not run the setup helper"
             fi ;;
        *)   echo "installing system components failed (exit $1)" ;;
    esac
}

# setup_no_pkexec: why the setup can't run on a system without pkexec, with the command that
# installs it on this distribution.
setup_no_pkexec() {
    local how
    case "$(libcheck_manager)" in
        apt) how="sudo apt install pkexec" ;;
        dnf) how="sudo dnf install polkit" ;;
        zypper) how="sudo zypper install polkit" ;;
        pacman) how="sudo pacman -S polkit" ;;
    esac
    if [[ -n "$how" ]]; then
        echo "polkit's pkexec is not installed, so nothing was changed; install it with: $how (and a polkit authentication agent, if your desktop has none)"
    else
        echo "polkit's pkexec is not installed, so nothing was changed; install polkit (and a polkit authentication agent, if your desktop has none) with your package manager"
    fi
}
# <<< setup

export PATH="$HERE/usr/bin:$PATH"

# A copy already running: hand over to it straight away (the binary asks it to raise its window
# and exits), without the setup and its password prompt.
if [[ -n "${XDG_RUNTIME_DIR:-}" ]] && setup_running "$XDG_RUNTIME_DIR/$APP.running"; then
    exec "$HERE/usr/bin/$APP" "$@"
fi

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
    reason=""
    if ! command -v pkexec >/dev/null 2>&1; then
        reason="$(setup_no_pkexec)"
    else
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
            setup_pkexec "$DST_HELPER" "$STAGE" 2>"$ERRF" || rc=$?
        else
            # $BASH: this script's own interpreter, as the absolute path pkexec needs.
            setup_pkexec "$BASH" "$STAGE/green-light-setup" "$STAGE" 2>"$ERRF" || rc=$?
        fi
        rm -rf "$STAGE"
        cat "$ERRF" >&2
        [[ $rc -eq 0 ]] || reason="$(setup_reason "$rc" "$ERRF")"
        rm -f "$ERRF"
    fi

    # The reason is handed to the app (GREEN_LIGHT_SETUP_FAILURE), which shows it in a banner and
    # in the messages of the actions that need the setup.
    if [[ -n "$reason" ]]; then
        echo "Green Light: setup did not complete: $reason." >&2
        export GREEN_LIGHT_SETUP_FAILURE="$reason"
    fi
fi

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
