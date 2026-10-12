#!/usr/bin/env bash
# Tests for scripts/privileged-install.sh.
#
# The real script runs as the current, unprivileged user inside bubblewrap:
# /etc, /var, /boot, /run, /tmp, /usr/lib/nvidia and the kernel module tree
# are scratch directories, and PATH holds only stub commands (apt-get,
# dpkg-query, dnf, rpm, modinfo, NVIDIA's installer, ...) plus the handful
# of real tools the script needs. The stubs keep a small fake package
# database, so removals, reverse dependencies and reinstalls behave like the
# real thing. Nothing on the real system is read or written, and nothing
# runs as root.
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
CASE_FAILED=0
T=""

# Real tools the script uses; everything else on PATH is a stub.
REAL_TOOLS=(bash cat chmod cp cut date flock grep head install ls mkdir mktemp mv nproc rm setsid
            sleep stat sha256sum tail touch getfacl)

stub() { # <name> <body>   (body runs with "$@" = the stub's arguments)
    printf '#!/bin/bash\necho "%s $*" >> "$GLT/calls"\n%s\n' "$1" "$2" > "$T/bin/$1"
    chmod 755 "$T/bin/$1"
}
unstub() { rm -f "$T/bin/$1"; }

# A fresh sandbox: an apt machine with headers, gcc, make and dkms, the
# distro driver 595.84 (one package held, plus Ubuntu's prebuilt module
# package that depends on it and provides the module), Secure Boot off.
new_case() {
    T="$WORK/case-$((PASS + FAIL))"   # no case name: a ":" would split PATH
    mkdir -p "$T"/{bin,etc/modprobe.d,etc/apt/preferences.d,etc/dnf,etc/dkms,var/log,var/tmp,var/lib,boot,modules/"$KVER"/build/include/generated,nvidia,home}
    local tool
    for tool in "${REAL_TOOLS[@]}"; do
        if command -v "$tool" >/dev/null 2>&1; then
            ln -s "$(readlink -f "$(command -v "$tool")")" "$T/bin/$tool"
        fi
    done
    touch "$T/modules/$KVER/build/Makefile"
    echo '#define CONFIG_CC_IS_GCC 1' > "$T/modules/$KVER/build/include/generated/autoconf.h"
    : > "$T/calls"
    printf 'me:x:%s:%s::/home/me:/bin/bash\nroot:x:0:0::/root:/bin/bash\n' "$ME_UID" "$ME_GID" > "$T/passwd"
    printf 'me:x:%s:\nroot:x:0:\n' "$ME_GID" > "$T/group"

    # Fake dpkg database: "<status>|<name>|<version>"; revdeps: "<pkg> <dependent>".
    printf '%s\n' 'ii |nvidia-driver-595|595.84-0ubuntu1' 'hi |libnvidia-gl-595:i386|595.84-0ubuntu1' \
        'ii |linux-modules-nvidia-595-generic|6.99.0-1' 'ii |nvidia-container-toolkit|1.17.0-1' \
        'ii |bash|5.2-1' 'rc |nvidia-dkms-570|570.1-1' > "$T/dpkg"
    echo 'nvidia-driver-595 linux-modules-nvidia-595-generic' > "$T/revdeps"
    printf 'libnvidia-gl-595:i386\nnvidia-container-toolkit\nbash\n' > "$T/held"
    # The module on disk for $KVER comes with this package.
    echo 'linux-modules-nvidia-595-generic' > "$T/mod-pkg"
    echo '595.84' > "$T/mod-version"
    echo 'Canonical Ltd. Kernel Module Signing' > "$T/mod-signer"

    stub uname 'echo '"$KVER"
    stub getent 'case "$1" in
  passwd) if [[ -n "${2:-}" ]]; then grep -E "^[^:]*:[^:]*:$2:" "$GLT/passwd" || exit 2; else cat "$GLT/passwd"; fi ;;
  group) grep -E "^[^:]*:[^:]*:$2:" "$GLT/group" || exit 2 ;;
esac'
    stub dpkg-query 'cat "$GLT/dpkg"; exit 1'
    # purge: drop the packages and their dependents; install: (re)add them.
    # Removing/adding the module package removes/restores the module.
    stub apt-get 'cmd="$1"; shift
