use crate::branches::{fetch_branches, DriverBranch, UNIX_DRIVERS_PAGE};
use crate::download::{download_run_file, verify_sha256};
use crate::install::{
    check_run_file, install_running, recovery_note, recovery_pending, run_privileged_discard_recovery, run_privileged_install, run_privileged_recover,
    run_privileged_setup_signing, setup_problem, InstallOptions, Recovered, RecoveryNote, StaleRecovery,
};
use crate::instance::Activations;
use crate::preflight::{
    conflict_summary, module_hint, secure_boot_advice, secure_boot_summary, signing_summary, validate_module_choice, validate_mok_password, ModuleChoice,
    MOK_PASSWORD_HINT,
};
use crate::system::{format_bytes, query_system, SecureBootStatus, SystemInfo, MIN_DISK_BYTES};
use crate::versions::{fetch_checksum, fetch_versions, is_sha256_hex, version_from_filename, DriverVersion};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::link::Link;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, Root, Sizable as _, Theme, ThemeMode, ThemeRegistry, TitleBar, WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    accesskit, div, hsla, img, px, size, uniform_list, Anchor, AnyElement, App, AppContext as _, AssetSource, Bounds, ClipboardItem, Context, Div, Element,
    ElementId, Entity, FocusHandle, Focusable as _, FontWeight, GlobalElementId, Hsla, Image, ImageFormat, InspectorElementId, InteractiveElement as _,
    IntoElement, KeyDownEvent, LayoutId, MouseButton, ParentElement as _, PathPromptOptions, Pixels, Position, Render, Role, ScrollHandle, ScrollStrategy,
    SharedString, Stateful, StatefulInteractiveElement as _, Style, Styled as _, Subscription, Task, Text, UniformListScrollHandle, Window, WindowBounds,
    WindowDecorations, WindowOptions,
};

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

const WEBSITE: &str = "https://github.com/labj1987/green-light";
const ISSUES: &str = "https://github.com/labj1987/green-light/issues";

// GPUI Kit's component icons plus the one this app adds.
gpui_kit::assets::icon_assets!(ExtraIcons, [RefreshCw]);

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  App state
// ─────────────────────────────────────────────────────────────────────────────

/// The one long operation running. Only one runs at a time: a second
/// download would write the same file and fight over the progress bar, and
/// the privileged actions must not run on a file still being written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    /// From the download through checksum verification and any question
    /// about an unverified file.
    Download,
    Install,
    Signing,
}

struct AppState {
    versions: Vec<DriverVersion>,
    selected_version: Option<DriverVersion>,
    downloaded_path: Option<String>,
    expected_checksum: Option<String>,
    use_dkms: bool,
    block_distro_driver: bool,
    module_choice: ModuleChoice,
    sysinfo: SystemInfo,
    sysinfo_loaded: bool,
    busy: Option<Busy>,
    /// Counts started operations, so a BusyGuard only ever ends its own.
    busy_token: u64,
    /// An install started by an earlier run of the app is still going.
    external_install: bool,
    download_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// The next install may drop stale recovery data (the user said so).
    discard_recovery_next: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            versions: vec![],
            selected_version: None,
            downloaded_path: None,
            expected_checksum: None,
            use_dkms: true,
            block_distro_driver: false,
            module_choice: ModuleChoice::Auto,
            sysinfo: SystemInfo::default(),
            sysinfo_loaded: false,
            busy: None,
            busy_token: 0,
            external_install: false,
            download_cancel: None,
            discard_recovery_next: false,
        }
    }
}

/// Which actions may start now, given what is running.
#[derive(Debug, PartialEq, Eq)]
struct ActionsEnabled {
    download: bool,
    open_file: bool,
    install: bool,
    setup_signing: bool,
}

fn actions_enabled(s: &AppState) -> ActionsEnabled {
    let idle = s.busy.is_none() && !s.external_install;
    ActionsEnabled {
        download: idle && s.selected_version.is_some(),
        open_file: idle,
        // The install options (the distro-driver block above all) start from
        // the machine's state, so not before that has been read.
        install: idle && s.sysinfo_loaded && s.downloaded_path.is_some(),
        setup_signing: idle && s.sysinfo_loaded && s.sysinfo.signing_key.setup_needed(),
    }
}

type SharedState = Rc<RefCell<AppState>>;

/// Holds the one running operation. Dropping it ends the operation, so a
/// task that panics (and whose callback therefore never runs) can't leave
/// every button disabled; `release` ends it early. It only ever ends its own
/// operation, never one started after it. The window redraws from the state,
/// so ending it needs no more than the state change (the status poll redraws
/// within two seconds when nothing else does).
struct BusyGuard {
    state: SharedState,
    token: u64,
}

thread_local! {
    /// Releases that found the state borrowed; done by the next status poll.
    static DEFERRED_RELEASES: RefCell<Vec<(SharedState, u64)>> = RefCell::default();
}

impl BusyGuard {
    /// Start `kind`, unless something is already running.
    fn start(state: &SharedState, kind: Busy) -> Option<Rc<Self>> {
        let mut s = state.borrow_mut();
        if s.busy.is_some() || s.external_install {
            return None;
        }
        s.busy_token += 1;
        s.busy = Some(kind);
        Some(Rc::new(Self { state: state.clone(), token: s.busy_token }))
    }

    fn release(&self) {
        end_busy(&self.state, self.token);
    }
}

fn end_busy(state: &SharedState, token: u64) {
    let Ok(mut s) = state.try_borrow_mut() else {
        // Called while the state is borrowed: finish on the next poll.
        DEFERRED_RELEASES.with(|d| d.borrow_mut().push((state.clone(), token)));
        return;
    };
    if s.busy.is_none() || s.busy_token != token {
        return;
    }
    s.busy = None;
    s.download_cancel = None;
}

fn run_deferred_releases() {
    let pending = DEFERRED_RELEASES.with(|d| std::mem::take(&mut *d.borrow_mut()));
    for (state, token) in pending {
        end_busy(&state, token);
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.release();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Async bridge: run a future on the shared Tokio runtime, deliver the result
//  back on the UI thread, in the window it was started from.
// ─────────────────────────────────────────────────────────────────────────────

fn spawn_async<F, T, CB>(window: &Window, cx: &App, future: F, callback: CB)
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
    CB: FnOnce(T, &mut Window, &mut App) + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel::<T>();
    crate::runtime().spawn(async move {
        let _ = tx.send(future.await);
    });
    window
        .spawn(cx, async move |cx| {
            if let Ok(val) = rx.await {
                let _ = cx.update(|window, cx| callback(val, window, cx));
            }
        })
        .detach();
}

/// A message this long stays until it is closed; shorter ones go away on
/// their own.
const LONG_TOAST: usize = 120;

/// A short message at the bottom of the window. Notifications show their
/// text as plain text, so error text needs no escaping. Nothing is shown
/// before the window has the toolkit's root view (pushing one earlier panics
/// in the toolkit).
fn toast(window: &mut Window, cx: &mut App, text: &str) {
    if window.root::<Root>().flatten().is_none() {
        eprintln!("green-light: not shown (the window is not up yet): {text}");
        return;
    }
    window.push_notification(Notification::new().message(text.to_string()).placement(Anchor::BottomCenter).autohide(text.chars().count() <= LONG_TOAST), cx);
    announce(text);
    window.refresh();
}

// ─────────────────────────────────────────────────────────────────────────────
//  Screen readers. GPUI reports an element to assistive technology only when
//  it has an id and a role, so the text, dialog names and announcements below
//  are spelled out; a bare string child is drawn but never read.
// ─────────────────────────────────────────────────────────────────────────────

/// Text a screen reader can read (a label node holding the text).
fn label(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Text {
    Text::new(id.into(), text.into())
}

/// GPUI Kit 0.9.1 gives a dialog no accessible name and has no way to set
/// one, so everything in a dialog, its buttons included, sits in a group
/// named after the title: a screen reader says the name when the keyboard
/// focus moves into the group, which is when the dialog opens.
fn named_group(id: impl Into<ElementId>, name: impl Into<SharedString>) -> Stateful<Div> {
    v_flex().id(id.into()).role(Role::Group).aria_label(name).gap_3()
}

// Every dialog is opened with `.overlay(false)`: the toolkit's dimming layer
// (0.9.1) also covers the invisible margin a client-decorated window keeps
// around itself for its shadow, which shows as a dark frame around the
// window on KDE and GNOME. Without it a dialog is as modal as with it (it
// takes the input and the focus); gone are the dimming, click-outside-to-close
// and dragging the window by its title bar or the dimmed area, for which the
// dialog's own title stands in (`dialog_title`).

/// The height a dialog's text may take before it scrolls. The toolkit cuts
/// off a dialog that is taller than the window instead of scrolling it
/// (0.9.1), which would take the buttons out of reach, so the part above
/// the buttons gets what the window leaves after the dialog's margin, title
/// and button row.
fn dialog_room(window: &Window) -> Pixels {
    (window.viewport_size().height - px(DIALOG_CHROME)).max(px(60.))
}

/// What a dialog needs of the window's height besides its text.
const DIALOG_CHROME: f32 = 220.;

/// A dialog's title, which is also the handle to move the window by: while a
/// dialog is open the toolkit keeps the pointer off everything under it, the
/// window's own title bar included.
fn dialog_title(title: impl Into<SharedString>) -> impl IntoElement {
    div().w_full().on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move()).child(label("title", title))
}

/// The button row at the bottom of a dialog.
fn dialog_buttons() -> Div {
    h_flex().justify_end().gap_2().pt_1()
}

/// What Enter does in a dialog. The toolkit hands Enter to the dialog (its
/// "confirm") before the focused button sees it, so the dialog looks at
/// where the focus is and acts as that button would: Enter on a button
/// presses it, and Enter anywhere else (a field, the close button) decides
/// nothing and leaves the dialog open.
fn enter_key(focused: &[bool]) -> Option<usize> {
    focused.iter().position(|&f| f)
}

thread_local! {
    /// The newest notification, for `Announcer`.
    static ANNOUNCEMENT: RefCell<SharedString> = RefCell::default();
}

/// Has the screen reader read `text` out (through the main window's `Announcer`).
fn announce(text: &str) {
    ANNOUNCEMENT.with(|current| {
        let mut current = current.borrow_mut();
        // The announcement is the change of the node's name, so the same
        // message twice in a row needs a name that differs: an invisible
        // zero-width space is added or dropped.
        *current = if current.as_ref() == text { format!("{text}\u{200b}").into() } else { text.to_string().into() };
    });
}

/// A live region: an invisible node whose accessible name is the newest
/// notification, marked "polite" so a screen reader reads each change out
/// once it has finished speaking. GPUI Kit's own notifications are not
/// announced (longbridge/gpui-kit issue 3659: no live regions).
struct Announcer(SharedString);

impl IntoElement for Announcer {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Announcer {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some("announcer".into())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn a11y_role(&self) -> Option<Role> {
        Some(Role::Status)
    }

    fn write_a11y_info(&self, node: &mut accesskit::Node) {
        node.set_live(accesskit::Live::Polite);
        if !self.0.is_empty() {
            node.set_label(self.0.to_string());
        }
    }

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        // Out of the flow and with no size: it takes no room in the window.
        let style = Style { position: Position::Absolute, ..Style::default() };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, _: Bounds<Pixels>, _: &mut (), _: &mut Window, _: &mut App) {}

    fn paint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, _: Bounds<Pixels>, _: &mut (), _: &mut (), _: &mut Window, _: &mut App) {}
}

