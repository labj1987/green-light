//! The AppRun's library check (the `libcheck` block in build-appimage.sh), run
//! in bash with a fake `ldd`, a fake `ldconfig` and a fake os-release.

use std::path::{Path, PathBuf};
use std::process::Command;

fn block() -> String {
    let script = include_str!("../build-appimage.sh");
    let start = script.find("# >>> libcheck").expect("libcheck block start");
    let end = script.find("# <<< libcheck").expect("libcheck block end");
    script[start..end].to_string()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libcheck-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn script_file(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

const LIBC_ONLY: &str = "\tlibc.so.6 (libc6,x86-64) => /lib64/libc.so.6";

/// A loader cache with everything the app loads by name at run time.
const CACHE_OK: &str = "\tlibc.so.6 (libc6,x86-64) => /lib64/libc.so.6\n\tlibvulkan.so.1 (libc6,x86-64) => /lib64/libvulkan.so.1\n\tlibEGL.so.1 (libc6,x86-64) => /lib64/libEGL.so.1\n\tlibwayland-client.so.0 (libc6,x86-64) => /lib64/libwayland-client.so.0";

/// Runs `call` after the block, with the fake ldd printing `ldd_out`, a complete
/// loader cache and the os-release `os`. Returns (exit status ok, stdout, stderr).
fn run(name: &str, ldd_out: &str, os: &str, call: &str) -> (bool, String, String) {
    run_with_cache(name, ldd_out, CACHE_OK, os, call)
}

/// `run`, with the fake `ldconfig -p` printing `cache`.
fn run_with_cache(name: &str, ldd_out: &str, cache: &str, os: &str, call: &str) -> (bool, String, String) {
    let dir = scratch(name);
    let ldd = dir.join("ldd");
    script_file(&ldd, &format!("#!/bin/sh\ncat <<'OUT'\n{ldd_out}\nOUT\nexit ${{LDD_EXIT:-0}}\n"));
    let ldconfig = dir.join("ldconfig");
    script_file(&ldconfig, &format!("#!/bin/sh\ncat <<'OUT'\n{cache}\nOUT\n"));
    let release = dir.join("os-release");
    std::fs::write(&release, os).unwrap();
    let script = format!("{}\n{call}\n", block());
    let out = Command::new("bash")
        .args(["-c", &script])
        .env("LIBCHECK_LDD", &ldd)
        .env("LIBCHECK_LDCONFIG", &ldconfig)
        .env("LIBCHECK_OS_RELEASE", &release)
        .env("LIBCHECK_DIALOG", "none")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("LD_LIBRARY_PATH")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(Path::new(&dir));
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

const MISSING: &str =
    "\tlinux-vdso.so.1 (0x00007ffd)\n\tlibxkbcommon-x11.so.0 => not found\n\tlibc.so.6 => /lib64/libc.so.6 (0x00007f)\n\tlibxcb.so.1 => not found";

#[test]
fn missing_libraries_are_listed_one_per_line() {
    let (ok, out, _) = run("list", MISSING, "ID=fedora\n", "libcheck_scan /x; printf '%s\\n' \"${LIBCHECK_LIBS[@]}\"");
    assert!(ok);
    assert_eq!(out, "libxkbcommon-x11.so.0\nlibxcb.so.1\n");
}

#[test]
fn nothing_missing_passes() {
    let (ok, out, err) = run("none", "\tlibc.so.6 => /lib64/libc.so.6 (0x1)", "ID=ubuntu\n", "export WAYLAND_DISPLAY=wayland-0; libcheck_run 'App' /x");
    assert!(ok, "{err}");
    assert!(out.is_empty() && err.is_empty());
}

#[test]
fn missing_libraries_stop_the_launch_with_a_message() {
    let (ok, _, err) = run("fail", MISSING, "ID=fedora\nVERSION_ID=45\n", "libcheck_run 'Green Light' /x");
    assert!(!ok);
    assert!(err.contains("missing: libxkbcommon-x11.so.0, libxcb.so.1."), "{err}");
    assert!(err.contains("sudo dnf install 'libxkbcommon-x11.so.0()(64bit)' 'libxcb.so.1()(64bit)'"), "{err}");
    // Nothing is bundled and no GTK is involved: the message must not send people after one.
    assert!(err.contains("Green Light takes them from the system") && !err.contains("GTK"), "{err}");
}

#[test]
fn the_install_hint_follows_the_distribution() {
    for (os, want) in [
        ("ID=fedora\n", "Install them with: sudo dnf install 'libEGL.so.1()(64bit)'."),
        ("ID=nobara\nID_LIKE=\"rhel centos fedora\"\n", "Install them with: sudo dnf install 'libEGL.so.1()(64bit)'."),
        ("ID=opensuse-tumbleweed\nID_LIKE=\"opensuse suse\"\n", "Install them with: sudo zypper install 'libEGL.so.1()(64bit)'."),
        ("ID=ubuntu\nID_LIKE=debian\n", "Install them with: sudo apt install libegl1."),
        ("ID=pop\nID_LIKE=\"ubuntu debian\"\n", "Install them with: sudo apt install libegl1."),
        ("ID=cachyos\nID_LIKE=arch\n", "Install them with: sudo pacman -S libglvnd."),
        ("ID=somethingelse\n", ""),
    ] {
        let (ok, out, _) = run("hint", "", os, "libcheck_install_hint libEGL.so.1");
        assert!(ok);
        assert_eq!(out.trim(), want, "{os}");
    }
}

#[test]
fn every_library_the_app_takes_from_the_system_has_its_package() {
    // The linked ones (see `readelf -d` on the binary) and the ones loaded by name.
    let libs = "libxcb.so.1 libxkbcommon.so.0 libxkbcommon-x11.so.0 libwayland-client.so.0 libvulkan.so.1 libEGL.so.1";
    let (_, out, _) = run("hint-all-apt", "", "ID=debian\n", &format!("libcheck_install_hint {libs}"));
    assert_eq!(out.trim(), "Install them with: sudo apt install libxcb1 libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libvulkan1 libegl1.");
    let (_, out, _) = run("hint-all-arch", "", "ID=arch\n", &format!("libcheck_install_hint {libs}"));
    assert_eq!(out.trim(), "Install them with: sudo pacman -S libxcb libxkbcommon libxkbcommon-x11 wayland vulkan-icd-loader libglvnd.");
}

#[test]
fn apt_and_pacman_name_the_packages_and_how_to_find_the_rest() {
    let (_, out, _) = run("hint-apt", "", "ID=debian\n", "libcheck_install_hint libEGL.so.1 libvulkan.so.1 libfoo.so.2 libbar.so.1");
    assert_eq!(
        out.trim(),
        "Install them with: sudo apt install libegl1 libvulkan1. Find the package that provides each of the others with: apt-file search libfoo.so.2; apt-file search libbar.so.1."
    );
    // A package is named once.
    let (_, out, _) = run("hint-arch", "", "ID=arch\n", "libcheck_install_hint libEGL.so.1 libEGL.so.1 libfoo.so.2");
    assert_eq!(out.trim(), "Install them with: sudo pacman -S libglvnd. Find the package that provides each of the others with: pacman -F libfoo.so.2.");
    let (_, out, _) = run("hint-none", "", "ID=ubuntu\n", "libcheck_install_hint libfoo.so.2");
    assert_eq!(out.trim(), "Find the package that provides each of them with: apt-file search libfoo.so.2.");
    let (ok, _, err) = run("hint-msg", "\tlibxkbcommon.so.0 => not found", "ID=ubuntu\n", "libcheck_run 'Test App' /x");
    assert!(!ok);
    assert!(
        err.contains("missing: libxkbcommon.so.0.") && err.ends_with("Install them with: sudo apt install libxkbcommon0. Then start Test App again.\n"),
        "{err}"
    );
}

#[test]
fn an_unknown_distribution_still_gets_a_message() {
    let (ok, _, err) = run("generic", MISSING, "ID=somethingelse\n", "libcheck_run 'Green Light' /x");
    assert!(!ok);
    assert!(err.contains("libxkbcommon-x11.so.0") && err.contains("package manager"), "{err}");
}

#[test]
fn the_apprun_checks_the_libraries_and_then_starts_the_one_binary() {
    let script = include_str!("../build-appimage.sh");
    let apprun = &script[script.find("<< 'APPRUN'").expect("AppRun heredoc")..];
    let apprun = &apprun[..apprun.find("\nAPPRUN\n").expect("AppRun heredoc end")];
    let check = apprun.find("\nlibcheck_run \"Green Light\" \"$HERE/usr/bin/$APP\" || exit 1\n").expect("libcheck_run call in AppRun");
    let exec = apprun.find("\nexec \"$HERE/usr/bin/$APP\" \"$@\"").expect("exec of the binary");
    assert!(check < exec);
    // The check comes before the launch-time setup's password prompt: an app that can't start
    // must not ask for one first.
    let setup = apprun.find("pkexec ").expect("the launch-time setup");
    assert!(check < setup && setup < exec);
    // The setup says why it did not complete when no polkit agent is running.
    assert!(apprun.contains("no polkit authentication agent is running"));
    assert!(!script[..script.find("<< 'APPRUN'").unwrap()].contains("\nlibcheck_run "));
    // Nothing is bundled or patched: one binary, as cargo built it.
    for gone in ["patchelf", "bundle_", "gtk_choice", "_GTK"] {
        assert!(!script.contains(gone), "{gone} is back in build-appimage.sh");
    }
}

/// libcheck_run with nothing wrong in ldd's output, the loader cache `cache`, and `env` first
/// (a Wayland session or not).
fn runtime(name: &str, cache: &str, os: &str, env: &str) -> (bool, String, String) {
    run_with_cache(name, "\tlibc.so.6 => /lib64/libc.so.6 (0x1)", cache, os, &format!("{env} libcheck_run 'Test App' /x"))
}

#[test]
fn a_wayland_session_needs_the_wayland_client_library() {
    let cache = format!("{LIBC_ONLY}\n\tlibvulkan.so.1 (libc6,x86-64) => /lib64/libvulkan.so.1");
    let (ok, _, err) = runtime("wl-missing", &cache, "ID=ubuntu\n", "export WAYLAND_DISPLAY=wayland-0;");
    assert!(!ok);
    assert!(err.contains("missing: libwayland-client.so.0.") && err.contains("sudo apt install libwayland-client0."), "{err}");
    // An X11 session never loads it.
    let (ok, _, err) = runtime("wl-x11", &cache, "ID=ubuntu\n", "");
    assert!(ok, "{err}");
}

#[test]
fn one_graphics_loader_is_enough_and_none_stops_the_launch() {
    for lib in ["libvulkan.so.1", "libEGL.so.1"] {
        let cache = format!("{LIBC_ONLY}\n\t{lib} (libc6,x86-64) => /lib64/{lib}");
        let (ok, _, err) = runtime("gpu-one", &cache, "ID=fedora\n", "");
        assert!(ok, "{lib}: {err}");
    }
    let (ok, _, err) = runtime("gpu-none", LIBC_ONLY, "ID=arch\n", "");
    assert!(!ok);
    assert!(
        err.contains("missing: libvulkan.so.1, libEGL.so.1.")
            && err.contains("one of libvulkan.so.1 and libEGL.so.1 is enough")
            && err.contains("sudo pacman -S vulkan-icd-loader libglvnd."),
        "{err}"
    );
    // A 32-bit copy (Steam brings one) is no use to the app.
    let cache = format!("{LIBC_ONLY}\n\tlibvulkan.so.1 (libc6) => /lib/i386-linux-gnu/libvulkan.so.1");
    let (ok, _, err) = runtime("gpu-32", &cache, "ID=ubuntu\n", "");
    assert!(!ok, "{err}");
    // One found through LD_LIBRARY_PATH counts.
    let dir = scratch("gpu-ldpath-lib");
    std::fs::write(dir.join("libEGL.so.1"), b"").unwrap();
    let (ok, _, err) = runtime("gpu-ldpath", LIBC_ONLY, "ID=ubuntu\n", &format!("export LD_LIBRARY_PATH=/nonexistent:{};", dir.display()));
    let _ = std::fs::remove_dir_all(&dir);
    assert!(ok, "{err}");
}

#[test]
fn without_a_usable_loader_cache_nothing_is_claimed() {
    // No glibc in the listing (a system that keeps its libraries elsewhere): it says nothing
    // about what the loader can find, so the app is started and reports a real problem itself.
    let (ok, _, err) = runtime("cache-odd", "\tlibfoo.so.1 (libc6,x86-64) => /nix/libfoo.so.1", "ID=nixos\n", "export WAYLAND_DISPLAY=wayland-0;");
    assert!(ok, "{err}");
    let (ok, _, err) = runtime("cache-empty", "", "ID=nixos\n", "");
    assert!(ok, "{err}");
}

const OLD_GLIBC: &str = "/x: /lib64/libc.so.6: version `GLIBC_2.38' not found (required by /x)\n/x: /lib64/libc.so.6: version `GLIBC_2.39' not found (required by /x)\n\tlibc.so.6 => /lib64/libc.so.6 (0x1)";

#[test]
fn an_old_system_glibc_stops_the_launch_with_its_version() {
    let (ok, _, err) = run("glibc", OLD_GLIBC, "ID=debian\n", "export LIBCHECK_GLIBC_HAVE=2.36 LDD_EXIT=1; libcheck_run 'Test App' /x");
    assert!(!ok);
    assert!(err.contains("Test App can't start: it needs glibc 2.39 or newer; this system has 2.36."), "{err}");
    // The system's own version, as getconf or ldd reports it.
    let (ok, _, err) = run("glibc-real", OLD_GLIBC, "ID=debian\n", "libcheck_run 'Test App' /x");
    assert!(!ok);
    let have = err.split("this system has ").nth(1).unwrap_or_default();
    assert!(have.starts_with("2.") && have.as_bytes()[2].is_ascii_digit(), "{err}");
    // Too old is the one thing to say: the run-time libraries are not looked at.
    let (ok, _, err) =
        run_with_cache("glibc-first", OLD_GLIBC, LIBC_ONLY, "ID=debian\n", "export LIBCHECK_GLIBC_HAVE=2.36 LDD_EXIT=1; libcheck_run 'Test App' /x");
    assert!(!ok);
    assert!(err.contains("needs glibc 2.39") && !err.contains("libvulkan"), "{err}");
}

#[test]
fn undefined_symbols_stop_the_launch() {
    let ldd = "\tlibxkbcommon.so.0 => /lib/libxkbcommon.so.0 (0x1)\nundefined symbol: xkb_a\t(/x)\nundefined symbol: xkb_b\t(/x)\nundefined symbol: xkb_c\t(/x)\nundefined symbol: xkb_d\t(/x)\nundefined symbol: xkb_e\t(/x)";
    let (ok, _, err) = run("undef", ldd, "ID=debian\n", "libcheck_run 'Test App' /x");
    assert!(!ok);
    assert!(
        err.contains("the system's libraries are too old: they lack xkb_a, xkb_b, xkb_c and 2 more. Update the system, then start Test App again."),
        "{err}"
    );
}

#[test]
fn other_missing_versions_stop_the_launch() {
    let ldd = "/x: /lib/libgcc_s.so.1: version `GCC_14.0' not found (required by /x)";
    let (ok, _, err) = run("other", ldd, "ID=debian\n", "export LDD_EXIT=1; libcheck_run 'Test App' /x");
    assert!(!ok);
    assert!(err.contains("too old: /x: /lib/libgcc_s.so.1: version `GCC_14.0' not found (required by /x)."), "{err}");
}

/// A fake loader for libcheck's fallback, printing `out`.
fn loader(dir: &Path, out: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("loader");
    script_file(&path, &format!("#!/bin/sh\ncat <<'OUT'\n{out}\nOUT\n"));
    path
}

#[test]
fn a_failing_ldd_alone_does_not_stop_the_launch() {
    // ldd fails without naming anything: the loader is asked, and finds nothing wrong.
    let dir = std::env::temp_dir().join(format!("libcheck-loader-ok-{}", std::process::id()));
    let ok_loader = loader(&dir, "\tlibc.so.6 => /lib64/libc.so.6 (0x1)");
    let (ok, _, err) = run("rc-ok", "", "ID=debian\n", &format!("export LDD_EXIT=3 LIBCHECK_LOADER={}; libcheck_run 'Test App' /x", ok_loader.display()));
    assert!(ok, "{err}");
    // The loader names a problem: that stops it.
    let bad_loader = loader(&dir.join("bad"), "\tlibxcb.so.1 => not found");
    let (ok, _, err) = run("rc-bad", "", "ID=debian\n", &format!("export LDD_EXIT=3 LIBCHECK_LOADER={}; libcheck_run 'Test App' /x", bad_loader.display()));
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!ok);
    assert!(err.contains("these system libraries are missing: libxcb.so.1."), "{err}");
}

#[test]
fn the_dialogs_get_plain_text() {
    let script = include_str!("../build-appimage.sh");
    assert!(script.contains("zenity --error --no-markup "));
    let (ok, out, _) = run("plain", "", "ID=debian\n", "libcheck_plain 'a <b> & c'");
    assert!(ok);
    assert_eq!(out, "a ‹b› and c\n");
}

#[test]
fn the_real_ldd_and_loader_cache_pass_a_complete_binary() {
    if !Path::new("/usr/bin/ldd").exists() && !Path::new("/bin/ldd").exists() {
        return;
    }
    // Only the linked libraries: whether this machine has a graphics loader is not the test's
    // business, so the cache is the complete fake one.
    let dir = scratch("real-ldd");
    let ldconfig = dir.join("ldconfig");
    script_file(&ldconfig, &format!("#!/bin/sh\ncat <<'OUT'\n{CACHE_OK}\nOUT\n"));
    let script = format!("{}\nlibcheck_run 'Test App' /bin/sh\n", block());
    let out = Command::new("bash")
        .args(["-c", &script])
        .env("LIBCHECK_DIALOG", "none")
        .env("LIBCHECK_LDCONFIG", &ldconfig)
        .env_remove("LIBCHECK_LDD")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn the_real_loader_cache_is_understood() {
    // On a machine with ldconfig, its listing has glibc in the form libcheck_cache looks for.
    let script = format!("{}\nlibcheck_cache; [[ -z \"$LIBCHECK_CACHE\" ]] || libcheck_has libc.so.6\n", block());
    let out = Command::new("bash").args(["-c", &script]).env_remove("LIBCHECK_LDCONFIG").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn libcheck_counts_only_the_binarys_own_symbols() {
    // A system library's own lazy unresolved symbol is its business.
    let ldd =
        "undefined symbol: _lazy\t(/lib/libfoo.so.1)\nsymbol foo version BAR not defined in file libbar.so.1 with link time reference\t(/lib/libbaz.so.2)";
    let (ok, _, err) = run("lib-own", ldd, "ID=debian\n", "libcheck_run 'Test App' /x");
    assert!(ok, "{err}");
    // ld.so's other format, for the app itself.
    let ldd = "symbol xkb_x version V_1.0.0 not defined in file libxkbcommon.so.0 with link time reference\t(/x)";
    let (ok, _, err) = run("lib-version", ldd, "ID=debian\n", "libcheck_run 'Test App' /x");
    assert!(!ok && err.contains("they lack xkb_x."), "{err}");
}
