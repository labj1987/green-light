mod activation;
mod branches;
mod download;
mod install;
mod instance;
mod preflight;
mod system;
#[cfg(test)]
mod test_http;
mod ui;
mod versions;

use std::sync::OnceLock;
use tokio::runtime::Runtime;

/// The Wayland app_id and X11 window class. It matches `StartupWMClass` in the
/// .desktop file, so the running window is tied to the desktop file (name,
/// icon) on either backend.
const APP_ID: &str = "io.github.labj1987.GreenLight";

static TOKIO_RT: OnceLock<Runtime> = OnceLock::new();

pub fn runtime() -> &'static Runtime {
    TOKIO_RT.get_or_init(|| tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("Failed to build Tokio runtime"))
}

/// The UI toolkit reports renderer and display-server problems through the
/// `log` crate; they go to stderr so a terminal or the journal shows them.
struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::max_level() && metadata.level() <= level_cap(metadata.target())
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("green-light: {} [{}] {}", record.level(), record.target(), record.args());
        }
    }

    fn flush(&self) {}
}

/// Crates whose info and debug messages are about their own workings (zbus
/// prints every step of its D-Bus handshake, raw bytes and all): only their
/// warnings and errors are shown, whatever GREEN_LIGHT_LOG asks for.
const QUIET_CRATES: &[&str] =
    &["zbus", "zbus_names", "zvariant", "ashpd", "async_io", "polling", "calloop", "mio", "hyper", "hyper_util", "h2", "reqwest", "rustls", "tracing"];

/// The most detailed level shown for messages from `target` (a module path).
fn level_cap(target: &str) -> log::LevelFilter {
    let krate = target.split("::").next().unwrap_or(target);
    if QUIET_CRATES.contains(&krate) {
        log::LevelFilter::Warn
    } else {
        log::LevelFilter::Trace
    }
}

fn main() {
    // Only the primary copy goes on: a second launch has asked the running
    // one to show its window.
    let activations = match instance::claim() {
        instance::Claim::Primary(activations) => activations,
        instance::Claim::HandedOver => return,
        instance::Claim::NotResponding(pid) => {
            ui::show_error_without_window(&instance::not_responding_message(pid));
            std::process::exit(1);
        }
    };
    log::set_logger(&StderrLog).ok();
    // GREEN_LIGHT_LOG=info (or debug) also shows which renderer was picked.
    let level = std::env::var("GREEN_LIGHT_LOG").ok().and_then(|level| level.parse().ok()).unwrap_or(log::LevelFilter::Warn);
    log::set_max_level(level);
    let _ = runtime();

    gpui_kit::application().with_assets(ui::Assets).run(move |cx| {
        ui::init(cx);
        ui::build_ui(cx, activations);
    });
}

#[cfg(test)]
mod tests {
    use super::level_cap;
    use log::{Level, LevelFilter};

    #[test]
    fn noisy_dependencies_only_warn() {
        assert_eq!(level_cap("zbus::connection::handshake::client"), LevelFilter::Warn);
        assert_eq!(level_cap("zbus"), LevelFilter::Warn);
        assert!(Level::Info > level_cap("zbus::conn"), "zbus's handshake lines are hidden at GREEN_LIGHT_LOG=info");
        assert!(Level::Warn <= level_cap("zbus::conn"), "its warnings still show");
        assert_eq!(level_cap("gpui::platform"), LevelFilter::Trace, "the toolkit's renderer choice still shows");
        assert_eq!(level_cap("green_light::ui"), LevelFilter::Trace);
        assert_eq!(level_cap("zbusy"), LevelFilter::Trace, "whole crate names only");
    }
}