// ─────────────────────────────────────────────────────────────────────────────
//  Dialogs
// ─────────────────────────────────────────────────────────────────────────────

/// How a dialog button looks.
#[derive(Clone, Copy, PartialEq)]
enum Look {
    Plain,
    /// The suggested action.
    Primary,
    /// An action that throws something away.
    Danger,
}

/// A question with a row of buttons. The answer is the index of the button
/// pressed; closing the dialog any other way (Escape, its close button) is
/// the answer `dismiss`. `default` is the button that has the keyboard focus
/// when it opens, so Enter and Space act on it.
struct Choice {
    id: &'static str,
    title: SharedString,
    body: SharedString,
    buttons: Vec<(&'static str, &'static str, Look)>,
    default: usize,
    dismiss: usize,
}

type Answer = Box<dyn FnOnce(usize, &mut Window, &mut App)>;

/// Shows `choice`; `on_answer` runs once, after the dialog is out of the way
/// (so it may open the next one).
fn show_choice(choice: Choice, on_answer: impl FnOnce(usize, &mut Window, &mut App) + 'static, window: &mut Window, cx: &mut App) {
    let Choice { id, title, body, buttons, default, dismiss } = choice;
    let slot: Rc<RefCell<Option<Answer>>> = Rc::new(RefCell::new(Some(Box::new(on_answer))));
    let focus: Rc<Vec<FocusHandle>> = Rc::new(buttons.iter().map(|_| cx.focus_handle()).collect());
    let first = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            focus[default].focus(window, cx);
        }
        let answer = {
            let slot = slot.clone();
            move |index: usize, window: &mut Window, cx: &mut App| {
                if let Some(callback) = slot.borrow_mut().take() {
                    window.defer(cx, move |window, cx| callback(index, window, cx));
                }
            }
        };
        let enter = answer.clone();
        let dismissed = answer.clone();
        let enter_focus = focus.clone();
        let row = buttons.iter().enumerate().fold(dialog_buttons(), |row, (index, &(button_id, text, look))| {
            let answer = answer.clone();
            row.child(
                Button::new(button_id)
                    .label(text)
                    .track_focus(&focus[index])
                    .map(|b| match look {
                        Look::Plain => b,
                        Look::Primary => b.primary(),
                        Look::Danger => b.danger(),
                    })
                    .on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        answer(index, window, cx);
                    }),
            )
        });
        dialog
            .title(dialog_title(title.clone()))
            .overlay(false)
            .w(px(440.))
            // Enter: what the focused button says (see `enter_key`).
            .on_ok(move |_, window, cx| {
                let focused: Vec<bool> = enter_focus.iter().map(|f| f.is_focused(window)).collect();
                match enter_key(&focused) {
                    Some(index) => {
                        enter(index, window, cx);
                        true
                    }
                    None => false,
                }
            })
            // Escape and the close button.
            .on_close(move |_, window, cx| dismissed(dismiss, window, cx))
            .child(
                named_group(id, title.clone())
                    .child(div().id("body").max_h(dialog_room(window)).overflow_y_scroll().child(label("text", body.clone())))
                    .child(row),
            )
    });
}

/// A message the user has to see, in a dialog over the window.
fn show_error(heading: &str, body: &str, window: &mut Window, cx: &mut App) {
    show_choice(
        Choice {
            id: "error",
            title: heading.to_string().into(),
            body: body.to_string().into(),
            buttons: vec![("error-close", "Close", Look::Plain)],
            default: 0,
            dismiss: 0,
        },
        |_, _, _| {},
        window,
        cx,
    );
}

// ─────────────────────────────────────────────────────────────────────────────
//  No window at all
// ─────────────────────────────────────────────────────────────────────────────

/// The window is drawn with the GPU, so with no usable Vulkan or OpenGL ES
/// driver (not even Mesa's software ones) there is nothing to draw an error
/// in. Says so through whatever the desktop offers, then exits: a process
/// left behind with no window would also keep later launches from starting.
fn no_window(error: &str) -> ! {
    let message = no_window_message(error, has_display());
    show_error_without_window(&message);
    std::process::exit(1);
}

fn has_display() -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"].into_iter().any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// Shows `message` with no window of the app's own: on stderr, and in the
/// first of the desktop's dialog or notification programs that shows it.
pub fn show_error_without_window(message: &str) {
    eprintln!("green-light: {message}");
    // With no display there is nowhere to show a dialog either.
    if !has_display() {
        return;
    }
    for (program, args) in error_dialogs(message) {
        let status = std::process::Command::new(program).args(&args).stdin(std::process::Stdio::null()).status();
        if status.is_ok_and(|status| helper_showed(program, status.code())) {
            break;
        }
    }
}

/// The programs tried, in order, to show an error when the app has no
/// window, with their arguments. gdbus sends a notification straight over
/// D-Bus, for desktops with a notification service but no notify-send (Pop!_OS
/// COSMIC); xmessage is the last resort.
fn error_dialogs(message: &str) -> Vec<(&'static str, Vec<String>)> {
    let args = |fixed: &[&str]| fixed.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    let with_message = |fixed: &[&str]| {
        let mut all = args(fixed);
        all.push(message.to_string());
        all
    };
    vec![
        ("kdialog", with_message(&["--title", "Green Light", "--error"])),
        ("zenity", with_message(&["--error", "--no-markup", "--title=Green Light", "--width=480", "--text"])),
        ("notify-send", with_message(&["--urgency=critical", "--app-name=Green Light", "Green Light"])),
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
                &gvariant_string(message),
                "@as []",
                "{'urgency': <byte 2>}",
                "int32 -1",
            ]),
        ),
        ("xmessage", with_message(&["-center", "-title", "Green Light"])),
    ]
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

/// Whether `program`, having ended with exit `code`, did show the message, so
/// the next one must not show it again. The dialogs end with 0 when the user
/// pressed their button and 1 when the user closed them another way; both
/// mean it was seen. Anything else (no display, a crash, a signal) does not.
/// notify-send and gdbus show nothing themselves: only 0 says the desktop
/// took the message.
fn helper_showed(program: &str, code: Option<i32>) -> bool {
    match program {
        "notify-send" | "gdbus" => code == Some(0),
        _ => matches!(code, Some(0 | 1)),
    }
}

