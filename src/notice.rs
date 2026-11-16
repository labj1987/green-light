//! notice.rs — telling the user something when the app has no window to say
//! it in: no working graphics driver, or a running copy that does not answer
//! (or is still starting). It goes through whatever the desktop offers, tried
//! in order until one shows it: a dialog (zenity, kdialog), a notification
//! (notify-send, or the notification service straight over D-Bus with gdbus,
//! which desktops without notify-send such as Pop!_OS COSMIC still have), or
//! xmessage. AppRun's `libcheck_show` (build-appimage.sh) tries the same
//! programs in the same order; a test holds the two together.

/// What a message is: an error the user has to act on, or information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An error dialog, or a critical notification that stays until closed.
    Error,
    /// An information dialog, or a normal notification with the desktop's
    /// default expiry.
    Info,
}

/// Shows `message` as an error, when there is a display to show it on.
pub fn show_error(message: &str) {
    show(Kind::Error, message);
}

/// Shows `message` as information, when there is a display to show it on.
pub fn show_info(message: &str) {
    show(Kind::Info, message);
}

fn show(kind: Kind, message: &str) {
    // With no display there is nowhere to show a dialog either.
    if !has_display() {
        return;
    }
    for (program, args) in commands(kind, message) {
        if run(program, &args).is_some_and(|(code, stderr)| showed(program, code, stderr.as_deref())) {
            break;
        }
    }
}

/// Runs `program` with `args`, waits for it, and returns its exit code and
/// what it wrote on stderr. Its stderr goes to a private temporary file,
/// read once the program has ended: reading a pipe to its end would also
/// wait for any process the program started that kept the pipe open.
fn run(program: &str, args: &[String]) -> Option<(Option<i32>, Option<String>)> {
    run_with(program, args, stderr_file(&std::env::temp_dir()))
}

/// `run` with stderr going to `file`. Without a file, stderr is unknown
/// (`None`), not empty.
fn run_with(program: &str, args: &[String], file: Option<std::fs::File>) -> Option<(Option<i32>, Option<String>)> {
    let (reader, stderr) = match file.and_then(|file| Some((file.try_clone().ok()?, file))) {
        Some((writer, reader)) => (Some(reader), std::process::Stdio::from(writer)),
        None => (None, std::process::Stdio::null()),
    };
    let status = std::process::Command::new(program).args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(stderr).status();
    let text = reader.and_then(|mut file| {
        use std::io::{Read as _, Seek as _};
        let mut bytes = Vec::new();
        (file.rewind().is_ok() && file.read_to_end(&mut bytes).is_ok()).then(|| String::from_utf8_lossy(&bytes).into_owned())
    });
    status.ok().map(|status| (status.code(), text))
}

/// A file in `dir` for a program's stderr that no other process can know the
/// name of: an unnamed one (`O_TMPFILE`) where the file system has them,
/// otherwise one with a random name (`named_file`). `None` when neither can
/// be made.
fn stderr_file(dir: &std::path::Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let unnamed = std::fs::OpenOptions::new().read(true).write(true).mode(0o600).custom_flags(libc::O_TMPFILE).open(dir);
    unnamed.ok().or_else(|| named_file(dir))
}

/// A file in `dir` with a random name, removed as soon as it is open (nothing
/// is left behind if this process is killed while the dialog is up); a name
/// that is taken is tried again with another.
fn named_file(dir: &std::path::Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    for _ in 0..16 {
        let path = dir.join(format!("green-light-notice-{:016x}", random()));
        match std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path) {
            Ok(file) => {
                let _ = std::fs::remove_file(&path);
                return Some(file);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return None,
        }
    }
    None
}

/// 64 unpredictable bits: the standard library's hash keys (seeded from the
/// system's random source) over the clock.
fn random() -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    hasher.write_u128(now.map_or(0, |now| now.as_nanos()));
    hasher.finish()
}

/// Whether there is a display server to show anything on.
pub fn has_display() -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"].into_iter().any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// The programs tried, in order, with their arguments. An error's
/// notifications have no expiry (0): COSMIC drops one with the default after
/// a few seconds, before anyone has read it. Information gets the normal
/// urgency and the desktop's default expiry (-1). kdialog and notification
/// bodies take markup, so they get the plain text (no &, < or >); xmessage
/// does not wrap, so its text is wrapped here.
fn commands(kind: Kind, message: &str) -> Vec<(&'static str, Vec<String>)> {
    let error = kind == Kind::Error;
    let args = |fixed: &[&str]| fixed.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    let with = |fixed: &[&str], text: String| {
        let mut all = args(fixed);
        all.push(text);
        all
    };
    let plain = plain_text(message);
    let notify_send: &[&str] = if error {
        &["--urgency=critical", "--expire-time=0", "--app-name=Green Light", "Green Light"]
    } else {
        &["--urgency=normal", "--app-name=Green Light", "Green Light"]
    };
    let (urgency, expiry) = if error { ("{'urgency': <byte 2>}", "int32 0") } else { ("{'urgency': <byte 1>}", "int32 -1") };
    vec![
        ("zenity", with(&[if error { "--error" } else { "--info" }, "--no-markup", "--title=Green Light", "--width=480", "--text"], message.to_string())),
        ("kdialog", with(&["--title", "Green Light", if error { "--error" } else { "--msgbox" }], plain.clone())),
        ("notify-send", with(notify_send, plain.clone())),
        (
            "gdbus",
            args(&[
                "call",
                "--session",
                "--dest",
                "org.freedesktop.Notifications",
                "--object-path",
                "/org/freedesktop/Notifications",
                "--method",
                "org.freedesktop.Notifications.Notify",
                "'Green Light'",
                "uint32 0",
                "''",
                "'Green Light'",
                &gvariant_string(&plain),
                "@as []",
                urgency,
                expiry,
            ]),
        ),
        ("xmessage", with(&["-center", "-title", "Green Light"], wrap(message, 80))),
    ]
}

