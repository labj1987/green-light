#!/usr/bin/env bash
# Tests for scripts/privileged-install.sh.
#
# The real script runs as the current, unprivileged user inside bubblewrap:
# /etc, /var, /boot, /tmp and the kernel module tree are scratch directories,
# and PATH holds only stub commands (apt-get, dpkg-query, dnf, the NVIDIA
# installer, ...) plus the handful of coreutils the script needs. Nothing on
# the real system is read or written, and nothing runs as root.
#
# Usage: scripts/tests/privileged-install-test.sh   (needs bwrap)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/../privileged-install.sh"
command -v bwrap >/dev/null 2>&1 || { echo "error: bwrap (bubblewrap) is required" >&2; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/gl-script-test.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

KVER="6.99.0-test"
ME_UID="$(id -u)"
ME_GID="$(id -g)"
PASS=0
FAIL=0
CASE=""
T=""

# Real tools the script uses; everything else on PATH is a stub.
REAL_TOOLS=(bash cat chmod cp cut date grep head install ls mkdir mktemp mv rm stat sha256sum tail touch)

stub() { # <name> <body>   (body runs with "$@" = the stub's arguments)
    printf '#!/bin/bash\necho "%s $*" >> "$GLT/calls"\n%s\n' "$1" "$2" > "$T/bin/$1"
    chmod 755 "$T/bin/$1"
}
unstub() { rm -f "$T/bin/$1"; }

# A fresh sandbox: an apt machine with headers, gcc, make and dkms, and one
# installed distro driver package that is held.
new_case() {
    CASE="$1"
    T="$WORK/case-$((PASS + FAIL))"   # no case name: a ":" would split PATH
    mkdir -p "$T"/{bin,etc/modprobe.d,etc/apt/preferences.d,etc/dnf,etc/dkms,var/log,var/tmp,var/lib,boot,modules/"$KVER"/build/include/generated,nvidia,home}
    local tool
    for tool in "${REAL_TOOLS[@]}"; do ln -s "$(readlink -f "$(command -v "$tool")")" "$T/bin/$tool"; done
    touch "$T/modules/$KVER/build/Makefile"
    echo '#define CONFIG_CC_IS_GCC 1' > "$T/modules/$KVER/build/include/generated/autoconf.h"
    : > "$T/calls"
    printf 'me:x:%s:%s::/home/me:/bin/bash\nroot:x:0:0::/root:/bin/bash\n' "$ME_UID" "$ME_GID" > "$T/passwd"
    printf 'me:x:%s:\nroot:x:0:\n' "$ME_GID" > "$T/group"
    printf 'ii |nvidia-driver-595|595.84-0ubuntu1\nhi |libnvidia-gl-595:i386|595.84-0ubuntu1\nii |nvidia-container-toolkit|1.17.0-1\nrc |nvidia-dkms-570|570.1-1\n' > "$T/dpkg"
    printf 'libnvidia-gl-595:i386\nnvidia-container-toolkit\nbash\n' > "$T/held"

    stub uname 'echo '"$KVER"
    stub getent 'case "$1" in
  passwd) if [[ -n "${2:-}" ]]; then grep -E "^[^:]*:[^:]*:$2:" "$GLT/passwd" || exit 2; else cat "$GLT/passwd"; fi ;;
  group) grep -E "^[^:]*:[^:]*:$2:" "$GLT/group" || exit 2 ;;
esac'
    stub apt-get 'if [[ "$1" == purge && -e "$GLT/fail-purge" ]]; then exit 100; fi
if [[ "$1" == install && "$*" == *"="* && -e "$GLT/fail-exact-reinstall" ]]; then exit 100; fi
if [[ "$1" == install && "$*" == *nvidia* && -e "$GLT/fail-reinstall" ]]; then exit 100; fi
exit 0'
    stub apt-mark 'if [[ "$1" == showhold ]]; then cat "$GLT/held"; fi; exit 0'
    stub dpkg-query 'cat "$GLT/dpkg"; exit 1'
    stub update-alternatives 'exit 0'
    stub update-initramfs 'if [[ -e "$GLT/fail-initramfs" ]]; then exit 1; fi; touch "/boot/initrd.img-$3"'
    stub lsinitramfs 'echo usr/lib/modprobe.d/blacklist-nouveau.conf'
    stub dkms 'if [[ "$1" == status ]]; then echo "nvidia/595.84, '"$KVER"', x86_64: installed"; fi'
    stub mokutil 'echo "SecureBoot disabled"'
    stub modinfo 'exit 1'
    stub gcc 'exit 0'
    stub make 'exit 0'

    RUN="$T/home/NVIDIA-Linux-x86_64-595.84.run"
    cat > "$RUN" <<'FAKE'
