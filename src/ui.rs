use crate::branches::{fetch_branches, DriverBranch, UNIX_DRIVERS_PAGE};
use crate::download::{download_run_file, verify_sha256};
use crate::install::{
    check_run_file, install_running, recovery_note, recovery_pending, run_privileged_discard_recovery, run_privileged_install, run_privileged_recover,
    run_privileged_setup_signing, InstallOptions, Recovered, RecoveryNote, StaleRecovery,
};
use crate::preflight::{
    conflict_summary, module_hint, secure_boot_advice, secure_boot_summary, signing_summary, validate_module_choice, validate_mok_password, ModuleChoice,
    MOK_PASSWORD_HINT,
};
use crate::system::{format_bytes, query_system, SecureBootStatus, SystemInfo, MIN_DISK_BYTES};
use crate::versions::{fetch_checksum, fetch_versions, is_sha256_hex, version_from_filename, DriverVersion};

use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, FileDialog, Label, ListBox, Orientation, ProgressBar, ScrolledWindow, SelectionMode, Spinner, Stack, Switch, TextView,
    WrapMode,
};
use libadwaita::prelude::*;
use libadwaita::{AboutDialog, ActionRow, Application, ApplicationWindow, Banner, ComboRow, HeaderBar, PreferencesGroup, Toast, ToastOverlay};

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

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

/// Holds the one running operation. Dropping it ends the operation, so a
/// task that panics (and whose callback therefore never runs) can't leave
/// every button disabled; `release` ends it early. It only ever ends its own
/// operation, never one started after it.
struct BusyGuard {
    state: Rc<RefCell<AppState>>,
    refresh: Rc<dyn Fn()>,
    token: u64,
}

impl BusyGuard {
    /// Start `kind`, unless something is already running.
    fn start(state: &Rc<RefCell<AppState>>, refresh: &Rc<dyn Fn()>, kind: Busy) -> Option<Rc<Self>> {
        let token = {
            let mut s = state.borrow_mut();
            if s.busy.is_some() || s.external_install {
                return None;
            }
            s.busy_token += 1;
            s.busy = Some(kind);
            s.busy_token
        };
        refresh();
        Some(Rc::new(Self { state: state.clone(), refresh: refresh.clone(), token }))
    }

