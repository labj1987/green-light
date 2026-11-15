//! Programs the app starts get the system's environment, not the AppImage's.
//!
//! The AppImage bundles GTK 4 and libadwaita as a fallback, and when it uses them its AppRun points them at their own modules
//! and schemas through a few environment variables (`GTK_EXE_PREFIX`, `GIO_MODULE_DIR`, ...).
//! A program started with those set would load the bundle's modules into its own GTK or GLib,
//! so AppRun names them in `APPIMAGE_BUNDLE_ENV` and keeps each one's earlier value in
//! `APPIMAGE_ORIG_<NAME>` (absent when it had none). [`command`] builds a
//! [`std::process::Command`] with those values put back. Outside the AppImage, and inside it when
//! AppRun runs the app on the system's GTK (its `gtk_choice`; the usual case on an up-to-date
//! desktop), nothing is set and nothing changes. The libraries themselves are found through the
//! binary's rpath, never `LD_LIBRARY_PATH`, so nothing else leaks.

use std::ffi::{OsStr, OsString};

const LIST: &str = "APPIMAGE_BUNDLE_ENV";
const ORIG: &str = "APPIMAGE_ORIG_";

/// What a child's environment needs: each variable with the value to set, or `None` to remove it.
pub fn restored() -> Vec<(OsString, Option<OsString>)> {
    restored_from(|name| std::env::var_os(name))
}

fn restored_from(get: impl Fn(&str) -> Option<OsString>) -> Vec<(OsString, Option<OsString>)> {
    let Some(list) = get(LIST) else { return Vec::new() };
    let mut out = Vec::new();
    for name in list.to_string_lossy().split_whitespace() {
        // AppRun only ever lists plain variable names; anything else is not ours to touch.
        if !name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') {
            continue;
        }
        let orig = format!("{ORIG}{name}");
        out.push((name.into(), get(&orig)));
        out.push((orig.into(), None));
    }
    out.push((LIST.into(), None));
    out
}

/// `std::process::Command::new(program)` with the system's environment for the bundle's variables.
pub fn command(program: impl AsRef<OsStr>) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    for (name, value) in restored() {
        match value {
            Some(value) => cmd.env(name, value),
            None => cmd.env_remove(name),
        };
    }
    cmd
}

/// Opens `uri` in the user's browser through `xdg-open`, with the system's environment (GTK's own
/// fallback, when no portal answers, would start the browser with the bundle's). False when
/// xdg-open can't be started, so that GTK can try its own way.
pub fn open_uri(uri: &str) -> bool {
    match open_command(uri).spawn() {
        Ok(mut child) => {
            // xdg-open returns once the browser is started; reap it off the GTK thread.
            std::thread::spawn(move || child.wait());
            true
        }
        Err(_) => false,
    }
}

fn open_command(uri: &str) -> std::process::Command {
    let mut cmd = command("xdg-open");
    cmd.arg(uri);
    cmd
}