/// Whether `program`, having ended with exit `code`, did show the message, so
/// the next one must not show it again. The dialogs end with 0 when the user
/// pressed their button and 1 when the user closed them another way; both
/// mean it was seen. Anything else (a crash, a signal) does not, nor does a
/// dialog that could not open the display (zenity then also exits 1; `stderr`
/// says so). notify-send and gdbus show nothing themselves: only 0 says the
/// desktop took the message; xmessage ends with 1 when it could not open the
/// display. With `stderr` unknown (`None`), a dialog's 1 is not taken as seen.
fn showed(program: &str, code: Option<i32>, stderr: Option<&str>) -> bool {
    match (program, stderr) {
        ("notify-send" | "gdbus" | "xmessage", _) => code == Some(0),
        (_, None) => code == Some(0),
        (_, Some(stderr)) => {
            matches!(code, Some(0 | 1)) && !["open display", "connect to display", "connect to X server"].iter().any(|text| stderr.contains(text))
        }
    }
}

/// `text` with nothing kdialog (Qt rich text) or a notification server (body
/// markup) would take as markup: no &, < or >. AppRun's libcheck_plain does the same.
fn plain_text(text: &str) -> String {
    text.replace('&', "and").replace('<', "‹").replace('>', "›")
}

/// `text` with its lines broken at spaces to at most `width` characters
/// (a longer word stays whole), as `fold -s` does for AppRun's xmessage.
fn wrap(text: &str, width: usize) -> String {
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split(' ') {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
                out.push(std::mem::take(&mut line));
            } else if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(line);
    }
    out.join("\n")
}