fn no_window_message(error: &str, has_display: bool) -> String {
    if has_display {
        format!(
            "Green Light could not open its window: no working graphics driver was found \
             (Vulkan or OpenGL ES). Green Light can draw with Mesa's software renderer when the \
             NVIDIA driver is broken or missing: install it (mesa-vulkan-drivers on Debian, \
             Ubuntu and Fedora; vulkan-swrast on Arch), then start Green Light again.\n\nDetails: {error}"
        )
    } else {
        format!(
            "Green Light could not open its window: there is no display to open it on \
             (neither WAYLAND_DISPLAY nor DISPLAY is set). Start it from your desktop session.\
             \n\nDetails: {error}"
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Version comparison helper
// ─────────────────────────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum VersionRelation {
    Newer,
    Same,
    Older,
    Unknown,
}

fn compare_versions(installed: &str, candidate: &str) -> VersionRelation {
    let parse = |s: &str| -> Vec<u32> { s.split('.').filter_map(|x| x.parse().ok()).collect() };
    let iv = parse(installed);
    let cv = parse(candidate);
    if iv.is_empty() || cv.is_empty() {
        return VersionRelation::Unknown;
    }
    match cv.cmp(&iv) {
        std::cmp::Ordering::Greater => VersionRelation::Newer,
        std::cmp::Ordering::Equal => VersionRelation::Same,
        std::cmp::Ordering::Less => VersionRelation::Older,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Toolkit start-up and the window
// ─────────────────────────────────────────────────────────────────────────────

/// The families tried for the UI font, in the order GPUI itself falls back to them.
const UI_FONTS: [&str; 9] = ["Ubuntu", "Adwaita Sans", "Cantarell", "Noto Sans", "DejaVu Sans", "Liberation Sans", "Arial", "Helvetica", "FreeSans"];

/// The families tried for fixed-width text (the Log tab), the toolkit's own
/// default first.
const MONO_FONTS: [&str; 10] = [
    "DejaVu Sans Mono",
    "Noto Sans Mono",
    "Liberation Mono",
    "Ubuntu Mono",
    "Ubuntu Sans Mono",
    "Adwaita Mono",
    "Source Code Pro",
    "Hack",
    "FreeMono",
    "Nimbus Mono PS",
];

/// The first of `families` that is installed.
fn first_installed(families: &[&'static str], installed: &[String]) -> Option<&'static str> {
    families.iter().copied().find(|family| installed.iter().any(|name| name == family))
}

/// Starts the toolkit, with installed families as its fonts. Left alone, the
/// theme asks for GPUI's "system UI font", a family no Linux desktop ships,
/// and for a fixed-width family that many lack; GPUI and the toolkit then
/// log warnings and fall back to the first family they find. Naming those
/// up front gives the same text and no warnings.
pub fn init(cx: &mut App) {
    let installed = cx.text_system().all_font_names();
    let ui_font = first_installed(&UI_FONTS, &installed);
    if let Some(ui_font) = ui_font {
        // The toolkit keeps a theme that is already there, and families set
        // on it are used as they are.
        let mut theme = Theme::default();
        theme.font_family = ui_font.into();
        theme.mono_font_family = first_installed(&MONO_FONTS, &installed).unwrap_or(ui_font).into();
        cx.set_global(theme);
    }
    gpui_kit::init(cx);
    if ui_font.is_some() {
        // What the toolkit does for a theme it creates itself: its light and
        // dark colours, then the mode.
        let registry = ThemeRegistry::global(cx);
        let light = registry.default_light_theme().clone();
        let dark = registry.default_dark_theme().clone();
        let theme = Theme::global_mut(cx);
        theme.light_theme = light;
        theme.dark_theme = dark;
        Theme::change(ThemeMode::Light, None, cx);
    }
}

pub fn build_ui(cx: &mut App, mut activations: Activations) {
    // With no display GPUI would run on with no window and no word about it.
    if ["WAYLAND_DISPLAY", "DISPLAY"].into_iter().all(|name| std::env::var_os(name).is_none_or(|value| value.is_empty())) {
        no_window("no display server to connect to");
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(760.), px(640.)), cx)),
        window_min_size: Some(size(px(560.), px(420.))),
        app_id: Some(crate::APP_ID.to_string()),
        // The app draws its own title bar, so it looks and works the same on
        // every desktop, including compositors with no server-side decorations.
        window_decorations: Some(WindowDecorations::Client),
        ..TitleBar::window_options()
    };
    let opened = gpui_kit::open_window(options, cx, |window, cx| {
        window.set_window_title("Green Light");
        // Follow the desktop's light/dark preference, now and when it changes.
        Theme::sync_system_appearance(Some(window), cx);
        window
            .observe_window_appearance(|window, cx| {
                Theme::sync_system_appearance(Some(window), cx);
            })
            .detach();
        cx.new(|cx| MainView::new(window, cx))
    });
    let (window, view) = match opened {
        Ok(opened) => opened,
        Err(e) => no_window(&format!("{e:#}")),
    };
    // The first loads, now that the window can show what went wrong.
    let _ = window.update(cx, |_, window, cx| {
        view.update(cx, |view, cx| view.start(window, cx));
        // Closing by the desktop (Alt+F4, the window list): the same question
        // as the title bar's close button.
        let state = view.read(cx).state.clone();
        let allow_close = view.read(cx).allow_close.clone();
        window.on_window_should_close(cx, move |window, cx| close_allowed(&state, &allow_close, window, cx));
    });

    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();

    // A second launch asks this one to show its window, with the activation
    // token the desktop gave that launch when there is one: on Wayland only
    // that raises the window (see activation.rs). Without a token, or off
    // Wayland, GPUI's own request is what there is; a Wayland compositor then
    // marks the window as wanting attention instead of raising it.
    cx.spawn(async move |cx| {
        while let Some(request) = activations.recv().await {
            let shown = window.update(cx, |_, window, _| {
                let raised = request.token.as_deref().is_some_and(|token| crate::activation::activate(window, token));
                if !raised {
                    window.activate_window();
                }
            });
            // The launch that asked waits for this; without it, it reports
            // this copy as not responding.
            if shown.is_ok() {
                request.done();
            }
        }
    })
    .detach();
}

/// Closing the window doesn't stop a running install (it carries on, or puts
/// the previous driver back, on its own), but the user must not take the
/// closed window as "done" and reboot. True when the window may close now;
/// otherwise the question is asked, and its Close Anyway closes it.
fn close_allowed(state: &SharedState, allow_close: &Rc<Cell<bool>>, window: &mut Window, cx: &mut App) -> bool {
    let busy = state.borrow().busy;
    if allow_close.get() || !matches!(busy, Some(Busy::Install) | Some(Busy::Signing)) {
        return true;
    }
    let allow_close = allow_close.clone();
    show_choice(
        Choice {
            id: "close-question",
            title: "The install is still running".into(),
            body: "Closing Green Light doesn't stop it: the install finishes, or puts the \
                   previous driver back, on its own. Its progress is in \
                   /var/log/green-light.log. Don't reboot until that log says it is done."
                .into(),
            buttons: vec![("close-keep", "Keep Open", Look::Plain), ("close-anyway", "Close Anyway", Look::Danger)],
            default: 0,
            dismiss: 0,
        },
        move |answer, window, _| {
            if answer == 1 {
                allow_close.set(true);
                window.remove_window();
            }
        },
        window,
        cx,
    );
    false
}

// ─────────────────────────────────────────────────────────────────────────────
//  The main window
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    System,
    Browse,
    Configure,
    Log,
}

const PAGES: [(Page, &str); 4] = [(Page::System, "System"), (Page::Browse, "Browse"), (Page::Configure, "Configure"), (Page::Log, "Log")];

enum ListStatus {
    Loading,
    Failed(String),
    Loaded,
}

/// The progress bar of the Configure tab.
#[derive(Default)]
struct ProgressBar {
    visible: bool,
    /// 0.0 to 1.0; `None` is an activity animation (size unknown, or an
    /// install, whose length nobody knows).
    fraction: Option<f32>,
    text: String,
}

/// The banner for recovery data an interrupted install left: its text and
/// button follow the install script's note.
struct RecoverBanner {
    text: String,
    button: &'static str,
    /// The button asks whether to retry or discard instead of restoring.
    discards: bool,
}

pub struct MainView {
    state: SharedState,
    page: Page,
    list_status: ListStatus,
    filter: Entity<InputState>,
    list_scroll: UniformListScrollHandle,
    /// The system info is being read (again).
    sysinfo_refreshing: bool,
    progress: ProgressBar,
    log: Vec<SharedString>,
    log_scroll: ScrollHandle,
    /// AppRun's launch-time setup failed or was cancelled: why.
    setup_problem: Option<String>,
    recover: Option<RecoverBanner>,
    allow_close: Rc<Cell<bool>>,
    /// Holds the keyboard focus until a control takes it, so Tab and the
    /// other keys work from the moment the window opens.
    focus_handle: FocusHandle,
    _poll: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl MainView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter versions…"));
        let subscription = cx.subscribe_in(&filter, window, |this, _, event: &InputEvent, _, cx| {
            if let InputEvent::Change = event {
                this.filter_changed(cx);
            }
        });
        // Every two seconds: an install that outlived an earlier window, and
        // recovery data an interrupted one left. The checks touch the file
        // system, so they run off the UI thread.
        let poll = cx.spawn_in(window, async move |this, cx| loop {
            let checked = crate::runtime()
                .spawn_blocking(|| {
                    let pending = recovery_pending();
                    let note = if pending { recovery_note() } else { None };
                    (pending, note, install_running())
                })
                .await;
            if let Ok((pending, note, running)) = checked {
                if this.update_in(cx, |this, window, cx| this.status_checked(pending, note, running, window, cx)).is_err() {
                    break;
                }
            }
            cx.background_executor().timer(Duration::from_secs(2)).await;
        });
        let view = Self {
            state: Rc::new(RefCell::new(AppState::default())),
            page: Page::System,
            list_status: ListStatus::Loading,
            filter,
            list_scroll: UniformListScrollHandle::new(),
            sysinfo_refreshing: false,
            progress: ProgressBar::default(),
            log: Vec::new(),
            log_scroll: ScrollHandle::new(),
            setup_problem: setup_problem(),
            recover: None,
            allow_close: Rc::new(Cell::new(false)),
            focus_handle: cx.focus_handle(),
            _poll: poll,
            _subscriptions: vec![subscription],
        };
        view.focus_handle.focus(window, cx);
        view
    }

    /// The first loads, run by `build_ui` once the window is up.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_sysinfo(window, cx);
        self.load_versions(window, cx);
    }

    fn log(&mut self, msg: impl Into<String>, cx: &mut Context<Self>) {
        self.log.push(msg.into().into());
        self.log_scroll.scroll_to_bottom();
        cx.notify();
    }

    // ── System info ──────────────────────────────────────────────────────────

    fn load_sysinfo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sysinfo_refreshing = true;
        self.log("Querying system info…", cx);
        let view = cx.weak_entity();
        spawn_async(window, cx, async move { tokio::task::spawn_blocking(query_system).await.unwrap_or_default() }, move |info, _, cx| {
            let _ = view.update(cx, |this, cx| this.sysinfo_loaded(info, cx));
        });
    }

    fn sysinfo_loaded(&mut self, info: SystemInfo, cx: &mut Context<Self>) {
        self.sysinfo_refreshing = false;
        match &info.installed_driver {
            Some(ver) => self.log(format!("Installed driver: {ver}"), cx),
            None => self.log("No NVIDIA driver detected", cx),
        }
        let mut s = self.state.borrow_mut();
        // The block option shows what is in place now: leaving it on keeps
        // an existing block, turning it off removes it.
        s.block_distro_driver = info.distro_driver_blocked && info.block_supported;
        s.sysinfo = info;
        s.sysinfo_loaded = true;
        drop(s);
        cx.notify();
    }

    // ── Versions ─────────────────────────────────────────────────────────────

    fn load_versions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list_status = ListStatus::Loading;
        {
            let mut s = self.state.borrow_mut();
            s.versions.clear();
            s.selected_version = None;
        }
        self.log("Fetching version list from NVIDIA…", cx);
        let view = cx.weak_entity();
        spawn_async(window, cx, fetch_versions(), move |result, window, cx| {
            let _ = view.update(cx, |this, cx| this.versions_loaded(result, window, cx));
        });
    }

    fn versions_loaded(&mut self, result: anyhow::Result<Vec<DriverVersion>>, window: &mut Window, cx: &mut Context<Self>) {
        let versions = match result {
            Err(e) => {
                self.log(format!("Error fetching versions: {e}"), cx);
                self.list_status = ListStatus::Failed(format!("Failed to load versions: {e}"));
                return;
            }
            Ok(versions) => versions,
        };
        self.log(format!("Found {} versions", versions.len()), cx);
        self.list_status = ListStatus::Loaded;
        self.state.borrow_mut().versions = versions;
        // The branch labels come separately and may be slow or unavailable;
        // the list is usable without them.
        let view = cx.weak_entity();
        spawn_async(window, cx, async { tokio::time::timeout(BRANCH_LABEL_TIMEOUT, fetch_branches(UNIX_DRIVERS_PAGE)).await }, move |result, _, cx| {
            let _ = view.update(cx, |this, cx| match result {
                Ok(Ok(branches)) => {
                    let mut s = this.state.borrow_mut();
                    for v in &mut s.versions {
                        v.branch = branches.classify(&v.version);
                    }
                    drop(s);
                    cx.notify();
                }
                Ok(Err(e)) => this.log(format!("No branch labels: NVIDIA's branch data is unavailable ({e:#})"), cx),
                Err(_) => this.log("No branch labels: NVIDIA's branch data took too long", cx),
            });
        });
    }

    /// The versions the filter lets through.
    fn visible_versions(&self, cx: &App) -> Vec<DriverVersion> {
        let filter = self.filter.read(cx).value().to_lowercase();
        self.state.borrow().versions.iter().filter(|v| filter.is_empty() || v.version.contains(&filter)).cloned().collect()
    }

    /// A version the filter hides is no longer the chosen one: Download must
    /// not fetch a driver the list doesn't show.
    fn filter_changed(&mut self, cx: &mut Context<Self>) {
        let visible = self.visible_versions(cx);
        let mut s = self.state.borrow_mut();
        if s.selected_version.as_ref().is_some_and(|sel| !visible.iter().any(|v| v.version == sel.version)) {
            s.selected_version = None;
        }
        drop(s);
        cx.notify();
    }

    /// Selection by version string, never by row index: with a filter the
    /// visible rows are a subset of the versions.
    fn select_version(&mut self, version: &str, cx: &mut Context<Self>) {
        let found = self.state.borrow().versions.iter().find(|v| v.version == version).cloned();
        self.state.borrow_mut().selected_version = found;
        cx.notify();
    }

    /// Up and Down in the list move the selection.
    fn list_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let step: isize = match event.keystroke.key.as_str() {
            "up" => -1,
            "down" => 1,
            _ => return,
        };
        let visible = self.visible_versions(cx);
        if visible.is_empty() {
            return;
        }
        let current = self.state.borrow().selected_version.as_ref().and_then(|sel| visible.iter().position(|v| v.version == sel.version));
        let next = match current {
            Some(i) => (i as isize + step).clamp(0, visible.len() as isize - 1) as usize,
            None => 0,
        };
        self.list_scroll.scroll_to_item(next, ScrollStrategy::Center);
        self.select_version(&visible[next].version, cx);
        cx.stop_propagation();
    }

    // ── Download ─────────────────────────────────────────────────────────────

    fn download_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (ver, free) = {
            let s = self.state.borrow();
            (s.selected_version.clone(), s.sysinfo.free_disk_bytes)
        };
        let Some(ver) = ver else { return };
        let Some(guard) = BusyGuard::start(&self.state, Busy::Download) else {
            return;
        };

        if free.is_some_and(|f| f < MIN_DISK_BYTES) {
            toast(window, cx, "Low disk space — install may fail");
        }

        let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.state.borrow_mut().download_cancel = Some(cancel_flag.clone());
        self.progress = ProgressBar { visible: true, fraction: Some(0.0), text: format!("Downloading {}…", ver.version) };
        cx.notify();

        // XDG_DOWNLOAD_DIR normally lives only in ~/.config/user-dirs.dirs,
        // not the environment; dirs reads that file.
        let dest_dir = dirs::download_dir().unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join("Downloads"));

        // Progress, sent from the Tokio side and drained on the UI thread. The
        // loop ends when the sender drops at the end of the download.
        let (prog_tx, mut prog_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, Option<u64>)>();
        let download_start = std::time::Instant::now();
        cx.spawn_in(window, async move |this, cx| {
            let mut last_ui: Option<std::time::Instant> = None;
            while let Some(mut msg) = prog_rx.recv().await {
                // Coalesce bursts: jump to the newest queued update.
                while let Ok(newer) = prog_rx.try_recv() {
                    msg = newer;
                }
                // Throttle redraws to ~10 Hz.
                if last_ui.is_some_and(|t| t.elapsed() < Duration::from_millis(100)) {
                    continue;
                }
                last_ui = Some(std::time::Instant::now());
                let (fraction, text) = progress_text(msg, download_start.elapsed().as_secs_f64());
                let updated = this.update(cx, |this, cx| {
                    if this.state.borrow().busy == Some(Busy::Download) {
                        this.progress.fraction = fraction;
                        if let Some(text) = text {
                            this.progress.text = text;
                        }
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();

        let url = ver.url.clone();
        let filename = ver.filename.clone();
        let view = cx.weak_entity();
        spawn_async(
            window,
            cx,
            async move {
                // Fetch errors stay distinct from "no checksum published":
                // only the latter may lead to an unverified install.
                let checksum = fetch_checksum(&ver).await.map_err(|e| format!("{e:#}"));
                let tx = prog_tx;
                let result = download_run_file(
                    &url,
                    &dest_dir,
                    &filename,
                    move |dl, total| {
                        let _ = tx.send((dl, total));
                    },
                    cancel_flag,
                )
                .await;
                (ver, checksum, result)
            },
            move |(ver, checksum, result), window, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.state.borrow_mut().download_cancel = None;
                    match result {
                        Err(e) => {
                            if e.to_string().contains("cancelled") {
                                this.log("Download cancelled.", cx);
                                this.progress.text = "Cancelled".into();
                            } else {
                                this.log(format!("Download failed: {e:#}"), cx);
                                this.progress.text = "Download failed".into();
                                toast(window, cx, "Download failed — see Log tab");
                            }
                            this.progress.fraction = Some(0.0);
                            guard.release();
                            cx.notify();
                        }
                        Ok(path) => {
                            this.log(format!("Download complete: {}", path.display()), cx);
                            this.progress.fraction = Some(1.0);
                            this.finish_download(guard, ver, path, checksum, window, cx);
                        }
                    }
                });
            },
        );
    }

    /// Verify a finished download against NVIDIA's checksum. With none
    /// published (`Ok(None)`) the user decides whether to use the file
    /// unverified; when the checksum couldn't be fetched (`Err`), the choice
    /// is to retry or to discard the file — never to skip the check.
    fn finish_download(
        &mut self,
        guard: Rc<BusyGuard>,
        ver: DriverVersion,
        path: PathBuf,
        checksum: Result<Option<String>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.weak_entity();
        match checksum {
            Ok(Some(cs)) => {
                self.log("Verifying SHA256…", cx);
                self.progress.text = "Verifying…".into();
                let file = path.clone();
                let expected = cs.clone();
                spawn_async(window, cx, async move { verify_sha256(&file, &expected).await }, move |verified, window, cx| {
                    let _ = view.update(cx, |this, cx| match verified {
                        Ok(()) => {
                            this.log("SHA256 verified OK", cx);
                            this.accept_download(&guard, &path, Some(cs), "Verified", "Download complete", window, cx);
                        }
                        Err(e) => {
                            this.log(format!("SHA256 FAILED: {e}"), cx);
                            this.discard_download(&guard, &path, "Checksum mismatch — file deleted", "Checksum mismatch — do not install", window, cx);
                        }
                    });
                });
            }
            Ok(None) => {
                self.log("NVIDIA publishes no SHA256 checksum for this version", cx);
                self.progress.text = "Downloaded — not verified".into();
                show_choice(
                    Choice {
                        id: "unverified",
                        title: "Install without verification?".into(),
                        body: "NVIDIA does not publish a SHA256 checksum for this version, so the download \
                               cannot be checked against it. Only continue if you trust the network you \
                               downloaded it over."
                            .into(),
                        buttons: vec![("unverified-discard", "Discard Download", Look::Plain), ("unverified-use", "Use Unverified", Look::Danger)],
                        default: 0,
                        dismiss: 0,
                    },
                    move |answer, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            if answer == 1 {
                                this.log("User chose to continue WITHOUT checksum verification", cx);
                                this.accept_download(&guard, &path, None, "Complete (unverified)", "Download complete (unverified)", window, cx);
                            } else {
                                this.discard_download(&guard, &path, "Discarded (not verified)", "Unverified download discarded", window, cx);
                            }
                        });
                    },
                    window,
                    cx,
                );
            }
            Err(e) => {
                self.log(format!("Could not fetch the SHA256 checksum: {e}"), cx);
                self.progress.text = "Downloaded — checksum not fetched yet".into();
                show_choice(
                    Choice {
                        id: "checksum-failed",
                        title: "Couldn't get NVIDIA's checksum".into(),
                        body: format!(
                            "The driver downloaded, but its SHA256 checksum could not be fetched to \
                             check it:\n\n{e}\n\nTry again once the connection to NVIDIA works."
                        )
                        .into(),
                        buttons: vec![("checksum-discard", "Discard Download", Look::Plain), ("checksum-retry", "Try Again", Look::Primary)],
                        default: 1,
                        dismiss: 0,
                    },
                    move |answer, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            if answer != 1 {
                                this.discard_download(&guard, &path, "Discarded (not verified)", "Download discarded", window, cx);
                                return;
                            }
                            this.log("Fetching the SHA256 checksum again…", cx);
                            this.progress.text = "Fetching checksum…".into();
                            let v = ver.clone();
                            let view = cx.weak_entity();
                            spawn_async(window, cx, async move { fetch_checksum(&v).await.map_err(|e| format!("{e:#}")) }, move |checksum, window, cx| {
                                let _ = view.update(cx, |this, cx| this.finish_download(guard, ver, path, checksum, window, cx));
                            });
                        });
                    },
                    window,
                    cx,
                );
            }
        }
        cx.notify();
    }

    /// Make the file the one to install.
    #[allow(clippy::too_many_arguments)]
    fn accept_download(
        &mut self,
        guard: &BusyGuard,
        path: &Path,
        checksum: Option<String>,
        progress_text: &str,
        message: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.progress.text = progress_text.into();
        {
            let mut s = self.state.borrow_mut();
            s.downloaded_path = Some(path.to_string_lossy().to_string());
            s.expected_checksum = checksum;
        }
        guard.release();
        self.page = Page::Configure;
        toast(window, cx, message);
        cx.notify();
    }

    fn discard_download(&mut self, guard: &BusyGuard, path: &Path, progress_text: &str, message: &str, window: &mut Window, cx: &mut Context<Self>) {
        match std::fs::remove_file(path) {
            Ok(()) => {
                self.log(format!("Deleted {}", path.display()), cx);
                self.progress.text = progress_text.into();
            }
            Err(e) => {
                self.log(format!("Could not delete {}: {}", path.display(), e), cx);
                self.progress.text = "Could not delete the download — remove it by hand".into();
            }
        }
        guard.release();
        toast(window, cx, message);
        cx.notify();
    }

    fn cancel_download(&mut self) {
        if let Some(flag) = &self.state.borrow().download_cancel {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    // ── Open a local file ────────────────────────────────────────────────────

    fn open_file_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !actions_enabled(&self.state.borrow()).open_file {
            return;
        }
        // The desktop's file dialog, through the portal. GPUI can't give it a
        // file-type filter, so the name is checked after the pick.
        let picked = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Open".into()) });
        let view = cx.weak_entity();
        cx.spawn_in(window, async move |_, cx| {
            let result = picked.await;
            let _ = cx.update(|window, cx| match result {
                Ok(Ok(Some(paths))) => {
                    if let Some(path) = paths.into_iter().next() {
                        let _ = view.update(cx, |this, cx| this.file_picked(path, window, cx));
                    }
                }
                Ok(Ok(None)) | Err(_) => {}
                Ok(Err(e)) => toast(window, cx, &format!("Could not open the file dialog: {e:#}")),
            });
        })
        .detach();
    }

    fn file_picked(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if !path.to_string_lossy().ends_with(".run") {
            show_error(NOT_A_RUN_FILE, &format!("{} is not an NVIDIA driver .run file.", path.display()), window, cx);
            return;
        }
        // The install script refuses symlinks; use the file itself. The
        // checks run commands (getent, getfacl): not on this thread.
        let view = cx.weak_entity();
        spawn_async(
            window,
            cx,
            async move {
                tokio::task::spawn_blocking(move || {
                    let path = std::fs::canonicalize(&path).unwrap_or(path);
                    let path_str = path.to_string_lossy().to_string();
                    let checked = check_run_file(&path_str);
                    (path_str, checked)
                })
                .await
            },
            move |checked, window, cx| {
                let Ok((path_str, checked)) = checked else { return };
                let _ = view.update(cx, |this, cx| {
                    if let Err(e) = checked {
                        this.log(format!("Can't use {path_str}: {e}"), cx);
                        show_error("This file can't be installed", &e.to_string(), window, cx);
                        return;
                    }
                    this.log(format!("Opened local file: {path_str}"), cx);
                    {
                        let mut s = this.state.borrow_mut();
                        s.downloaded_path = Some(path_str);
                        s.expected_checksum = None;
                    }
                    this.page = Page::Configure;
                    cx.notify();
                });
            },
        );
    }

    // ── Install ──────────────────────────────────────────────────────────────

    fn install_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let s = self.state.borrow();
        if !actions_enabled(&s).install {
            return;
        }
        let Some(run_file) = s.downloaded_path.clone() else { return };

        if s.sysinfo.free_disk_bytes.is_some_and(|free| free < MIN_DISK_BYTES) {
            toast(window, cx, "Warning: low disk space — install may fail");
        }

        // Refuse module flavors that cannot work, before asking for a password.
        let driver_major = Path::new(&run_file)
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(version_from_filename)
            .and_then(|v| v.split('.').next().and_then(|m| m.parse::<u32>().ok()));
        if let Err(msg) = validate_module_choice(s.module_choice, &s.sysinfo.gpu_compute_caps, driver_major) {
            drop(s);
            self.log(format!("Install not started: {msg}"), cx);
            toast(window, cx, &msg);
            return;
        }

        let opts = InstallOptions {
            use_dkms: s.use_dkms,
            block_distro_driver: s.block_distro_driver,
            module: s.module_choice,
            run_file,
            sha256: s.expected_checksum.clone(),
            discard_recovery: s.discard_recovery_next,
        };
        drop(s);
        self.state.borrow_mut().discard_recovery_next = false;

        let Some(guard) = BusyGuard::start(&self.state, Busy::Install) else {
            return;
        };
        self.progress = ProgressBar { visible: true, fraction: None, text: "Installing… the desktop stays up; this takes a few minutes".into() };
        self.page = Page::Log;
        self.log(format!("Starting install: {}", opts.run_file), cx);
        self.log("The new driver installs alongside the running one; reboot afterward to switch over.", cx);

        // The script's output goes into the Log tab line by line as it runs;
        // the result is shown only after the last line (the sender is dropped
        // when the install call returns, before the result is sent).
        let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        crate::runtime().spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                run_privileged_install(&opts, |line| {
                    let _ = line_tx.send(line.to_string());
                })
            })
            .await;
            let _ = result_tx.send(result);
        });

        cx.spawn_in(window, async move |this, cx| {
            while let Some(line) = line_rx.recv().await {
                if this.update(cx, |this, cx| this.log(line, cx)).is_err() {
                    return;
                }
            }
            let result: Result<anyhow::Result<()>, String> = match result_rx.await {
                Ok(Ok(r)) => Ok(r),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("the install task ended unexpectedly".to_string()),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                guard.release();
                this.progress.visible = false;
                match result {
                    Ok(Ok(())) => {
                        this.log("Install completed successfully.", cx);
                        this.log("Reboot to switch to the new driver.", cx);
                        toast(window, cx, "Install complete — reboot to activate");
                    }
                    Ok(Err(e)) if e.downcast_ref::<StaleRecovery>().is_some() => {
                        this.log(format!("Install not started: {e}"), cx);
                        // Only on the user's word: install anyway, dropping that data.
                        let view = cx.weak_entity();
                        show_choice(
                            Choice {
                                id: "stale-recovery",
                                title: "Recovery data from an interrupted install".into(),
                                body: e.to_string().into(),
                                buttons: vec![("stale-cancel", "Cancel", Look::Plain), ("stale-discard", "Discard and Install", Look::Danger)],
                                default: 0,
                                dismiss: 0,
                            },
                            move |answer, window, cx| {
                                if answer == 1 {
                                    let _ = view.update(cx, |this, cx| {
                                        this.state.borrow_mut().discard_recovery_next = true;
                                        this.install_clicked(window, cx);
                                    });
                                }
                            },
                            window,
                            cx,
                        );
                    }
                    Ok(Err(e)) => {
                        this.log(format!("Install failed: {e:#}"), cx);
                        show_error("Install failed", &format!("{e:#}"), window, cx);
                    }
                    Err(e) => this.log(format!("Task error: {e}"), cx),
                }
                // A failed install may have changed (or put back) packages too.
                this.load_sysinfo(window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    // ── Module signing ───────────────────────────────────────────────────────

    fn signing_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !actions_enabled(&self.state.borrow()).setup_signing {
            return;
        }
        let prompt = cx.new(|cx| SigningPrompt::new(window, cx));
        let view = cx.weak_entity();
        let first = Cell::new(true);
        let setup_focus = cx.focus_handle();
        let cancel_focus = cx.focus_handle();
        let done = Rc::new(Cell::new(false));
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) {
                prompt.read(cx).first.focus_handle(cx).focus(window, cx);
            }
            let valid = prompt.read(cx).valid;
            // Takes the password out of the fields (they are cleared), then
            // starts the setup once the dialog is out of the way.
            let set_up = {
                let prompt = prompt.clone();
                let view = view.clone();
                let done = done.clone();
                move |window: &mut Window, cx: &mut App| {
                    if done.replace(true) {
                        return;
                    }
                    let password = prompt.update(cx, |prompt, cx| prompt.take_password(window, cx));
                    if validate_mok_password(&password, &password).is_err() {
                        return;
                    }
                    let view = view.clone();
                    window.defer(cx, move |window, cx| {
                        let _ = view.update(cx, |this, cx| this.run_signing_setup(password, window, cx));
                    });
                }
            };
            let enter = set_up.clone();
            let (on_setup, on_cancel) = (setup_focus.clone(), cancel_focus.clone());
            dialog
                .title(dialog_title(SIGNING_TITLE))
                .overlay(false)
                .w(px(460.))
                // Enter on Set Up Signing (once the passwords are fine) sets
                // up, on Cancel cancels; in a field it does nothing.
                .on_ok(move |_, window, cx| match enter_key(&[on_cancel.is_focused(window), on_setup.is_focused(window)]) {
                    Some(1) if valid => {
                        enter(window, cx);
                        true
                    }
                    Some(0) => true,
                    _ => false,
                })
                .child(
                    named_group("signing", SIGNING_TITLE).child(div().id("body").max_h(dialog_room(window)).overflow_y_scroll().child(prompt.clone())).child(
                        dialog_buttons()
                            .child(Button::new("signing-cancel").label("Cancel").track_focus(&cancel_focus).on_click(|_, window, cx| window.close_dialog(cx)))
                            .child(Button::new("signing-setup").primary().label("Set Up Signing").disabled(!valid).track_focus(&setup_focus).on_click(
                                move |_, window, cx| {
                                    window.close_dialog(cx);
                                    set_up(window, cx);
                                },
                            )),
                    ),
                )
        });
    }

    fn run_signing_setup(&mut self, password: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(guard) = BusyGuard::start(&self.state, Busy::Signing) else {
            return;
        };
        self.page = Page::Log;
        self.log("Setting up module signing…", cx);
        let view = cx.weak_entity();
        spawn_async(
            window,
            cx,
            async move { tokio::task::spawn_blocking(move || run_privileged_setup_signing(&password)).await },
            move |outcome, window, cx| {
                guard.release();
                let _ = view.update(cx, |this, cx| {
                    match outcome {
                        Ok(outcome) => {
                            for line in &outcome.lines {
                                this.log(line.clone(), cx);
                            }
                            match outcome.result {
                                Ok(()) => {
                                    this.log("Module signing setup finished.", cx);
                                    toast(window, cx, "Module signing set up — see Log tab");
                                }
                                Err(e) => {
                                    this.log(format!("Module signing setup failed: {e}"), cx);
                                    toast(window, cx, "Signing setup failed — see Log tab");
                                }
                            }
                        }
                        Err(e) => this.log(format!("Task error: {e}"), cx),
                    }
                    this.load_sysinfo(window, cx);
                });
            },
        );
        cx.notify();
    }

    // ── Recovery from an interrupted install ─────────────────────────────────

    fn status_checked(&mut self, pending: bool, note: Option<RecoveryNote>, running: bool, window: &mut Window, cx: &mut Context<Self>) {
        run_deferred_releases();
        // Redrawn on every check: an operation whose task ended without its
        // callback (see BusyGuard) shows as ended within two seconds.
        cx.notify();
        if self.state.borrow().busy.is_some() {
            return;
        }
        self.recover = pending.then(|| match note {
            Some(RecoveryNote::Refused(why)) => RecoverBanner {
                text: format!(
                    "An earlier driver install was interrupted, but its recovery data no \
                     longer matches this computer: {why}. Nothing was restored."
                ),
                button: "Retry or Discard…",
                discards: true,
            },
            Some(RecoveryNote::Failed(why)) => {
                RecoverBanner { text: format!("Restoring the driver from before an interrupted install failed: {why}."), button: "Try Again", discards: false }
            }
            None => RecoverBanner { text: RECOVER_TEXT.to_string(), button: "Restore Previous Driver", discards: false },
        });
        if running != self.state.borrow().external_install {
            self.state.borrow_mut().external_install = running;
            if !running {
                self.load_sysinfo(window, cx);
            }
        }
    }

    fn recover_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.recover.as_ref().is_some_and(|r| r.discards) {
            self.run_recovery(false, window, cx);
            return;
        }
        // Data that no longer matched the computer when last checked: the
        // user picks between trying again (it is checked again, and restored
        // if the reason has gone) and discarding it.
        let view = cx.weak_entity();
        show_choice(
            Choice {
                id: "recovery-question",
                title: "Recovery data from an interrupted install".into(),
                body: "When it was last checked, this data no longer matched the computer. Retry \
                       Recovery checks again and restores the previous driver if it matches now. \
                       Discard Recovery Data drops it for good, keeping the driver as it is."
                    .into(),
                buttons: vec![
                    ("recovery-cancel", "Cancel", Look::Plain),
                    ("recovery-discard", "Discard Recovery Data", Look::Danger),
                    ("recovery-retry", "Retry Recovery", Look::Primary),
                ],
                default: 2,
                dismiss: 0,
            },
            move |answer, window, cx| {
                let _ = view.update(cx, |this, cx| match answer {
                    2 => this.run_recovery(false, window, cx),
                    1 => this.run_recovery(true, window, cx),
                    _ => {}
                });
            },
            window,
            cx,
        );
    }

    /// Restore (false) or discard (true) the recovery data.
    fn run_recovery(&mut self, discard: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(guard) = BusyGuard::start(&self.state, Busy::Install) else {
            // The banner stays up while something else runs: say why nothing happened.
            toast(window, cx, "Wait until the running operation has finished");
            return;
        };
        self.recover = None;
        self.page = Page::Log;
        if discard {
            self.progress = ProgressBar { visible: true, fraction: None, text: "Discarding the recovery data…".into() };
            self.log("Discarding the recovery data of the interrupted install, as chosen…", cx);
        } else {
            self.progress = ProgressBar { visible: true, fraction: None, text: "Restoring the previous driver…".into() };
            self.log("Restoring the driver setup from before the interrupted install…", cx);
        }
        let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        crate::runtime().spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                let send = |line: &str| {
                    let _ = line_tx.send(line.to_string());
                };
                if discard {
                    run_privileged_discard_recovery(send).map(|()| Recovered::Discarded)
                } else {
                    run_privileged_recover(send)
                }
            })
            .await;
            let _ = result_tx.send(result);
        });
        cx.spawn_in(window, async move |this, cx| {
            while let Some(line) = line_rx.recv().await {
                if this.update(cx, |this, cx| this.log(line, cx)).is_err() {
                    return;
                }
            }
            let result = match result_rx.await {
                Ok(Ok(r)) => r.map_err(|e| format!("{e:#}")),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("the restore task ended unexpectedly".to_string()),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                guard.release();
                this.progress.visible = false;
                match result {
                    Ok(Recovered::Restored) => {
                        this.log("The previous driver setup is back and checked.", cx);
                        toast(window, cx, "Previous driver restored");
                    }
                    Ok(Recovered::Nothing) => {
                        this.log("Nothing needed putting back: that install had finished.", cx);
                        toast(window, cx, "Nothing needed restoring");
                    }
                    Ok(Recovered::Discarded) => {
                        this.log("The recovery data was discarded; the driver is left as it is.", cx);
                        toast(window, cx, "Recovery data discarded");
                    }
                    Err(e) if discard => {
                        this.log(format!("Discarding failed: {e}"), cx);
                        show_error("Discarding the recovery data failed", &e, window, cx);
                    }
                    Err(e) => {
                        this.log(format!("Restoring failed: {e}"), cx);
                        show_error("Restoring the previous driver failed", &e, window, cx);
                    }
                }
                this.load_sysinfo(window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    // ── Rendering ────────────────────────────────────────────────────────────

    fn render_system(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = self.state.borrow();
        let loaded = s.sysinfo_loaded;
        let info = &s.sysinfo;
        let refreshing = self.sysinfo_refreshing;
        let value = |checking: &str, text: String| if loaded { text } else { checking.to_string() };

        let gpu = if refreshing { "Detecting…".to_string() } else { value("Detecting…", info.gpu_name.clone().unwrap_or_else(|| "Not detected".into())) };
        let driver = if refreshing {
            "Detecting…".to_string()
        } else {
            value("Detecting…", info.installed_driver.clone().unwrap_or_else(|| "Not installed".into()))
        };
        let gens = if info.gpu_compute_caps.is_empty() {
            String::new()
        } else {
            let mut g: Vec<&str> = info.gpu_compute_caps.iter().map(|c| crate::preflight::generation_name(*c)).collect();
            g.dedup();
            format!(" ({})", g.join(", "))
        };
        let module = value("Detecting…", format!("{}{}", info.module_flavor, gens));
        let kernel = value("Detecting…", info.kernel_version.clone());
        let dkms = value(
            "Checking…",
            if info.dkms_status.is_empty() {
                "No NVIDIA DKMS modules registered".to_string()
            } else {
                info.dkms_status.iter().map(|e| format!("{}/{} [{}] {}", e.module, e.version, e.kernel, e.status)).collect::<Vec<_>>().join("\n")
            },
        );
        let secure_boot = value("Checking…", secure_boot_summary(&info.secure_boot, &info.signing_key));
        let mut signing_text = signing_summary(&info.signing_key, &info.module_signature);
        if info.signing_pair.as_ref().is_some_and(|p| p.key_hidden) {
            signing_text.push_str("\nIts private key is readable only by root, so it is checked during the install.");
        }
        let signing = value("Checking…", signing_text);
        let disk =
            value("Checking…", info.free_disk_bytes.map(|free| format!("{} free on /", format_bytes(free))).unwrap_or_else(|| "Could not determine".into()));
        let reboot = value("Checking…", if info.reboot_required { "Yes — new driver installed, reboot to activate".into() } else { "No".into() });
        let actions = actions_enabled(&s);
        // Nothing to set up once the key is enrolled or queued: no button
        // rather than an action that looks needed.
        let show_signing = !loaded || info.signing_key.setup_needed();
        drop(s);

        let signing_button = show_signing.then(|| {
            Button::new("setup-signing")
                .label("Set Up Signing")
                .disabled(!actions.setup_signing)
                .tooltip("Create or reuse this computer's module-signing key and enroll it")
                .on_click(cx.listener(|this, _, window, cx| this.signing_clicked(window, cx)))
        });

        page_scroll("system-page")
            .child(group(
                "hardware",
                "Hardware",
                vec![
                    row("gpu", "GPU", gpu, None, cx),
                    row("driver", "Installed Driver", driver, None, cx),
                    row("module", "Kernel Module", module, None, cx),
                    row("kernel", "Kernel", kernel, None, cx),
                ],
                cx,
            ))
            .child(group(
                "status",
                "Status",
                vec![
                    row("dkms", "DKMS Modules", dkms, None, cx),
                    row("secure-boot", "Secure Boot", secure_boot, None, cx),
                    row("signing", "Module Signing", signing, signing_button.map(|b| b.into_any_element()), cx),
                    row("disk", "Free Disk Space", disk, None, cx),
                    row("reboot", "Reboot Required", reboot, None, cx),
                ],
                cx,
            ))
            .child(h_flex().justify_center().child(
                Button::new("refresh-sysinfo").ghost().label("Refresh System Info").on_click(cx.listener(|this, _, window, cx| this.load_sysinfo(window, cx))),
            ))
            .into_any_element()
    }

    fn render_browse(&self, cx: &mut Context<Self>) -> AnyElement {
        let visible = self.visible_versions(cx);
        let s = self.state.borrow();
        let installed = s.sysinfo.installed_driver.clone();
        let selected = s.selected_version.as_ref().map(|v| v.version.clone());
        let actions = actions_enabled(&s);
        drop(s);

        let status_text = match (&self.list_status, &selected) {
            (ListStatus::Loading, _) => "Loading…".to_string(),
            (ListStatus::Failed(_), _) => "Could not load version list".to_string(),
            (ListStatus::Loaded, Some(v)) => format!("Selected: {v}"),
            (ListStatus::Loaded, None) => "No version selected".to_string(),
        };

        let theme = cx.theme().clone();
        let count = visible.len();
        let rows = Rc::new(visible);
        let list = uniform_list("versions", count, {
            let rows = rows.clone();
            let installed = installed.clone();
            let selected = selected.clone();
            cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                range
                    .map(|ix| {
                        let ver = &rows[ix];
                        version_row(ver, ix, installed.as_deref(), selected.as_deref() == Some(ver.version.as_str()), cx)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .track_scroll(&self.list_scroll)
        .size_full();

        v_flex()
            .size_full()
            .when_some(
                match &self.list_status {
                    ListStatus::Failed(e) => Some(e.clone()),
                    _ => None,
                },
                |page, e| page.child(banner("list-error", e, None, cx)),
            )
            .child(div().px_3().pt_3().child(Input::new(&self.filter).cleanable(true).aria_label("Filter versions")))
            .child(
                div()
                    .id("version-list")
                    .role(Role::List)
                    .aria_label("Driver versions")
                    .flex_1()
                    .min_h_0()
                    .m_3()
                    .border_1()
                    .border_color(theme.border)
                    .rounded(theme.radius)
                    .bg(theme.secondary)
                    .focusable()
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| this.list_key(event, cx)))
                    .map(|list_box| match self.list_status {
                        ListStatus::Loading => list_box.flex().items_center().justify_center().child(Progress::new("list-loading").loading(true).w(px(160.))),
                        _ => list_box.child(list),
                    }),
            )
            .child(h_flex().justify_center().text_sm().text_color(theme.muted_foreground).child(label("selected", status_text)))
            .child(
                h_flex()
                    .justify_between()
                    .p_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("open-file")
                            .ghost()
                            .label("Open .run File…")
                            .tooltip("Use a locally downloaded .run file")
                            .disabled(!actions.open_file)
                            .on_click(cx.listener(|this, _, window, cx| this.open_file_clicked(window, cx))),
                    )
                    .child(
                        Button::new("download")
                            .primary()
                            .label("Download")
                            .disabled(!actions.download)
                            .on_click(cx.listener(|this, _, window, cx| this.download_clicked(window, cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_configure(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = self.state.borrow();
        let actions = actions_enabled(&s);

        // The file to install.
        let (file_title, file_sub, checksum, version_status) = match &s.downloaded_path {
            Some(path) => {
                let fname = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path).to_string();
                let checksum = match s.expected_checksum.as_deref().filter(|c| is_sha256_hex(c)) {
                    Some(cs) => format!("SHA256: {}… (verified)", &cs[..16]),
                    None => "No checksum available".to_string(),
                };
                let parsed = version_from_filename(&fname);
                let status = if parsed.is_none() {
                    // e.g. NVIDIA-Linux-x86_64-595.84-vulkan.run — don't guess.
                    "Unknown".to_string()
                } else if let (Some(inst), Some(candidate)) = (s.sysinfo.installed_driver.as_ref(), parsed.as_deref()) {
                    match compare_versions(inst, candidate) {
                        VersionRelation::Newer => format!("Upgrade: {inst} → {candidate}"),
                        VersionRelation::Same => format!("Reinstall: same version ({inst})"),
                        VersionRelation::Older => format!("Downgrade: {inst} → {candidate}"),
                        VersionRelation::Unknown => "—".to_string(),
                    }
                } else {
                    "No driver currently installed".to_string()
                };
                (fname, path.clone(), checksum, status)
            }
            None => ("No file selected".to_string(), "Go to Browse tab to select or download a driver".to_string(), "—".to_string(), "—".to_string()),
        };

        let disk = match s.sysinfo.free_disk_bytes {
            Some(free) if free < MIN_DISK_BYTES => format!("Low: {} free (2 GB recommended)", format_bytes(free)),
            Some(free) => format!("{} free", format_bytes(free)),
            None => "Could not determine".to_string(),
        };
        let secure_boot = match &s.sysinfo.secure_boot {
            SecureBootStatus::Enabled => secure_boot_advice(&s.sysinfo.secure_boot, &s.sysinfo.signing_key, s.sysinfo.signing_pair.as_ref(), s.use_dkms)
                .unwrap_or_else(|| "Enabled".to_string()),
            SecureBootStatus::Disabled => "Disabled".to_string(),
            SecureBootStatus::Unknown => "Unknown".to_string(),
        };
        let packages = conflict_summary(&s.sysinfo.distro_packages).unwrap_or_else(|| "None found".to_string());
        let hint = module_hint(&s.sysinfo.gpu_compute_caps);
        let use_dkms = s.use_dkms;
        let block = s.block_distro_driver;
        let block_supported = !s.sysinfo_loaded || s.sysinfo.block_supported;
        let module_index = match s.module_choice {
            ModuleChoice::Auto => 0,
            ModuleChoice::Open => 1,
            ModuleChoice::Proprietary => 2,
        };
        let downloading = s.busy == Some(Busy::Download) && s.download_cancel.is_some();
        drop(s);

        let dkms_switch =
            Switch::new("dkms-switch").checked(use_dkms).accessibility_label("Enable DKMS").on_click(cx.listener(|this, checked: &bool, _, cx| {
                // The Secure Boot advice depends on it.
                this.state.borrow_mut().use_dkms = *checked;
                cx.notify();
            }));
        let block_switch = Switch::new("block-switch").checked(block).disabled(!block_supported).accessibility_label("Block Distro Driver Packages").on_click(
            cx.listener(|this, checked: &bool, _, cx| {
                this.state.borrow_mut().block_distro_driver = *checked;
                cx.notify();
            }),
        );
        let modules = RadioGroup::horizontal("module-choice")
            .selected_index(Some(module_index))
            .children([
                Radio::new("module-auto").label("Automatic (recommended)"),
                Radio::new("module-open").label("Open"),
                Radio::new("module-proprietary").label("Proprietary"),
            ])
            .on_click(cx.listener(|this, index: &usize, _, cx| {
                this.state.borrow_mut().module_choice = match index {
                    1 => ModuleChoice::Open,
                    2 => ModuleChoice::Proprietary,
                    _ => ModuleChoice::Auto,
                };
                cx.notify();
            }));

        let progress = &self.progress;
        page_scroll("configure-page")
            .child(group(
                "selected-driver",
                "Selected Driver",
                vec![
                    row("file", file_title, file_sub, None, cx),
                    row("checksum", "Checksum", checksum, None, cx),
                    row("version-status", "Version Status", version_status, None, cx),
                ],
                cx,
            ))
            .child(group(
                "checks",
                "Pre-install Checks",
                vec![
                    row("disk-check", "Disk Space", disk, None, cx),
                    row("secure-boot-check", "Secure Boot", secure_boot, None, cx),
                    row("packages-check", "Existing Driver Packages", packages, None, cx),
                ],
                cx,
            ))
            .child(group(
                "options",
                "Install Options",
                vec![
                    row("dkms-option", "Enable DKMS", "Automatically rebuild kernel module on kernel updates", Some(dkms_switch.into_any_element()), cx),
                    v_flex().gap_2().child(row("module-option", "Kernel Module", hint, None, cx)).child(div().px_3().pb_3().child(modules)).into_any_element(),
                    row(
                        "block-option",
                        "Block Distro Driver Packages",
                        if block_supported { BLOCK_SUBTITLE } else { "Not available here: this dnf has no way to exclude packages for good" },
                        Some(block_switch.into_any_element()),
                        cx,
                    ),
                ],
                cx,
            ))
            .when(progress.visible, |page| {
                page.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Progress::new("progress")
                                .accessibility_label(progress.text.clone())
                                .loading(progress.fraction.is_none())
                                .value(progress.fraction.unwrap_or(0.0) * 100.0),
                        )
                        .child(h_flex().justify_center().text_sm().child(label("progress-text", progress.text.clone()))),
                )
            })
            .child(
                h_flex()
                    .justify_center()
                    .gap_2()
                    .pt_2()
                    .child(
                        Button::new("install")
                            .primary()
                            .label("Install Driver")
                            .disabled(!actions.install)
                            .on_click(cx.listener(|this, _, window, cx| this.install_clicked(window, cx))),
                    )
                    .when(downloading, |buttons| {
                        buttons.child(
                            Button::new("cancel-download").danger().label("Cancel Download").on_click(cx.listener(|this, _, _, _| this.cancel_download())),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_log(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let text: String = self.log.iter().map(|l| format!("{l}\n")).collect();
        v_flex()
            .size_full()
            .child(
                div()
                    .id("log")
                    .role(Role::Log)
                    .aria_label("Log")
                    .flex_1()
                    .min_h_0()
                    .m_2()
                    .p_2()
                    .border_1()
                    .border_color(theme.border)
                    .rounded(theme.radius)
                    .bg(theme.secondary)
                    .font_family(theme.mono_font_family.clone())
                    .text_sm()
                    .overflow_y_scroll()
                    .track_scroll(&self.log_scroll)
                    .children(self.log.iter().enumerate().map(|(i, line)| div().w_full().child(label(("log-line", i), line.clone())))),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .px_2()
                    .pb_2()
                    .child(
                        Button::new("copy-log")
                            .ghost()
                            .label("Copy Log")
                            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))),
                    )
                    .child(Button::new("clear-log").ghost().label("Clear Log").on_click(cx.listener(|this, _, _, cx| {
                        this.log.clear();
                        cx.notify();
                    }))),
            )
            .into_any_element()
    }
}

impl Render for MainView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let page_index = PAGES.iter().position(|(p, _)| *p == self.page).unwrap_or(0);
        let (reboot_required, external_install) = {
            let s = self.state.borrow();
            (s.sysinfo.reboot_required, s.external_install)
        };
        let state = self.state.clone();
        let allow_close = self.allow_close.clone();

        let title_bar = TitleBar::new()
            .on_close_window(move |_, window, cx| {
                if close_allowed(&state, &allow_close, window, cx) {
                    window.remove_window();
                }
            })
            .child(
                h_flex()
                    .gap_2()
                    // Clicks on the buttons must not start a window drag.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new("refresh")
                            .ghost()
                            .small()
                            .icon(Icon::new(gpui_kit::assets::IconName::RefreshCw))
                            .tooltip("Refresh version list")
                            .accessibility_label("Refresh version list")
                            .on_click(cx.listener(|this, _, window, cx| this.load_versions(window, cx))),
                    ),
            )
            .child(div().on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()).child(
                TabBar::new("pages").segmented().small().selected_index(page_index).children(PAGES.iter().map(|(_, name)| Tab::new().label(*name))).on_click(
                    cx.listener(|this, index: &usize, _, cx| {
                        this.page = PAGES[*index].0;
                        cx.notify();
                    }),
                ),
            ))
            .child(
                h_flex().px_2().on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()).child(
                    Button::new("about")
                        .ghost()
                        .small()
                        .icon(Icon::new(gpui_kit::component::IconName::Info))
                        .tooltip("About")
                        .accessibility_label("About")
                        .on_click(cx.listener(|this, _, window, cx| this.show_about(window, cx))),
                ),
            );

        let content = match self.page {
            Page::System => self.render_system(cx),
            Page::Browse => self.render_browse(cx),
            Page::Configure => self.render_configure(cx),
            Page::Log => self.render_log(cx),
        };

        let recover_banner = self.recover.as_ref().map(|r| {
            let button = Button::new("recover").small().label(r.button).on_click(cx.listener(|this, _, window, cx| this.recover_clicked(window, cx)));
            banner("recover-banner", r.text.clone(), Some(button.into_any_element()), cx)
        });

        v_flex()
            .id("main")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(title_bar)
            .when_some(self.setup_problem.clone(), |main, text| main.child(banner("setup-banner", text, None, cx)))
            .when(reboot_required, |main| main.child(banner("reboot-banner", "A reboot is required to activate the new driver.", None, cx)))
            .when(external_install, |main| {
                main.child(banner(
                    "running-banner",
                    "A driver install started earlier is still running. Don't reboot until it finishes; \
                     its progress is in /var/log/green-light.log.",
                    None,
                    cx,
                ))
            })
            .children(recover_banner)
            .child(div().flex_1().min_h_0().child(content))
            .child(Announcer(ANNOUNCEMENT.with(|text| text.borrow().clone())))
    }
}

impl MainView {
    fn show_about(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (driver, gpu, kernel) = {
            let s = self.state.borrow();
            (
                s.sysinfo.installed_driver.clone().unwrap_or_else(|| "Not detected".to_string()),
                s.sysinfo.gpu_name.clone().unwrap_or_else(|| "Not detected".to_string()),
                s.sysinfo.kernel_version.clone(),
            )
        };
        let icon = Arc::new(Image::from_bytes(ImageFormat::Png, include_bytes!("../data/green-light-128.png").to_vec()));
        let close_focus = cx.focus_handle();
        let first = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) {
                close_focus.focus(window, cx);
            }
            let muted = cx.theme().muted_foreground;
            dialog.title(dialog_title(ABOUT_TITLE)).overlay(false).w(px(400.)).child(
                named_group("about", ABOUT_TITLE)
                    .child(
                        v_flex()
                            .id("body")
                            .max_h(dialog_room(window))
                            .overflow_y_scroll()
                            .gap_1()
                            .items_center()
                            // Fixed size: in the scrolling column it would otherwise shrink
                            // when the dialog is short (to 16 px on a small screen).
                            .child(img(icon.clone()).size(px(64.)).flex_none())
                            .child(div().text_xl().font_weight(FontWeight::BOLD).child(label("name", "Green Light")))
                            .child(div().text_color(muted).child(label("version", env!("CARGO_PKG_VERSION"))))
                            .child(div().text_center().child(label("summary", "A GUI for installing NVIDIA drivers from official .run files.")))
                            .child(
                                v_flex()
                                    .items_center()
                                    .text_sm()
                                    .child(label("gpu", format!("GPU: {gpu}")))
                                    .child(label("driver", format!("Driver: {driver}")))
                                    .child(label("kernel", format!("Kernel: {kernel}"))),
                            )
                            .child(
                                h_flex()
                                    .gap_4()
                                    .child(Link::new("about-website").href(WEBSITE).child("Website"))
                                    .child(Link::new("about-issues").href(ISSUES).child("Report an Issue")),
                            )
                            .child(
                                v_flex()
                                    .gap_1()
                                    .items_center()
                                    .text_sm()
                                    .text_color(muted)
                                    .text_center()
                                    .child(label("developer", "Developer: Linnard Alex Brown Jr."))
                                    .child(label("built-with", "Built with: Claude Code (Anthropic), Codex (OpenAI)"))
                                    .child(label("license", "License: AGPL-3.0-or-later")),
                            ),
                    )
                    .child(
                        dialog_buttons()
                            .child(Button::new("about-close").track_focus(&close_focus).label("Close").on_click(|_, window, cx| window.close_dialog(cx))),
                    ),
            )
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Building blocks
// ─────────────────────────────────────────────────────────────────────────────

/// A page's scrollable column.
fn page_scroll(id: &'static str) -> Stateful<Div> {
    v_flex().id(id).size_full().overflow_y_scroll().p_3().gap_4()
}

/// A titled box of rows.
fn group(id: &'static str, title: &'static str, rows: Vec<AnyElement>, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let count = rows.len();
    v_flex()
        .id(id)
        .role(Role::Group)
        .aria_label(title)
        .gap_2()
        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(label((id, 0usize), title)))
        .child(
            v_flex()
                .w_full()
                .border_1()
                .border_color(theme.border)
                .rounded(theme.radius)
                .bg(theme.secondary)
                .children(rows.into_iter().enumerate().map(|(i, r)| div().when(i > 0 && i < count, |d| d.border_t_1().border_color(theme.border)).child(r))),
        )
}

/// A row: a title, a line (or more) of text under it, and an optional
/// control on the right.
fn row(id: &'static str, title: impl Into<SharedString>, subtitle: impl Into<SharedString>, suffix: Option<AnyElement>, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    h_flex()
        .id(id)
        .w_full()
        .gap_3()
        .px_3()
        .py_2()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(label((id, 0usize), title.into()))
                .child(div().text_sm().text_color(muted).child(label((id, 1usize), subtitle.into()))),
        )
        .children(suffix)
        .into_any_element()
}

/// A notice across the top of the window, with an optional button.
fn banner(id: &'static str, text: impl Into<SharedString>, button: Option<AnyElement>, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .id(id)
        .role(Role::Status)
        .w_full()
        .gap_3()
        .px_3()
        .py_2()
        .bg(theme.accent)
        .text_color(theme.accent_foreground)
        .border_b_1()
        .border_color(theme.border)
        .child(div().flex_1().min_w_0().text_sm().child(label((id, 0usize), text.into())))
        .children(button)
}

/// One version in the Browse list.
fn version_row(ver: &DriverVersion, index: usize, installed: Option<&str>, selected: bool, cx: &mut Context<MainView>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let subtitle = match installed {
        Some(inst) => match compare_versions(inst, &ver.version) {
            VersionRelation::Newer => format!("{} · Upgrade available", ver.filename),
            VersionRelation::Same => format!("{} · Currently installed", ver.filename),
            VersionRelation::Older => format!("{} · Older than installed", ver.filename),
            VersionRelation::Unknown => ver.filename.clone(),
        },
        None => ver.filename.clone(),
    };
    let is_installed = installed == Some(ver.version.as_str());
    let key = ver.version.clone();
    let id = |part: &str| ElementId::Name(format!("{part}:{key}").into());
    let version = ver.version.clone();
    h_flex()
        .id(id("row"))
        .role(Role::ListItem)
        .aria_label(format!("{}, {}", ver.version, subtitle))
        .aria_selected(selected)
        .w_full()
        .h(px(VERSION_ROW_HEIGHT))
        .gap_3()
        .px_3()
        .when(index > 0, |r| r.border_t_1().border_color(theme.border))
        .when(selected, |r| r.bg(theme.list_active).border_color(theme.list_active_border))
        .when(!selected, |r| r.hover(|r| r.bg(theme.list_hover)))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| this.select_version(&version, cx)))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    div().when(is_installed, |d| d.text_color(theme.success)).font_weight(FontWeight::MEDIUM).child(label(id("version"), ver.version.clone())),
                )
                .child(div().text_sm().text_color(theme.muted_foreground).truncate().child(label(id("file"), subtitle))),
        )
        .when_some(ver.branch, |r, branch| r.child(branch_tag(branch, theme.is_dark()).child(label(id("branch"), branch.label()))))
}

