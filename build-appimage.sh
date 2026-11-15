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

# ── Bundled GTK 4 / libadwaita runtime ───────────────────────────────
# The AppImage carries GTK 4, libadwaita and everything they link, taken from the build machine
# (the release runner, Ubuntu 24.04, glibc 2.39), so it starts on desktops that ship neither
# (a stock KDE or Hyprland install). What stays on the host is the AppImage project's excludelist
# (github.com/AppImageCommunity/pkg2appimage, file `excludelist` at commit 15a64c2, 2024-11-03):
# glibc, libstdc++/libgcc_s, the GL/EGL/GLX/glvnd and DRM/GBM stack, libX11/libxcb, Wayland's
# client library, fontconfig/freetype/harfbuzz/fribidi, zlib, expat and the rest of that list,
# plus our own additions below: the Vulkan loader (its drivers and layers are the host's) and
# Wayland's other client libraries (they pair with the host's libwayland-client and Mesa).
# Nothing is found through LD_LIBRARY_PATH: every bundled ELF file gets an rpath relative to
# itself ($ORIGIN), so the programs the app starts never load a bundled library.
# The bundle is the fallback: AppRun's gtk_choice runs the plain binary on the system's GTK 4 and
# libadwaita whenever they are there and new enough, and the rpath-patched copy otherwise.
BUNDLE_EXCLUDE=(
    ld-linux.so.2 ld-linux-x86-64.so.2 libanl.so.1 libBrokenLocale.so.1 libcidn.so.1 libc.so.6
    libdl.so.2 libm.so.6 libmvec.so.1 libnss_compat.so.2 libnss_dns.so.2 libnss_files.so.2
    libnss_hesiod.so.2 libnss_nisplus.so.2 libnss_nis.so.2 libpthread.so.0 libresolv.so.2
    librt.so.1 libthread_db.so.1 libutil.so.1 libstdc++.so.6 libGL.so.1 libEGL.so.1
    libGLdispatch.so.0 libGLX.so.0 libOpenGL.so.0 libdrm.so.2 libglapi.so.0 libgbm.so.1
    libxcb.so.1 libX11.so.6 libX11-xcb.so.1 libwayland-client.so.0 libasound.so.2
    libfontconfig.so.1 libfreetype.so.6 libharfbuzz.so.0 libcom_err.so.2 libexpat.so.1
    libgcc_s.so.1 libgpg-error.so.0 libICE.so.6 libSM.so.6 libusb-1.0.so.0 libuuid.so.1
    libz.so.1 libjack.so.0 libpipewire-0.3.so.0 libxcb-dri3.so.0 libxcb-dri2.so.0
    libfribidi.so.0 libgmp.so.10
    # Ours:
    libvulkan.so.1 libwayland-egl.so.1 libwayland-cursor.so.0 libwayland-server.so.0
)

bundle_excluded() { [[ " ${BUNDLE_EXCLUDE[*]} " == *" $1 "* ]]; }

# bundle_ldd FILE: prints "soname path" for each library FILE loads (its whole closure, as ldd
# resolves it right now); fails, naming them, when one cannot be found.
bundle_ldd() {
    local out
    out="$(ldd "$1")" || return 1
    if grep -q '=> not found' <<<"$out"; then
        echo "==> ERROR: $1 needs libraries this machine does not have:" >&2
        grep '=> not found' <<<"$out" >&2
        return 1
    fi
    awk '$2 == "=>" && $3 ~ /^\// { print $1, $3 }' <<<"$out"
}

