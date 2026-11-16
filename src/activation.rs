//! activation.rs — raising the window for a second launch, on Wayland.
//!
//! A Wayland compositor only gives a window the focus when asked with an
//! activation token that came from the user's action: the click on the
//! launcher that started the second copy. That copy passes its token over
//! the single-instance socket (instance.rs), and the running copy must hand
//! it to the compositor with `xdg_activation_v1.activate(token, surface)`.
//! That also ends the launcher's "starting…" feedback for the second copy.
//!
//! GPUI (the 0.3.12 snapshot) has no call for this: `Window::activate_window`
//! asks for a token of its own, which without a recent key press or click in
//! the window only marks it as wanting attention. So the request is sent
//! here, on GPUI's own Wayland connection, through the window's raw handles.

use gpui_kit::Window;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry::WlRegistry, wl_surface::WlSurface};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;

/// Asks the compositor to raise and focus `window` with `token`. False when
/// the window is not a Wayland one or the compositor has no activation
/// protocol; the caller then falls back to GPUI's own request.
pub fn activate(window: &Window, token: &str) -> bool {
    let (Ok(display), Ok(surface)) = (HasDisplayHandle::display_handle(window), HasWindowHandle::window_handle(window)) else {
        return false;
    };
    let (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) = (display.as_raw(), surface.as_raw()) else {
        return false;
    };
    // SAFETY: both pointers come from the live window just borrowed: its
    // wl_display and wl_surface, valid for the length of this call, which
    // runs on the UI thread that owns the connection.
    let sent = unsafe { send(display.display.as_ptr(), surface.surface.as_ptr(), token) };
    if let Err(e) = &sent {
        log::warn!("could not pass the activation token to the compositor: {e:#}");
    }
    sent.is_ok()
}

/// The state of the short-lived event queue used to find the protocol: it
/// needs to hear nothing.
struct Quiet;

impl Dispatch<WlRegistry, GlobalListContents> for Quiet {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_noop!(Quiet: ignore XdgActivationV1);

/// # Safety
/// `display` must be a live `wl_display` and `surface` a live `wl_surface`
/// of that display, and no other thread may be reading the connection.
unsafe fn send(display: *mut std::ffi::c_void, surface: *mut std::ffi::c_void, token: &str) -> anyhow::Result<()> {
    // A guest handle on GPUI's connection: dropping it closes nothing.
    let connection = Connection::from_backend(Backend::from_foreign_display(display.cast()));
    let (globals, queue) = registry_queue_init::<Quiet>(&connection)?;
    let activation: XdgActivationV1 = globals.bind(&queue.handle(), 1..=1, ())?;
    let surface = WlSurface::from_id(&connection, ObjectId::from_ptr(WlSurface::interface(), surface.cast())?)?;
    activation.activate(token.to_string(), &surface);
    activation.destroy();
    connection.flush()?;
    Ok(())
}