/// The rows of the Browse list all have this height (the list draws only
/// the rows in view).
const VERSION_ROW_HEIGHT: f32 = 52.;

/// Branch badge (Production/New Feature/Older Production/Legacy), from
/// NVIDIA's own data; omitted rather than guessed without it.
fn branch_tag(branch: DriverBranch, dark: bool) -> Tag {
    let (background, text) = branch_colors(branch, dark);
    Tag::custom(background, text, text.opacity(0.4)).small().rounded_full()
}

/// A branch badge's background and text colours: a tint of the branch's
/// colour with text of the same hue that reads on it, in the light and the
/// dark theme alike (at least 4.5:1, see the tests). The toolkit's own
/// success and info tags put light text on a light green or cyan in its dark
/// theme.
fn branch_colors(branch: DriverBranch, dark: bool) -> (Hsla, Hsla) {
    let (hue, saturation) = match branch {
        DriverBranch::Production => (142., 0.65),
        DriverBranch::NewFeature => (199., 0.75),
        DriverBranch::OlderProduction => (38., 0.85),
        DriverBranch::Legacy => (220., 0.12),
    };
    let hue = hue / 360.;
    if dark {
        (hsla(hue, saturation * 0.6, 0.17, 1.), hsla(hue, saturation, 0.80, 1.))
    } else {
        (hsla(hue, saturation, 0.92, 1.), hsla(hue, saturation, 0.24, 1.))
    }
}

