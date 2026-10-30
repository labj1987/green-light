#!/usr/bin/env bash
# Tests for scripts/privileged-install.sh and the green-light-recover.service
# boot unit.
#
# The real script runs as the current, unprivileged user inside bubblewrap:
# /etc, /var, /boot, /run, /tmp, /usr/lib/nvidia and the kernel module tree
# are scratch directories, and PATH holds only stub commands (apt-get,
# dpkg-query, dnf, rpm, modinfo, NVIDIA's installer, ...) plus the handful
# of real tools the script needs. The stubs keep a small fake package
# database, so removals, reverse dependencies, downloads and reinstalls
# behave like the real thing; $GLT/offline cuts the network (and, for dnf,
# makes loading repository metadata fail). Nothing on the real system is
# read or written, and nothing runs as root.
#
# Usage: scripts/tests/privileged-install-test.sh   (needs bwrap)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/../privileged-install.sh"
UNIT="$HERE/../../data/green-light-recover.service"
command -v bwrap >/dev/null 2>&1 || { echo "error: bwrap (bubblewrap) is required" >&2; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/gl-script-test.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

KVER="6.99.0-test"
KVER2="7.0.0-newer"
ME_UID="$(id -u)"
ME_GID="$(id -g)"
PASS=0
FAIL=0
CASE=""
CASE_FAILED=0
T=""

# Real tools the script uses; everything else on PATH is a stub.
REAL_TOOLS=(bash cat chmod cp cut date flock grep head install ls mkdir mktemp mv nproc rm setsid
            sleep sort stat sha256sum sync tail touch true env getfacl)

stub() { # <name> <body>   (body runs with "$@" = the stub's arguments)
    printf '#!/bin/bash\necho "%s $*" >> "$GLT/calls"\n%s\n' "$1" "$2" > "$T/bin/$1"
    chmod 755 "$T/bin/$1"
}
unstub() { rm -f "$T/bin/$1"; }

# A fresh sandbox: an apt machine with headers, gcc, make and dkms, and the
# distro driver 595.84: nvidia-driver-595, a held i386 library, and Ubuntu's
# prebuilt module package that depends on the driver and provides the
# module (installed automatically). Secure Boot off; online.
new_case() {
    T="$WORK/case-$((PASS + FAIL))"   # no case name: a ":" would split PATH
    mkdir -p "$T"/{bin,etc/modprobe.d,etc/apt/preferences.d,etc/dnf,etc/dkms,var/log,var/tmp,var/lib} \
             "$T"/{var/cache/apt/archives,var/cache/akmods/nvidia,boot,nvidia,home} \
             "$T/modules/$KVER/build/include/generated"
    local tool
    for tool in "${REAL_TOOLS[@]}"; do
        # type -P: the program, also for names that are shell builtins (true).
        if type -P "$tool" >/dev/null 2>&1; then
            ln -s "$(readlink -f "$(type -P "$tool")")" "$T/bin/$tool"
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
        'ii |bash|5.2-1' 'ii |unrelated|1.0' 'rc |nvidia-dkms-570|570.1-1' > "$T/dpkg"
    echo 'nvidia-driver-595 linux-modules-nvidia-595-generic' > "$T/revdeps"
    printf 'libnvidia-gl-595:i386\nnvidia-container-toolkit\nbash\n' > "$T/held"
    echo 'linux-modules-nvidia-595-generic' > "$T/auto-pkgs"
    # The module on disk for $KVER comes with this package; for another
    # kernel K, mod-version.K / mod-signer.K.
    echo 'linux-modules-nvidia-595-generic' > "$T/mod-pkg"
    echo '595.84' > "$T/mod-version"
    echo 'Canonical Ltd. Kernel Module Signing' > "$T/mod-signer"

    stub uname 'cat "$GLT/uname" 2>/dev/null || echo '"$KVER"
    stub getent 'case "$1" in
  passwd) if [[ -n "${2:-}" ]]; then grep -E "^[^:]*:[^:]*:$2:" "$GLT/passwd" || exit 2; else cat "$GLT/passwd"; fi ;;
  group) grep -E "^[^:]*:[^:]*:$2:" "$GLT/group" || exit 2 ;;
esac'
    stub dpkg-query 'cat "$GLT/dpkg"; exit 1'
    stub dpkg 'exit 0'
    # purge: drop the packages and their dependents (and, with
    # $GLT/remove-unrelated, something no plan includes); -s purge: list
    # them; download: a .deb in the current directory; install: package
    # names, name=version or .deb paths (linux-modules-nvidia-595-<kernel>
    # brings that kernel's module). Removing/adding the module package
    # removes/restores the module. $GLT/fail-reinstall-times: how many
    # driver reinstalls fail before they work.
    stub apt-get 'if [[ "$1" == -s ]]; then shift; sim=1; else sim=0; fi
cmd="$1"; shift
grep -E "^SigIgn" /proc/self/status > "$GLT/apt-signals"
mod_off() { if [[ -f "$GLT/mod-version" ]]; then mv "$GLT/mod-version" "$GLT/mod-version.off"; mv "$GLT/mod-signer" "$GLT/mod-signer.off" 2>/dev/null; fi; }
mod_on() { if [[ -f "$GLT/mod-version.off" ]]; then mv "$GLT/mod-version.off" "$GLT/mod-version"; mv "$GLT/mod-signer.off" "$GLT/mod-signer" 2>/dev/null; fi; }
add() { # <name> <version>
  echo "ii |$1|$2" >> "$GLT/dpkg"
  if [[ "$1" == linux-modules-nvidia-595-[0-9]* ]]; then
    k="${1#linux-modules-nvidia-595-}"; echo 595.84 > "$GLT/mod-version.$k"; echo "Canonical Ltd. Kernel Module Signing" > "$GLT/mod-signer.$k"
  fi
  if grep -qx "$1" "$GLT/mod-pkg"; then
    mod_on
    # A current version from the archive: rebuilt by DKMS, unsigned.
    if [[ "$2" == current ]]; then echo 596.1 > "$GLT/mod-version"; : > "$GLT/mod-signer"; fi
  fi
}
case "$cmd" in
  purge)
    [[ $sim -eq 0 && -e "$GLT/fail-purge" ]] && exit 100
    [[ $sim -eq 0 && -e "$GLT/purge-wait" ]] && sleep "$(cat "$GLT/purge-wait")"
    for p in "$@"; do
      [[ "$p" == -* ]] && continue
      for d in $p $(grep "^$p " "$GLT/revdeps" | cut -d" " -f2); do
        if [[ $sim -eq 1 ]]; then echo "Purg $d [$(grep "|$d|" "$GLT/dpkg" | cut -d"|" -f3)]"; continue; fi
        grep -v "|$d|" "$GLT/dpkg" > "$GLT/dpkg.new"; mv "$GLT/dpkg.new" "$GLT/dpkg"
        if grep -qx "$d" "$GLT/mod-pkg"; then mod_off; fi
      done
    done
    if [[ $sim -eq 0 && -e "$GLT/remove-unrelated" ]]; then grep -v "|unrelated|" "$GLT/dpkg" > "$GLT/dpkg.new"; mv "$GLT/dpkg.new" "$GLT/dpkg"; fi ;;
  download)
    [[ -e "$GLT/offline" || -e "$GLT/no-download" ]] && exit 100
    name="${1%%=*}"; ver="${1#*=}"; arch=amd64; [[ "$name" == *:* ]] && arch="${name#*:}"
    touch "${name%%:*}_${ver//:/%3a}_${arch}.deb" ;;
  install)
    if [[ "$*" == *nvidia* || "$*" == *.deb* ]]; then
      [[ -e "$GLT/fail-reinstall" ]] && exit 100
      if [[ -s "$GLT/fail-reinstall-times" ]]; then
        n="$(cat "$GLT/fail-reinstall-times")"
        if [[ "$n" -gt 0 ]]; then echo $((n - 1)) > "$GLT/fail-reinstall-times"; exit 100; fi
      fi
      for p in "$@"; do
        [[ "$p" == -* || "$p" == /* ]] && continue
        [[ -e "$GLT/offline" ]] && exit 100
        [[ "$p" == *"="* && -e "$GLT/fail-exact-reinstall" ]] && exit 100
      done
      for p in "$@"; do
        [[ "$p" == -* ]] && continue
        if [[ "$p" == /*.deb ]]; then
          f="${p##*/}"; f="${f%.deb}"; name="${f%%_*}"; rest="${f#*_}"; ver="${rest%_*}"; arch="${rest##*_}"
          [[ "$arch" == amd64 ]] || name="$name:$arch"
          add "$name" "${ver//%3a/:}"
        elif [[ "$p" == *=* ]]; then add "${p%%=*}" "${p#*=}"
        else add "$p" current
        fi
      done
    fi ;;
esac
exit 0'
    stub apt-mark 'case "$1" in showhold) cat "$GLT/held" ;; showauto) cat "$GLT/auto-pkgs" ;; esac; exit 0'
    stub update-alternatives 'exit 0'
    stub update-initramfs 'if [[ -e "$GLT/initramfs-wait" ]]; then sleep "$(cat "$GLT/initramfs-wait")"; fi
if [[ -e "$GLT/fail-initramfs" ]]; then exit 1; fi; touch "/boot/initrd.img-$3"'
    stub lsinitramfs 'echo usr/lib/modprobe.d/blacklist-nouveau.conf'
    # dkms: status; autoinstall -k K builds the module for K when the
    # restored packages include DKMS sources ($GLT/dkms-builds).
    stub dkms 'case "$1" in
  status) echo "nvidia/595.84, '"$KVER"', x86_64: installed" ;;
  autoinstall) [[ -e "$GLT/dkms-builds" ]] && { echo 595.84 > "$GLT/mod-version.$3"; : > "$GLT/mod-signer.$3"; } ;;
