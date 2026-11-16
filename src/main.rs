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
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("green-light: {} [{}] {}", record.level(), record.target(), record.args());
        }
    }

    fn flush(&self) {}
}

fn main() {
    // Only the primary copy goes on: a second launch has asked the running
    // one to show its window.
    let Some(activations) = instance::claim() else {
        return;
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