mod_off() { if [[ -f "$GLT/mod-version" ]]; then mv "$GLT/mod-version" "$GLT/mod-version.off"; mv "$GLT/mod-signer" "$GLT/mod-signer.off" 2>/dev/null; fi; }
mod_on() { if [[ -f "$GLT/mod-version.off" ]]; then mv "$GLT/mod-version.off" "$GLT/mod-version"; mv "$GLT/mod-signer.off" "$GLT/mod-signer" 2>/dev/null; fi; }
case "$cmd" in
  purge)
    [[ -e "$GLT/fail-purge" ]] && exit 100
    for p in "$@"; do
      [[ "$p" == -* ]] && continue
      for d in $p $(grep "^$p " "$GLT/revdeps" | cut -d" " -f2); do
        grep -v "|$d|" "$GLT/dpkg" > "$GLT/dpkg.new"; mv "$GLT/dpkg.new" "$GLT/dpkg"
        if grep -qx "$d" "$GLT/mod-pkg"; then mod_off; fi
      done
    done ;;
  install)
    if [[ "$*" == *nvidia* ]]; then
      [[ -e "$GLT/fail-reinstall" ]] && exit 100
      [[ "$*" == *"="* && -e "$GLT/fail-exact-reinstall" ]] && exit 100
      for p in "$@"; do
        [[ "$p" == -* ]] && continue
        name="${p%%=*}"; ver="${p#*=}"; [[ "$p" == *=* ]] || ver="current"
        echo "ii |$name|$ver" >> "$GLT/dpkg"
        if grep -qx "$name" "$GLT/mod-pkg"; then
          mod_on
          # A current version from the archive: rebuilt by DKMS, unsigned.
          if [[ "$ver" == current ]]; then echo 596.1 > "$GLT/mod-version"; : > "$GLT/mod-signer"; fi
        fi
      done
    fi ;;
esac
exit 0'
    stub apt-mark 'if [[ "$1" == showhold ]]; then cat "$GLT/held"; fi; exit 0'
    stub update-alternatives 'exit 0'
    stub update-initramfs 'if [[ -e "$GLT/fail-initramfs" ]]; then exit 1; fi; touch "/boot/initrd.img-$3"'
    stub lsinitramfs 'echo usr/lib/modprobe.d/blacklist-nouveau.conf'
    stub dkms 'if [[ "$1" == status ]]; then echo "nvidia/595.84, '"$KVER"', x86_64: installed"; fi'
    stub mokutil 'case "$1" in --sb-state) cat "$GLT/sb" 2>/dev/null || echo "SecureBoot disabled" ;; esac'
    stub modinfo 'field="$4"; [[ -f "$GLT/mod-version" ]] || exit 1
case "$field" in version) cat "$GLT/mod-version" ;; signer) cat "$GLT/mod-signer" 2>/dev/null ;; esac'
    stub nvidia-installer 'if [[ "$1" == --version ]]; then echo "nvidia-installer:  version $(cat "$GLT/run-version")"; fi'
    stub gcc 'exit 0'
    # The test build: fails for a flavor named in $GLT/fail-build.
    stub make 'for f in $(cat "$GLT/fail-build" 2>/dev/null); do [[ "$2" == */$f ]] && { echo "error: implicit declaration"; exit 2; }; done; exit 0'

    RUN="$T/home/NVIDIA-Linux-x86_64-595.84.run"
    # The fake installer. installer-mode: ok (default), fail, partial (puts
    # its files in place, then fails), removes-old (takes out an earlier .run
    # driver, then fails); installer-wait: seconds to run first.
    cat > "$RUN" <<'FAKE'
#!/bin/bash
echo "installer $*" >> "$GLT/calls"
case "$1" in
  --check) exit 0 ;;
  --extract-only) mkdir -p "$3/kernel-open" "$3/kernel"; exit 0 ;;
