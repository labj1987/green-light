//! The AppRun's library check (the `libcheck` block in build-appimage.sh), run
//! in bash with a fake `ldd` and a fake os-release, and its bundle_env block.

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

/// Runs `call` after the block, with the fake ldd printing `ldd_out` and the
/// os-release `os`. Returns (exit status ok, stdout, stderr).
fn run(name: &str, ldd_out: &str, os: &str, call: &str) -> (bool, String, String) {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch(name);
    let ldd = dir.join("ldd");
    std::fs::write(&ldd, format!("#!/bin/sh\ncat <<'OUT'\n{ldd_out}\nOUT\n")).unwrap();
    std::fs::set_permissions(&ldd, std::fs::Permissions::from_mode(0o755)).unwrap();
    let release = dir.join("os-release");
    std::fs::write(&release, os).unwrap();
    let script = format!("{}\n{call}\n", block());
    let out = Command::new("bash")
        .args(["-c", &script])
        .env("LIBCHECK_LDD", &ldd)
        .env("LIBCHECK_OS_RELEASE", &release)
        .env("LIBCHECK_DIALOG", "none")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(Path::new(&dir));
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

const MISSING: &str = "\tlinux-vdso.so.1 (0x00007ffd)\n\tlibfribidi.so.0 => not found\n\tlibc.so.6 => /lib64/libc.so.6 (0x00007f)\n\tlibEGL.so.1 => not found";

#[test]
fn missing_libraries_are_listed_one_per_line() {
    let (ok, out, _) = run("list", MISSING, "ID=fedora\n", "libcheck_missing /x");
    assert!(ok);
    assert_eq!(out, "libfribidi.so.0\nlibEGL.so.1\n");
}

#[test]
fn nothing_missing_passes() {
    let (ok, out, err) = run("none", "\tlibc.so.6 => /lib64/libc.so.6 (0x1)", "ID=ubuntu\n", "libcheck_run 'App' /x");
    assert!(ok, "{err}");
    assert!(out.is_empty() && err.is_empty());
}

#[test]
fn missing_libraries_stop_the_launch_with_a_message() {
    let (ok, _, err) = run("fail", MISSING, "ID=fedora\nVERSION_ID=45\n", "libcheck_run 'Green Light' /x");
    assert!(!ok);
    assert!(err.contains("missing: libfribidi.so.0, libEGL.so.1."), "{err}");
    assert!(err.contains("sudo dnf install 'libfribidi.so.0()(64bit)' 'libEGL.so.1()(64bit)'"), "{err}");
    // GTK 4 and libadwaita come with the app now; the message must not send people after them.
    assert!(err.contains("brings GTK 4 and libadwaita with it"), "{err}");
}

#[test]
fn the_install_hint_follows_the_distribution() {
    for (os, want) in [
        ("ID=fedora\n", "sudo dnf install 'libEGL.so.1()(64bit)'"),
        ("ID=nobara\nID_LIKE=\"rhel centos fedora\"\n", "sudo dnf install 'libEGL.so.1()(64bit)'"),
        ("ID=opensuse-tumbleweed\nID_LIKE=\"opensuse suse\"\n", "sudo zypper install 'libEGL.so.1()(64bit)'"),
        // apt and pacman cannot install by library name: no command, the generic advice instead.
        ("ID=ubuntu\nID_LIKE=debian\n", ""),
        ("ID=cachyos\nID_LIKE=arch\n", ""),
        ("ID=somethingelse\n", ""),
    ] {
        let (ok, out, _) = run("hint", "", os, "libcheck_install_hint libEGL.so.1");
        assert!(ok);
        assert_eq!(out.trim(), want, "{os}");
    }
}

#[test]
fn an_unknown_distribution_still_gets_a_message() {
    let (ok, _, err) = run("generic", MISSING, "ID=somethingelse\n", "libcheck_run 'Kernel Pop' /x");
    assert!(!ok);
    assert!(err.contains("libfribidi.so.0") && err.contains("package manager"), "{err}");
}

#[test]
fn the_apprun_checks_before_setup_and_sets_up_the_bundle_last() {
    // The check runs inside AppRun (not while building), before any password prompt, and the
    // bundle's GTK variables are set only for the app itself, right before it starts.
    let script = include_str!("../build-appimage.sh");
    let apprun = &script[script.find("<< 'APPRUN'").expect("AppRun heredoc")..];
    let check = apprun.find("\nlibcheck_run ").expect("libcheck_run call in AppRun");
    if let Some(prompt) = apprun.find("pkexec") {
        assert!(check < prompt, "libcheck must come before pkexec");
    }
    let env = apprun.find("\nbundle_env \"$HERE\"").expect("bundle_env call in AppRun");
    let exec = apprun.find("\nexec ").expect("exec in AppRun");
    assert!(check < env && env < exec);
    assert!(!script[..script.find("<< 'APPRUN'").unwrap()].contains("\nlibcheck_run "));
}

#[test]
fn bundle_env_keeps_the_hosts_values() {
    let script = include_str!("../build-appimage.sh");
    let start = script.find("# >>> bundle_env").expect("bundle_env block start");
    let end = script.find("# <<< bundle_env").expect("bundle_env block end");
    let body = format!(
        "{}\nbundle_env /app\nfor v in $APPIMAGE_BUNDLE_ENV; do o=APPIMAGE_ORIG_$v; echo \"$v=${{!v}} orig=${{!o-unset}}\"; done\n",
        &script[start..end]
    );
    let out = Command::new("bash")
        .args(["-c", &body])
        .env_remove("GTK_EXE_PREFIX")
        .env_remove("GDK_PIXBUF_MODULE_FILE")
        .env_remove("GIO_MODULE_DIR")
        .env("GSETTINGS_SCHEMA_DIR", "/home/u/schemas")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("GTK_EXE_PREFIX=/app/usr orig=unset"), "{out}");
    assert!(out.contains("GIO_MODULE_DIR=/app/usr/lib/gio/modules orig=unset"), "{out}");
    assert!(out.contains("GDK_PIXBUF_MODULE_FILE=/app/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache orig=unset"), "{out}");
    assert!(out.contains("GSETTINGS_SCHEMA_DIR=/app/usr/share/glib-2.0/schemas:/home/u/schemas orig=/home/u/schemas"), "{out}");
}