/// The download's progress bar value and text for (downloaded, total) bytes
/// after `elapsed` seconds; with no total, an activity animation and the
/// text unchanged.
fn progress_text((dl, total): (u64, Option<u64>), elapsed: f64) -> (Option<f32>, Option<String>) {
    let Some(t) = total.filter(|t| *t > 0) else {
        return (None, None);
    };
    let speed = if elapsed > 0.0 { dl as f64 / elapsed } else { 0.0 };
    let eta = if speed > 0.0 {
        let remaining = t.saturating_sub(dl) as f64 / speed;
        if remaining < 60.0 {
            format!("{remaining:.0}s remaining")
        } else {
            format!("{:.0}m remaining", remaining / 60.0)
        }
    } else {
        "calculating…".to_string()
    };
    let text = format!("{:.1} / {:.1} MB  ({:.1} MB/s)  {}", dl as f64 / 1_000_000.0, t as f64 / 1_000_000.0, speed / 1_000_000.0, eta);
    (Some((dl as f64 / t as f64) as f32), Some(text))
}

// ─────────────────────────────────────────────────────────────────────────────
//  The signing dialog's content: two password fields and the rule
// ─────────────────────────────────────────────────────────────────────────────

const SIGNING_TITLE: &str = "Set Up Module Signing";

