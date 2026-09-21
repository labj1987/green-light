mod download;
mod install;
mod system;
mod ui;
mod versions;

use gtk4::prelude::*;
use std::sync::OnceLock;
use tokio::runtime::Runtime;

static TOKIO_RT: OnceLock<Runtime> = OnceLock::new();

pub fn runtime() -> &'static Runtime {
    TOKIO_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to build Tokio runtime")
    })
}

fn main() {
    let _ = runtime();

    // Set program name before GTK init. On Wayland the app_id GNOME sees is
    // the GApplication ID, not prgname; on X11 it's prgname. Setting both
    // prgname and StartupWMClass (in the .desktop file) to the application
    // ID makes the running window match the desktop file on either backend.
    glib::set_prgname(Some("io.github.labj1987.GreenLight"));
    glib::set_application_name("Green Light");

    let app = libadwaita::Application::builder()
        .application_id("io.github.labj1987.GreenLight")
        .flags(gio::ApplicationFlags::FLAGS_NONE)
        .build();

    app.connect_activate(|app| {
        if let Some(window) = app.windows().first() {
            window.present();
            return;
        }
        ui::build_ui(app);
    });

    std::process::exit(app.run().get() as i32);
}