    fn release(&self) {
        let Ok(mut s) = self.state.try_borrow_mut() else {
            // Called while the state is borrowed: finish on the next idle.
            let (state, refresh, token) = (self.state.clone(), self.refresh.clone(), self.token);
            glib::idle_add_local_once(move || {
                BusyGuard { state, refresh, token }.release();
            });
            return;
        };
        if s.busy.is_none() || s.busy_token != self.token {
            return;
        }
        s.busy = None;
        s.download_cancel = None;
        drop(s);
        (self.refresh)();
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.release();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Async bridge: run future on Tokio, deliver result to GTK main thread
// ─────────────────────────────────────────────────────────────────────────────

fn spawn_async<F, T, CB>(future: F, callback: CB)
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
    CB: FnOnce(T) + 'static,
{
    // The heavy future runs on the Tokio runtime; only the oneshot await
    // (runtime-independent) runs on the glib main context. No polling.
    let (tx, rx) = tokio::sync::oneshot::channel::<T>();
    crate::runtime().spawn(async move {
        let _ = tx.send(future.await);
    });
    glib::MainContext::default().spawn_local(async move {
        if let Ok(val) = rx.await {
            callback(val);
        }
    });
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
//  build_ui
// ─────────────────────────────────────────────────────────────────────────────

pub fn build_ui(app: &Application) {
    let state = Rc::new(RefCell::new(AppState::default()));

    // ── Window ───────────────────────────────────────────────────────────────
    let window = ApplicationWindow::builder().application(app).title("Green Light").default_width(760).default_height(640).build();

    let toast_overlay = ToastOverlay::new();

    // ── Header ───────────────────────────────────────────────────────────────
    let header = HeaderBar::new();

    let refresh_btn = Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Refresh version list").build();
    header.pack_start(&refresh_btn);

    let about_btn = Button::builder().icon_name("help-about-symbolic").tooltip_text("About").build();
    header.pack_end(&about_btn);

    // Driver badge in header — always visible
    let driver_badge = Label::new(Some("Driver: checking…"));
    driver_badge.add_css_class("dim-label");
    driver_badge.set_margin_start(8);
    driver_badge.set_margin_end(8);
    header.set_title_widget(Some(&driver_badge));

    // ── Stack ────────────────────────────────────────────────────────────────
    let stack = Stack::new();
    stack.set_transition_type(gtk4::StackTransitionType::SlideLeftRight);
    let switcher = gtk4::StackSwitcher::builder().stack(&stack).build();

    // We'll replace the header title with the switcher after sysinfo loads
    // For now driver_badge is the title — swapped below.

    // ═════════════════════════════════════════════════════════════════════════
    //  PAGE 1: System Info
    // ═════════════════════════════════════════════════════════════════════════
    let sysinfo_page = GtkBox::new(Orientation::Vertical, 12);
    sysinfo_page.set_margin_top(12);
    sysinfo_page.set_margin_bottom(12);
    sysinfo_page.set_margin_start(12);
    sysinfo_page.set_margin_end(12);

    let hw_group = PreferencesGroup::builder().title("Hardware").build();
    let gpu_row = ActionRow::builder().title("GPU").subtitle("Detecting…").build();
    let driver_row = ActionRow::builder().title("Installed Driver").subtitle("Detecting…").build();
    let module_row = ActionRow::builder().title("Kernel Module").subtitle("Detecting…").build();
    let kernel_row = ActionRow::builder().title("Kernel").subtitle("Detecting…").build();
    hw_group.add(&gpu_row);
    hw_group.add(&driver_row);
    hw_group.add(&module_row);
    hw_group.add(&kernel_row);
    sysinfo_page.append(&hw_group);

    let status_group = PreferencesGroup::builder().title("Status").build();
    let dkms_row = ActionRow::builder().title("DKMS Modules").subtitle("Checking…").build();
    let secureboot_row = ActionRow::builder().title("Secure Boot").subtitle("Checking…").build();
    let signing_row = ActionRow::builder().title("Module Signing").subtitle("Checking…").build();
    signing_row.set_subtitle_lines(0);
    let signing_btn = Button::builder()
        .label("Set Up Signing")
        .valign(Align::Center)
        .sensitive(false)
        .tooltip_text("Create or reuse this computer's module-signing key and enroll it")
        .build();
    signing_row.add_suffix(&signing_btn);
    let disk_row = ActionRow::builder().title("Free Disk Space").subtitle("Checking…").build();
    let reboot_row = ActionRow::builder().title("Reboot Required").subtitle("Checking…").build();
    status_group.add(&dkms_row);
    status_group.add(&secureboot_row);
    status_group.add(&signing_row);
    status_group.add(&disk_row);
    status_group.add(&reboot_row);
    sysinfo_page.append(&status_group);

    let refresh_sysinfo_btn = Button::builder().label("Refresh System Info").halign(Align::Center).margin_top(8).build();
    refresh_sysinfo_btn.add_css_class("flat");
    sysinfo_page.append(&refresh_sysinfo_btn);

    stack.add_titled(&sysinfo_page, Some("sysinfo"), "System");

    // ═════════════════════════════════════════════════════════════════════════
    //  PAGE 2: Browse
    // ═════════════════════════════════════════════════════════════════════════
    let browse_page = GtkBox::new(Orientation::Vertical, 0);

    let banner = Banner::new("");
    banner.set_revealed(false);
    browse_page.append(&banner);

    let search_bar = gtk4::SearchBar::new();
    let search_entry = gtk4::SearchEntry::new();
    search_entry.set_placeholder_text(Some("Filter versions…"));
    search_bar.set_child(Some(&search_entry));
    search_bar.set_show_close_button(false);
    search_bar.set_search_mode(true);
    browse_page.append(&search_bar);

    let list_box = ListBox::new();
    list_box.set_selection_mode(SelectionMode::Single);
    list_box.add_css_class("boxed-list");
    list_box.set_margin_top(8);
    list_box.set_margin_bottom(8);
    list_box.set_margin_start(12);
    list_box.set_margin_end(12);
    let scroll = ScrolledWindow::builder().vexpand(true).child(&list_box).build();
    browse_page.append(&scroll);

    let list_spinner = Spinner::new();
    list_spinner.set_halign(Align::Center);
    list_spinner.set_size_request(48, 48);
    browse_page.append(&list_spinner);

    let selected_label = Label::builder().label("No version selected").margin_top(4).margin_bottom(4).build();
    selected_label.add_css_class("dim-label");
    browse_page.append(&selected_label);

    let browse_bar = gtk4::ActionBar::new();
    let open_file_btn = Button::builder().label("Open .run File…").tooltip_text("Use a locally downloaded .run file").build();
    open_file_btn.add_css_class("flat");
    let download_btn = Button::builder().label("Download").sensitive(false).build();
    download_btn.add_css_class("suggested-action");
    browse_bar.pack_start(&open_file_btn);
    browse_bar.pack_end(&download_btn);
    browse_page.append(&browse_bar);

    stack.add_titled(&browse_page, Some("browse"), "Browse");

    // ═════════════════════════════════════════════════════════════════════════
    //  PAGE 3: Configure & Install
    // ═════════════════════════════════════════════════════════════════════════
    let config_page = GtkBox::new(Orientation::Vertical, 12);
    config_page.set_margin_top(12);
    config_page.set_margin_bottom(12);
    config_page.set_margin_start(12);
    config_page.set_margin_end(12);

    let file_group = PreferencesGroup::builder().title("Selected Driver").build();
    let file_row = ActionRow::builder().title("No file selected").subtitle("Go to Browse tab to select or download a driver").build();
    let checksum_row = ActionRow::builder().title("Checksum").subtitle("—").build();
    let version_status_row = ActionRow::builder().title("Version Status").subtitle("—").build();
    file_group.add(&file_row);
    file_group.add(&checksum_row);
    file_group.add(&version_status_row);
    config_page.append(&file_group);

    // Warnings group (disk, secure boot)
    let warn_group = PreferencesGroup::builder().title("Pre-install Checks").build();
    let disk_warn_row = ActionRow::builder().title("Disk Space").subtitle("—").build();
    let sb_warn_row = ActionRow::builder().title("Secure Boot").subtitle("—").build();
    let pkg_warn_row = ActionRow::builder().title("Existing Driver Packages").subtitle("—").build();
    for row in [&disk_warn_row, &sb_warn_row, &pkg_warn_row] {
        row.set_subtitle_lines(0);
    }
    warn_group.add(&disk_warn_row);
    warn_group.add(&sb_warn_row);
    warn_group.add(&pkg_warn_row);
    config_page.append(&warn_group);

    let opts_group = PreferencesGroup::builder().title("Install Options").build();

    let dkms_opt_row = ActionRow::builder().title("Enable DKMS").subtitle("Automatically rebuild kernel module on kernel updates").build();
    let dkms_switch = Switch::builder().valign(Align::Center).active(true).build();
    dkms_opt_row.add_suffix(&dkms_switch);
    dkms_opt_row.set_activatable_widget(Some(&dkms_switch));

    let block_opt_row = ActionRow::builder().title("Block Distro Driver Packages").subtitle(BLOCK_SUBTITLE).build();
    block_opt_row.set_subtitle_lines(0);
    let block_switch = Switch::builder().valign(Align::Center).build();
    block_opt_row.add_suffix(&block_switch);
    block_opt_row.set_activatable_widget(Some(&block_switch));

    let module_combo = ComboRow::builder()
        .title("Kernel Module")
        .subtitle("Automatic: the installer picks from the detected GPUs")
        .model(&gtk4::StringList::new(&["Automatic (recommended)", "Open", "Proprietary"]))
        .build();

    opts_group.add(&dkms_opt_row);
    opts_group.add(&module_combo);
    opts_group.add(&block_opt_row);
    config_page.append(&opts_group);

    let progress = ProgressBar::new();
    progress.set_show_text(true);
    progress.set_text(Some(""));
    progress.set_visible(false);
    config_page.append(&progress);

    // Download + cancel buttons in an hbox
    let dl_cancel_box = GtkBox::new(Orientation::Horizontal, 8);
    dl_cancel_box.set_halign(Align::Center);
    dl_cancel_box.set_margin_top(8);

    let install_btn = Button::builder().label("Install Driver").sensitive(false).build();
    install_btn.add_css_class("pill");
    install_btn.add_css_class("suggested-action");

    let cancel_dl_btn = Button::builder().label("Cancel Download").visible(false).build();
    cancel_dl_btn.add_css_class("pill");
    cancel_dl_btn.add_css_class("destructive-action");

    dl_cancel_box.append(&install_btn);
    dl_cancel_box.append(&cancel_dl_btn);
    config_page.append(&dl_cancel_box);

    stack.add_titled(&config_page, Some("configure"), "Configure");

    // ═════════════════════════════════════════════════════════════════════════
    //  PAGE 4: Log
    // ═════════════════════════════════════════════════════════════════════════
    let log_page = GtkBox::new(Orientation::Vertical, 0);
    let log_view = TextView::builder().editable(false).monospace(true).wrap_mode(WrapMode::Word).vexpand(true).build();
    log_view.add_css_class("card");
    log_view.set_margin_top(8);
    log_view.set_margin_bottom(8);
    log_view.set_margin_start(8);
    log_view.set_margin_end(8);
    let log_scroll = ScrolledWindow::builder().vexpand(true).child(&log_view).build();
    let clear_btn = Button::builder().label("Clear Log").halign(Align::End).margin_end(8).margin_bottom(8).build();
    clear_btn.add_css_class("flat");
    log_page.append(&log_scroll);
    log_page.append(&clear_btn);
    stack.add_titled(&log_page, Some("log"), "Log");

    // ── Assemble window ───────────────────────────────────────────────────────
    // Now set the switcher as header title
    header.set_title_widget(Some(&switcher));

    let content = GtkBox::new(Orientation::Vertical, 0);
    content.append(&header);

    // Reboot required banner — shown at top of every page when relevant
    let reboot_banner = Banner::new("A reboot is required to activate the new driver.");
    reboot_banner.set_revealed(false);
    content.append(&reboot_banner);

    let running_banner = Banner::new(
        "A driver install started earlier is still running. Don't reboot until it finishes; \
         its progress is in /var/log/green-light.log.",
    );
    running_banner.set_revealed(false);
    content.append(&running_banner);

    // An install was cut off (power loss, crash) and left its journal. The
    // banner's text and action follow the script's note (see the timer below).
    let recover_banner = Banner::new(RECOVER_TEXT);
    recover_banner.set_button_label(Some("Restore Previous Driver"));
    recover_banner.set_revealed(false);
    content.append(&recover_banner);
    // The banner's button discards the data instead of restoring from it.
    let recover_discards = Rc::new(std::cell::Cell::new(false));

    content.append(&stack);
    toast_overlay.set_child(Some(&content));
    window.set_content(Some(&toast_overlay));

    // ─────────────────────────────────────────────────────────────────────────
    //  Helpers
    // ─────────────────────────────────────────────────────────────────────────

    let log_fn: Rc<dyn Fn(String)> = {
        let log_view = log_view.clone();
        Rc::new(move |msg: String| {
            let buf = log_view.buffer();
            let mut end = buf.end_iter();
            buf.insert(&mut end, &format!("{}\n", msg));
            let mark = buf.create_mark(None, &buf.end_iter(), false);
            log_view.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
        })
    };

    // Button sensitivity, from the state; call after every change to it.
    let refresh_actions: Rc<dyn Fn()> = {
        let state = state.clone();
        let download_btn = download_btn.clone();
        let open_file_btn = open_file_btn.clone();
        let install_btn = install_btn.clone();
        let signing_btn = signing_btn.clone();
        Rc::new(move || {
            let a = actions_enabled(&state.borrow());
            download_btn.set_sensitive(a.download);
            open_file_btn.set_sensitive(a.open_file);
            install_btn.set_sensitive(a.install);
            signing_btn.set_sensitive(a.setup_signing);
            // Nothing to set up once the key is enrolled or queued: hide the
            // button rather than leave an action that looks needed.
            let s = state.borrow();
            signing_btn.set_visible(!s.sysinfo_loaded || s.sysinfo.signing_key.setup_needed());
        })
    };

    let update_config_tab: Rc<dyn Fn()> = {
        let file_row = file_row.clone();
        let checksum_row = checksum_row.clone();
        let version_status_row = version_status_row.clone();
        let disk_warn_row = disk_warn_row.clone();
        let sb_warn_row = sb_warn_row.clone();
        let pkg_warn_row = pkg_warn_row.clone();
        let module_combo = module_combo.clone();
        let refresh_actions = refresh_actions.clone();
        let state = state.clone();
        Rc::new(move || {
            let s = state.borrow();

            // File info
            if let Some(path) = &s.downloaded_path {
                let fname = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path).to_string();
                file_row.set_title(&fname);
                file_row.set_subtitle(path.as_str());
                if let Some(cs) = s.expected_checksum.as_deref().filter(|c| is_sha256_hex(c)) {
                    checksum_row.set_subtitle(&format!("SHA256: {}… (verified)", &cs[..16]));
                } else {
                    checksum_row.set_subtitle("No checksum available");
                }
                // Version comparison
                let parsed = version_from_filename(&fname);
                if parsed.is_none() {
                    // e.g. NVIDIA-Linux-x86_64-595.84-vulkan.run — don't guess.
                    version_status_row.set_subtitle("Unknown");
                } else if let (Some(inst), Some(candidate)) = (s.sysinfo.installed_driver.as_ref(), parsed.as_deref()) {
                    match compare_versions(inst, candidate) {
                        VersionRelation::Newer => version_status_row.set_subtitle(&format!("Upgrade: {} → {}", inst, candidate)),
                        VersionRelation::Same => version_status_row.set_subtitle(&format!("Reinstall: same version ({})", inst)),
                        VersionRelation::Older => version_status_row.set_subtitle(&format!("Downgrade: {} → {}", inst, candidate)),
                        VersionRelation::Unknown => version_status_row.set_subtitle("—"),
                    }
                } else {
                    version_status_row.set_subtitle("No driver currently installed");
                }
            } else {
                file_row.set_title("No file selected");
                file_row.set_subtitle("Go to Browse tab to select or download a driver");
                checksum_row.set_subtitle("—");
                version_status_row.set_subtitle("—");
            }

            // Disk check
            match s.sysinfo.free_disk_bytes {
                Some(free) if free < MIN_DISK_BYTES => {
                    disk_warn_row.set_subtitle(&format!("Low: {} free (2 GB recommended)", format_bytes(free)));
                }
                Some(free) => {
                    disk_warn_row.set_subtitle(&format!("{} free", format_bytes(free)));
                }
                None => disk_warn_row.set_subtitle("Could not determine"),
            }

            // Secure boot check
            match &s.sysinfo.secure_boot {
                SecureBootStatus::Enabled => sb_warn_row.set_subtitle(
                    &secure_boot_advice(&s.sysinfo.secure_boot, &s.sysinfo.signing_key, s.sysinfo.signing_pair.as_ref(), s.use_dkms)
                        .unwrap_or_else(|| "Enabled".to_string()),
                ),
                SecureBootStatus::Disabled => sb_warn_row.set_subtitle("Disabled"),
                SecureBootStatus::Unknown => sb_warn_row.set_subtitle("Unknown"),
            }

            // Distro driver packages the install will remove
            pkg_warn_row.set_subtitle(&conflict_summary(&s.sysinfo.distro_packages).unwrap_or_else(|| "None found".to_string()));

            // Kernel module flavor hint for the detected GPUs
            module_combo.set_subtitle(&module_hint(&s.sysinfo.gpu_compute_caps));
            drop(s);
            refresh_actions();
        })
    };

    // ─────────────────────────────────────────────────────────────────────────
    //  Load system info
    // ─────────────────────────────────────────────────────────────────────────

    let load_sysinfo = {
        let gpu_row = gpu_row.clone();
        let driver_row = driver_row.clone();
        let module_row = module_row.clone();
        let kernel_row = kernel_row.clone();
        let dkms_row = dkms_row.clone();
        let secureboot_row = secureboot_row.clone();
        let signing_row = signing_row.clone();
        let disk_row = disk_row.clone();
        let reboot_row = reboot_row.clone();
        let reboot_banner = reboot_banner.clone();
        let driver_badge = driver_badge.clone();
        let state = state.clone();
        let update_config_tab = update_config_tab.clone();
        let log_fn = log_fn.clone();
        let list_box = list_box.clone();
        let search_entry = search_entry.clone();
        let block_switch = block_switch.clone();
        let block_opt_row = block_opt_row.clone();

        move || {
            gpu_row.set_subtitle("Detecting…");
            driver_row.set_subtitle("Detecting…");
            log_fn("Querying system info…".to_string());

            let log_fn = log_fn.clone();
            let gpu_row = gpu_row.clone();
            let driver_row = driver_row.clone();
            let module_row = module_row.clone();
            let kernel_row = kernel_row.clone();
            let dkms_row = dkms_row.clone();
            let secureboot_row = secureboot_row.clone();
            let signing_row = signing_row.clone();
            let disk_row = disk_row.clone();
            let reboot_row = reboot_row.clone();
            let reboot_banner = reboot_banner.clone();
            let driver_badge = driver_badge.clone();
            let state = state.clone();
            let update_config_tab = update_config_tab.clone();
            let list_box = list_box.clone();
            let search_entry = search_entry.clone();
            let block_switch = block_switch.clone();
            let block_opt_row = block_opt_row.clone();

            spawn_async(async move { tokio::task::spawn_blocking(query_system).await.unwrap_or_default() }, move |info| {
                // GPU
                gpu_row.set_subtitle(info.gpu_name.as_deref().unwrap_or("Not detected"));

                // Driver
                if let Some(ref ver) = info.installed_driver {
                    driver_row.set_subtitle(ver.as_str());
                    driver_badge.set_label(&format!("Driver: {}", ver));
                    log_fn(format!("Installed driver: {}", ver));
                } else {
                    driver_row.set_subtitle("Not installed");
                    driver_badge.set_label("Driver: not installed");
                    log_fn("No NVIDIA driver detected".to_string());
                }

                // Kernel module flavor (loaded) and GPU generation
                let gens = if info.gpu_compute_caps.is_empty() {
                    String::new()
                } else {
                    let mut g: Vec<&str> = info.gpu_compute_caps.iter().map(|c| crate::preflight::generation_name(*c)).collect();
                    g.dedup();
                    format!(" ({})", g.join(", "))
                };
                module_row.set_subtitle(&format!("{}{}", info.module_flavor, gens));

                // Kernel
                kernel_row.set_subtitle(&info.kernel_version);

                // DKMS
                if info.dkms_status.is_empty() {
                    dkms_row.set_subtitle("No NVIDIA DKMS modules registered");
                } else {
                    let summary =
                        info.dkms_status.iter().map(|e| format!("{}/{} [{}] {}", e.module, e.version, e.kernel, e.status)).collect::<Vec<_>>().join("\n");
                    dkms_row.set_subtitle(&summary);
                }

                // Secure boot
                secureboot_row.set_subtitle(&secure_boot_summary(&info.secure_boot, &info.signing_key));

                // Module signing: key state, installed module's signer
                let mut signing_text = signing_summary(&info.signing_key, &info.module_signature);
                if info.signing_pair.as_ref().is_some_and(|p| p.key_hidden) {
                    signing_text.push_str("\nIts private key is readable only by root, so it is checked during the install.");
                }
                signing_row.set_subtitle(&signing_text);

                // Disk
                match info.free_disk_bytes {
                    Some(free) => disk_row.set_subtitle(&format!("{} free on /", format_bytes(free))),
                    None => disk_row.set_subtitle("Could not determine"),
                }

                // Reboot
                if info.reboot_required {
                    reboot_row.set_subtitle("Yes — new driver installed, reboot to activate");
                    reboot_banner.set_revealed(true);
                } else {
                    reboot_row.set_subtitle("No");
                    reboot_banner.set_revealed(false);
                }

                // The block option shows what is in place now: leaving it
                // on keeps an existing block, turning it off removes it.
                block_switch.set_active(info.distro_driver_blocked);
                block_switch.set_sensitive(info.block_supported);
                block_opt_row.set_subtitle(if info.block_supported {
                    BLOCK_SUBTITLE
                } else {
                    "Not available here: this dnf has no way to exclude packages for good"
                });

                {
                    let mut s = state.borrow_mut();
                    s.block_distro_driver = info.distro_driver_blocked && info.block_supported;
                    s.sysinfo = info;
                    s.sysinfo_loaded = true;
                }
                // Update configure tab with new disk/secureboot/version info
                update_config_tab();
                // Re-populate browse list with version badges now that driver is known
                let versions = state.borrow().versions.clone();
                if !versions.is_empty() {
                    let installed = state.borrow().sysinfo.installed_driver.clone();
                    populate_list(&list_box, &versions, &search_entry.text(), installed.as_deref());
                }
            });
        }
    };

    load_sysinfo();

    {
        let ls = load_sysinfo.clone();
        refresh_sysinfo_btn.connect_clicked(move |_| ls());
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Set Up Signing: one-time MOK password, then the privileged setup
    // ─────────────────────────────────────────────────────────────────────────
    {
        let window = window.clone();
        let log_fn = log_fn.clone();
        let toast_overlay = toast_overlay.clone();
        let stack = stack.clone();
        let load_sysinfo = load_sysinfo.clone();
        let state = state.clone();
        let refresh_actions = refresh_actions.clone();

        signing_btn.connect_clicked(move |_| {
            let pw1 = gtk4::PasswordEntry::builder().placeholder_text("One-time password").show_peek_icon(true).build();
            let pw2 = gtk4::PasswordEntry::builder().placeholder_text("Type it again").show_peek_icon(true).build();
            let hint = Label::builder().label(MOK_PASSWORD_HINT).wrap(true).xalign(0.0).build();
            hint.add_css_class("dim-label");
            let fields = GtkBox::new(Orientation::Vertical, 8);
            fields.append(&pw1);
            fields.append(&pw2);
            fields.append(&hint);

            let dialog = libadwaita::AlertDialog::new(
                Some("Set Up Module Signing"),
                Some(
                    "Pick a one-time password to approve this computer's module-signing key. \
                     On the next restart a blue MOK Manager screen appears before Linux starts: \
                     choose Enroll MOK and type this same password there once. Any key another \
                     app has already queued (such as a kernel-signing key) is kept in the same \
                     request, and this password enrolls all of them; a password chosen earlier \
                     no longer works.",
                ),
            );
            dialog.set_extra_child(Some(&fields));
            dialog.add_responses(&[("cancel", "Cancel"), ("setup", "Set Up Signing")]);
            dialog.set_response_appearance("setup", libadwaita::ResponseAppearance::Suggested);
            dialog.set_response_enabled("setup", false);
            dialog.set_close_response("cancel");

            // Weak references: the entries' handlers must not keep the dialog alive.
            for (entry, other) in [(&pw1, &pw2), (&pw2, &pw1)] {
                let dialog = dialog.downgrade();
                let other = other.downgrade();
                let hint = hint.downgrade();
                entry.connect_changed(move |entry| {
                    let (Some(dialog), Some(other), Some(hint)) = (dialog.upgrade(), other.upgrade(), hint.upgrade()) else {
                        return;
                    };
                    match validate_mok_password(&entry.text(), &other.text()) {
                        Ok(()) => {
                            hint.set_label("Passwords match.");
                            dialog.set_response_enabled("setup", true);
                        }
                        Err(msg) => {
                            hint.set_label(&msg);
                            dialog.set_response_enabled("setup", false);
                        }
                    }
                });
            }

            let log_fn = log_fn.clone();
            let toast_overlay = toast_overlay.clone();
            let stack = stack.clone();
            let load_sysinfo = load_sysinfo.clone();
            let state = state.clone();
            let refresh_actions = refresh_actions.clone();
            dialog.choose(Some(&window), gio::Cancellable::NONE, move |response| {
                if response != "setup" {
                    return;
                }
                let password = pw1.text().to_string();
                pw1.set_text("");
                pw2.set_text("");
                if validate_mok_password(&password, &password).is_err() {
                    return;
                }
                let Some(guard) = BusyGuard::start(&state, &refresh_actions, Busy::Signing) else {
                    return;
                };
                stack.set_visible_child_name("log");
                log_fn("Setting up module signing…".to_string());

                spawn_async(async move { tokio::task::spawn_blocking(move || run_privileged_setup_signing(&password)).await }, move |outcome| {
                    guard.release();
                    match outcome {
                        Ok(outcome) => {
                            for line in &outcome.lines {
                                log_fn(line.clone());
                            }
                            match outcome.result {
                                Ok(()) => {
                                    log_fn("Module signing setup finished.".to_string());
                                    toast_overlay.add_toast(Toast::new("Module signing set up — see Log tab"));
                                }
                                Err(e) => {
                                    log_fn(format!("Module signing setup failed: {}", e));
                                    toast_overlay.add_toast(Toast::new("Signing setup failed — see Log tab"));
                                }
                            }
                        }
                        Err(e) => log_fn(format!("Task error: {}", e)),
                    }
                    load_sysinfo();
                });
            });
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Load versions
    // ─────────────────────────────────────────────────────────────────────────

    let load_versions = {
        let list_box = list_box.clone();
        let list_spinner = list_spinner.clone();
        let refresh_actions = refresh_actions.clone();
        let selected_label = selected_label.clone();
        let banner = banner.clone();
        let state = state.clone();
        let log_fn = log_fn.clone();
        let search_entry = search_entry.clone();

        move || {
            list_spinner.start();
            list_spinner.set_visible(true);
            banner.set_revealed(false);
            while let Some(c) = list_box.first_child() {
                list_box.remove(&c);
            }
            refresh_actions();
            selected_label.set_label("Loading…");
            log_fn("Fetching version list from NVIDIA…".to_string());

            let list_box = list_box.clone();
            let list_spinner = list_spinner.clone();
            let selected_label = selected_label.clone();
            let banner = banner.clone();
            let state = state.clone();
            let log_fn = log_fn.clone();
            let search_entry = search_entry.clone();

            spawn_async(fetch_versions(), move |result| {
                list_spinner.stop();
                list_spinner.set_visible(false);
                match result {
                    Err(e) => {
                        log_fn(format!("Error fetching versions: {}", e));
                        banner.set_title(&format!("Failed to load versions: {}", e));
                        banner.set_revealed(true);
                        selected_label.set_label("Could not load version list");
                    }
                    Ok(versions) => {
                        log_fn(format!("Found {} versions", versions.len()));
                        selected_label.set_label("No version selected");
                        state.borrow_mut().versions = versions.clone();
                        let installed = state.borrow().sysinfo.installed_driver.clone();
                        populate_list(&list_box, &versions, &search_entry.text(), installed.as_deref());

                        // The branch labels come separately and may be slow or
                        // unavailable; the list is usable without them.
                        let list_box = list_box.clone();
                        let state = state.clone();
                        let log_fn = log_fn.clone();
                        spawn_async(
                            async { tokio::time::timeout(BRANCH_LABEL_TIMEOUT, fetch_branches(UNIX_DRIVERS_PAGE)).await },
                            move |result| match result {
                                Ok(Ok(branches)) => {
                                    {
                                        let mut s = state.borrow_mut();
                                        for v in &mut s.versions {
                                            v.branch = branches.classify(&v.version);
                                        }
                                    }
                                    add_branch_badges(&list_box, &state.borrow().versions);
                                }
                                Ok(Err(e)) => log_fn(format!("No branch labels: NVIDIA's branch data is unavailable ({e:#})")),
                                Err(_) => log_fn("No branch labels: NVIDIA's branch data took too long".to_string()),
                            },
                        );
                    }
                }
            });
        }
    };

    load_versions();

    {
        let lv = load_versions.clone();
        refresh_btn.connect_clicked(move |_| lv());
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Row selection — connected exactly once. The version is looked up by the
    //  row's title (the version string), never by row index: when the search
    //  filter is active the visible rows are a subset, so an index into the
    //  full versions vec would select the wrong driver.
    // ─────────────────────────────────────────────────────────────────────────
    {
        let state = state.clone();
        let refresh_actions = refresh_actions.clone();
        let selected_label = selected_label.clone();
        list_box.connect_row_selected(move |_, row| {
            let selected = row
                .and_then(|r| r.downcast_ref::<ActionRow>().map(|ar| ar.title().to_string()))
                .and_then(|title| state.borrow().versions.iter().find(|v| v.version == title).cloned());

            match selected {
                Some(ver) => {
                    selected_label.set_label(&format!("Selected: {}", ver.version));
                    state.borrow_mut().selected_version = Some(ver);
                }
                None => {
                    selected_label.set_label("No version selected");
                    state.borrow_mut().selected_version = None;
                }
            }
            // Selecting a row never enables Download while something runs.
            refresh_actions();
        });
    }

    {
        let list_box = list_box.clone();
        let state = state.clone();
        search_entry.connect_search_changed(move |entry| {
            // Clone what we need and drop the borrow BEFORE populate_list:
            // rebuilding the list can remove the selected row, which fires
            // row-selected(None) and borrow_mut()s the state.
            let (versions, installed) = {
                let s = state.borrow();
                (s.versions.clone(), s.sysinfo.installed_driver.clone())
            };
            populate_list(&list_box, &versions, &entry.text(), installed.as_deref());
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Download
    // ─────────────────────────────────────────────────────────────────────────
    let dl_ui = DownloadUi {
        state: state.clone(),
        progress: progress.clone(),
        cancel_btn: cancel_dl_btn.clone(),
        log: log_fn.clone(),
        update_config_tab: update_config_tab.clone(),
        refresh_actions: refresh_actions.clone(),
        stack: stack.clone(),
        toast: toast_overlay.clone(),
        window: window.clone(),
        guard: None,
    };
    {
        let dl_ui = dl_ui.clone();
        download_btn.connect_clicked(move |_| {
            let mut ui = dl_ui.clone();
            let (ver, free) = {
                let s = ui.state.borrow();
                (s.selected_version.clone(), s.sysinfo.free_disk_bytes)
            };
            let Some(ver) = ver else { return };
            let Some(guard) = BusyGuard::start(&ui.state, &ui.refresh_actions, Busy::Download) else {
                return;
            };
            ui.guard = Some(guard);

            if free.is_some_and(|f| f < MIN_DISK_BYTES) {
                ui.toast.add_toast(Toast::new("Low disk space — install may fail"));
            }

            let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            ui.state.borrow_mut().download_cancel = Some(cancel_flag.clone());
            ui.cancel_btn.set_visible(true);
            ui.progress.set_visible(true);
            ui.progress.set_fraction(0.0);
            ui.progress.set_text(Some(&format!("Downloading {}…", ver.version)));

            // XDG_DOWNLOAD_DIR normally lives only in ~/.config/user-dirs.dirs,
            // not the environment — ask GLib, which reads that file.
            let dest_dir = glib::user_special_dir(glib::UserDirectory::Downloads)
                .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join("Downloads"));

            // Progress channel — sent from the Tokio side, drained on the GTK
            // main context. The loop exits when the sender drops at download
            // end, so nothing is leaked.
            let (prog_tx, mut prog_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, Option<u64>)>();
            let download_start = std::time::Instant::now();
            {
                let progress = ui.progress.clone();
                glib::MainContext::default().spawn_local(async move {
                    let mut last_ui = std::time::Instant::now();
                    while let Some(mut msg) = prog_rx.recv().await {
                        // Coalesce bursts — jump to the newest queued update
                        while let Ok(newer) = prog_rx.try_recv() {
                            msg = newer;
                        }
                        // Throttle UI redraws to ~10 Hz
                        if last_ui.elapsed() < std::time::Duration::from_millis(100) {
                            continue;
                        }
                        last_ui = std::time::Instant::now();
                        let (dl, total) = msg;
                        if let Some(t) = total {
                            progress.set_fraction(dl as f64 / t as f64);
                            let elapsed = download_start.elapsed().as_secs_f64();
                            let speed = if elapsed > 0.0 { dl as f64 / elapsed } else { 0.0 };
                            let eta = if speed > 0.0 {
                                let remaining = t.saturating_sub(dl) as f64 / speed;
                                if remaining < 60.0 {
                                    format!("{:.0}s remaining", remaining)
                                } else {
                                    format!("{:.0}m remaining", remaining / 60.0)
                                }
                            } else {
                                "calculating…".to_string()
                            };
                            progress.set_text(Some(&format!(
                                "{:.1} / {:.1} MB  ({:.1} MB/s)  {}",
                                dl as f64 / 1_000_000.0,
                                t as f64 / 1_000_000.0,
                                speed / 1_000_000.0,
                                eta
                            )));
                        } else {
                            progress.pulse();
                        }
                    }
                });
            }

            let url = ver.url.clone();
            let filename = ver.filename.clone();
            spawn_async(
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
                move |(ver, checksum, result)| {
                    ui.cancel_btn.set_visible(false);
                    ui.state.borrow_mut().download_cancel = None;
                    match result {
                        Err(e) => {
                            if e.to_string().contains("cancelled") {
                                (ui.log)("Download cancelled.".to_string());
                                ui.progress.set_text(Some("Cancelled"));
                            } else {
                                (ui.log)(format!("Download failed: {e:#}"));
                                ui.progress.set_text(Some("Download failed"));
                                ui.toast.add_toast(Toast::new("Download failed — see Log tab"));
                            }
                            ui.end();
                        }
                        Ok(path) => {
                            (ui.log)(format!("Download complete: {}", path.display()));
                            ui.progress.set_fraction(1.0);
                            finish_download(ui, ver, path, checksum);
                        }
                    }
                },
            );
        });
    }

    // Cancel download button
    {
        let state = state.clone();
        cancel_dl_btn.connect_clicked(move |_| {
            if let Some(flag) = &state.borrow().download_cancel {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Open local file
    // ─────────────────────────────────────────────────────────────────────────
    {
        let window = window.clone();
        let state = state.clone();
        let update_config_tab = update_config_tab.clone();
        let stack = stack.clone();
        let log_fn = log_fn.clone();

        open_file_btn.connect_clicked(move |_| {
            if state.borrow().busy.is_some() {
                return;
            }
            let dialog = FileDialog::builder().title("Select NVIDIA .run File").modal(true).build();
            let filter = gtk4::FileFilter::new();
            filter.add_pattern("*.run");
            filter.set_name(Some("NVIDIA .run files"));
            let filters = gio::ListStore::new::<gtk4::FileFilter>();
            filters.append(&filter);
            dialog.set_filters(Some(&filters));

            let state = state.clone();
            let update_config_tab = update_config_tab.clone();
            let stack = stack.clone();
            let log_fn = log_fn.clone();
            let window = window.clone();

            dialog.open(Some(&window.clone()), gio::Cancellable::NONE, move |result| {
                let Some(path) = result.ok().and_then(|f| f.path()) else { return };
                // The install script refuses symlinks; use the file itself.
                // The checks run commands (getent, getfacl): not on this thread.
                spawn_async(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let path = std::fs::canonicalize(&path).unwrap_or(path);
                            let path_str = path.to_string_lossy().to_string();
                            let checked = check_run_file(&path_str);
                            (path_str, checked)
                        })
                        .await
                    },
                    move |checked| {
                        let Ok((path_str, checked)) = checked else { return };
                        if let Err(e) = checked {
                            log_fn(format!("Can't use {path_str}: {e}"));
                            show_error(&window, "This file can't be installed", &e.to_string());
                            return;
                        }
                        log_fn(format!("Opened local file: {}", path_str));
                        {
                            let mut s = state.borrow_mut();
                            s.downloaded_path = Some(path_str);
                            s.expected_checksum = None;
                        }
                        update_config_tab();
                        stack.set_visible_child_name("configure");
                    },
                );
            });
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Switches
    // ─────────────────────────────────────────────────────────────────────────
    {
        let s = state.clone();
        let update_config_tab = update_config_tab.clone();
        dkms_switch.connect_active_notify(move |sw| {
            s.borrow_mut().use_dkms = sw.is_active();
            // The Secure Boot advice depends on it.
            update_config_tab();
        });
    }
    {
        let s = state.clone();
        module_combo.connect_selected_notify(move |row| {
            s.borrow_mut().module_choice = match row.selected() {
                1 => ModuleChoice::Open,
                2 => ModuleChoice::Proprietary,
                _ => ModuleChoice::Auto,
            };
        });
    }
    {
        let s = state.clone();
        block_switch.connect_active_notify(move |sw| {
            s.borrow_mut().block_distro_driver = sw.is_active();
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Install
    // ─────────────────────────────────────────────────────────────────────────
    {
        let state = state.clone();
        let log_fn = log_fn.clone();
        let stack = stack.clone();
        let toast_overlay = toast_overlay.clone();
        let progress = progress.clone();
        let load_sysinfo = load_sysinfo.clone();
        let refresh_actions = refresh_actions.clone();
        let window = window.clone();

        install_btn.connect_clicked(move |install_btn| {
            let install_btn = install_btn.clone();
            let s = state.borrow();
            if s.busy.is_some() {
                return;
            }
            let Some(ref run_file) = s.downloaded_path else { return };

            if let Some(free) = s.sysinfo.free_disk_bytes {
                if free < MIN_DISK_BYTES {
                    toast_overlay.add_toast(Toast::new("Warning: low disk space — install may fail"));
                }
            }

            // Refuse module flavors that cannot work, before asking for a password.
            let driver_major = Path::new(run_file)
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(version_from_filename)
                .and_then(|v| v.split('.').next().and_then(|m| m.parse::<u32>().ok()));
            if let Err(msg) = validate_module_choice(s.module_choice, &s.sysinfo.gpu_compute_caps, driver_major) {
                log_fn(format!("Install not started: {}", msg));
                toast_overlay.add_toast(Toast::new(&msg));
                return;
            }

            let opts = InstallOptions {
                use_dkms: s.use_dkms,
                block_distro_driver: s.block_distro_driver,
                module: s.module_choice,
                run_file: run_file.clone(),
                sha256: s.expected_checksum.clone(),
                discard_recovery: s.discard_recovery_next,
            };
            drop(s);
            state.borrow_mut().discard_recovery_next = false;

            let Some(guard) = BusyGuard::start(&state, &refresh_actions, Busy::Install) else {
                return;
            };
            progress.set_visible(true);
            progress.set_text(Some("Installing… the desktop stays up; this takes a few minutes"));
            stack.set_visible_child_name("log");
            log_fn(format!("Starting install: {}", opts.run_file));
            log_fn("The new driver installs alongside the running one; reboot afterward to switch over.".to_string());

            // Pulse the bar while the install runs; the timer stops itself
            // once the install is no longer running.
            {
                let progress = progress.clone();
                let state = state.clone();
                glib::timeout_add_local(std::time::Duration::from_millis(150), move || {
                    if state.borrow().busy == Some(Busy::Install) {
                        progress.pulse();
                        glib::ControlFlow::Continue
                    } else {
                        glib::ControlFlow::Break
                    }
                });
            }

            // The script's output goes into the Log tab line by line as it
            // runs; the result is shown only after the last line (the sender
            // is dropped when the install call returns, before the result is
            // sent).
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

            let log_fn = log_fn.clone();
            let toast_overlay = toast_overlay.clone();
            let progress = progress.clone();
            let load_sysinfo = load_sysinfo.clone();
            let window = window.clone();
            let state = state.clone();

            glib::MainContext::default().spawn_local(async move {
                while let Some(line) = line_rx.recv().await {
                    log_fn(line);
                }
                let result: Result<anyhow::Result<()>, String> = match result_rx.await {
                    Ok(Ok(r)) => Ok(r),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err("the install task ended unexpectedly".to_string()),
                };
                guard.release();
                progress.set_visible(false);
                {
                    match result {
                        Ok(Ok(())) => {
                            log_fn("Install completed successfully.".to_string());
                            log_fn("Reboot to switch to the new driver.".to_string());
                            toast_overlay.add_toast(Toast::new("Install complete — reboot to activate"));
                        }
                        Ok(Err(e)) if e.downcast_ref::<StaleRecovery>().is_some() => {
                            log_fn(format!("Install not started: {e}"));
                            // Only on the user's word: install anyway, dropping that data.
                            let dialog = libadwaita::AlertDialog::new(Some("Recovery data from an interrupted install"), Some(&e.to_string()));
                            dialog.add_responses(&[("cancel", "Cancel"), ("discard", "Discard and Install")]);
                            dialog.set_response_appearance("discard", libadwaita::ResponseAppearance::Destructive);
                            dialog.set_default_response(Some("cancel"));
                            dialog.set_close_response("cancel");
                            let state = state.clone();
                            let install_btn = install_btn.clone();
                            dialog.choose(Some(&window), gio::Cancellable::NONE, move |response| {
                                if response == "discard" {
                                    state.borrow_mut().discard_recovery_next = true;
                                    install_btn.emit_clicked();
                                }
                            });
                        }
                        Ok(Err(e)) => {
                            log_fn(format!("Install failed: {e:#}"));
                            show_error(&window, "Install failed", &format!("{e:#}"));
                        }
                        Err(e) => {
                            log_fn(format!("Task error: {}", e));
                        }
                    }
                    // A failed install may have changed (or put back) packages too.
                    load_sysinfo();
                }
            });
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  About dialog
    // ─────────────────────────────────────────────────────────────────────────
    {
        let window = window.clone();
        let state = state.clone();
        about_btn.connect_clicked(move |_| {
            let (driver, gpu, kernel) = {
                let s = state.borrow();
                (
                    s.sysinfo.installed_driver.clone().unwrap_or_else(|| "Not detected".to_string()),
                    s.sysinfo.gpu_name.clone().unwrap_or_else(|| "Not detected".to_string()),
                    s.sysinfo.kernel_version.clone(),
                )
            };

            let dialog = AboutDialog::builder()
                .application_name("Green Light")
                .version(env!("CARGO_PKG_VERSION"))
                .developers(vec!["Linnard Alex Brown Jr."])
                // GTK's Agpl30 is "AGPL version 3 or later" (Agpl30Only is the
                // strict variant), matching AGPL-3.0-or-later in Cargo.toml.
                .license_type(gtk4::License::Agpl30)
                .website("https://github.com/labj1987/green-light")
                .issue_url("https://github.com/labj1987/green-light/issues")
                .comments(format!(
                    "GTK4 + Rust GUI for installing NVIDIA drivers from official .run files.\n\nGPU: {}\nDriver: {}\nKernel: {}",
                    gpu, driver, kernel
                ))
                .build();
            dialog.add_credit_section(Some("Built with"), &["Claude Code (Anthropic)", "Codex (OpenAI)"]);
            dialog.present(Some(&window));
        });
    }

    // Clear log
    {
        let lv = log_view.clone();
        clear_btn.connect_clicked(move |_| {
            lv.buffer().set_text("");
        });
    }

    // Closing the window doesn't stop a running install (it carries on, or
    // puts the previous driver back, on its own), but the user must not take
    // the closed window as "done" and reboot.
    {
        let state = state.clone();
        let allow_close = Rc::new(std::cell::Cell::new(false));
        window.connect_close_request(move |w| {
            let busy = state.borrow().busy;
            if allow_close.get() || !matches!(busy, Some(Busy::Install) | Some(Busy::Signing)) {
                return glib::Propagation::Proceed;
            }
            let dialog = libadwaita::AlertDialog::new(
                Some("The install is still running"),
                Some(
                    "Closing Green Light doesn't stop it: the install finishes, or puts the \
                     previous driver back, on its own. Its progress is in \
                     /var/log/green-light.log. Don't reboot until that log says it is done.",
                ),
            );
            dialog.add_responses(&[("keep", "Keep Open"), ("close", "Close Anyway")]);
            dialog.set_response_appearance("close", libadwaita::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("keep"));
            dialog.set_close_response("keep");
            let allow_close = allow_close.clone();
            let window = w.clone();
            dialog.choose(Some(w), gio::Cancellable::NONE, move |response| {
                if response == "close" {
                    allow_close.set(true);
                    window.close();
                }
            });
            glib::Propagation::Stop
        });
    }

    // Restore Previous Driver: roll back a cut-off install from its journal.
    {
        let state = state.clone();
        let refresh_actions = refresh_actions.clone();
        let log_fn = log_fn.clone();
        let stack = stack.clone();
        let progress = progress.clone();
        let toast_overlay = toast_overlay.clone();
        let load_sysinfo = load_sysinfo.clone();
        let window = window.clone();
        let recover_discards = recover_discards.clone();
        let banner = recover_banner.clone();
        let dialog_window = window.clone();
        // Restore (false) or discard (true) the recovery data.
        let run_op: Rc<dyn Fn(bool)> = Rc::new(move |discard: bool| {
            let Some(guard) = BusyGuard::start(&state, &refresh_actions, Busy::Install) else {
                return;
            };
            banner.set_revealed(false);
            stack.set_visible_child_name("log");
            progress.set_visible(true);
            if discard {
                progress.set_text(Some("Discarding the recovery data…"));
                log_fn("Discarding the recovery data of the interrupted install, as chosen…".to_string());
            } else {
                progress.set_text(Some("Restoring the previous driver…"));
                log_fn("Restoring the driver setup from before the interrupted install…".to_string());
            }
            let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            crate::runtime().spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    let send = |line: &str| {
                        let _ = line_tx.send(line.to_string());
                    };
                    if discard {
                        run_privileged_discard_recovery(send).map(|()| Recovered::Nothing)
                    } else {
                        run_privileged_recover(send)
                    }
                })
                .await;
                let _ = result_tx.send(result);
            });
            let log_fn = log_fn.clone();
            let progress = progress.clone();
            let toast_overlay = toast_overlay.clone();
            let load_sysinfo = load_sysinfo.clone();
            let window = window.clone();
            glib::MainContext::default().spawn_local(async move {
                while let Some(line) = line_rx.recv().await {
                    log_fn(line);
                }
                let result = match result_rx.await {
                    Ok(Ok(r)) => r.map_err(|e| format!("{e:#}")),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err("the restore task ended unexpectedly".to_string()),
                };
                guard.release();
                progress.set_visible(false);
                match result {
                    Ok(Recovered::Restored) => {
                        log_fn("The previous driver setup is back and checked.".to_string());
                        toast_overlay.add_toast(Toast::new("Previous driver restored"));
                    }
                    Ok(Recovered::Nothing) => {
                        log_fn("Nothing needed putting back: that install had finished.".to_string());
                        toast_overlay.add_toast(Toast::new("Nothing needed restoring"));
                    }
                    Err(e) => {
                        log_fn(format!("Restoring failed: {e}"));
                        show_error(&window, "Restoring the previous driver failed", &e);
                    }
                }
                load_sysinfo();
            });
        });
        // Data that no longer matched the computer when last checked: the
        // user picks between trying again (it is checked again, and restored
        // if the reason has gone) and discarding it.
        recover_banner.connect_button_clicked(move |_| {
            if !recover_discards.get() {
                run_op(false);
                return;
            }
            let dialog = libadwaita::AlertDialog::new(
                Some("Recovery data from an interrupted install"),
                Some(
                    "When it was last checked, this data no longer matched the computer. Retry \
                     Recovery checks again and restores the previous driver if it matches now. \
                     Discard Recovery Data drops it for good, keeping the driver as it is.",
                ),
            );
            dialog.add_responses(&[("cancel", "Cancel"), ("discard", "Discard Recovery Data"), ("retry", "Retry Recovery")]);
            dialog.set_response_appearance("discard", libadwaita::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("retry"));
            dialog.set_close_response("cancel");
            let run_op = run_op.clone();
            dialog.choose(Some(&dialog_window), gio::Cancellable::NONE, move |response| match response.as_str() {
                "retry" => run_op(false),
                "discard" => run_op(true),
                _ => {}
            });
        });
    }

    // An install started before this window opened (the app was closed
    // during it) is still running: say so and start nothing until it ends.
    {
        let state = state.clone();
        let refresh_actions = refresh_actions.clone();
        let running_banner = running_banner.clone();
        let recover_banner = recover_banner.clone();
        let recover_discards = recover_discards.clone();
        let load_sysinfo = load_sysinfo.clone();
        let check = move || {
            if state.borrow().busy.is_some() {
                return glib::ControlFlow::Continue;
            }
            if recovery_pending() {
                let (text, button, discards) = match recovery_note() {
                    Some(RecoveryNote::Refused(why)) => (
                        format!(
                            "An earlier driver install was interrupted, but its recovery data no \
                             longer matches this computer: {why}. Nothing was restored."
                        ),
                        "Retry or Discard…",
                        true,
                    ),
                    Some(RecoveryNote::Failed(why)) => (format!("Restoring the driver from before an interrupted install failed: {why}."), "Try Again", false),
                    None => (RECOVER_TEXT.to_string(), "Restore Previous Driver", false),
                };
                recover_banner.set_title(&text);
                recover_banner.set_button_label(Some(button));
                recover_discards.set(discards);
                recover_banner.set_revealed(true);
            } else {
                recover_banner.set_revealed(false);
            }
            let running = install_running();
            if running != state.borrow().external_install {
                state.borrow_mut().external_install = running;
                running_banner.set_revealed(running);
                refresh_actions();
                if !running {
                    load_sysinfo();
                }
            }
            glib::ControlFlow::Continue
        };
        check();
        glib::timeout_add_seconds_local(2, check);
    }

    window.present();
}

const RECOVER_TEXT: &str = "An earlier driver install was cut off before it finished. Restore \
     the driver this computer had before (this also happens on its own at the next boot).";

const BLOCK_SUBTITLE: &str = "Keep apt or dnf from installing the distribution's NVIDIA driver over this one later";

/// A message the user has to see, in a dialog over the window.
fn show_error(window: &ApplicationWindow, heading: &str, body: &str) {
    let dialog = libadwaita::AlertDialog::new(Some(heading), Some(body));
    dialog.add_responses(&[("close", "Close")]);
    dialog.set_close_response("close");
    dialog.present(Some(window));
}

// ─────────────────────────────────────────────────────────────────────────────
//  After a download: checksum verification, or a decision without one
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct DownloadUi {
    state: Rc<RefCell<AppState>>,
    progress: ProgressBar,
    cancel_btn: Button,
    log: Rc<dyn Fn(String)>,
    update_config_tab: Rc<dyn Fn()>,
    refresh_actions: Rc<dyn Fn()>,
    stack: gtk4::Stack,
    toast: ToastOverlay,
    window: ApplicationWindow,
    /// The running download; shared by every step of it (verification,
    /// questions), so it ends when the last of them is done.
    guard: Option<Rc<BusyGuard>>,
}

impl DownloadUi {
    /// The download is over, whichever way it went.
    fn end(&self) {
        if let Some(guard) = &self.guard {
            guard.release();
        }
    }

    /// Make the file the one to install.
    fn accept(&self, path: &Path, checksum: Option<String>, progress_text: &str, toast: &str) {
        self.progress.set_text(Some(progress_text));
        {
            let mut s = self.state.borrow_mut();
            s.downloaded_path = Some(path.to_string_lossy().to_string());
            s.expected_checksum = checksum;
        }
        self.end();
        (self.update_config_tab)();
        self.stack.set_visible_child_name("configure");
        self.toast.add_toast(Toast::new(toast));
    }

    fn discard(&self, path: &Path, progress_text: &str, toast: &str) {
        match std::fs::remove_file(path) {
            Ok(()) => {
                (self.log)(format!("Deleted {}", path.display()));
                self.progress.set_text(Some(progress_text));
            }
            Err(e) => {
                (self.log)(format!("Could not delete {}: {}", path.display(), e));
                self.progress.set_text(Some("Could not delete the download — remove it by hand"));
            }
        }
        self.end();
        self.toast.add_toast(Toast::new(toast));
    }
}

/// Verify a finished download against NVIDIA's checksum. With none published
/// (`Ok(None)`) the user decides whether to use the file unverified; when
/// the checksum couldn't be fetched (`Err`), the choice is to retry or to
/// discard the file — never to skip the check.
fn finish_download(ui: DownloadUi, ver: DriverVersion, path: PathBuf, checksum: Result<Option<String>, String>) {
    match checksum {
        Ok(Some(cs)) => {
            (ui.log)("Verifying SHA256…".to_string());
            ui.progress.set_text(Some("Verifying…"));
            let file = path.clone();
            let expected = cs.clone();
            spawn_async(async move { verify_sha256(&file, &expected).await }, move |verified| match verified {
                Ok(()) => {
                    (ui.log)("SHA256 verified OK".to_string());
                    ui.accept(&path, Some(cs), "Verified", "Download complete");
                }
                Err(e) => {
                    (ui.log)(format!("SHA256 FAILED: {}", e));
                    ui.discard(&path, "Checksum mismatch — file deleted", "Checksum mismatch — do not install");
                }
            });
        }
        Ok(None) => {
            (ui.log)("NVIDIA publishes no SHA256 checksum for this version".to_string());
            ui.progress.set_text(Some("Downloaded — not verified"));
            let dialog = libadwaita::AlertDialog::new(
                Some("Install without verification?"),
                Some(
                    "NVIDIA does not publish a SHA256 checksum for this version, so the download \
                     cannot be checked against it. Only continue if you trust the network you \
                     downloaded it over.",
                ),
            );
            dialog.add_responses(&[("cancel", "Discard Download"), ("use", "Use Unverified")]);
            dialog.set_response_appearance("use", libadwaita::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            let window = ui.window.clone();
            dialog.choose(Some(&window), gio::Cancellable::NONE, move |response| {
                if response == "use" {
                    (ui.log)("User chose to continue WITHOUT checksum verification".to_string());
                    ui.accept(&path, None, "Complete (unverified)", "Download complete (unverified)");
                } else {
                    ui.discard(&path, "Discarded (not verified)", "Unverified download discarded");
                }
            });
        }
        Err(e) => {
            (ui.log)(format!("Could not fetch the SHA256 checksum: {}", e));
            ui.progress.set_text(Some("Downloaded — checksum not fetched yet"));
            let dialog = libadwaita::AlertDialog::new(
                Some("Couldn't get NVIDIA's checksum"),
                Some(&format!(
                    "The driver downloaded, but its SHA256 checksum could not be fetched to \
                     check it:\n\n{e}\n\nTry again once the connection to NVIDIA works."
                )),
            );
            dialog.add_responses(&[("discard", "Discard Download"), ("retry", "Try Again")]);
            dialog.set_response_appearance("retry", libadwaita::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("retry"));
            dialog.set_close_response("discard");
            let window = ui.window.clone();
            dialog.choose(Some(&window), gio::Cancellable::NONE, move |response| {
                if response != "retry" {
                    ui.discard(&path, "Discarded (not verified)", "Download discarded");
                    return;
                }
                (ui.log)("Fetching the SHA256 checksum again…".to_string());
                ui.progress.set_text(Some("Fetching checksum…"));
                let v = ver.clone();
                spawn_async(async move { fetch_checksum(&v).await.map_err(|e| format!("{e:#}")) }, move |checksum| finish_download(ui, ver, path, checksum));
            });
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Populate version list with version-status badges
// ─────────────────────────────────────────────────────────────────────────────

fn populate_list(list_box: &ListBox, versions: &[DriverVersion], filter: &str, installed: Option<&str>) {
    while let Some(c) = list_box.first_child() {
        list_box.remove(&c);
    }
    let filter = filter.to_lowercase();

    for ver in versions {
        if !filter.is_empty() && !ver.version.contains(&filter) {
            continue;
        }

        let subtitle = if let Some(inst) = installed {
            match compare_versions(inst, &ver.version) {
                VersionRelation::Newer => format!("{} · Upgrade available", ver.filename),
                VersionRelation::Same => format!("{} · Currently installed", ver.filename),
                VersionRelation::Older => format!("{} · Older than installed", ver.filename),
                VersionRelation::Unknown => ver.filename.clone(),
            }
        } else {
            ver.filename.clone()
        };

        let row = ActionRow::builder().title(&ver.version).subtitle(&subtitle).activatable(true).build();

        // Highlight the currently installed version
        if installed.map(|i| i == ver.version).unwrap_or(false) {
            row.add_css_class("success");
        }

        if let Some(branch) = ver.branch {
            add_branch_badge(&row, branch);
        }

        list_box.append(&row);
    }
}

/// How long the branch labels may take before the list goes without them.
const BRANCH_LABEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Branch badge (Production/New Feature/Older Production/Legacy), from
/// NVIDIA's own data; omitted rather than guessed without it.
fn add_branch_badge(row: &ActionRow, branch: DriverBranch) {
    let badge = Label::new(Some(branch.label()));
    badge.add_css_class("caption");
    badge.add_css_class("pill");
    badge.add_css_class(branch.css_class());
    badge.set_valign(Align::Center);
    row.add_suffix(&badge);
    row.set_widget_name("badged");
}

/// Badges for rows already shown, once the labels have arrived; the list
/// isn't rebuilt, so the selection stays.
fn add_branch_badges(list_box: &ListBox, versions: &[DriverVersion]) {
    let mut child = list_box.first_child();
    while let Some(c) = child {
        child = c.next_sibling();
        let Ok(row) = c.downcast::<ActionRow>() else { continue };
        if row.widget_name() == "badged" {
            continue;
        }
        let title = row.title();
        if let Some(branch) = versions.iter().find(|v| v.version == title).and_then(|v| v.branch) {
            add_branch_badge(&row, branch);
        }
    }
}

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
        let state = Rc::new(RefCell::new(AppState::default()));
        let refreshed = Rc::new(std::cell::Cell::new(0));
        let r = refreshed.clone();
        let refresh: Rc<dyn Fn()> = Rc::new(move || r.set(r.get() + 1));
        let first = BusyGuard::start(&state, &refresh, Busy::Download).unwrap();
        assert!(BusyGuard::start(&state, &refresh, Busy::Install).is_none());
        // Dropped without running its callback, as after a panic: ends it.
        drop(first);
        assert_eq!(state.borrow().busy, None);
        let second = BusyGuard::start(&state, &refresh, Busy::Install).unwrap();
        // A stale guard of the finished operation must not end the new one.
        let stale = BusyGuard { state: state.clone(), refresh: refresh.clone(), token: 1 };
        drop(stale);
        assert_eq!(state.borrow().busy, Some(Busy::Install));
        second.release();
        assert_eq!(state.borrow().busy, None);
        drop(second);
        assert!(refreshed.get() >= 3);
    }

    #[test]
    fn compare_unknown_on_unparseable() {
        assert!(compare_versions("", "595.84") == VersionRelation::Unknown);
        assert!(compare_versions("595.84", "abc") == VersionRelation::Unknown);
    }
}