esac
echo "$PPID" > "$GLT/script-pid"
if [[ -e "$GLT/installer-wait" ]]; then sleep "$(cat "$GLT/installer-wait")"; fi
new_run() {
    echo 595.84 > "$GLT/run-version"; echo 595.84 > "$GLT/mod-version"; : > "$GLT/mod-signer"
    printf '#!/bin/bash\necho "nvidia-uninstall $*" >> "$GLT/calls"\nrm -f "$GLT/run-version" "$GLT/mod-version" "$GLT/mod-signer" "$GLT/bin/nvidia-uninstall"\n' > "$GLT/bin/nvidia-uninstall"
    chmod 755 "$GLT/bin/nvidia-uninstall"
}
case "$(cat "$GLT/installer-mode" 2>/dev/null || echo ok)" in
  ok) new_run; exit 0 ;;
  fail) exit 1 ;;
  partial) new_run; exit 1 ;;
  removes-old) rm -f "$GLT/run-version" "$GLT/mod-version" "$GLT/bin/nvidia-uninstall"; exit 1 ;;
  exit127) exit 127 ;;
esac
FAKE
    chmod 644 "$RUN"
}

# An earlier driver installed from NVIDIA's .run instead of distro packages.
previous_run_driver() {
    printf '%s\n' 'ii |bash|5.2-1' > "$T/dpkg"
    : > "$T/held"
    echo 580.1 > "$T/run-version"
    echo 580.1 > "$T/mod-version"
    : > "$T/mod-signer"
    stub nvidia-uninstall 'rm -f "$GLT/run-version" "$GLT/mod-version" "$GLT/bin/nvidia-uninstall"'
}

# bwrap arguments for /usr/lib: the host's, except nvidia/ and modules/,
# which are scratch directories (so they exist on any host).
usr_lib_args() {
    local e
    USR_LIB_ARGS=(--tmpfs /usr/lib)
    shopt -s nullglob dotglob
    for e in /usr/lib/*; do
        case "${e##*/}" in nvidia|modules) continue ;; esac
        if [[ -L "$e" ]]; then
            USR_LIB_ARGS+=(--symlink "$(readlink "$e")" "$e")
        else
            USR_LIB_ARGS+=(--ro-bind "$e" "$e")
        fi
    done
    shopt -u nullglob dotglob
}
usr_lib_args

# Run a command in the sandbox.
sandbox() {
    bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --tmpfs /run \
        --tmpfs /etc --ro-bind-try /etc/ld.so.cache /etc/ld.so.cache \
        --bind "$T/etc/modprobe.d" /etc/modprobe.d \
        --bind "$T/etc/apt" /etc/apt --bind "$T/etc/dnf" /etc/dnf --bind "$T/etc/dkms" /etc/dkms \
        --tmpfs /var --bind "$T/var/log" /var/log --bind "$T/var/tmp" /var/tmp --bind "$T/var/lib" /var/lib \
        --bind "$T/boot" /boot "${USR_LIB_ARGS[@]}" \
        --bind "$T/nvidia" /usr/lib/nvidia --bind "$T/modules" /usr/lib/modules \
        --bind "$T" "$T" \
        --clearenv --setenv PATH "$T/bin" --setenv GLT "$T" --setenv PKEXEC_UID "$ME_UID" \
        --setenv LANG C.UTF-8 \
        "$@"
}

# Run the script in the sandbox. Sets RC and OUT.
run_script() {
    RC=0
    OUT="$(sandbox "$T/bin/bash" "$SCRIPT" "$@" 2>&1)" || RC=$?
}

sha() { sha256sum "$1" | cut -d' ' -f1; }
run_default() { run_script "$RUN" "$(sha "$RUN")" "$@"; }

# Start the script in the background with its output going to $T/out.
# The caller waits for it with wait_script.
start_script() {
    sandbox "$T/bin/bash" "$SCRIPT" "$RUN" "$(sha "$RUN")" > "$T/out" 2>&1 &
    BG=$!
}
wait_script() { RC=0; wait "$BG" || RC=$?; OUT="$(cat "$T/out")"; }
# Wait until the fake installer is running and print the script's PID.
script_pid() {
    local _
    for _ in $(seq 100); do
        [[ -s "$T/script-pid" ]] && { cat "$T/script-pid"; return 0; }
        sleep 0.1
    done
    echo "installer never started" >&2
    return 1
}

log_has() { grep -qF -- "$1" "$T/var/log/green-light.log"; }

