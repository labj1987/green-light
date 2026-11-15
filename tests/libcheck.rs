//! The AppRun's library check (the `libcheck` block in build-appimage.sh), run
//! in bash with a fake `ldd` and a fake os-release.

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

const MISSING: &str =
    "\tlinux-vdso.so.1 (0x00007ffd)\n\tlibadwaita-1.so.0 => not found\n\tlibc.so.6 => /lib64/libc.so.6 (0x00007f)\n\tlibgtk-4.so.1 => not found";

#[test]
fn missing_libraries_are_listed_one_per_line() {
    let (ok, out, _) = run("list", MISSING, "ID=fedora\n", "libcheck_missing /x");
    assert!(ok);
    assert_eq!(out, "libadwaita-1.so.0\nlibgtk-4.so.1\n");
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
    assert!(err.contains("libadwaita-1.so.0, libgtk-4.so.1"), "{err}");
    assert!(err.contains("sudo dnf install gtk4 libadwaita"), "{err}");
}

#[test]
fn the_install_hint_follows_the_distribution() {
    for (os, want) in [
        ("ID=ubuntu\nID_LIKE=debian\n", "sudo apt install libgtk-4-1 libadwaita-1-0"),
        ("ID=pop\nID_LIKE=\"ubuntu debian\"\n", "sudo apt install libgtk-4-1 libadwaita-1-0"),
        ("ID=fedora\n", "sudo dnf install gtk4 libadwaita"),
        ("ID=nobara\nID_LIKE=\"rhel centos fedora\"\n", "sudo dnf install gtk4 libadwaita"),
        ("ID=cachyos\nID_LIKE=arch\n", "sudo pacman -S gtk4 libadwaita"),
        ("ID=opensuse-tumbleweed\nID_LIKE=\"opensuse suse\"\n", "sudo zypper install libgtk-4-1 libadwaita-1-0"),
        ("ID=somethingelse\n", ""),
    ] {
        let (ok, out, _) = run("hint", "", os, "libcheck_install_hint");
        assert!(ok);
        assert_eq!(out.trim(), want, "{os}");
    }
}

#[test]
fn an_unknown_distribution_still_gets_a_message() {
    let (ok, _, err) = run("generic", MISSING, "ID=somethingelse\n", "libcheck_run 'Kernel Pop' /x");
    assert!(!ok);
    assert!(err.contains("GTK 4") && err.contains("libadwaita") && err.contains("package manager"), "{err}");
}