#!/bin/bash
echo "installer $*" >> "$GLT/calls"
if [[ "$1" == --check ]]; then exit 0; fi
if [[ -e "$GLT/installer-rc" ]]; then exit "$(cat "$GLT/installer-rc")"; fi
exit 0
FAKE
    chmod 644 "$RUN"
}

# Run the script in the sandbox. Sets RC and OUT.
run_script() {
    local extra=()
    # The host's /usr/lib/nvidia (if any) is replaced too, so the stale-marker
    # removal only ever sees the scratch copy.
    if [[ -d /usr/lib/nvidia ]]; then extra+=(--bind "$T/nvidia" /usr/lib/nvidia); fi
    RC=0
    OUT="$(bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp \
        --tmpfs /etc --ro-bind-try /etc/ld.so.cache /etc/ld.so.cache \
        --bind "$T/etc/modprobe.d" /etc/modprobe.d \
        --bind "$T/etc/apt" /etc/apt --bind "$T/etc/dnf" /etc/dnf --bind "$T/etc/dkms" /etc/dkms \
        --tmpfs /var --bind "$T/var/log" /var/log --bind "$T/var/tmp" /var/tmp --bind "$T/var/lib" /var/lib \
        --bind "$T/boot" /boot --bind "$T/modules" /usr/lib/modules \
        "${extra[@]}" --bind "$T" "$T" \
        --clearenv --setenv PATH "$T/bin" --setenv GLT "$T" --setenv PKEXEC_UID "$ME_UID" \
        --setenv LANG C.UTF-8 \
        "$T/bin/bash" "$SCRIPT" "$@" 2>&1)" || RC=$?
}

sha() { sha256sum "$1" | cut -d' ' -f1; }
run_default() { run_script "$RUN" "$(sha "$RUN")" "$@"; }

ok() { :; }
fail() {
    echo "FAIL [$CASE]: $*"
    echo "---- output"; echo "$OUT"; echo "---- calls"; cat "$T/calls"; echo "----"
    CASE_FAILED=1
}
expect_rc() { [[ "$RC" == "$1" ]] || fail "exit code $RC, expected $1"; }
expect_out() { grep -qF -- "$1" <<<"$OUT" || fail "output lacks: $1"; }
expect_call() { grep -qxF -- "$1" "$T/calls" || fail "no call: $1"; }
expect_call_like() { grep -qE -- "$1" "$T/calls" || fail "no call matching: $1"; }
expect_no_call_like() { ! grep -qE -- "$1" "$T/calls" || fail "unexpected call matching: $1"; }
expect_file() { [[ -f "$1" ]] || fail "missing file: $1"; }
expect_no_file() { [[ ! -e "$1" ]] || fail "file should not exist: $1"; }

begin() { new_case "$1"; CASE_FAILED=0; }
end() {
    if [[ $CASE_FAILED -eq 0 ]]; then PASS=$((PASS + 1)); echo "ok   $CASE"; else FAIL=$((FAIL + 1)); fi
}

# ── Cases ──────────────────────────────────────────────────────────────

begin "apt install succeeds"
run_default --dkms
expect_rc 0
expect_call "apt-mark unhold libnvidia-gl-595:i386"
expect_call "apt-get purge -y nvidia-driver-595 libnvidia-gl-595:i386"
expect_no_call_like "purge.*nvidia-container-toolkit"
expect_no_call_like "purge.*nvidia-dkms-570"
expect_call_like "^installer .*--dkms"
expect_call "update-initramfs -u -k $KVER"
expect_file "$T/etc/modprobe.d/blacklist-nouveau.conf"
expect_no_file "$T/etc/apt/preferences.d/green-light-nvidia"
expect_out "Verify: all checks passed"
grep -q "==== NVIDIA driver install started" "$T/var/log/green-light.log" || fail "log file not written"
end