fail() {
    echo "FAIL [$CASE]: $*"
    if [[ $CASE_FAILED -eq 0 ]]; then
        echo "---- output"; echo "$OUT"; echo "---- calls"; cat "$T/calls"; echo "----"
    fi
    CASE_FAILED=1
}
expect_rc() { [[ "$RC" == "$1" ]] || fail "exit code $RC, expected $1"; }
expect_out() { grep -qF -- "$1" <<<"$OUT" || fail "output lacks: $1"; }
expect_log() { log_has "$1" || fail "log file lacks: $1"; }
expect_call() { grep -qxF -- "$1" "$T/calls" || fail "no call: $1"; }
expect_call_like() { grep -qE -- "$1" "$T/calls" || fail "no call matching: $1"; }
expect_no_call_like() { ! grep -qE -- "$1" "$T/calls" || fail "unexpected call matching: $1"; }
expect_file() { [[ -f "$1" ]] || fail "missing file: $1"; }
expect_no_file() { [[ ! -e "$1" ]] || fail "file should not exist: $1"; }
expect_pkg() { grep -q "|$1|" "$T/dpkg" || fail "package not installed: $1"; }

begin() { CASE="$1"; new_case; CASE_FAILED=0; }
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
expect_out "also removed, as they depend on the driver: linux-modules-nvidia-595-generic"
expect_call_like "^installer --extract-only "
expect_call_like "^make -C .*/kernel-open .*KERNEL_UNAME=$KVER modules"
expect_call_like "^make -C .*/kernel .*KERNEL_UNAME=$KVER modules"
expect_call_like "^installer --silent .*--dkms"
expect_call "update-initramfs -u -k $KVER"
expect_file "$T/etc/modprobe.d/blacklist-nouveau.conf"
expect_no_file "$T/etc/apt/preferences.d/green-light-nvidia"
expect_out "Verify: all checks passed"
expect_log "==== NVIDIA driver install started"
end

begin "a chosen module flavor is the only one test-built"
run_default --kernel-module-type=proprietary
expect_rc 0
expect_call_like "^make -C .*/unpacked/kernel "
expect_no_call_like "^make -C .*/kernel-open "
end

begin "a module that doesn't build stops the install before anything changes"
echo kernel-open > "$T/fail-build"
run_default
expect_rc 1
expect_out "The NVIDIA kernel module (kernel-open) doesn't build for kernel $KVER. No changes made."
expect_out "Build log: error: implicit declaration"
expect_no_call_like "^(apt-get purge|apt-mark unhold|installer --silent)"
expect_no_file "$T/etc/modprobe.d/blacklist-nouveau.conf"
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

if command -v setfacl >/dev/null 2>&1 && setfacl -m u:nobody:rw "$WORK" 2>/dev/null; then
    setfacl -b "$WORK"
    begin "an ACL letting another user write the file is refused"
    setfacl -m "u:$(id -nu 65534 2>/dev/null || echo nobody):rw" "$RUN"
    run_default
    expect_rc 2
    expect_out "has an access control list letting user:"
    expect_no_call_like "^(apt-get purge|installer --silent)"
    end

    begin "an ACL that only grants reading is fine"
    setfacl -m "u:$(id -nu 65534 2>/dev/null || echo nobody):r" "$RUN"
    run_default
    expect_rc 0
    end
else
    echo "skip ACL cases (no setfacl or no ACL support here)"
fi

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

begin "installer failure rolls everything back, reverse dependencies included"
echo fail > "$T/installer-mode"
printf 'options nouveau modeset=1\n' > "$T/etc/modprobe.d/nvidia-drm-modeset.conf"
touch "$T/nvidia/alternate-install-present"
run_default --dkms
expect_rc 3
expect_out "NVIDIA installer exited with code 1"
expect_call_like "^apt-get install -y --allow-downgrades .*nvidia-driver-595=595.84-0ubuntu1"
expect_call_like "^apt-get install -y --allow-downgrades .*libnvidia-gl-595:i386=595.84-0ubuntu1"
expect_call_like "^apt-get install -y --allow-downgrades .*linux-modules-nvidia-595-generic=6.99.0-1"
expect_pkg linux-modules-nvidia-595-generic
expect_call "apt-mark hold libnvidia-gl-595:i386"
expect_no_file "$T/etc/modprobe.d/blacklist-nouveau.conf"
grep -qx 'options nouveau modeset=1' "$T/etc/modprobe.d/nvidia-drm-modeset.conf" \
    || fail "modeset config not restored"
