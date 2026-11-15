//! The AppRun's library check (the `libcheck` block in build-appimage.sh), run
//! in bash with a fake `ldd` and a fake os-release, its bundle_env block, and its
//! gtk_choice block (system GTK or the bundled copy).

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
    // AppRun picks the GTK first, checks the chosen binary's libraries before any password
    // prompt, and sets the bundle's GTK variables only on the bundled path, right before exec.
    let script = include_str!("../build-appimage.sh");
    let apprun = &script[script.find("<< 'APPRUN'").expect("AppRun heredoc")..];
    let choice = apprun.find("\ngtk_choice ").expect("gtk_choice call in AppRun");
    let check = apprun.find("\nlibcheck_run ").expect("libcheck_run call in AppRun");
    assert!(apprun[check..].starts_with("\nlibcheck_run ") && apprun[check..].contains("\"$GTK_BIN\" \"$GTK_NOTE\""));
    if let Some(prompt) = apprun.find("pkexec") {
        assert!(check < prompt, "libcheck must come before pkexec");
    }
    let env = apprun.find("\nif [[ $GTK_CHOICE == bundled ]]; then bundle_env \"$HERE\"; fi\n").expect("bundle_env only on the bundled path");
    let exec = apprun.find("\nexec \"$GTK_BIN\" \"$@\"").expect("exec of the chosen binary");
    assert!(choice < check && check < env && env < exec);
    assert_eq!(apprun.matches("bundle_env \"$HERE\"").count(), 1);
    assert!(!script[..script.find("<< 'APPRUN'").unwrap()].contains("\nlibcheck_run "));
    // Both binaries are in the AppDir, the host one without an rpath.
    assert!(script.contains("\nbundle_gtk_runtime \"$APPDIR\" \"bundled/$APP\"\n"));
    assert!(script.contains("patchelf --print-rpath \"$APPDIR/usr/bin/$APP\""));
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

fn gtk_block() -> String {
    let script = include_str!("../build-appimage.sh");
    let start = script.find("# >>> gtk_choice").expect("gtk_choice block start");
    let end = script.find("# <<< gtk_choice").expect("gtk_choice block end");
    script[start..end].to_string()
}

/// What gtk_choice decided: GTK_CHOICE, GTK_BIN, whether APPIMAGE_BUNDLE_ENV survived, stderr.
struct Choice {
    choice: String,
    bin: String,
    bundle_env_kept: bool,
    note: String,
    err: String,
}