struct SigningPrompt {
    first: Entity<InputState>,
    second: Entity<InputState>,
    /// The password rule, or what is wrong with what was typed.
    hint: SharedString,
    valid: bool,
    _subscriptions: Vec<Subscription>,
}

impl SigningPrompt {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let first = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("One-time password"));
        let second = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("Type it again"));
        let subscriptions = [&first, &second]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(input, window, |this, _, event: &InputEvent, _, cx| {
                    if let InputEvent::Change = event {
                        this.check(cx);
                    }
                })
            })
            .collect();
        Self { first, second, hint: MOK_PASSWORD_HINT.into(), valid: false, _subscriptions: subscriptions }
    }

    fn check(&mut self, cx: &mut Context<Self>) {
        let (a, b) = (self.first.read(cx).value(), self.second.read(cx).value());
        match validate_mok_password(&a, &b) {
            Ok(()) => {
                self.hint = "Passwords match.".into();
                self.valid = true;
            }
            Err(msg) => {
                self.hint = msg.into();
                self.valid = false;
            }
        }
        cx.notify();
    }

    /// The password typed; both fields are emptied.
    fn take_password(&mut self, window: &mut Window, cx: &mut Context<Self>) -> String {
        let password = self.first.read(cx).value().to_string();
        for input in [&self.first, &self.second] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.valid = false;
        password
    }
}