expect_file "$T/nvidia/alternate-install-present"
[[ "$(grep -c "^update-initramfs" "$T/calls")" == 1 ]] || fail "initramfs not rebuilt once on rollback"
expect_out "Restored and checked the previous driver setup"
end

begin "Secure Boot: a rollback that leaves an unsigned module is reported as failed"
echo fail > "$T/installer-mode"
echo "SecureBoot enabled" > "$T/sb"
touch "$T/fail-exact-reinstall"
run_default
expect_rc 4
expect_call_like "^apt-get install -y nvidia-driver-595 "
expect_out "not signed by a key it trusts"
expect_out "Don't reboot"
end

begin "without Secure Boot, current versions are an acceptable restore"
echo fail > "$T/installer-mode"
touch "$T/fail-exact-reinstall"
run_default
expect_rc 3
expect_out "is now 596.1 (it was 595.84)"
end

begin "a partial install of the new driver is uninstalled on rollback"
echo partial > "$T/installer-mode"
run_default
expect_rc 3
expect_call "nvidia-uninstall --silent"
expect_no_file "$T/run-version"
end

begin "an earlier .run driver removed by the failed installer means exit 4"
previous_run_driver
echo removes-old > "$T/installer-mode"
run_default
expect_rc 4
expect_out "The driver 580.1 installed earlier from NVIDIA's .run file was removed"
end

begin "an earlier .run driver left in place by the failed installer is a clean rollback"
previous_run_driver
echo fail > "$T/installer-mode"
run_default
expect_rc 3
end

begin "installer exiting 127 is not reported as an authorization failure"
echo exit127 > "$T/installer-mode"
run_default
expect_rc 3
end

begin "failed rollback is reported as such"
echo fail > "$T/installer-mode"
touch "$T/fail-reinstall"
run_default
expect_rc 4
expect_out "could not be fully restored"
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

begin "initramfs failure after a good install is exit 5, block kept as asked"
touch "$T/fail-initramfs"
run_default --block-distro-driver
expect_rc 5
expect_no_call_like "^apt-get install -y --allow-downgrades"
expect_file "$T/etc/modprobe.d/blacklist-nouveau.conf"
expect_file "$T/etc/apt/preferences.d/green-light-nvidia"
end

begin "a missing command (exit 127) is reported as an ordinary failure"
rm "$T/bin/sha256sum"
run_default
expect_rc 1
end

begin "the reader going away (SIGPIPE) doesn't stop the install"
echo 1 > "$T/installer-wait"
RC=0
set +o pipefail
sandbox "$T/bin/bash" "$SCRIPT" "$RUN" "$(sha "$RUN")" 2>&1 | head -c 1 > /dev/null
RC="${PIPESTATUS[0]}"
set -o pipefail
OUT="$(cat "$T/var/log/green-light.log")"
expect_rc 0
expect_log "==== Done. Reboot to switch to the new driver. ===="
end

begin "the reader going away before a failed install still rolls back"
echo 1 > "$T/installer-wait"
echo fail > "$T/installer-mode"
set +o pipefail
sandbox "$T/bin/bash" "$SCRIPT" "$RUN" "$(sha "$RUN")" 2>&1 | head -c 1 > /dev/null
RC="${PIPESTATUS[0]}"
set -o pipefail
OUT="$(cat "$T/var/log/green-light.log")"
expect_rc 3
expect_log "Restored and checked the previous driver setup"
expect_pkg nvidia-driver-595
end

begin "SIGTERM during the install rolls back"
echo 2 > "$T/installer-wait"
start_script
kill -TERM "$(script_pid)"
wait_script
expect_rc 3
expect_out "stopped by SIGTERM"
expect_out "Restored and checked the previous driver setup"
expect_call "nvidia-uninstall --silent"
expect_pkg nvidia-driver-595
expect_no_file "$T/etc/modprobe.d/blacklist-nouveau.conf"
end

begin "SIGHUP is ignored: the install finishes"
echo 2 > "$T/installer-wait"
start_script
kill -HUP "$(script_pid)"
wait_script
expect_rc 0
expect_out "==== Done."
end

begin "SIGHUP with a failing installer still rolls back"
echo 2 > "$T/installer-wait"
echo fail > "$T/installer-mode"
start_script
kill -HUP "$(script_pid)"
wait_script
expect_rc 3
expect_pkg linux-modules-nvidia-595-generic
end