esac'
    stub mokutil 'case "$1" in --sb-state) cat "$GLT/sb" 2>/dev/null || echo "SecureBoot disabled" ;; esac'
    stub modinfo 'k="$2"; field="$4"; v="$GLT/mod-version"; sg="$GLT/mod-signer"
if [[ "$k" != '"$KVER"' ]]; then v="$GLT/mod-version.$k"; sg="$GLT/mod-signer.$k"; fi
[[ -f "$v" ]] || exit 1
case "$field" in version) cat "$v" ;; signer) cat "$sg" 2>/dev/null ;; esac'
    stub nvidia-installer 'if [[ "$1" == --version ]]; then echo "nvidia-installer:  version $(cat "$GLT/run-version")"; fi'
    stub gcc 'exit 0'
    # The test build: records the signal dispositions and descriptors it
    # inherited, and fails for a flavor named in $GLT/fail-build.
    stub make 'grep -E "^Sig(Ign)" /proc/self/status > "$GLT/make-signals"
[[ -e /proc/self/fd/9 ]] && touch "$GLT/lock-leaked-to-make"
if [[ -e "$GLT/build-wait" ]]; then sleep "$(cat "$GLT/build-wait")"; fi
for f in $(cat "$GLT/fail-build" 2>/dev/null); do [[ "$2" == */$f ]] && { echo "error: implicit declaration"; exit 2; }; done; exit 0'

    RUN="$T/home/NVIDIA-Linux-x86_64-595.84.run"
    # The fake installer. installer-mode: ok (default), fail, partial (puts
    # its files in place, then fails), removes-old (takes out an earlier .run
    # driver, then fails); installer-wait: seconds to run first;
    # offline-at-installer: the network goes away while it runs.
    cat > "$RUN" <<'FAKE'
#!/bin/bash
label="NVIDIA Accelerated Graphics Driver for Linux-x86_64 595.84"
echo "installer $*" >> "$GLT/calls"
case "$1" in
  --check) exit 0 ;;
  --extract-only) mkdir -p "$3/kernel-open" "$3/kernel"; exit 0 ;;
esac
echo "$PPID" > "$GLT/script-pid"
echo "$$" > "$GLT/installer-pid"
if [[ -e "$GLT/offline-at-installer" ]]; then touch "$GLT/offline"; fi
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