impl Render for SigningPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_3()
            .child(label(
                "signing-text",
                "Pick a one-time password to approve this computer's module-signing key. \
                 On the next restart a blue MOK Manager screen appears before Linux starts: \
                 choose Enroll MOK and type this same password there once. Any key another \
                 app has already queued (such as a kernel-signing key) is kept in the same \
                 request, and this password enrolls all of them; a password chosen earlier \
                 no longer works.",
            ))
            .child(Input::new(&self.first).mask_toggle().aria_label("One-time password"))
            .child(Input::new(&self.second).mask_toggle().aria_label("Type it again"))
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(label("signing-hint", self.hint.clone())))
    }
}

// ─────────────────────────────────────────────────────────────────────────────

const ABOUT_TITLE: &str = "About Green Light";

const NOT_A_RUN_FILE: &str = "Not an NVIDIA .run file";

const RECOVER_TEXT: &str = "An earlier driver install was cut off before it finished. Restore \
     the driver this computer had before (this also happens on its own at the next boot).";

const BLOCK_SUBTITLE: &str = "Keep apt or dnf from installing the distribution's NVIDIA driver over this one later";

/// How long the branch labels may take before the list goes without them.
const BRANCH_LABEL_TIMEOUT: Duration = Duration::from_secs(20);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preflight::SigningKeyState;

    #[test]
    fn compare_newer_same_older() {
        assert!(compare_versions("595.84", "595.91.07") == VersionRelation::Newer);
        assert!(compare_versions("595.84", "595.84") == VersionRelation::Same);
        assert!(compare_versions("595.91.07", "595.84") == VersionRelation::Older);
        assert!(compare_versions("580.178.04", "595.45.04") == VersionRelation::Newer);
    }

    #[test]
    fn compare_numeric_not_lexicographic() {
        // 595.9 < 595.10 numerically even though "9" > "1" as text.
        assert!(compare_versions("595.9", "595.10") == VersionRelation::Newer);
    }

    #[test]
    fn one_operation_at_a_time() {
        let ver = DriverVersion { version: "595.84".into(), filename: "NVIDIA-Linux-x86_64-595.84.run".into(), url: String::new(), branch: None };
        let mut s = AppState { selected_version: Some(ver), downloaded_path: Some("/x.run".into()), sysinfo_loaded: true, ..AppState::default() };
        s.sysinfo.signing_key = SigningKeyState::NoKey;
        let all = ActionsEnabled { download: true, open_file: true, install: true, setup_signing: true };
        assert_eq!(actions_enabled(&s), all);
        let none = ActionsEnabled { download: false, open_file: false, install: false, setup_signing: false };
        for busy in [Busy::Download, Busy::Install, Busy::Signing] {
            s.busy = Some(busy);
            assert_eq!(actions_enabled(&s), none, "{busy:?}");
        }
        s.busy = None;
        s.external_install = true;
        assert_eq!(actions_enabled(&s), none, "install from an earlier run");
        s.external_install = false;
        // Before system info is in, no install: its options aren't known yet.
        s.sysinfo_loaded = false;
        let early = actions_enabled(&s);
        assert!(!early.install && early.download && early.open_file);
        s.selected_version = None;
        s.downloaded_path = None;
        let idle = actions_enabled(&s);
        assert!(!idle.download && !idle.install && !idle.setup_signing && idle.open_file);
    }

    #[test]
    fn busy_guard_ends_only_its_own_operation() {
        let state: SharedState = Rc::new(RefCell::new(AppState::default()));
        let first = BusyGuard::start(&state, Busy::Download).unwrap();
        assert!(BusyGuard::start(&state, Busy::Install).is_none());
        // Dropped without running its callback, as after a panic: ends it.
        drop(first);
        assert_eq!(state.borrow().busy, None);
        let second = BusyGuard::start(&state, Busy::Install).unwrap();
        // A stale guard of the finished operation must not end the new one.
        let stale = BusyGuard { state: state.clone(), token: 1 };
        drop(stale);
        assert_eq!(state.borrow().busy, Some(Busy::Install));
        second.release();
        assert_eq!(state.borrow().busy, None);
        drop(second);
    }

    #[test]
    fn a_release_while_the_state_is_borrowed_happens_later() {
        let state: SharedState = Rc::new(RefCell::new(AppState::default()));
        let guard = BusyGuard::start(&state, Busy::Signing).unwrap();
        {
            let _borrowed = state.borrow();
            guard.release();
        }
        assert_eq!(state.borrow().busy, Some(Busy::Signing), "not while borrowed");
        run_deferred_releases();
        assert_eq!(state.borrow().busy, None, "done by the next poll");
    }

    #[test]
    fn compare_unknown_on_unparseable() {
        assert!(compare_versions("", "595.84") == VersionRelation::Unknown);
        assert!(compare_versions("595.84", "abc") == VersionRelation::Unknown);
    }

    #[test]
    fn enter_acts_as_the_focused_button() {
        assert_eq!(enter_key(&[false, true]), Some(1));
        assert_eq!(enter_key(&[true, false, false]), Some(0), "Enter on Cancel cancels");
        assert_eq!(enter_key(&[false, false]), None, "Enter anywhere else decides nothing");
    }

    #[test]
    fn download_progress_text() {
        let (fraction, text) = progress_text((50_000_000, Some(100_000_000)), 10.0);
        assert_eq!(fraction, Some(0.5));
        assert_eq!(text.as_deref(), Some("50.0 / 100.0 MB  (5.0 MB/s)  10s remaining"));
        assert_eq!(progress_text((5, None), 1.0), (None, None), "no size: activity only");
    }

    #[test]
    fn an_error_dialog_that_was_seen_is_not_shown_twice() {
        assert!(helper_showed("zenity", Some(0)));
        assert!(helper_showed("zenity", Some(1)), "closed without OK: still seen");
        assert!(helper_showed("kdialog", Some(1)));
        assert!(!helper_showed("zenity", Some(255)), "could not open a display");
        assert!(!helper_showed("kdialog", None), "killed by a signal");
        assert!(helper_showed("notify-send", Some(0)));
        assert!(!helper_showed("notify-send", Some(1)), "no notification service");
        assert!(helper_showed("gdbus", Some(0)));
        assert!(!helper_showed("gdbus", Some(1)), "no notification service");
    }

    #[test]
    fn without_notify_send_a_notification_goes_over_dbus_then_xmessage() {
        let dialogs = error_dialogs("it's\nbroken");
        let programs: Vec<_> = dialogs.iter().map(|(program, _)| *program).collect();
        assert_eq!(programs, ["kdialog", "zenity", "notify-send", "gdbus", "xmessage"]);
        let gdbus = &dialogs[3].1;
        assert!(gdbus.windows(2).any(|w| w == ["--method", "org.freedesktop.Notifications.Notify"]));
        assert!(gdbus.contains(&"'it\\'s\\nbroken'".to_string()), "{gdbus:?}");
        assert_eq!(dialogs[4].1.last().map(String::as_str), Some("it's\nbroken"));
        assert_eq!(gvariant_string("a\\b"), "'a\\\\b'");
    }

    #[test]
    fn the_no_window_message_names_the_real_cause() {
        let gpu = no_window_message("x", true);
        assert!(gpu.contains("graphics driver") && gpu.contains("software renderer") && gpu.ends_with("Details: x"));
        let headless = no_window_message("x", false);
        assert!(headless.contains("no display") && !headless.contains("graphics driver"));
    }

    /// WCAG contrast ratio of two opaque colours.
    fn contrast(a: Hsla, b: Hsla) -> f32 {
        let luminance = |c: Hsla| {
            let c = gpui_kit::Rgba::from(c);
            let lin = |v: f32| if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
            0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
        };
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn branch_badges_are_readable_in_both_themes() {
        for branch in [DriverBranch::Production, DriverBranch::NewFeature, DriverBranch::OlderProduction, DriverBranch::Legacy] {
            for dark in [false, true] {
                let (background, text) = branch_colors(branch, dark);
                let ratio = contrast(background, text);
                assert!(ratio >= 4.5, "{} badge, dark={dark}: contrast {ratio:.2}", branch.label());
            }
        }
    }

    #[test]
    fn fonts_are_the_first_installed_of_the_list() {
        let installed = ["Noto Sans Mono", "Noto Sans", "Cantarell"].map(String::from);
        assert_eq!(first_installed(&UI_FONTS, &installed), Some("Cantarell"));
        assert_eq!(first_installed(&MONO_FONTS, &installed), Some("Noto Sans Mono"));
        assert_eq!(first_installed(&UI_FONTS, &[]), None);
    }
}
