//! Programs the app starts get the system's environment, not the AppImage's.
//!
//! The AppImage bundles GTK 4 and libadwaita, and its AppRun points them at their own modules
//! and schemas through a few environment variables (`GTK_EXE_PREFIX`, `GIO_MODULE_DIR`, ...).
//! A program started with those set would load the bundle's modules into its own GTK or GLib,
//! so AppRun names them in `APPIMAGE_BUNDLE_ENV` and keeps each one's earlier value in
//! `APPIMAGE_ORIG_<NAME>` (absent when it had none). [`command`] builds a
//! [`std::process::Command`] with those values put back. Outside the AppImage nothing is set and
//! nothing changes. The libraries themselves are found through the binary's rpath, never
//! `LD_LIBRARY_PATH`, so nothing else leaks.

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = vars.iter().map(|(k, v)| (k.to_string(), OsString::from(v))).collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn outside_the_appimage_nothing_changes() {
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
                    let code = text.split("#[cfg(test)]").next().unwrap_or_default();
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