begin "a second install while one runs is refused"
RC=0
OUT="$(sandbox "$T/bin/bash" -c 'flock /run/green-light-install.lock sleep 3 & sleep 0.5; "$0" "$@"' \
    "$SCRIPT" "$RUN" "$(sha "$RUN")" 2>&1)" || RC=$?
expect_rc 2
expect_out "Another Green Light install is still running"
expect_no_call_like "^installer"
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
echo fail > "$T/installer-mode"
run_default
expect_rc 3
expect_file "$T/etc/apt/preferences.d/green-light-nvidia"
end

# dnf machines: no apt-get, a fake rpm database instead ("<name.arch>|<nvra>").
dnf_case() {
    begin "$1"
    unstub apt-get; unstub apt-mark; unstub dpkg-query; unstub update-initramfs; unstub lsinitramfs
    printf '%s\n' 'akmod-nvidia.x86_64|akmod-nvidia-595.84-1.fc44.x86_64' \
        'xorg-x11-drv-nvidia.x86_64|xorg-x11-drv-nvidia-595.84-1.fc44.x86_64' \
        'nvidia-persistenced.x86_64|nvidia-persistenced-595.84-1.fc44.x86_64' \
        'bash.x86_64|bash-5.2-1.fc44.x86_64' > "$T/rpm"
    # Removing xorg-x11-drv-nvidia takes nvidia-persistenced (not in the
    # globs) with it, like a reverse dependency.
    echo 'xorg-x11-drv-nvidia nvidia-persistenced' > "$T/revdeps"
    echo 'akmod-nvidia' > "$T/mod-pkg"
    stub rpm 'if [[ "$*" == *ARCH* ]]; then cat "$GLT/rpm"; else cut -d"|" -f1 "$GLT/rpm" | sed "s/\.[^.]*$//" | grep -E "^(akmod-nvidia|xorg-x11-drv-nvidia|kmod-nvidia|nvidia-driver|nvidia-settings)"; fi'
    stub sed 'exec /usr/bin/sed "$@"'
    stub dnf 'cmd="$1"; shift
case "$cmd" in
  remove)
    for p in "$@"; do [[ "$p" == -* ]] && continue
      for d in $p $(grep "^$p " "$GLT/revdeps" | cut -d" " -f2); do
        grep "^$d\." "$GLT/rpm" >> "$GLT/rpm.removed"; grep -v "^$d\." "$GLT/rpm" > "$GLT/rpm.new"; mv "$GLT/rpm.new" "$GLT/rpm"
        if grep -qx "$d" "$GLT/mod-pkg"; then mv "$GLT/mod-version" "$GLT/mod-version.off"; fi
      done
    done ;;
  install)
    if [[ "$*" == *nvidia* ]]; then
      [[ -e "$GLT/fail-reinstall" ]] && exit 1
      for p in "$@"; do [[ "$p" == -* ]] && continue
        grep -F "|$p" "$GLT/rpm.removed" >> "$GLT/rpm" || true
        if grep -q "^$(cat "$GLT/mod-pkg")-" <<<"$p" && [[ -f "$GLT/mod-version.off" ]]; then mv "$GLT/mod-version.off" "$GLT/mod-version"; fi
      done
    fi ;;
esac
exit 0'
    stub dracut 'touch "/boot/initramfs-$3.img"'
    stub lsinitrd 'echo usr/lib/modprobe.d/blacklist-nouveau.conf'
}

dnf_case "dnf5 install with block"
stub dnf5 'exit 0'
run_default --block-distro-driver
expect_rc 0
expect_call "dnf remove -y akmod-nvidia xorg-x11-drv-nvidia"
expect_out "also removed, as they depend on the driver: nvidia-persistenced.x86_64"
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

dnf_case "dnf rollback reinstalls the exact removal set"
echo fail > "$T/installer-mode"
run_default
expect_rc 3
expect_call_like "^dnf install -y .*akmod-nvidia-595.84-1.fc44.x86_64"
expect_call_like "^dnf install -y .*nvidia-persistenced-595.84-1.fc44.x86_64"
grep -q '^nvidia-persistenced\.' "$T/rpm" || fail "nvidia-persistenced not reinstalled"
end

echo
echo "$PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