/// The part of a source file that is not test code: everything before its `#[cfg(test)]` tests
/// module (`mod tests`, possibly `pub`). Other `#[cfg(test)]` items (a `mod test_http;`, a helper)
/// don't end it.
#[cfg(test)]
fn production_code(text: &str) -> &str {
    let end = text.match_indices("#[cfg(test)]\n").map(|(i, m)| i + m.len()).find(|&i| {
        let rest = text[i..].trim_start_matches("pub(crate) ").trim_start_matches("pub ");
        rest.starts_with("mod tests {")
    });
    end.map_or(text, |end| &text[..end - "#[cfg(test)]\n".len()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = vars.iter().map(|(k, v)| (k.to_string(), OsString::from(v))).collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn outside_the_appimage_or_on_the_host_path_nothing_changes() {
        assert!(restored_from(env(&[("GIO_MODULE_DIR", "/x")])).is_empty());
    }

    #[test]
    fn earlier_values_come_back_and_unset_ones_go() {
        let got = restored_from(env(&[
            (LIST, "GIO_MODULE_DIR GSETTINGS_SCHEMA_DIR"),
            ("GIO_MODULE_DIR", "/tmp/.mount_x/usr/lib/gio/modules"),
            ("GSETTINGS_SCHEMA_DIR", "/tmp/.mount_x/usr/share/glib-2.0/schemas:/home/u/schemas"),
            ("APPIMAGE_ORIG_GSETTINGS_SCHEMA_DIR", "/home/u/schemas"),
        ]));
        let want: Vec<(OsString, Option<OsString>)> = vec![
            ("GIO_MODULE_DIR".into(), None),
            ("APPIMAGE_ORIG_GIO_MODULE_DIR".into(), None),
            ("GSETTINGS_SCHEMA_DIR".into(), Some("/home/u/schemas".into())),
            ("APPIMAGE_ORIG_GSETTINGS_SCHEMA_DIR".into(), None),
            (LIST.into(), None),
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn odd_names_are_ignored() {
        let got = restored_from(env(&[(LIST, "PATH=x lower GTK_EXE_PREFIX")]));
        assert_eq!(got[0].0, OsString::from("GTK_EXE_PREFIX"));
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn only_the_tests_module_is_left_out_of_the_scan() {
        let text = "mod a;\n#[cfg(test)]\nmod test_http;\nfn f() { x(); }\n#[cfg(test)]\nmod tests {\n    fn g() {}\n}\n";
        assert_eq!(production_code(text), "mod a;\n#[cfg(test)]\nmod test_http;\nfn f() { x(); }\n");
        assert_eq!(production_code("fn f() {}\n"), "fn f() {}\n");
        assert_eq!(production_code("fn f() {}\n#[cfg(test)]\npub mod tests {\n}\n"), "fn f() {}\n");
    }

    #[test]
    fn open_uri_starts_xdg_open_through_command() {
        let cmd = open_command("https://example.org/x");
        assert_eq!(cmd.get_program(), "xdg-open");
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["https://example.org/x"]);
    }

    #[test]
    fn every_program_is_started_through_command() {
        // Outside test code, nothing may build a Command directly: it would inherit the bundle's
        // variables.
        fn scan(dir: &std::path::Path, bad: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    scan(&path, bad);
                } else if path.extension().is_some_and(|e| e == "rs") && !path.ends_with("host_env.rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let code = production_code(&text);
                    for (n, line) in code.lines().enumerate() {
                        if line.contains("Command::new(") && !line.trim_start().starts_with("//") {
                            bad.push(format!("{}:{}", path.display(), n + 1));
                        }
                    }
                }
            }
        }
        let mut bad = Vec::new();
        scan(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut bad);
        assert!(bad.is_empty(), "use host_env::command instead of Command::new at {bad:?}");
    }

    #[test]
    fn a_child_sees_the_system_environment() {
        // Run a real child through `command`, with the parent's environment as AppRun leaves it.
        let parent = [
            (LIST, "GIO_MODULE_DIR GTK_EXE_PREFIX"),
            ("GIO_MODULE_DIR", "/bundle/gio"),
            ("GTK_EXE_PREFIX", "/bundle/usr"),
            ("APPIMAGE_ORIG_GTK_EXE_PREFIX", "/opt/gtk"),
        ];
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "env"]);
        // What the child would inherit from the app, then what `command` does to it.
        for (name, value) in parent {
            cmd.env(name, value);
        }
        for (name, value) in restored_from(env(&parent)) {
            match value {
                Some(value) => cmd.env(name, value),
                None => cmd.env_remove(name),
            };
        }
        let out = String::from_utf8(cmd.output().unwrap().stdout).unwrap();
        assert!(out.lines().any(|l| l == "GTK_EXE_PREFIX=/opt/gtk"), "{out}");
        assert!(!out.contains("GIO_MODULE_DIR=") && !out.contains("APPIMAGE_"), "{out}");
    }
}