# Another driver installed by other means after an interrupted install.
other_driver_installed_since() {
    echo 600.1 > "$T/run-version"
    stub nvidia-uninstall 'rm -f "$GLT/run-version" "$GLT/bin/nvidia-uninstall"'
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

# Run a command in the sandbox. Extra bwrap options go in SANDBOX_EXTRA.
SANDBOX_EXTRA=()
sandbox() {
    bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --tmpfs /run \
        --tmpfs /etc --ro-bind-try /etc/ld.so.cache /etc/ld.so.cache \
        --bind "$T/etc/modprobe.d" /etc/modprobe.d \
        --bind "$T/etc/apt" /etc/apt --bind "$T/etc/dnf" /etc/dnf --bind "$T/etc/dkms" /etc/dkms \
        --tmpfs /var --bind "$T/var/log" /var/log --bind "$T/var/tmp" /var/tmp --bind "$T/var/lib" /var/lib \
        --bind "$T/var/cache" /var/cache \
        --bind "$T/boot" /boot "${USR_LIB_ARGS[@]}" \
        --bind "$T/nvidia" /usr/lib/nvidia --bind "$T/modules" /usr/lib/modules \
        --bind "$T" "$T" \
        --clearenv --setenv PATH "$T/bin" --setenv GLT "$T" --setenv PKEXEC_UID "$ME_UID" \
        --setenv LANG C.UTF-8 "${SANDBOX_EXTRA[@]}" \
        "$@"
}

log_size() { if [[ -f "$T/var/log/green-light.log" ]]; then wc -c < "$T/var/log/green-light.log"; else echo 0; fi; }

# Run the script in the sandbox. Sets RC and OUT (the script prints nothing
# without a terminal, so OUT is this run's part of the log file).
run_script() {
    local before
    before="$(log_size)"
    RC=0
    sandbox "$T/bin/bash" "$SCRIPT" "$@" > "$T/stdout" 2>&1 || RC=$?
    OUT="$(tail -c +"$((before + 1))" "$T/var/log/green-light.log" 2>/dev/null)"
}

sha() { sha256sum "$1" | cut -d' ' -f1; }
run_default() { run_script "$RUN" "$(sha "$RUN")" "$@"; }

# Start the script in the background; wait for it with wait_script.
start_script() {
    LOG_BEFORE="$(log_size)"
    sandbox "$T/bin/bash" "$SCRIPT" "$RUN" "$(sha "$RUN")" > "$T/stdout" 2>&1 &
    BG=$!
}
wait_script() {
    RC=0
    wait "$BG" || RC=$?
    OUT="$(tail -c +"$((LOG_BEFORE + 1))" "$T/var/log/green-light.log")"
}
# Wait until <file> exists in $T and print it (a PID).
wait_for() {
    local _
    for _ in $(seq 150); do
        [[ -s "$T/$1" ]] && { cat "$T/$1"; return 0; }
        sleep 0.1
    done
    echo "$1 never appeared" >&2
    return 1
}
# PID of the script itself, before the fake installer (which records it)
# runs: the oldest bash process with the script's command line.
script_pid_now() {
    local p
    for p in $(pgrep -f "$SCRIPT $RUN" || true); do
        grep -q "bash" "/proc/$p/comm" 2>/dev/null && { echo "$p"; return 0; }
    done
    return 1
}

fail() {
    echo "FAIL [$CASE]: $*"
    if [[ $CASE_FAILED -eq 0 ]]; then
        echo "---- output"; echo "$OUT"; echo "---- calls"; cat "$T/calls"; echo "----"
    fi
    CASE_FAILED=1
}
expect_rc() { [[ "$RC" == "$1" ]] || fail "exit code $RC, expected $1"; }
expect_out() { grep -qF -- "$1" <<<"$OUT" || fail "output lacks: $1"; }
expect_no_out() { ! grep -qF -- "$1" <<<"$OUT" || fail "output should not have: $1"; }
expect_call() { grep -qxF -- "$1" "$T/calls" || fail "no call: $1"; }
expect_call_like() { grep -qE -- "$1" "$T/calls" || fail "no call matching: $1"; }
expect_no_call_like() { ! grep -qE -- "$1" "$T/calls" || fail "unexpected call matching: $1"; }
expect_file() { [[ -f "$1" ]] || fail "missing file: $1"; }
expect_no_file() { [[ ! -e "$1" ]] || fail "file should not exist: $1"; }
expect_pkg() { grep -q "|$1|" "$T/dpkg" || fail "package not installed: $1"; }
expect_journal() { [[ -d "$T/var/lib/green-light/journal" ]] || fail "no journal"; }
expect_no_journal() { [[ ! -e "$T/var/lib/green-light/journal" ]] || fail "journal left behind"; }

begin() { CASE="$1"; new_case; CASE_FAILED=0; SANDBOX_EXTRA=(); }
end() {
    if [[ $CASE_FAILED -eq 0 ]]; then PASS=$((PASS + 1)); echo "ok   $CASE"; else FAIL=$((FAIL + 1)); fi
}

# ── Cases ──────────────────────────────────────────────────────────────

begin "the log is readable by the app whatever umask the script gets"
rm -f "$T/var/log/green-light.log"
RC=0
(umask 077; sandbox "$T/bin/bash" "$SCRIPT" "$RUN" "$(sha "$RUN")" > "$T/stdout" 2>&1) || RC=$?
OUT="$(cat "$T/var/log/green-light.log" 2>/dev/null)"
expect_rc 0
[[ "$(stat -c %a "$T/var/log/green-light.log")" == 644 ]] || fail "log mode $(stat -c %a "$T/var/log/green-light.log"), expected 644"
end

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
grep -qx 'blacklist nova_core' "$T/etc/modprobe.d/blacklist-nouveau.conf" \
    && grep -qx 'blacklist nova_drm' "$T/etc/modprobe.d/blacklist-nouveau.conf" \
    || fail "the blacklist file should also block nova_core and nova_drm"
expect_no_file "$T/etc/apt/preferences.d/green-light-nvidia"
expect_out "Verify: all checks passed"
expect_no_journal
[[ ! -s "$T/stdout" ]] || fail "wrote to stdout/stderr without a terminal: $(cat "$T/stdout")"
# Every line of the run carries its PID, and only that one.
grep -qE '^[0-9-]+ [0-9:]+ \[green-light [0-9]+\] ==== NVIDIA driver install started ====$' <<<"$OUT" \
    || fail "log lines don't carry the run's PID"
[[ "$(grep -oE '\[green-light [0-9]+\]' <<<"$OUT" | sort -u | wc -l)" == 1 ]] || fail "more than one PID in one run's lines"
# SIGPIPE keeps its default for what the script runs (bit 13 of SigIgn),
# even when it was already ignored when the script started (as on CI).
(( (16#$(cut -f2 "$T/make-signals") & (1 << 12)) == 0 )) || fail "make inherited an ignored SIGPIPE"
expect_no_file "$T/lock-leaked-to-make"
end

begin "dkms builds the module for every kernel, signed with the machine key"
mkdir -p "$T/var/lib/shim-signed/mok" "$T/modules/6.99.1-test/build"
openssl req -new -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=test Secure Boot Module Signature key" \
    -keyout "$T/var/lib/shim-signed/mok/MOK.priv" -outform DER -out "$T/var/lib/shim-signed/mok/MOK.der" >/dev/null 2>&1
# DKMS signs what it builds: install puts a module signed by the machine key
stub dkms 'case "$1" in
  status) echo "nvidia/595.84, '"$KVER"', x86_64: installed" ;;
  install) k="${*: -1}"; if [[ "$k" == '"$KVER"' ]]; then echo 595.84 > "$GLT/mod-version"; echo "test Secure Boot Module Signature key" > "$GLT/mod-signer"; else echo 595.84 > "$GLT/mod-version.$k"; echo "test Secure Boot Module Signature key" > "$GLT/mod-signer.$k"; fi ;;
esac'
run_default --dkms
expect_rc 0
expect_call "dkms build --force -m nvidia -v 595.84 -k $KVER"
expect_call "dkms install --force -m nvidia -v 595.84 -k $KVER"
expect_call "dkms build --force -m nvidia -v 595.84 -k 6.99.1-test"
expect_call "update-initramfs -u -k 6.99.1-test"
expect_out "Verify: nvidia module is signed by: test Secure Boot Module Signature key"
expect_out "Verify: nvidia module for 6.99.1-test is signed by: test Secure Boot Module Signature key"
expect_no_out "is unsigned"
end

begin "the test build uses the kernel's compiler: versioned gcc, or LLVM=1 for clang"
printf '#define CONFIG_CC_IS_GCC 1\n#define CONFIG_CC_VERSION_TEXT "x86_64-linux-gnu-gcc-15 (Ubuntu 15.2.0) 15.2.0"\n' \
    > "$T/modules/$KVER/build/include/generated/autoconf.h"
stub x86_64-linux-gnu-gcc-15 'exit 0'
run_default --kernel-module-type=open
expect_rc 0
expect_call_like "^make -C .*/kernel-open .*CC=x86_64-linux-gnu-gcc-15 KERNEL_UNAME"
printf '#define CONFIG_CC_IS_CLANG 1\n#define CONFIG_CC_VERSION_TEXT "clang version 19"\n' \
    > "$T/modules/$KVER/build/include/generated/autoconf.h"
stub clang 'exit 0'
: > "$T/calls"
run_default --kernel-module-type=open
expect_rc 0
expect_call_like "^make -C .*/kernel-open .*LLVM=1 KERNEL_UNAME"
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
expect_no_journal
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

begin "installer failure rolls everything back from the kept copies"
echo fail > "$T/installer-mode"
printf 'options nouveau modeset=1\n' > "$T/etc/modprobe.d/nvidia-drm-modeset.conf"
touch "$T/nvidia/alternate-install-present"
run_default --dkms
expect_rc 3
expect_out "NVIDIA installer exited with code 1"
expect_call_like "^apt-get download nvidia-driver-595=595.84-0ubuntu1"
expect_call_like "^apt-get install -y --allow-downgrades /var/lib/green-light/journal/cache/nvidia-driver-595_595.84-0ubuntu1_amd64.deb .*libnvidia-gl-595_595.84-0ubuntu1_i386.deb .*linux-modules-nvidia-595-generic_6.99.0-1_amd64.deb"
expect_pkg linux-modules-nvidia-595-generic
expect_pkg "libnvidia-gl-595:i386"
expect_call "apt-mark hold libnvidia-gl-595:i386"
expect_call "apt-mark auto linux-modules-nvidia-595-generic"
expect_no_file "$T/etc/modprobe.d/blacklist-nouveau.conf"
grep -qx 'options nouveau modeset=1' "$T/etc/modprobe.d/nvidia-drm-modeset.conf" \
    || fail "modeset config not restored"
expect_file "$T/nvidia/alternate-install-present"
[[ "$(grep -c "^update-initramfs" "$T/calls")" == 1 ]] || fail "initramfs not rebuilt once on rollback"
expect_out "Restored and checked the previous driver setup"
expect_no_journal
# The rollback ignores SIGTERM itself; what it runs must not inherit that
# (bit 15 of SigIgn).
(( (16#$(cut -f2 "$T/apt-signals") & (1 << 14)) == 0 )) || fail "apt-get inherited an ignored SIGTERM"
end

begin "apt's own cache is used before downloading"
echo fail > "$T/installer-mode"
touch "$T/var/cache/apt/archives/nvidia-driver-595_595.84-0ubuntu1_amd64.deb"
run_default
expect_rc 3
expect_no_call_like "^apt-get download nvidia-driver-595="
end

begin "only what this run's purge removed is put back"
echo fail > "$T/installer-mode"
touch "$T/remove-unrelated"
run_default
expect_rc 3
expect_no_call_like "^apt-get install .*unrelated"
end

begin "Secure Boot: a rollback that leaves an unsigned module is reported as failed"
echo fail > "$T/installer-mode"
echo "SecureBoot enabled" > "$T/sb"
touch "$T/no-download" "$T/fail-exact-reinstall"
run_default
expect_rc 4
expect_call_like "^apt-get install -y nvidia-driver-595 "
expect_out "not signed by a key it trusts"
expect_out "Don't reboot"
expect_journal
grep -q '^failed:' "$T/var/lib/green-light/recovery-status" || fail "no note for the app"
end

begin "Secure Boot: a module unsigned before and after is restored as it was"
previous_run_driver
echo "SecureBoot enabled" > "$T/sb"
echo fail > "$T/installer-mode"
run_default
expect_rc 3
end

begin "without Secure Boot, current versions are an acceptable restore"
echo fail > "$T/installer-mode"
touch "$T/no-download" "$T/fail-exact-reinstall"
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

begin "failed rollback is reported as such and keeps the journal"
echo fail > "$T/installer-mode"
touch "$T/fail-reinstall"
run_default
expect_rc 4
expect_out "could not be fully restored"
expect_journal
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
expect_no_journal
end

begin "a missing command (exit 127) is reported as an ordinary failure"
rm "$T/bin/sha256sum"
run_default
expect_rc 1
end

begin "nothing is written to a pipe that can go away"
echo 1 > "$T/installer-wait"
set +o pipefail
sandbox "$T/bin/bash" "$SCRIPT" "$RUN" "$(sha "$RUN")" 2>&1 | head -c 1 > /dev/null
RC="${PIPESTATUS[0]}"
set -o pipefail
OUT="$(cat "$T/var/log/green-light.log")"
expect_rc 0
expect_out "==== Done. Reboot to switch to the new driver. ===="
end

begin "SIGTERM while the module test-builds: stops, nothing changed"
echo 2 > "$T/build-wait"
start_script
sleep 1
kill -TERM "$(script_pid_now)"
wait_script
expect_rc 1
expect_out "Stopped as requested. No changes made."
expect_no_call_like "^(apt-get purge|installer --silent)"
expect_no_journal
end

begin "SIGTERM during the purge: everything is put back"
echo 2 > "$T/purge-wait"
start_script
for _ in $(seq 100); do grep -q "^apt-get purge -y" "$T/calls" && break; sleep 0.1; done
kill -TERM "$(script_pid_now)"
wait_script
expect_rc 3
expect_out "Stopping as requested, before NVIDIA's installer"
expect_no_call_like "^installer --silent"
expect_pkg nvidia-driver-595
expect_no_journal
end

begin "SIGTERM while a successful installer runs: the install is finished, not undone"
echo 2 > "$T/installer-wait"
start_script
kill -TERM "$(wait_for script-pid)"
wait_script
expect_rc 0
expect_out "it had already succeeded, so the install is finished"
expect_out "==== Done."
expect_no_call_like "^nvidia-uninstall"
end

begin "SIGTERM while a failing installer runs: rolled back"
echo 2 > "$T/installer-wait"
echo fail > "$T/installer-mode"
start_script
kill -TERM "$(wait_for script-pid)"
wait_script
expect_rc 3
expect_pkg linux-modules-nvidia-595-generic
end

begin "SIGHUP is ignored: the install finishes"
echo 2 > "$T/installer-wait"
start_script
kill -HUP "$(wait_for script-pid)"
wait_script
expect_rc 0
expect_out "==== Done."
end

begin "a second install while one runs is refused, and so is signing setup"
RC=0
sandbox "$T/bin/bash" -c 'flock /run/green-light-install.lock sleep 3 & sleep 0.5; "$0" "$@"' \
    "$SCRIPT" "$RUN" "$(sha "$RUN")" > /dev/null 2>&1 || RC=$?
OUT="$(cat "$T/var/log/green-light.log")"
expect_rc 2
expect_out "Another Green Light install is still running"
expect_no_call_like "^installer"
RC=0
sandbox "$T/bin/bash" -c 'flock /run/green-light-install.lock sleep 3 & sleep 0.5; echo pass1234 | "$0" --setup-signing' \
    "$SCRIPT" > /dev/null 2>&1 || RC=$?
OUT="$(cat "$T/var/log/green-light.log")"
expect_rc 1
expect_out "A Green Light install is running; set up signing once it has finished."
expect_no_out "==== Module signing setup started"
end

begin "under systemd: its own scope, and shutdown held off while armed"
SANDBOX_EXTRA=(--dir /run/systemd/system)
stub systemd-run 'while [[ $# -gt 0 && "$1" == --* && "$1" != -- ]]; do shift; done
[[ "${1:-}" == -- ]] && shift
exec "$@"'
stub systemd-inhibit 'while [[ "$1" == --* ]]; do shift; done; exec "$@"'
run_default
expect_rc 0
expect_call "systemd-run --scope --quiet --collect true"
expect_call_like "^systemd-run --scope --quiet --collect --unit=green-light-install-[0-9]+ "
expect_call_like "^systemd-inhibit --what=shutdown:sleep --mode=block --who=Green Light "
end

begin "when systemd-run can't make a scope, the install still runs"
SANDBOX_EXTRA=(--dir /run/systemd/system)
stub systemd-run 'exit 1'
run_default
expect_rc 0
expect_out "systemd-run could not give the install a scope of its own"
expect_out "==== Done."
end

# ── Power loss / SIGKILL, the journal and recovery ─────────────────────

# Kill the script and the fake installer outright, as a power loss would.
kill_hard() {
    local p
    for p in "$@"; do kill -KILL "$p" 2>/dev/null || true; done
    wait "$BG" 2>/dev/null || true
}
recover() { run_script --recover; }
# Cut an install off during its purge, leaving a journal in phase armed
# (the stubbed purge itself still finishes).
cut_off_during_purge() {
    echo 3 > "$T/purge-wait"
    start_script
    for _ in $(seq 100); do grep -q "^apt-get purge -y" "$T/calls" && break; sleep 0.1; done
    sleep 0.3
    kill_hard "$(script_pid_now)"
    sleep 3
    rm -f "$T/purge-wait"
}

begin "cut off during the purge (armed): --recover puts it back, offline"
cut_off_during_purge
expect_journal
grep -qx armed "$T/var/lib/green-light/journal/phase" || fail "phase is not armed"
touch "$T/offline"
: > "$T/calls"
recover
expect_rc 3
expect_out "An earlier install was cut off (armed)"
expect_call "dpkg --configure -a"
expect_no_call_like "^apt-get (download|install -y nvidia)"
expect_pkg nvidia-driver-595
expect_pkg linux-modules-nvidia-595-generic
expect_call "apt-mark hold libnvidia-gl-595:i386"
expect_no_journal
end

begin "cut off while the installer ran: the boot unit's command rolls back"
echo 30 > "$T/installer-wait"
echo partial > "$T/installer-mode"
start_script
script="$(wait_for script-pid)"
installer="$(wait_for installer-pid)"
kill_hard "$script" "$installer"
grep -qx installer-started "$T/var/lib/green-light/journal/phase" || fail "phase is not installer-started"
# The new driver had put files in place before the cut.
echo 595.84 > "$T/run-version"
printf '#!/bin/bash\necho "nvidia-uninstall $*" >> "$GLT/calls"\nrm -f "$GLT/run-version" "$GLT/bin/nvidia-uninstall"\n' > "$T/bin/nvidia-uninstall"
chmod 755 "$T/bin/nvidia-uninstall"
: > "$T/calls"
EXEC="$(grep '^ExecStart=' "$UNIT" | cut -d= -f2-)"
[[ "$EXEC" == "/usr/lib/green-light/privileged-install.sh --recover" ]] || fail "unexpected ExecStart: $EXEC"
grep -qx 'ConditionPathExists=|/var/lib/green-light/journal' "$UNIT" || fail "unit not conditional on the journal"
grep -qx 'ConditionPathExists=|/var/lib/green-light/journal.old' "$UNIT" || fail "unit not conditional on a journal moved aside"
grep -qx 'SuccessExitStatus=3 6' "$UNIT" || fail "unit's success codes"
# As systemd runs it: INVOCATION_ID set, output kept for the journal.
SANDBOX_EXTRA=(--setenv INVOCATION_ID test)
recover
expect_rc 3
expect_call "nvidia-uninstall --silent"
expect_pkg nvidia-driver-595
expect_no_journal
grep -q "Restored and checked the previous driver setup" "$T/stdout" || fail "nothing on the unit's output"
expect_no_call_like "^systemd-run"
end

begin "cut off after the installer had finished: recovery only clears the journal"
echo 30 > "$T/initramfs-wait"
start_script
for _ in $(seq 150); do grep -q "^update-initramfs" "$T/calls" && break; sleep 0.1; done
kill_hard "$(script_pid_now)"
mkdir -p "$T/var/lib/green-light/journal"
echo installer-done > "$T/var/lib/green-light/journal/phase"
: > "$T/calls"
recover
expect_rc 0
expect_out "nothing to put back"
expect_no_call_like "^apt-get install"
expect_no_journal
end

begin "cut off while the journal was replaced: the one moved aside is recovered"
cut_off_during_purge
mv "$T/var/lib/green-light/journal" "$T/var/lib/green-light/journal.old"
: > "$T/calls"
recover
expect_rc 3
expect_pkg nvidia-driver-595
expect_no_journal
expect_no_file "$T/var/lib/green-light/journal.old"
end

begin "cut off while the journal was cleared: the cleared one is not rolled back"
mkdir -p "$T/var/lib/green-light/journal.dead"
echo armed > "$T/var/lib/green-light/journal.dead/phase"
: > "$T/calls"
recover
expect_rc 0
expect_no_call_like "^apt-get install"
expect_no_journal
expect_no_file "$T/var/lib/green-light/journal.dead"
end

begin "recovery at boot into a newer kernel: that kernel gets its module too"
cut_off_during_purge
mkdir -p "$T/modules/$KVER2"
echo "$KVER2" > "$T/uname"
: > "$T/calls"
recover
expect_rc 3
expect_call "apt-get install -y linux-modules-nvidia-595-$KVER2"
expect_call "update-initramfs -u -k $KVER"
expect_call "update-initramfs -u -k $KVER2"
expect_no_journal
end

begin "a newer kernel the previous driver had a module for must get one back"
mkdir -p "$T/modules/$KVER2"
echo 595.84 > "$T/mod-version.$KVER2"
cut_off_during_purge
rm -f "$T/mod-version.$KVER2"
echo "$KVER2" > "$T/uname"
touch "$T/offline"
recover
expect_rc 4
expect_out "There is no nvidia kernel module for $KVER2 (there was"
expect_journal
end

begin "a newer kernel the previous driver never covered is a warning, not a failure"
cut_off_during_purge
mkdir -p "$T/modules/$KVER2"
echo "$KVER2" > "$T/uname"
touch "$T/offline"
recover
expect_rc 3
expect_out "WARNING: There is no nvidia kernel module for $KVER2, which the previous driver didn't have one for either"
expect_no_journal
end

begin "recovery into a newer kernel builds a DKMS module for it"
# A DKMS setup: the module comes from nvidia-dkms-595, built per kernel.
printf '%s\n' 'ii |nvidia-driver-595|595.84-0ubuntu1' 'ii |nvidia-dkms-595|595.84-0ubuntu1' \
    'ii |bash|5.2-1' > "$T/dpkg"
echo 'nvidia-driver-595 nvidia-dkms-595' > "$T/revdeps"
echo 'nvidia-dkms-595' > "$T/mod-pkg"
echo 'nvidia-dkms-595' > "$T/auto-pkgs"
: > "$T/held"
: > "$T/mod-signer"
cut_off_during_purge
mkdir -p "$T/modules/$KVER2"
echo "$KVER2" > "$T/uname"
touch "$T/dkms-builds"
: > "$T/calls"
recover
expect_rc 3
expect_call "dkms autoinstall -k $KVER2"
grep -q 595.84 "$T/mod-version.$KVER2" || fail "no module for $KVER2"
end

begin "a renamed .run still counts as this install's driver after a cut-off"
RENAMED="$T/home/NVIDIA-Linux-x86_64-595.84 (1).run"
mv "$RUN" "$RENAMED"
RUN="$RENAMED"
echo 30 > "$T/installer-wait"
echo partial > "$T/installer-mode"
start_script
kill_hard "$(wait_for script-pid)" "$(wait_for installer-pid)"
grep -q 'TARGET_RUN_VER="595.84"' "$T/var/lib/green-light/journal/state" || fail "target version not recorded"
echo 595.84 > "$T/run-version"
printf '#!/bin/bash\necho "nvidia-uninstall $*" >> "$GLT/calls"\nrm -f "$GLT/run-version" "$GLT/bin/nvidia-uninstall"\n' > "$T/bin/nvidia-uninstall"
chmod 755 "$T/bin/nvidia-uninstall"
recover
expect_rc 3
expect_call "nvidia-uninstall --silent"
end

begin "an interrupted recovery recognises what it had already reinstalled"
touch "$T/no-download" "$T/fail-exact-reinstall"
cut_off_during_purge
# The recovery installs current versions, then is cut off while it
# rebuilds the initramfs.
echo 30 > "$T/initramfs-wait"
sandbox "$T/bin/bash" "$SCRIPT" --recover > /dev/null 2>&1 &
BG=$!
for _ in $(seq 150); do grep -q "^update-initramfs" "$T/calls" && break; sleep 0.1; done
for p in $(pgrep -f "$SCRIPT --recover" || true); do kill -KILL "$p" 2>/dev/null || true; done
wait "$BG" 2>/dev/null || true
rm -f "$T/initramfs-wait"
grep -q 'nvidia-driver-595=current' "$T/var/lib/green-light/journal/reinstalled" \
    || fail "the reinstall was not recorded before the cut"
recover
expect_rc 3
expect_no_out "Not restoring from the recovery data"
end

begin "a carried journal keeps the earlier install's driver and installer state"
echo 30 > "$T/installer-wait"
echo partial > "$T/installer-mode"
start_script
kill_hard "$(wait_for script-pid)" "$(wait_for installer-pid)"
rm -f "$T/script-pid" "$T/installer-pid"
echo 595.84 > "$T/run-version"
# Its uninstaller fails at first, so restoring fails and the journal is carried.
printf '#!/bin/bash\necho "nvidia-uninstall $*" >> "$GLT/calls"\n[[ -e "$GLT/uninstall-works" ]] || exit 1\nrm -f "$GLT/run-version" "$GLT/bin/nvidia-uninstall"\n' > "$T/bin/nvidia-uninstall"
chmod 755 "$T/bin/nvidia-uninstall"
# A second install, of another driver, is cut off in turn.
RUN2="$T/home/NVIDIA-Linux-x86_64-600.0.run"
sed 's/Linux-x86_64 595.84"/Linux-x86_64 600.0"/' "$RUN" > "$RUN2"
chmod 644 "$RUN2"
echo fail > "$T/installer-mode"
LOG_BEFORE="$(log_size)"
sandbox "$T/bin/bash" "$SCRIPT" "$RUN2" "$(sha "$RUN2")" > /dev/null 2>&1 &
BG=$!
kill_hard "$(wait_for script-pid)" "$(wait_for installer-pid)"
OUT="$(tail -c +"$((LOG_BEFORE + 1))" "$T/var/log/green-light.log")"
expect_out "keeps its recovery data as the starting point"
STATE="$T/var/lib/green-light/journal/state"
grep -q 'KNOWN_TARGETS=(\[0\]="595.84")' "$STATE" || fail "earlier target not kept"
grep -q 'EARLIER_INSTALLER_STARTED="1"' "$STATE" || fail "earlier installer state not kept"
touch "$T/uninstall-works"
recover
expect_rc 3
expect_no_out "Not restoring from the recovery data"
expect_no_file "$T/run-version"
end

begin "a journal left behind is dealt with before the next install"
cut_off_during_purge
run_default
expect_rc 0
expect_out "An earlier install was cut off (armed); putting the driver setup it changed back first"
expect_out "Restored and checked the previous driver setup"
expect_out "==== Done."
expect_no_journal
end

begin "a journal whose driver was changed since is left alone, until discarded"
cut_off_during_purge
other_driver_installed_since
: > "$T/calls"
recover
expect_rc 6
expect_out "Not restoring from the recovery data of an earlier interrupted install"
expect_out "the driver installed now from NVIDIA's installer (600.1)"
expect_no_call_like "^apt-get install"
expect_journal
grep -q '^refused:' "$T/var/lib/green-light/recovery-status" || fail "no note for the app"
run_default
expect_rc 6
expect_out "discard it to install. No changes made."
expect_no_call_like "^apt-get purge"
run_script --discard-recovery
expect_rc 0
expect_no_journal
expect_no_file "$T/var/lib/green-light/recovery-status"
end

begin "an install asked to discard stale recovery data goes ahead"
cut_off_during_purge
other_driver_installed_since
run_default --discard-recovery
expect_rc 0
expect_out "Discarding the recovery data of an earlier interrupted install, as asked."
expect_out "==== Done."
end

begin "distro driver packages installed since also make a journal stale"
cut_off_during_purge
echo 'ii |nvidia-driver-610|610.1-0ubuntu1' >> "$T/dpkg"
recover
expect_rc 6
expect_out "the distribution driver package nvidia-driver-610=610.1-0ubuntu1 was installed after it"
end

begin "a failed recovery's baseline is carried into the next install"
cut_off_during_purge
echo 2 > "$T/fail-reinstall-times"
echo fail > "$T/installer-mode"
run_default
expect_rc 3
expect_out "this install keeps its recovery data as the starting point to restore"
# The rollback of the new install restores the original setup, from the
# copies the first install kept.
expect_call_like "^apt-get install -y --allow-downgrades /var/lib/green-light/journal/cache/nvidia-driver-595_595.84-0ubuntu1_amd64.deb"
expect_pkg nvidia-driver-595
grep -q '|nvidia-driver-595|595.84-0ubuntu1' "$T/dpkg" || fail "not the original version"
expect_no_journal
end

begin "a carried baseline's kernel is rebuilt by the rollback of an install on a newer one"
cut_off_during_purge
echo 2 > "$T/fail-reinstall-times"
echo fail > "$T/installer-mode"
mkdir -p "$T/modules/$KVER2/build/include/generated"
touch "$T/modules/$KVER2/build/Makefile"
echo '#define CONFIG_CC_IS_GCC 1' > "$T/modules/$KVER2/build/include/generated/autoconf.h"
echo "$KVER2" > "$T/uname"
: > "$T/calls"
run_default
expect_out "this install keeps its recovery data as the starting point to restore"
# After this install's own installer: its rollback covers the kernel the
# carried baseline was recorded on, not only the running one.
LAST_INSTALLER="$(grep -n '^installer ' "$T/calls" | tail -1 | cut -d: -f1)"
tail -n +"$LAST_INSTALLER" "$T/calls" | grep -qxF "update-initramfs -u -k $KVER" \
    || fail "the rollback did not rebuild $KVER, the carried baseline's kernel"
tail -n +"$LAST_INSTALLER" "$T/calls" | grep -qxF "update-initramfs -u -k $KVER2" \
    || fail "the rollback did not rebuild the running kernel $KVER2"
end

begin "discarding with no journal left removes a stale recovery note"
mkdir -p "$T/var/lib/green-light"
echo "refused: the driver was changed since" > "$T/var/lib/green-light/recovery-status"
run_script --discard-recovery
expect_rc 0
expect_out "Recovery data discarded."
expect_no_file "$T/var/lib/green-light/recovery-status"
end

begin "--recover with no journal does nothing"
recover
expect_rc 0
expect_out "Nothing to recover."
end

begin "--recover takes the shutdown inhibitor too"
cut_off_during_purge
SANDBOX_EXTRA=(--dir /run/systemd/system)
stub systemd-run 'while [[ $# -gt 0 && "$1" == --* && "$1" != -- ]]; do shift; done
[[ "${1:-}" == -- ]] && shift
exec "$@"'
stub systemd-inhibit 'while [[ "$1" == --* ]]; do shift; done; exec "$@"'
: > "$T/calls"
recover
expect_rc 3
expect_call_like "^systemd-inhibit --what=shutdown:sleep --mode=block "
end

if command -v systemd-analyze >/dev/null 2>&1; then
    begin "systemd-analyze verify accepts the boot unit"
    sed "s#^ExecStart=/usr/lib/green-light/privileged-install.sh#ExecStart=$SCRIPT#" "$UNIT" \
        > "$T/green-light-recover.service"
    # Units it is ordered against may not exist on this machine (no display
    # manager, akmods or dkms); that is not an error in the unit.
    if ! OUT="$(systemd-analyze verify "$T/green-light-recover.service" 2>&1)" \
        && grep -vE '(display-manager|akmods|dkms)\.service' <<<"$OUT" | grep -q .; then
        fail "systemd-analyze verify failed"
    fi
    end
fi

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
# akmod-nvidia's module comes from kmod-nvidia-<kernel>, built here by akmods.
# Offline, dnf fails to load repository metadata unless all repos are off.
dnf_case() {
    begin "$1"
    unstub apt-get; unstub apt-mark; unstub dpkg-query; unstub update-initramfs; unstub lsinitramfs
    printf '%s\n' 'akmod-nvidia.x86_64|akmod-nvidia-595.84-1.fc44.x86_64' \
        "kmod-nvidia-$KVER.x86_64|kmod-nvidia-$KVER-595.84-1.fc44.x86_64" \
        'xorg-x11-drv-nvidia.x86_64|xorg-x11-drv-nvidia-595.84-1.fc44.x86_64' \
        'nvidia-persistenced.x86_64|nvidia-persistenced-595.84-1.fc44.x86_64' \
        'bash.x86_64|bash-5.2-1.fc44.x86_64' > "$T/rpm"
    echo 'xorg-x11-drv-nvidia nvidia-persistenced' > "$T/revdeps"
    echo "kmod-nvidia-$KVER" > "$T/mod-pkg"
    echo 'nvidia-persistenced.x86_64' > "$T/auto-pkgs"
    stub rpm 'if [[ "$*" == *ARCH* ]]; then cat "$GLT/rpm"; else
  while IFS="|" read -r key _; do n="${key%.*}"
    [[ "$n" =~ ^(akmod-nvidia|xorg-x11-drv-nvidia|kmod-nvidia|nvidia-driver|nvidia-settings) ]] && echo "$n"
  done < "$GLT/rpm"; fi'
    stub dnf 'cmd="$1"; shift
tolerant=0; norepos=0
for a in "$@"; do
  [[ "$a" == --skip-unavailable || "$a" == --setopt=strict=0 ]] && tolerant=1
  [[ "$a" == "--disablerepo=*" ]] && norepos=1
done
# Expired metadata and no network: loading the repositories fails.
if [[ -e "$GLT/offline" && $norepos -eq 0 && "$cmd" != repoquery ]]; then echo "Failed to download metadata"; exit 1; fi
case "$cmd" in
  remove)
    sim=0; [[ "$1" == --assumeno ]] && { sim=1; shift; echo "Package   Arch   Version   Repository   Size"; echo "Removing:"; }
    for p in "$@"; do [[ "$p" == -* ]] && continue
      for d in $p $(grep "^$p " "$GLT/revdeps" | cut -d" " -f2); do
        if [[ $sim -eq 1 ]]; then echo " $d   x86_64   595.84-1.fc44   @System   1 M"; continue; fi
        grep "^$d\." "$GLT/rpm" >> "$GLT/rpm.removed"; grep -v "^$d\." "$GLT/rpm" > "$GLT/rpm.new"; mv "$GLT/rpm.new" "$GLT/rpm"
        if grep -qx "$d" "$GLT/mod-pkg"; then mv "$GLT/mod-version" "$GLT/mod-version.off"; fi
      done
    done
    [[ $sim -eq 1 ]] && { echo; echo "Transaction Summary:"; exit 1; } ;;
  download)
    dest="$2"; spec="$3"; grep -qF "|$spec" "$GLT/rpm" && [[ "$spec" != kmod-nvidia-6* ]] && touch "$dest/$spec.rpm" ;;
  repoquery)
    if [[ "$*" == *--userinstalled* ]]; then cut -d"|" -f1 "$GLT/rpm" | grep -vxF -f "$GLT/auto-pkgs"; else cut -d"|" -f1 "$GLT/rpm"; fi ;;
  install)
    if [[ "$*" == *nvidia* ]]; then
      [[ -e "$GLT/fail-reinstall" ]] && exit 1
      for p in "$@"; do [[ "$p" == -* ]] && continue
        # With the repositories off only local files can be installed.
        [[ $norepos -eq 1 && "$p" != /* ]] && exit 1
        spec="${p##*/}"; spec="${spec%.rpm}"
        # akmods builds kmod-nvidia-<kernel> here; no repository has it.
        if [[ "$spec" == kmod-nvidia-6* && "$p" != /* ]]; then [[ $tolerant -eq 1 ]] && continue; exit 1; fi
        grep -F "|$spec" "$GLT/rpm.removed" >> "$GLT/rpm" || true
        if [[ "$spec" == "$(cat "$GLT/mod-pkg")"-* && -f "$GLT/mod-version.off" ]]; then mv "$GLT/mod-version.off" "$GLT/mod-version"; fi
      done
    fi ;;
esac
exit 0'
    # akmods: builds the module package for the kernel and installs it.
    stub akmods '[[ -e /proc/self/fd/9 ]] && touch "$GLT/lock-leaked-to-akmods"
grep "^kmod-nvidia-" "$GLT/rpm.removed" >> "$GLT/rpm"; [[ -f "$GLT/mod-version.off" ]] && mv "$GLT/mod-version.off" "$GLT/mod-version"; exit 0'
    stub dracut 'touch "/boot/initramfs-$3.img"'
    stub lsinitrd 'echo usr/lib/modprobe.d/blacklist-nouveau.conf'
}

dnf_case "dnf5 install with block"
stub dnf5 'exit 0'
run_default --block-distro-driver
expect_rc 0
expect_call "dnf remove -y akmod-nvidia kmod-nvidia-$KVER xorg-x11-drv-nvidia"
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

dnf_case "dnf rollback: kept copies with repositories off, akmods rebuilds the local kmod"
echo fail > "$T/installer-mode"
run_default
expect_rc 3
expect_call_like "^dnf install -y --disablerepo=\* /var/lib/green-light/journal/cache/akmod-nvidia-595.84-1.fc44.x86_64.rpm"
expect_call_like "nvidia-persistenced-595.84-1.fc44.x86_64.rpm"
expect_no_call_like "^dnf install .*kmod-nvidia-$KVER-"
expect_call "akmods --force --kernels $KVER"
expect_no_file "$T/lock-leaked-to-akmods"
expect_call "dnf mark remove nvidia-persistenced"
grep -q '^nvidia-persistenced\.' "$T/rpm" || fail "nvidia-persistenced not reinstalled"
grep -q "^kmod-nvidia-$KVER\." "$T/rpm" || fail "kmod not rebuilt"
expect_out "Restored and checked the previous driver setup"
end

dnf_case "dnf rollback with no network and expired metadata works from the kept copies"
echo fail > "$T/installer-mode"
touch "$T/offline-at-installer"
run_default
expect_rc 3
expect_call_like "^dnf install -y --disablerepo=\* "
expect_no_call_like "^dnf install -y --setopt=strict=0"
expect_out "Restored and checked the previous driver setup"
end

dnf_case "dnf5: akmods' kept kmod for this kernel is installed, nothing rebuilt"
stub dnf5 'exit 0'
echo fail > "$T/installer-mode"
touch "$T/var/cache/akmods/nvidia/kmod-nvidia-$KVER-595.84-1.fc44.x86_64.rpm"
run_default
expect_rc 3
expect_call_like "^dnf install -y --disablerepo=\* .*kmod-nvidia-$KVER-595.84-1.fc44.x86_64.rpm"
expect_no_call_like "^akmods"
expect_call "dnf mark dependency nvidia-persistenced"
end

# ── Signing setup: one enrollment request, one password ────────────────
# shim keeps one waiting request with one password; another tool's key
# already waiting (a kernel-signing key) must be carried over into this
# request, never silently left needing a different password.
mok_case() {   # sets up the tools, a module-signing key and an efivars dir
    local tool
    for tool in perl openssl sed tr; do ln -sf "$(readlink -f "$(type -P "$tool")")" "$T/bin/$tool"; done
    mkdir -p "$T/var/lib/shim-signed/mok" "$T/efivars" "$T/other"
    openssl req -new -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=Test Module Signing key" \
        -keyout "$T/var/lib/shim-signed/mok/MOK.priv" -outform DER -out "$T/var/lib/shim-signed/mok/MOK.der" >/dev/null 2>&1
    openssl req -new -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=Other app key" \
        -keyout "$T/other/k.pem" -outform DER -out "$T/other/c.der" >/dev/null 2>&1
    # A tmpfs over /sys/firmware: the efivars path exists in the sandbox even on
    # machines without EFI (CI runners).
    SANDBOX_EXTRA=(--tmpfs /sys/firmware --bind "$T/efivars" /sys/firmware/efi/efivars)
    # --import: records the subjects and writes a MokNew holding the
    # certificates; $GLT/import-fails: how many imports fail before one works.
    stub mokutil 'case "$1" in
  --test-key)
    # Like mokutil: a key in the built-in keyring is reported on stderr with exit 255.
    if [[ -e "$GLT/builtin" ]]; then echo "$2 is already in the built-in trusted keyring" >&2; exit 255; fi
    if [[ -e "$GLT/enrolled" ]]; then echo "$2 is already enrolled"; else echo "$2 is not enrolled"; fi ;;
  --generate-hash) read -r a; read -r b; printf "\x246\x24salt\x24hash\n" ;;
  --revoke-import) rm -f /sys/firmware/efi/efivars/MokNew-* ;;
  --import)
    if [[ -s "$GLT/import-fails" ]] && (( $(cat "$GLT/import-fails") > 0 )); then
      echo $(( $(cat "$GLT/import-fails") - 1 )) > "$GLT/import-fails"; exit 1
    fi
    shift; files=()
    for f in "$@"; do [[ "$f" == --hash-file ]] && break; files+=("$f"); openssl x509 -inform DER -in "$f" -noout -subject -nameopt RFC2253 >> "$GLT/imported"; done
    { printf "attr"; cat "${files[@]}"; } > /sys/firmware/efi/efivars/MokNew-605dab50-e046-4300-abb6-3dd810dd8b23 ;;
esac'
    stub mount 'exit "$(cat "$GLT/mount-rc" 2>/dev/null || echo 0)"'
}
MOKNEW_NAME="MokNew-605dab50-e046-4300-abb6-3dd810dd8b23"
write_moknew() {   # <type: x509|sha256> <der>...: an efivarfs MokNew with one list per file
    local kind="$1"; shift
    perl -e '
        my $kind = shift;
        my %g = (x509 => "a159c0a5e494a74a87b5ab155c2bf072", sha256 => "2616c4c14c509240aca941f936934328");
        my $out = pack("V", 7);
        for my $f (@ARGV) {
            open(my $h, "<:raw", $f) or die; local $/; my $d = <$h>;
            my $entry = ("\x11" x 16) . $d;
            $out .= pack("H*", $g{$kind}) . pack("V3", 28 + length $entry, 0, length $entry) . $entry;
        }
        binmode STDOUT; print $out;
    ' "$kind" "$@" > "$T/efivars/$MOKNEW_NAME"
}
# One x509 list whose sizes are off: <header size> <extra bytes in the list>.
malformed_moknew() {
    perl -e '
        my ($hsize, $adj, $f) = @ARGV;
        open(my $h, "<:raw", $f) or die; local $/; my $d = <$h>;
        my $entry = ("\x11" x 16) . $d;
        binmode STDOUT;
        print pack("V", 7) . pack("H*", "a159c0a5e494a74a87b5ab155c2bf072")
            . pack("V3", 28 + length($entry) + $adj, $hsize, length $entry) . $entry . ("\0" x $adj);
    ' "$1" "$2" "$T/other/c.der" > "$T/efivars/$MOKNEW_NAME"
}
fpr() { openssl x509 -inform DER -in "$1" -noout -fingerprint -sha1 | sed 's/.*=//; s/://g' | tr 'A-F' 'a-f'; }
# Signing setup with <password> on stdin; sets RC and OUT (this run's log).
setup_signing() {
    local before
    before="$(log_size)"
    RC=0
    sandbox "$T/bin/bash" -c 'printf "%s\n" "$1" | "$0" --setup-signing' "$SCRIPT" "$1" > /dev/null 2>&1 || RC=$?
    OUT="$(tail -c +"$((before + 1))" "$T/var/log/green-light.log" 2>/dev/null)"
}
list_fps() { tail -n +2 "$T/var/lib/mok-pending/list" | sort | tr '\n' ' '; }
fps_of() { local f; for f in "$@"; do fpr "$f"; done | sort | tr '\n' ' '; }
list_size_ok() { [[ "$(head -1 "$T/var/lib/mok-pending/list")" == "moknew-size $(stat -c %s "$T/efivars/$MOKNEW_NAME")" ]]; }

begin "signing setup carries a key another tool queued into one request"
mok_case
write_moknew x509 "$T/other/c.der"
RC=0
sandbox "$T/bin/bash" -c 'echo pass1234 | "$0" --setup-signing' "$SCRIPT" > /dev/null 2>&1 || RC=$?
OUT="$(cat "$T/var/log/green-light.log")"
expect_rc 0
expect_out "To enroll: Other app key (already waiting for enrollment)"
expect_out "To enroll: Test Module Signing key"
expect_out "That one password enrolls all of these keys"
expect_call "mokutil --revoke-import"
[[ "$(sort "$T/imported" | tr '\n' '|')" == "subject=CN=Other app key|subject=CN=Test Module Signing key|" ]] \
    || fail "imported [$(tr '\n' '|' < "$T/imported" 2>/dev/null)]"
[[ "$(list_fps)" == "$(fps_of "$T/other/c.der" "$T/var/lib/shim-signed/mok/MOK.der")" ]] \
    || fail "the shared pending list is wrong: $(cat "$T/var/lib/mok-pending/list" 2>/dev/null)"
list_size_ok || fail "the shared list must record MokNew's size: $(head -1 "$T/var/lib/mok-pending/list" 2>/dev/null)"
[[ "$(stat -c %a "$T/var/lib/mok-pending/list")" == 644 && "$(stat -c %a "$T/var/lib/mok-pending")" == 755 ]] \
    || fail "the shared list must be readable by the apps"
[[ -z "$(compgen -G "$T/var/lib/mok-pending/.list.*")" ]] || fail "a temp list was left behind"
grep -q pass1234 "$T/calls" && fail "the password reached an argument list"
end

begin "signing setup installs a missing mokutil without passing on the lock"
mok_case
mv "$T/bin/mokutil" "$T/mokutil.saved"
stub apt-get '[[ -e /proc/self/fd/9 ]] && touch "$GLT/lock-leaked-to-apt"
[[ "$1 $3" == "install mokutil" ]] && cp "$GLT/mokutil.saved" "$GLT/bin/mokutil"
exit 0'
setup_signing pass1234
expect_rc 0
expect_call "apt-get install -y mokutil"
expect_no_file "$T/lock-leaked-to-apt"
end

begin "signing setup re-queues its own waiting key under the new password"
mok_case
write_moknew x509 "$T/var/lib/shim-signed/mok/MOK.der"
RC=0
sandbox "$T/bin/bash" -c 'echo pass5678 | "$0" --setup-signing' "$SCRIPT" > /dev/null 2>&1 || RC=$?
OUT="$(cat "$T/var/log/green-light.log")"
expect_rc 0
expect_call "mokutil --revoke-import"
[[ "$(cat "$T/imported")" == "subject=CN=Test Module Signing key" ]] || fail "imported [$(cat "$T/imported" 2>/dev/null)]"
end

begin "signing setup with every key in the built-in keyring queues nothing (mokutil exits 255)"
mok_case
touch "$T/builtin"
RC=0
sandbox "$T/bin/bash" -c 'echo pass5678 | "$0" --setup-signing' "$SCRIPT" > /dev/null 2>&1 || RC=$?
OUT="$(cat "$T/var/log/green-light.log")"
expect_rc 0
expect_out "already enrolled; nothing to do"
expect_no_call_like "^mokutil --import"
end

begin "signing setup with every key enrolled queues nothing"
mok_case
touch "$T/enrolled"
RC=0
sandbox "$T/bin/bash" -c 'echo pass5678 | "$0" --setup-signing' "$SCRIPT" > /dev/null 2>&1 || RC=$?
OUT="$(cat "$T/var/log/green-light.log")"
expect_rc 0
expect_out "already enrolled; nothing to do"
expect_no_call_like "^mokutil --import"
end

begin "signing setup with every key enrolled removes the shared list"
mok_case
write_moknew x509 "$T/other/c.der"
mkdir -p "$T/var/lib/mok-pending"; printf 'moknew-size 1\n%s\n' "$(fpr "$T/other/c.der")" > "$T/var/lib/mok-pending/list"
touch "$T/enrolled"
setup_signing pass5678
expect_rc 0
expect_no_file "$T/var/lib/mok-pending/list"
end

begin "signing setup carries over several signature lists"
mok_case
openssl req -new -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=Third key" \
    -keyout "$T/other/k3.pem" -outform DER -out "$T/other/c3.der" >/dev/null 2>&1
write_moknew x509 "$T/other/c.der" "$T/other/c3.der"
setup_signing pass1234
expect_rc 0
expect_out "To enroll: Other app key (already waiting for enrollment)"
expect_out "To enroll: Third key (already waiting for enrollment)"
[[ "$(sort "$T/imported" | tr '\n' '|')" == "subject=CN=Other app key|subject=CN=Test Module Signing key|subject=CN=Third key|" ]] \
    || fail "imported [$(tr '\n' '|' < "$T/imported" 2>/dev/null)]"
[[ "$(list_fps)" == "$(fps_of "$T/other/c.der" "$T/other/c3.der" "$T/var/lib/shim-signed/mok/MOK.der")" ]] \
    || fail "the shared list should hold all three keys"
list_size_ok || fail "the shared list must record MokNew's size"
end

begin "signing setup refuses a malformed waiting request and leaves it alone"
mok_case
for sizes in "4096 0" "0 5"; do
    # shellcheck disable=SC2086  # two arguments
    malformed_moknew $sizes
    cp "$T/efivars/$MOKNEW_NAME" "$T/moknew-before"
    : > "$T/calls"
    setup_signing pass5678
    expect_rc 1
    expect_out "The waiting enrollment request could not be read"
    expect_no_call_like "^mokutil --(revoke-)?import"
    cmp -s "$T/efivars/$MOKNEW_NAME" "$T/moknew-before" || fail "the malformed request ($sizes) was changed"
done
end

begin "signing setup leaves a request with an unreadable certificate alone"
mok_case
printf 'not a certificate' > "$T/other/junk.der"
write_moknew x509 "$T/other/c.der" "$T/other/junk.der"
setup_signing pass5678
expect_rc 1
expect_out "holds an entry that is not a readable certificate, so Green Light will not replace it"
expect_no_call_like "^mokutil --(revoke-)?import"
expect_file "$T/efivars/$MOKNEW_NAME"
end

begin "signing setup retries a failed import once"
mok_case
write_moknew x509 "$T/other/c.der"
echo 1 > "$T/import-fails"
setup_signing pass1234
expect_rc 0
expect_out "mokutil --import failed; trying once more"
[[ "$(grep -c '^mokutil --import' "$T/calls")" == 2 ]] || fail "expected two import calls"
[[ "$(list_fps)" == "$(fps_of "$T/other/c.der" "$T/var/lib/shim-signed/mok/MOK.der")" ]] || fail "the shared list was not written after the retry"
list_size_ok || fail "the shared list must record MokNew's size after the retry"
end

begin "signing setup whose import fails twice names every key no longer queued"
mok_case
write_moknew x509 "$T/other/c.der"
mkdir -p "$T/var/lib/mok-pending"; printf 'moknew-size 1\n%s\n' "$(fpr "$T/other/c.der")" > "$T/var/lib/mok-pending/list"
echo 2 > "$T/import-fails"
setup_signing pass1234
expect_rc 1
[[ "$(grep -c '^mokutil --import' "$T/calls")" == 2 ]] || fail "the import should be tried exactly twice"
expect_out "None of these keys is waiting for enrollment now:"
expect_out "Other app key (SHA1 $(fpr "$T/other/c.der"))"
expect_out "Test Module Signing key (SHA1 $(fpr "$T/var/lib/shim-signed/mok/MOK.der"))"
expect_out "Set up signing again to queue them all."
expect_no_file "$T/var/lib/mok-pending/list"
expect_no_file "$T/efivars/$MOKNEW_NAME"
end

begin "signing setup mounts the EFI variables when they are not mounted"
mok_case
SANDBOX_EXTRA=(--tmpfs /sys/firmware --dir /sys/firmware/efi/efivars)
echo 32 > "$T/mount-rc"
setup_signing pass1234
expect_rc 1
expect_call "mount -t efivarfs -o nosuid,nodev,noexec efivarfs /sys/firmware/efi/efivars"
expect_out "The EFI variables are not mounted and could not be mounted"
expect_no_call_like "^mokutil"
echo 0 > "$T/mount-rc"
: > "$T/calls"
setup_signing pass1234
expect_call "mount -t efivarfs -o nosuid,nodev,noexec efivarfs /sys/firmware/efi/efivars"
expect_out "The EFI variables are not mounted; mounting them"
expect_call_like "^mokutil --import"
end

begin "signing setup does not mount over mounted EFI variables"
mok_case
setup_signing pass1234
expect_rc 0
expect_no_call_like "^mount"
end

begin "signing setup takes only 8 to 16 printable ASCII characters without spaces"
mok_case
for pw in "pass word1" "pässwörd1" "short" "seventeen-chars-x" $'tab\there1'; do
    : > "$T/calls"
    setup_signing "$pw"
    expect_rc 1
    expect_out "must be 8 to 16 printable ASCII characters without spaces"
    expect_no_call_like "^mokutil"
done
setup_signing 'Abc123!@#$%^&*()'
expect_rc 0
end

begin "signing setup leaves a waiting request it cannot carry over alone"
mok_case
write_moknew sha256 "$T/other/c.der"
RC=0
sandbox "$T/bin/bash" -c 'echo pass5678 | "$0" --setup-signing' "$SCRIPT" > /dev/null 2>&1 || RC=$?
OUT="$(cat "$T/var/log/green-light.log")"
expect_rc 1
expect_out "holds more than certificates"
expect_no_call_like "^mokutil --(revoke-)?import"
expect_file "$T/efivars/MokNew-605dab50-e046-4300-abb6-3dd810dd8b23"
end

echo
echo "$PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