begin "group-writable file in the owner's private group is accepted"
chmod 664 "$RUN"
run_default
expect_rc 0
end

begin "group-writable file in a shared group is refused before any change"
chmod 664 "$RUN"
printf 'me:x:%s:bob\n' "$ME_GID" > "$T/group"
run_default
expect_rc 2
expect_out "other members of group"
expect_no_call_like "^(apt-get|apt-mark|installer)"
end

begin "group-writable file whose group is another account's primary group is refused"
chmod 664 "$RUN"
printf 'bob:x:4242:%s::/home/bob:/bin/bash\n' "$ME_GID" >> "$T/passwd"
run_default
expect_rc 2
end

begin "world-writable file is refused"
chmod 646 "$RUN"
run_default
expect_rc 2
expect_out "can be changed by every user"
end

begin "symlink, relative path, bad checksum argument and unknown flag are usage errors"
ln -s "$RUN" "$T/home/link.run"
run_script "$T/home/link.run" "$(sha "$RUN")"
expect_rc 2
expect_out "Refusing symlink"
run_script "NVIDIA.run" "$(sha "$RUN")"
expect_rc 2
run_script "$RUN" "abc"
expect_rc 2
run_default --hold
expect_rc 2
expect_out "Unknown argument: --hold"
expect_no_call_like "^(apt-get|installer)"
end

begin "checksum mismatch changes nothing"
run_script "$RUN" "$(printf '0%.0s' {1..64})"
expect_rc 1
expect_out "SHA256 mismatch"
expect_no_call_like "^(apt-get purge|apt-mark unhold|installer --silent)"
end

begin "missing kernel headers abort before anything is removed"
rm "$T/modules/$KVER/build/Makefile"
run_default --dkms
expect_rc 1
expect_out "can't be built for kernel $KVER: missing the kernel headers"
expect_out "No changes made"
expect_no_call_like "^(apt-get purge|apt-mark unhold|apt-mark hold)"
expect_no_call_like "^installer --silent"
expect_no_file "$T/etc/modprobe.d/blacklist-nouveau.conf"
end

begin "missing compiler and dkms abort before anything is removed"
unstub gcc
unstub dkms
run_default --dkms
expect_rc 1
expect_out "missing gcc, dkms"
expect_no_call_like "^apt-get purge"
end

begin "clang-built kernel needs clang and ld.lld"
printf '#define CONFIG_CC_IS_CLANG 1\n#define CONFIG_LD_IS_LLD 1\n' > "$T/modules/$KVER/build/include/generated/autoconf.h"
run_default
expect_rc 1
expect_out "missing clang, ld.lld"
end

begin "installer failure rolls everything back"
echo 1 > "$T/installer-rc"
printf 'options nouveau modeset=1\n' > "$T/etc/modprobe.d/nvidia-drm-modeset.conf"
touch "$T/nvidia/alternate-install-present"
run_default --dkms
expect_rc 3
expect_out "NVIDIA installer exited with code 1"
expect_call "apt-get install -y --allow-downgrades nvidia-driver-595=595.84-0ubuntu1 libnvidia-gl-595:i386=595.84-0ubuntu1"
expect_call "apt-mark hold libnvidia-gl-595:i386"
expect_no_file "$T/etc/modprobe.d/blacklist-nouveau.conf"
grep -qx 'options nouveau modeset=1' "$T/etc/modprobe.d/nvidia-drm-modeset.conf" \
    || fail "modeset config not restored"
if [[ -d /usr/lib/nvidia ]]; then expect_file "$T/nvidia/alternate-install-present"; fi
[[ "$(grep -c "^update-initramfs" "$T/calls")" == 1 ]] || fail "initramfs not rebuilt once on rollback"
expect_out "Restored the previous driver setup"
end

begin "installer exiting 127 is not reported as an authorization failure"
echo 127 > "$T/installer-rc"
run_default
expect_rc 3
end