/// `text` as a GVariant text-format string, which is how gdbus takes its
/// arguments.
fn gvariant_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_way_to_show_a_message_is_tried_in_the_apprun_order() {
        for kind in [Kind::Error, Kind::Info] {
            let names: Vec<_> = commands(kind, "x").into_iter().map(|(program, _)| program).collect();
            assert_eq!(names, ["zenity", "kdialog", "notify-send", "gdbus", "xmessage"]);
        }
        // AppRun's libcheck_show tries them in the same order.
        let script = include_str!("../build-appimage.sh");
        let show = &script[script.find("libcheck_show() {").unwrap()..];
        let show = &show[..show.find("\n}\n").unwrap()];
        let order: Vec<usize> =
            ["zenity", "kdialog", "notify-send", "gdbus", "xmessage"].iter().map(|name| show.find(&format!("command -v {name} ")).expect(name)).collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
    }

    #[test]
    fn an_error_dialog_that_was_seen_is_not_shown_twice() {
        assert!(showed("zenity", Some(0), Some("")));
        assert!(showed("zenity", Some(1), Some("")), "closed without OK: still seen");
        assert!(showed("kdialog", Some(1), Some("")));
        assert!(!showed("zenity", Some(255), Some("")), "an error of its own");
        // zenity that can't reach the display exits 1 too: that is not "seen".
        assert!(!showed("zenity", Some(1), Some("(zenity:12): Gtk-WARNING **: cannot open display: :9")));
        assert!(!showed("zenity", Some(1), Some("Failed to open display")));
        assert!(!showed("kdialog", None, Some("")), "killed by a signal");
        assert!(showed("notify-send", Some(0), Some("")));
        assert!(!showed("notify-send", Some(1), Some("")), "no notification service");
        assert!(showed("gdbus", Some(0), Some("")));
        assert!(!showed("gdbus", Some(1), Some("")), "no notification service");
        assert!(showed("xmessage", Some(0), Some("")));
        assert!(!showed("xmessage", Some(1), Some("")), "could not open the display");
    }

    #[test]
    fn errors_stay_until_closed() {
        let commands = commands(Kind::Error, "it's\nbroken");
        assert_eq!(commands[0].1[0], "--error");
        assert!(commands[1].1.contains(&"--error".to_string()));
        assert!(commands[2].1.contains(&"--expire-time=0".to_string()), "{:?}", commands[2].1);
        assert!(commands[2].1.contains(&"--urgency=critical".to_string()));
        let gdbus = &commands[3].1;
        assert!(gdbus.windows(2).any(|w| w == ["--method", "org.freedesktop.Notifications.Notify"]));
        // Notify's eight arguments: app, replaces, icon, summary, body, actions, hints, expiry.
        let notify = &gdbus[gdbus.len() - 8..];
        assert_eq!(notify[4], "'it\\'s\\nbroken'");
        assert_eq!(&notify[6..], ["{'urgency': <byte 2>}", "int32 0"]);
        assert_eq!(gvariant_string("a\tb\r\\"), r"'a\tb\r\\'");
    }

    #[test]
    fn information_uses_the_info_dialogs_and_a_normal_notification() {
        let commands = commands(Kind::Info, "x");
        assert_eq!(commands[0].1[0], "--info");
        assert!(commands[1].1.contains(&"--msgbox".to_string()));
        assert!(!commands[1].1.contains(&"--error".to_string()));
        let notify_send = &commands[2].1;
        assert!(notify_send.contains(&"--urgency=normal".to_string()));
        assert!(!notify_send.iter().any(|arg| arg.starts_with("--expire-time")), "the desktop's default expiry: {notify_send:?}");
        let gdbus = &commands[3].1;
        assert_eq!(&gdbus[gdbus.len() - 2..], ["{'urgency': <byte 1>}", "int32 -1"]);
    }

    #[test]
    fn the_dialogs_get_the_message_as_it_is_and_the_others_without_markup() {
        let commands = commands(Kind::Error, "a <b> & c");
        assert_eq!(commands[0].1.last().map(String::as_str), Some("a <b> & c"), "zenity takes it as plain text");
        assert_eq!(commands[1].1.last().map(String::as_str), Some("a ‹b› and c"));
        assert_eq!(commands[2].1.last().map(String::as_str), Some("a ‹b› and c"));
        assert_eq!(commands[4].1.last().map(String::as_str), Some("a <b> & c"));
        assert_eq!(wrap(&"word ".repeat(30), 80).lines().map(|l| l.chars().count()).max(), Some(79));
        assert_eq!(wrap("aaa bbb ccc", 7), "aaa bbb\nccc");
        assert_eq!(wrap("x\nyy zz", 4), "x\nyy\nzz", "a paragraph break stays");
        assert_eq!(wrap(&"w".repeat(9), 4), "w".repeat(9), "a long word stays whole");
    }

    #[test]
    fn a_dialogs_stderr_is_read_after_it_ends_even_if_a_child_keeps_it_open() {
        // A "dialog" that leaves a background process holding its stderr: a
        // pipe read to its end would wait the 5 s that process sleeps.
        let started = std::time::Instant::now();
        let (code, stderr) = run("sh", &["-c".to_string(), "echo 'cannot open display: :9999' >&2; sleep 5 & exit 1".to_string()]).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(3), "{:?}", started.elapsed());
        assert_eq!(code, Some(1));
        assert!(!showed("zenity", code, stderr.as_deref()), "{stderr:?}");
    }

    #[test]
    fn a_dialogs_stderr_is_read_whatever_its_bytes() {
        let (code, stderr) = run("sh", &["-c".to_string(), "printf '\\377\\376 Failed to open display\\n' >&2; exit 1".to_string()]).unwrap();
        assert_eq!(code, Some(1));
        let stderr = stderr.unwrap();
        assert!(stderr.contains("Failed to open display"), "{stderr:?}");
        assert!(!showed("zenity", code, Some(&stderr)));
    }

    #[test]
    fn a_named_stderr_file_is_gone_as_soon_as_it_is_open() {
        let dir = std::env::temp_dir().join(format!("green-light-notice-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A file system without unnamed files gets the random names.
        for _ in 0..3 {
            let (_, stderr) = run_with("sh", &["-c".to_string(), "echo seen >&2".to_string()], named_file(&dir)).unwrap();
            assert_eq!(stderr.as_deref(), Some("seen\n"));
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "nothing left behind");
        let _ = std::fs::remove_dir_all(&dir);
        assert_ne!(random(), random());
    }

    #[test]
    fn a_dialog_whose_stderr_could_not_be_kept_does_not_stop_the_chain() {
        // Nowhere to put its stderr: a dialog's exit 1 may be "no display".
        let nowhere = std::path::Path::new("/nonexistent/green-light");
        assert!(stderr_file(nowhere).is_none() && named_file(nowhere).is_none());
        let (code, stderr) = run_with("sh", &["-c".to_string(), "exit 1".to_string()], None).unwrap();
        assert_eq!((code, stderr.as_deref()), (Some(1), None));
        assert!(!showed("zenity", code, None), "the next program is tried");
        assert!(!showed("kdialog", code, None));
        assert!(showed("zenity", Some(0), None), "0 is pressed, whatever stderr said");
    }
}