# bundle_gtk_runtime APPDIR BINARY...: puts the GTK 4 + libadwaita runtime the given binaries
# (paths under APPDIR/usr/bin, already copied there) need into APPDIR/usr:
#   lib/                                the libraries (rpath $ORIGIN)
#   lib/gdk-pixbuf-2.0/2.10.0/          image loaders (SVG icons) and their loaders.cache
#   lib/gio/modules/                    the dconf GSettings backend, so settings live in the
#                                       host's dconf like every other GTK app's
#   lib/gtk-4.0/4.0.0/immodules/        the IBus input method module (X11 sessions; on Wayland
#                                       GTK speaks to the input method itself)
#   share/glib-2.0/schemas/             GTK's and the desktop's GSettings schemas, compiled
# AppRun's bundle_env block points GTK at them (GTK_EXE_PREFIX, GDK_PIXBUF_MODULE_FILE,
# GIO_MODULE_DIR, GSETTINGS_SCHEMA_DIR), and the app gives every program it starts the host's
# values of those back.
bundle_gtk_runtime() {
    local appdir="$1"; shift
    local usr="$appdir/usr" syslib pixver pixdir loaders giomods immods f soname path pkg
    local work="$appdir/.."
    syslib="$(pkg-config --variable=libdir gtk4)"
    pixver="$(pkg-config --variable=gdk_pixbuf_binary_version gdk-pixbuf-2.0)"
    pixdir="$(pkg-config --variable=gdk_pixbuf_moduledir gdk-pixbuf-2.0)"
    loaders="$usr/lib/gdk-pixbuf-2.0/$pixver/loaders"
    giomods="$usr/lib/gio/modules"
    immods="$usr/lib/gtk-4.0/4.0.0/immodules"
    local modules=("$pixdir/libpixbufloader-svg.so" "$syslib/gio/modules/libdconfsettings.so"
        "$syslib/gtk-4.0/4.0.0/immodules/libim-ibus.so")
    for f in "${modules[@]}" "$syslib/gdk-pixbuf-2.0/gdk-pixbuf-query-loaders"; do
        [[ -e "$f" ]] && continue
        echo "==> ERROR: $f is missing (packages librsvg2-common, dconf-gsettings-backend, ibus-gtk4, libgdk-pixbuf2.0-bin)" >&2
        return 1
    done
    for f in patchelf glib-compile-schemas gio-querymodules dpkg; do
        command -v "$f" >/dev/null 2>&1 || { echo "==> ERROR: $f is not installed" >&2; return 1; }
    done

    echo "==> Bundling GTK 4 and libadwaita"
    mkdir -p "$usr/lib" "$loaders" "$giomods" "$immods" "$usr/share/glib-2.0/schemas"
    # Only the SVG loader: Ubuntu builds PNG and JPEG into libgdk_pixbuf itself, and the app shows
    # no other image formats (the check below fails the build if that ever changes).
    for f in png jpeg; do
        if [[ -e "$pixdir/libpixbufloader-$f.so" ]]; then
            echo "==> ERROR: gdk-pixbuf's $f loader is a module here, not built in; bundle it too" >&2
            return 1
        fi
    done
    cp "$pixdir/libpixbufloader-svg.so" "$loaders/"
    cp "$syslib/gio/modules/libdconfsettings.so" "$giomods/"
    cp "$syslib/gtk-4.0/4.0.0/immodules/libim-ibus.so" "$immods/"
    printf '%s\n' "${modules[@]}" > "$work/bundle-sources"

    # The libraries: the ldd closure of the binaries and the modules, minus the excludelist.
    local elves=()
    for f in "$@"; do elves+=("$usr/bin/$f"); done
    elves+=("$loaders"/*.so "$giomods"/*.so "$immods"/*.so)
    for f in "${elves[@]}"; do
        bundle_ldd "$f" > "$work/bundle-closure" || return 1
        while read -r soname path; do
            bundle_excluded "$soname" && continue
            if [[ ! -e "$usr/lib/$soname" ]]; then
                cp -L "$path" "$usr/lib/$soname"
                echo "$path" >> "$work/bundle-sources"
            fi
        done < "$work/bundle-closure"
    done

    # rpath, not LD_LIBRARY_PATH. DT_RPATH (--force-rpath), not DT_RUNPATH, so that a
    # LD_LIBRARY_PATH the user has set cannot swap in a host library the bundle wasn't built with.
    # The binaries' rpath also names the loader directory: loaders.cache lists bare file names,
    # which dlopen() finds there wherever the AppImage is mounted.
    local rel
    for f in "$@"; do
        rel="$(realpath --relative-to="$(dirname "$usr/bin/$f")" "$usr/lib")"
        patchelf --force-rpath --set-rpath "\$ORIGIN/$rel:\$ORIGIN/$rel/gdk-pixbuf-2.0/$pixver/loaders" "$usr/bin/$f"
    done
    # shellcheck disable=SC2016 # $ORIGIN is for the dynamic linker, not the shell
    for f in "$usr/lib"/*.so*; do patchelf --force-rpath --set-rpath '$ORIGIN' "$f"; done
    # shellcheck disable=SC2016
    for f in "$loaders"/*.so "$immods"/*.so; do patchelf --force-rpath --set-rpath '$ORIGIN/../../..' "$f"; done
    # shellcheck disable=SC2016
    for f in "$giomods"/*.so; do patchelf --force-rpath --set-rpath '$ORIGIN/../..' "$f"; done

    # Check: everything bundled now loads its libraries from the AppDir, or from the host only
    # when the excludelist says so.
    local inside bad=0
    inside="$(realpath "$usr/lib")"
    for f in "${elves[@]}" "$usr/lib"/*.so*; do
        bundle_ldd "$f" > "$work/bundle-closure" || return 1
        while read -r soname path; do
            bundle_excluded "$soname" && continue
            [[ "$(realpath "$path")" == "$inside/"* ]] && continue
            echo "==> ERROR: $f loads $soname from $path, not from the AppDir" >&2
            bad=1
        done < "$work/bundle-closure"
    done
    rm -f "$work/bundle-closure"
    [[ $bad -eq 0 ]] || return 1

    local cache
    cache="$("$syslib/gdk-pixbuf-2.0/gdk-pixbuf-query-loaders" "$loaders"/*.so)"
    cache="${cache//\"$(realpath "$loaders")\//\"}"
    cache="${cache//\"$loaders\//\"}"
    printf '%s\n' "$cache" > "$loaders/../loaders.cache"
    if grep -q '^"/.*\.so"$' "$loaders/../loaders.cache"; then
        echo "==> ERROR: loaders.cache still names absolute paths" >&2
        return 1
    fi
    gio-querymodules "$giomods"

    # GSettings schemas: GTK's own (its file, colour and emoji choosers abort without them) and the
    # desktop ones GTK reads when there is no settings portal.
    cp /usr/share/glib-2.0/schemas/org.gtk.gtk4.*.gschema.xml \
       /usr/share/glib-2.0/schemas/org.gnome.desktop.*.xml "$usr/share/glib-2.0/schemas/"
    glib-compile-schemas --strict "$usr/share/glib-2.0/schemas"

    # The bundled libraries' copyright and licence notices, from their packages.
    local docs="$usr/share/doc/bundled-libraries" pkgs=()
    mkdir -p "$docs"
    while read -r path; do
        pkg="$(dpkg -S "$path" 2>/dev/null || dpkg -S "$(realpath "$path")" 2>/dev/null \
            || dpkg -S "${path#/usr}" 2>/dev/null || true)"
        pkg="${pkg%%:*}"
        if [[ -z "$pkg" || ! -f "/usr/share/doc/$pkg/copyright" ]]; then
            echo "==> ERROR: no copyright notice found for $path" >&2
            return 1
        fi
        cp "/usr/share/doc/$pkg/copyright" "$docs/$pkg.copyright"
        [[ " ${pkgs[*]} " == *" $pkg "* ]] || pkgs+=("$pkg")
    done < <(sort -u "$work/bundle-sources")
    # The licence texts those files point to in /usr/share/common-licenses (Debian's directory,
    # which other distributions don't have).
    local lic
    mkdir -p "$docs/common-licenses"
    while read -r lic; do
        lic="${lic%.}"
        if [[ ! -f "$lic" ]]; then
            echo "==> ERROR: $lic, named by a bundled copyright file, is missing" >&2
            return 1
        fi
        cp -L "$lic" "$docs/common-licenses/"
    done < <(grep -oh '/usr/share/common-licenses/[A-Za-z0-9][A-Za-z0-9._+-]*' "$docs"/*.copyright | sort -u)
    {
        echo "The libraries and modules in usr/lib are unmodified files from these $(. /etc/os-release && echo "$PRETTY_NAME") packages"
        echo "(source package and version, then the binary package). Each one's copyright file is <package>.copyright"
        echo "here; the licence texts they name under /usr/share/common-licenses/ are in common-licenses/."
        echo
        dpkg-query -W -f '${source:Package} ${source:Version}: ${Package} ${Version}\n' "${pkgs[@]}" | sort -u
    } > "$docs/PACKAGES"
    rm -f "$work/bundle-sources"
    echo "==> Bundled $(find "$usr/lib" -name '*.so*' | wc -l) libraries and modules ($(du -sh "$usr/lib" | cut -f1))"
}

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
        pkg-config zsync wget file desktop-file-utils \
        patchelf librsvg2-common dconf-gsettings-backend ibus-gtk4 libgdk-pixbuf2.0-bin gsettings-desktop-schemas
fi

for tool in wget file desktop-file-validate patchelf; do
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

# Two copies of the binary; AppRun's gtk_choice picks one at launch. usr/bin/$APP stays as cargo
# built it and loads the system's GTK 4 and libadwaita, like releases before the bundle;
# usr/bin/bundled/$APP gets the rpath into the bundled copy.
mkdir -p "$APPDIR/usr/bin/bundled"
cp "${CARGO_TARGET_DIR:-target}/release/$APP" "$APPDIR/usr/bin/"
cp "${CARGO_TARGET_DIR:-target}/release/$APP" "$APPDIR/usr/bin/bundled/"
bundle_gtk_runtime "$APPDIR" "bundled/$APP"
if [[ -n "$(patchelf --print-rpath "$APPDIR/usr/bin/$APP")" ]]; then
    echo "==> ERROR: usr/bin/$APP must load the system's libraries, but it has an rpath" >&2
    exit 1
fi
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

# >>> libcheck: before any setup, make sure the app can start. `ldd -r` on the chosen binary
# (gtk_choice) must report no missing library, no missing symbol version (a glibc older than the
# build's 2.39, for one) and no undefined symbol. On the bundled path GTK 4 and libadwaita come
# with the AppImage and are found through the binary's rpath (ldd follows it), so what can be
# missing is only what the AppImage deliberately takes from the system: glibc, the graphics
# stack (OpenGL/EGL, DRM, the Vulkan loader), X11/xcb and Wayland client libraries, fonts
# (fontconfig, freetype, harfbuzz, fribidi), zlib and expat.
# LIBCHECK_LDD, LIBCHECK_OS_RELEASE, LIBCHECK_GLIBC_HAVE and LIBCHECK_DIALOG=none exist for the tests.

# libcheck_scan BINARY: sorts what `ldd -r BINARY` reports into LIBCHECK_LIBS (libraries not
# found), LIBCHECK_GLIBC (the newest GLIBC_x.y version that is needed but missing),
# LIBCHECK_OTHER (other "not found" lines), LIBCHECK_SYMBOLS (undefined symbols) and LIBCHECK_RC
# (ldd's exit status). Without ldd, the dynamic loader is asked directly, as ldd -r does.
libcheck_scan() {
    local out line rc=0 re='version .GLIBC_([0-9]+(\.[0-9]+)*). not found'
    LIBCHECK_LIBS=(); LIBCHECK_GLIBC=""; LIBCHECK_OTHER=(); LIBCHECK_SYMBOLS=()
    if [[ -n "${LIBCHECK_LDD:-}" ]]; then
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
        elif [[ "$line" == "undefined symbol: "* ]]; then
            line="${line#undefined symbol: }"
            LIBCHECK_SYMBOLS+=("${line%%[[:space:]]*}")
        fi
    done <<<"$out"
    LIBCHECK_RC=$rc
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
libvulkan.so.1 libvulkan1 vulkan-icd-loader
libEGL.so.1 libegl1 libglvnd
libGL.so.1 libgl1 libglvnd
libfontconfig.so.1 libfontconfig1 fontconfig
libfreetype.so.6 libfreetype6 freetype2
libharfbuzz.so.0 libharfbuzz0b harfbuzz
libX11.so.6 libx11-6 libx11
libxcb.so.1 libxcb1 libxcb
libwayland-client.so.0 libwayland-client0 wayland
libz.so.1 zlib1g zlib
libexpat.so.1 libexpat1 expat
TABLE
}

# libcheck_install_hint LIBRARY...: how to install the packages that provide LIBRARY..., as one
# or two sentences, for the system's package manager; nothing for an unknown distribution.
# dnf and zypper install by library name (rpm's provides); apt and pacman get the package names
# of the usual host libraries, and the command that finds the package for any other one.
libcheck_install_hint() {
    local f="${LIBCHECK_OS_RELEASE:-/etc/os-release}" ids id mgr="" cmd lib pkg out=""
    local pkgs=() unknown=()
    [[ -r "$f" ]] || return 0
    # shellcheck disable=SC1090
    ids="$(. "$f" 2>/dev/null; echo "${ID:-} ${ID_LIKE:-}")"
    for id in $ids; do
        case "$id" in
            fedora|rhel|centos|rocky|almalinux) mgr=dnf ;;
            opensuse*|suse|sles) mgr=zypper ;;
            debian|ubuntu) mgr=apt ;;
            arch) mgr=pacman ;;
            *) continue ;;
        esac
        break
    done
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
                out+="${out:+ }Find the package that provides $(libcheck_list "${unknown[@]}") with: $cmd <library>."
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

# libcheck_run NAME BINARY [NOTE]: returns 1 (after showing why) when BINARY can't start here.
# NOTE replaces the sentence that says what the AppImage brings and what it takes from the system.
libcheck_run() {
    local msg have hint more=""
    libcheck_scan "$2"
    if [[ -n "$LIBCHECK_GLIBC" ]]; then
        have="$(libcheck_glibc_version)"
        msg="$1 can't start: it needs glibc $LIBCHECK_GLIBC or newer; this system has ${have:-an older one}."
        msg+=" This system is too old for this version of $1."
    elif ((${#LIBCHECK_LIBS[@]})); then
        msg="$1 can't start because these system libraries are missing: $(libcheck_list "${LIBCHECK_LIBS[@]}")."
        msg+=" ${3:-$1 brings GTK 4 and libadwaita with it, but takes these from the system (graphics, windowing and font libraries every desktop normally has).}"
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
        msg+=" ${3:-Update the system, then start $1 again.}"
    elif ((LIBCHECK_RC != 0)); then
        msg="$1 can't start: checking its libraries with ldd failed (exit $LIBCHECK_RC)."
    else
        return 0
    fi
    libcheck_show "$1" "$msg"
    return 1
}
# <<< libcheck

# >>> bundle_env: point the bundled GTK 4 at its own modules, image loaders and schemas
# (build-appimage.sh, bundle_gtk_runtime). The names go in APPIMAGE_BUNDLE_ENV and each one's
# earlier value in APPIMAGE_ORIG_<NAME> (left unset when it had none), so the app can start other
# programs with the environment they would have had without the AppImage.
bundle_env() {
    local var
    export APPIMAGE_BUNDLE_ENV="GTK_EXE_PREFIX GDK_PIXBUF_MODULE_FILE GIO_MODULE_DIR GSETTINGS_SCHEMA_DIR"
    for var in $APPIMAGE_BUNDLE_ENV; do
        if [[ -n "${!var+set}" ]]; then
            export "APPIMAGE_ORIG_$var=${!var}"
        else
            unset "APPIMAGE_ORIG_$var"
        fi
    done
    export GTK_EXE_PREFIX="$1/usr"
    export GDK_PIXBUF_MODULE_FILE="$1/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
    export GIO_MODULE_DIR="$1/usr/lib/gio/modules"
    export GSETTINGS_SCHEMA_DIR="$1/usr/share/glib-2.0/schemas${GSETTINGS_SCHEMA_DIR:+:$GSETTINGS_SCHEMA_DIR}"
}
# <<< bundle_env

# >>> gtk_choice: which GTK 4 and libadwaita the app runs on. usr/bin/APP is the plain build and
# loads them from the system, like releases before the bundle; usr/bin/bundled/APP is the same
# program with an rpath into the AppImage's own copy (bundle_gtk_runtime). The system's copy wins
# when the plain binary links against it completely: `ldd -r` reports no missing library and no
# undefined symbol (a libadwaita older than 1.5 or a GTK older than 4.12 lacks symbols the app
# uses). Without ldd nothing can be checked, so the bundled copy is used. The variable VAR
# (host or bundled) overrides the check, for support. One line on stderr says which was taken.
# gtk_choice NAME VAR HOST-BINARY BUNDLED-BINARY sets GTK_CHOICE (host or bundled), GTK_BIN (the
# binary to run) and GTK_NOTE (libcheck_run's note: empty on the bundled path).
# Only shell builtins here, so that the check also works when PATH has no ldd at all.
gtk_choice() {
    local name="$1" var="$2" value out line why="" rc=0 which hint="" forced=0
    value="${!var:-}"
    GTK_CHOICE=""
    case "$value" in
        host|bundled) GTK_CHOICE="$value"; forced=1; why="set by $var=$value" ;;
        "") ;;
        *) printf '%s: ignoring %s=%s (it takes host or bundled)\n' "$name" "$var" "$value" >&2 ;;
    esac
    if [[ -z "$GTK_CHOICE" ]]; then
        if [[ -n "${LIBCHECK_LDD:-}" ]]; then
            out="$("$LIBCHECK_LDD" -r "$3" 2>&1)" || rc=$?
        elif command -v ldd >/dev/null 2>&1; then
            out="$(ldd -r "$3" 2>&1)" || rc=$?
        else
            GTK_CHOICE=bundled
            why="ldd is not installed, so the system's copy can't be checked"
        fi
    fi
    if [[ -z "$GTK_CHOICE" ]]; then
        while IFS= read -r line; do
            line="${line#"${line%%[![:space:]]*}"}"
            case "$line" in
                *"=> not found"*) why="the system has no ${line%% *}" ;;
                "undefined symbol: "*)
                    line="${line#undefined symbol: }"
                    why="the system's libraries lack ${line%%[[:space:]]*}" ;;
                *"not found"*) why="$line" ;;
                *) continue ;;
            esac
            break
        done <<<"$out"
        if [[ -z "$why" && $rc -ne 0 ]]; then
            why="ldd could not check the system's copy (exit $rc)"
        fi
        if [[ -n "$why" ]]; then GTK_CHOICE=bundled; else GTK_CHOICE=host; fi
    fi
    if [[ $GTK_CHOICE == host ]]; then
        GTK_BIN="$3"
        GTK_NOTE="$var=host makes $name use the system's GTK 4 and libadwaita, which needs these from the system; unset it to use the copy $name brings."
        # No bundle environment on this path, so the app has nothing to restore for its children.
        unset APPIMAGE_BUNDLE_ENV
        which="system's"
        [[ $forced -eq 1 ]] || hint="$var=bundled uses the bundled copy"
    else
        GTK_BIN="$4"
        GTK_NOTE=""
        which="bundled"
        [[ $forced -eq 1 ]] || hint="$var=host forces the system's"
    fi
    printf '%s: using the %s GTK 4 and libadwaita%s%s\n' "$name" "$which" "${why:+: $why}" "${hint:+ ($hint)}" >&2
}
# <<< gtk_choice

gtk_choice "Green Light" GREEN_LIGHT_GTK "$HERE/usr/bin/$APP" "$HERE/usr/bin/bundled/$APP"
libcheck_run "Green Light" "$GTK_BIN" "$GTK_NOTE" || exit 1

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

if [[ $GTK_CHOICE == bundled ]]; then bundle_env "$HERE"; fi
export PATH="$HERE/usr/bin:$PATH"
exec "$GTK_BIN" "$@"
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