begin "rollback falls back to the current versions"
echo 1 > "$T/installer-rc"
touch "$T/fail-exact-reinstall"
run_default
expect_rc 3
expect_call "apt-get install -y nvidia-driver-595 libnvidia-gl-595:i386"
end

begin "failed rollback is reported as such"
echo 1 > "$T/installer-rc"
touch "$T/fail-reinstall"
run_default
expect_rc 4
expect_out "did not fully work"
end

begin "failed purge restores the holds"
touch "$T/fail-purge"
run_default
expect_rc 3
expect_out "apt-get purge of the distro NVIDIA packages failed"
expect_call "apt-mark hold libnvidia-gl-595:i386"
expect_no_call_like "^installer --silent"
expect_no_file "$T/etc/modprobe.d/blacklist-nouveau.conf"
end

begin "initramfs failure after a good install is exit 5, without rollback"
touch "$T/fail-initramfs"
run_default
expect_rc 5
expect_no_call_like "^apt-get install -y --allow-downgrades"
expect_file "$T/etc/modprobe.d/blacklist-nouveau.conf"
end

begin "a missing command (exit 127) is reported as an ordinary failure"
rm "$T/bin/sha256sum"
run_default
expect_rc 1
end

begin "apt block: pin written, then removed by an install without it"
run_default --block-distro-driver
expect_rc 0
PIN="$T/etc/apt/preferences.d/green-light-nvidia"
expect_file "$PIN"
grep -q '^Package: src:nvidia-graphics-drivers\* ' "$PIN" || fail "pin has no source package match"
grep -qx 'Pin-Priority: -1' "$PIN" || fail "pin priority is not -1"
[[ "$(stat -c %a "$PIN")" == 644 ]] || fail "pin mode is not 644"
run_default
expect_rc 0
expect_no_file "$PIN"
expect_out "Removed the block on the distro's NVIDIA driver packages"
end

begin "apt block survives a rolled-back install"
run_default --block-distro-driver
echo 1 > "$T/installer-rc"
run_default
expect_rc 3
expect_file "$T/etc/apt/preferences.d/green-light-nvidia"
end

# dnf machines: no apt-get, rpm packages instead.
dnf_case() {
    begin "$1"
    unstub apt-get; unstub apt-mark; unstub dpkg-query; unstub update-initramfs; unstub lsinitramfs
    stub dnf 'if [[ "$1" == install && "$*" == *nvidia* && -e "$GLT/fail-reinstall" ]]; then exit 1; fi; exit 0'
    stub rpm 'echo "akmod-nvidia|akmod-nvidia-595.84-1.fc44.x86_64"; echo "xorg-x11-drv-nvidia|xorg-x11-drv-nvidia-595.84-1.fc44.x86_64"'
    stub dracut 'touch "/boot/initramfs-$3.img"'
    stub lsinitrd 'echo usr/lib/modprobe.d/blacklist-nouveau.conf'
}

dnf_case "dnf5 install with block"
stub dnf5 'exit 0'
run_default --block-distro-driver
expect_rc 0
expect_call "dnf remove -y akmod-nvidia xorg-x11-drv-nvidia"
expect_call "dracut --force --kver $KVER"
expect_file "$T/etc/dnf/libdnf5.conf.d/90-green-light-nvidia.conf"
grep -qx 'excludepkgs=akmod-nvidia\*,xorg-x11-drv-nvidia\*,kmod-nvidia\*,nvidia-driver\*,nvidia-settings\*' \
    "$T/etc/dnf/libdnf5.conf.d/90-green-light-nvidia.conf" || fail "wrong excludepkgs line"
end

dnf_case "dnf4 cannot block, install still succeeds"
run_default --block-distro-driver
expect_rc 0
expect_out "needs apt or dnf5"
expect_no_file "$T/etc/dnf/libdnf5.conf.d/90-green-light-nvidia.conf"
end

dnf_case "dnf rollback reinstalls the exact packages"
echo 1 > "$T/installer-rc"
run_default
expect_rc 3
expect_call "dnf install -y akmod-nvidia-595.84-1.fc44.x86_64 xorg-x11-drv-nvidia-595.84-1.fc44.x86_64"
end

echo
echo "$PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