/// Runs gtk_choice for host binary `host`. `ldd` is the fake `ldd -r` output (printed on stderr,
/// as the real ldd prints undefined symbols), or None for a system without ldd; `real_ldd` uses
/// the machine's own. `value` is APP_GTK.
fn choose(name: &str, ldd: Option<&str>, real_ldd: bool, value: Option<&str>, host: &str) -> Choice {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch(name);
    let script = format!(
        "{}\ngtk_choice 'Test App' APP_GTK '{host}' /bundled/app\nprintf '%s\\n%s\\n%s\\n%s\\n' \"$GTK_CHOICE\" \"$GTK_BIN\" \"${{APPIMAGE_BUNDLE_ENV-unset}}\" \"$GTK_NOTE\"\n",
        gtk_block()
    );
    let mut cmd = Command::new("/bin/bash");
    cmd.args(["-c", &script]).env("APPIMAGE_BUNDLE_ENV", "GIO_MODULE_DIR").env_remove("APP_GTK").env_remove("LIBCHECK_LDD");
    if let Some(out) = ldd {
        let fake = dir.join("ldd");
        // Fails unless called as `ldd -r BINARY`.
        std::fs::write(&fake, format!("#!/bin/sh\n[ \"$1\" = -r ] || exit 9\ncat >&2 <<'OUT'\n{out}\nOUT\n")).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        cmd.env("LIBCHECK_LDD", &fake);
    } else if !real_ldd {
        // An empty directory as the whole PATH: no ldd, and no other program either.
        cmd.env("PATH", &dir);
    }
    if let Some(value) = value {
        cmd.env("APP_GTK", value);
    }
    let out = cmd.output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let lines: Vec<&str> = stdout.lines().collect();
    Choice {
        choice: lines[0].to_string(),
        bin: lines[1].to_string(),
        bundle_env_kept: lines[2] != "unset",
        note: lines.get(3).unwrap_or(&"").to_string(),
        err: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

const HOST_OK: &str = "\tlinux-vdso.so.1 (0x00007ffd)\n\tlibadwaita-1.so.0 => /lib/x86_64-linux-gnu/libadwaita-1.so.0 (0x1)\n\tlibgtk-4.so.1 => /lib/x86_64-linux-gnu/libgtk-4.so.1 (0x2)\n\tlibc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x3)";

#[test]
fn a_complete_system_gtk_is_used() {
    let c = choose("host-ok", Some(HOST_OK), false, None, "/host/app");
    assert_eq!((c.choice.as_str(), c.bin.as_str()), ("host", "/host/app"), "{}", c.err);
    // No bundle environment on the host path: host_env has nothing to restore.
    assert!(!c.bundle_env_kept);
    assert_eq!(c.err, "Test App: using the system's GTK 4 and libadwaita (APP_GTK=bundled uses the bundled copy)\n");
}

#[test]
fn a_missing_system_library_means_the_bundle() {
    let ldd = "\tlibadwaita-1.so.0 => not found\n\tlibgtk-4.so.1 => /lib64/libgtk-4.so.1 (0x2)";
    let c = choose("host-missing", Some(ldd), false, None, "/host/app");
    assert_eq!((c.choice.as_str(), c.bin.as_str()), ("bundled", "/bundled/app"), "{}", c.err);
    assert!(c.bundle_env_kept && c.note.is_empty());
    assert_eq!(c.err, "Test App: using the bundled GTK 4 and libadwaita: the system has no libadwaita-1.so.0 (APP_GTK=host forces the system's)\n");
}

#[test]
fn an_old_system_libadwaita_means_the_bundle() {
    // libadwaita 1.4 resolves, but lacks the 1.5 symbols the app calls.
    let ldd = format!("{HOST_OK}\nundefined symbol: adw_dialog_present\t(/host/app)\nundefined symbol: adw_about_dialog_new\t(/host/app)");
    let c = choose("host-undefined", Some(&ldd), false, None, "/host/app");
    assert_eq!(c.choice, "bundled", "{}", c.err);
    assert!(c.err.contains(": the system's libraries lack adw_dialog_present (APP_GTK=host"), "{}", c.err);
}

#[test]
fn an_old_system_glibc_means_the_bundle() {
    let ldd = "/host/app: /lib64/libc.so.6: version `GLIBC_2.39' not found (required by /host/app)";
    let c = choose("host-glibc", Some(ldd), false, None, "/host/app");
    assert_eq!(c.choice, "bundled", "{}", c.err);
    assert!(c.err.contains("GLIBC_2.39' not found"), "{}", c.err);
}

#[test]
fn without_ldd_the_bundle_is_used() {
    let c = choose("no-ldd", None, false, None, "/host/app");
    assert_eq!((c.choice.as_str(), c.bin.as_str()), ("bundled", "/bundled/app"), "{}", c.err);
    assert!(c.err.contains("ldd is not installed"), "{}", c.err);
}

#[test]
fn a_failing_ldd_means_the_bundle() {
    let script = format!("{}\ngtk_choice 'Test App' APP_GTK /host/app /bundled/app\necho \"$GTK_CHOICE\"\n", gtk_block());
    let out = Command::new("bash").args(["-c", &script]).env("LIBCHECK_LDD", "false").env_remove("APP_GTK").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "bundled\n");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("ldd could not check the system's copy (exit 1)"), "{err}");
}

#[test]
fn the_override_wins_either_way() {
    let c = choose("force-host", Some("\tlibadwaita-1.so.0 => not found"), false, Some("host"), "/host/app");
    assert_eq!((c.choice.as_str(), c.bin.as_str()), ("host", "/host/app"));
    assert_eq!(c.err, "Test App: using the system's GTK 4 and libadwaita: set by APP_GTK=host\n");
    assert!(c.note.contains("APP_GTK=host") && !c.bundle_env_kept);

    let c = choose("force-bundled", Some(HOST_OK), false, Some("bundled"), "/host/app");
    assert_eq!((c.choice.as_str(), c.bin.as_str()), ("bundled", "/bundled/app"));
    assert_eq!(c.err, "Test App: using the bundled GTK 4 and libadwaita: set by APP_GTK=bundled\n");

    // Without ldd too.
    let c = choose("force-host-no-ldd", None, false, Some("host"), "/host/app");
    assert_eq!(c.choice, "host");

    // Anything else is ignored, and the check runs as usual.
    let c = choose("force-other", Some(HOST_OK), false, Some("system"), "/host/app");
    assert_eq!(c.choice, "host");
    assert!(c.err.starts_with("Test App: ignoring APP_GTK=system (it takes host or bundled)\n"), "{}", c.err);
}

#[test]
fn the_real_ldd_accepts_a_complete_binary() {
    if !Path::new("/usr/bin/ldd").exists() && !Path::new("/bin/ldd").exists() {
        return;
    }
    let c = choose("real-ldd", None, true, None, "/bin/sh");
    assert_eq!(c.choice, "host", "{}", c.err);
}

#[test]
fn libcheck_takes_a_note_for_the_host_path() {
    let (ok, _, err) = run("note", MISSING, "ID=fedora\n", "libcheck_run 'Green Light' /x 'GREEN_LIGHT_GTK=host is set.'");
    assert!(!ok);
    assert!(err.contains("missing: libfribidi.so.0, libEGL.so.1. GREEN_LIGHT_GTK=host is set. Install"), "{err}");
}
