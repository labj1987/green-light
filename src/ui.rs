use crate::download::{download_run_file, verify_sha256};
use crate::install::{run_privileged_install, InstallOptions};
use crate::system::{format_bytes, query_system, SecureBootStatus, SystemInfo, MIN_DISK_BYTES};
use crate::versions::{fetch_checksum, fetch_versions, version_from_filename, DriverVersion};

use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, FileDialog, Label,
    ListBox, Orientation, ProgressBar, ScrolledWindow, SelectionMode,
    Spinner, Stack, Switch, TextView, WrapMode,
};
use libadwaita::prelude::*;
use libadwaita::{
    AboutDialog, ActionRow, Application, ApplicationWindow, Banner, HeaderBar,
    PreferencesGroup, Toast, ToastOverlay,
};

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
//  App state
// ─────────────────────────────────────────────────────────────────────────────

struct AppState {
    versions: Vec<DriverVersion>,
    selected_version: Option<DriverVersion>,
    downloaded_path: Option<String>,
    expected_checksum: Option<String>,
    use_dkms: bool,
    hold_packages: bool,
    sysinfo: SystemInfo,
    download_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            versions: vec![],
            selected_version: None,
            downloaded_path: None,
            expected_checksum: None,
            use_dkms: true,
            hold_packages: false,
            sysinfo: SystemInfo::default(),
            download_cancel: None,
        }
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
enum VersionRelation { Newer, Same, Older, Unknown }

fn compare_versions(installed: &str, candidate: &str) -> VersionRelation {
    let parse = |s: &str| -> Vec<u32> {
        s.split('.').filter_map(|x| x.parse().ok()).collect()
    };
    let iv = parse(installed);
    let cv = parse(candidate);
    if iv.is_empty() || cv.is_empty() { return VersionRelation::Unknown; }
    match cv.cmp(&iv) {
        std::cmp::Ordering::Greater => VersionRelation::Newer,
        std::cmp::Ordering::Equal   => VersionRelation::Same,
        std::cmp::Ordering::Less    => VersionRelation::Older,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  build_ui
// ─────────────────────────────────────────────────────────────────────────────

pub fn build_ui(app: &Application) {
    let state = Rc::new(RefCell::new(AppState::default()));

    // ── Window ───────────────────────────────────────────────────────────────
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Green Light")
        .default_width(760)
        .default_height(640)
        .build();



    let toast_overlay = ToastOverlay::new();

    // ── Header ───────────────────────────────────────────────────────────────
    let header = HeaderBar::new();

    let refresh_btn = Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Refresh version list")
        .build();
    header.pack_start(&refresh_btn);

    let about_btn = Button::builder()
        .icon_name("help-about-symbolic")
        .tooltip_text("About")
        .build();
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
    let kernel_row = ActionRow::builder().title("Kernel").subtitle("Detecting…").build();
    hw_group.add(&gpu_row);
    hw_group.add(&driver_row);
    hw_group.add(&kernel_row);
    sysinfo_page.append(&hw_group);

    let status_group = PreferencesGroup::builder().title("Status").build();
    let dkms_row = ActionRow::builder().title("DKMS Modules").subtitle("Checking…").build();
    let secureboot_row = ActionRow::builder().title("Secure Boot").subtitle("Checking…").build();
    let disk_row = ActionRow::builder().title("Free Disk Space").subtitle("Checking…").build();
    let reboot_row = ActionRow::builder().title("Reboot Required").subtitle("Checking…").build();
    status_group.add(&dkms_row);
    status_group.add(&secureboot_row);
    status_group.add(&disk_row);
    status_group.add(&reboot_row);
    sysinfo_page.append(&status_group);

    let refresh_sysinfo_btn = Button::builder()
        .label("Refresh System Info")
        .halign(Align::Center)
        .margin_top(8)
        .build();
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

    let selected_label = Label::builder()
        .label("No version selected")
        .margin_top(4)
        .margin_bottom(4)
        .build();
    selected_label.add_css_class("dim-label");
    browse_page.append(&selected_label);

    let browse_bar = gtk4::ActionBar::new();
    let open_file_btn = Button::builder()
        .label("Open .run File…")
        .tooltip_text("Use a locally downloaded .run file")
        .build();
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
    let file_row = ActionRow::builder()
        .title("No file selected")
        .subtitle("Go to Browse tab to select or download a driver")
        .build();
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
    warn_group.add(&disk_warn_row);
    warn_group.add(&sb_warn_row);
    config_page.append(&warn_group);

    let opts_group = PreferencesGroup::builder().title("Install Options").build();

    let dkms_opt_row = ActionRow::builder()
        .title("Enable DKMS")
        .subtitle("Automatically rebuild kernel module on kernel updates")
        .build();
    let dkms_switch = Switch::builder().valign(Align::Center).active(true).build();
    dkms_opt_row.add_suffix(&dkms_switch);
    dkms_opt_row.set_activatable_widget(Some(&dkms_switch));

    let hold_opt_row = ActionRow::builder()
        .title("Hold Package Version")
        .subtitle("Pin the driver version with apt-mark hold")
        .build();
    let hold_switch = Switch::builder().valign(Align::Center).build();
    hold_opt_row.add_suffix(&hold_switch);
    hold_opt_row.set_activatable_widget(Some(&hold_switch));

    opts_group.add(&dkms_opt_row);
    opts_group.add(&hold_opt_row);
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

    let install_btn = Button::builder()
        .label("Install Driver")
        .sensitive(false)
        .build();
    install_btn.add_css_class("pill");
    install_btn.add_css_class("suggested-action");

    let cancel_dl_btn = Button::builder()
        .label("Cancel Download")
        .visible(false)
        .build();
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
    let log_view = TextView::builder()
        .editable(false)
        .monospace(true)
        .wrap_mode(WrapMode::Word)
        .vexpand(true)
        .build();
    log_view.add_css_class("card");
    log_view.set_margin_top(8);
    log_view.set_margin_bottom(8);
    log_view.set_margin_start(8);
    log_view.set_margin_end(8);
    let log_scroll = ScrolledWindow::builder().vexpand(true).child(&log_view).build();
    let clear_btn = Button::builder()
        .label("Clear Log")
        .halign(Align::End)
        .margin_end(8)
        .margin_bottom(8)
        .build();
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

    content.append(&stack);
    toast_overlay.set_child(Some(&content));
    window.set_content(Some(&toast_overlay));

    // ─────────────────────────────────────────────────────────────────────────
    //  Helpers
    // ─────────────────────────────────────────────────────────────────────────

    let log_fn = {
        let log_view = log_view.clone();
        move |msg: String| {
            let buf = log_view.buffer();
            let mut end = buf.end_iter();
            buf.insert(&mut end, &format!("{}\n", msg));
            let mark = buf.create_mark(None, &buf.end_iter(), false);
            log_view.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
        }
    };

    let update_config_tab = {
        let file_row = file_row.clone();
        let checksum_row = checksum_row.clone();
        let version_status_row = version_status_row.clone();
        let disk_warn_row = disk_warn_row.clone();
        let sb_warn_row = sb_warn_row.clone();
        let install_btn = install_btn.clone();
        let state = state.clone();
        move || {
            let s = state.borrow();

            // File info
            if let Some(path) = &s.downloaded_path {
                let fname = std::path::Path::new(path)
                    .file_name().and_then(|n| n.to_str()).unwrap_or(path).to_string();
                file_row.set_title(&fname);
                file_row.set_subtitle(path.as_str());
                if let Some(cs) = &s.expected_checksum {
                    checksum_row.set_subtitle(&format!("SHA256: {}…", &cs[..16]));
                } else {
                    checksum_row.set_subtitle("No checksum available");
                }
                // Version comparison
                let parsed = version_from_filename(&fname);
                if parsed.is_none() {
                    // e.g. NVIDIA-Linux-x86_64-595.84-vulkan.run — don't guess.
                    version_status_row.set_subtitle("Unknown");
                } else if let (Some(inst), Some(candidate)) =
                    (s.sysinfo.installed_driver.as_ref(), parsed.as_deref())
                {
                    match compare_versions(inst, candidate) {
                        VersionRelation::Newer => version_status_row.set_subtitle(
                            &format!("Upgrade: {} → {}", inst, candidate)),
                        VersionRelation::Same  => version_status_row.set_subtitle(
                            &format!("Reinstall: same version ({})", inst)),
                        VersionRelation::Older => version_status_row.set_subtitle(
                            &format!("Downgrade: {} → {}", inst, candidate)),
                        VersionRelation::Unknown => version_status_row.set_subtitle("—"),
                    }
                } else {
                    version_status_row.set_subtitle("No driver currently installed");
                }
                install_btn.set_sensitive(true);
            } else {
                file_row.set_title("No file selected");
                file_row.set_subtitle("Go to Browse tab to select or download a driver");
                checksum_row.set_subtitle("—");
                version_status_row.set_subtitle("—");
                install_btn.set_sensitive(false);
            }

            // Disk check
            match s.sysinfo.free_disk_bytes {
                Some(free) if free < MIN_DISK_BYTES => {
                    disk_warn_row.set_subtitle(&format!(
                        "Low: {} free (2 GB recommended)", format_bytes(free)));
                }
                Some(free) => {
                    disk_warn_row.set_subtitle(&format!("{} free", format_bytes(free)));
                }
                None => disk_warn_row.set_subtitle("Could not determine"),
            }

            // Secure boot check
            match &s.sysinfo.secure_boot {
                SecureBootStatus::Enabled => sb_warn_row.set_subtitle(
                    "Enabled — MOK enrollment may be required after install"),
                SecureBootStatus::Disabled => sb_warn_row.set_subtitle("Disabled"),
                SecureBootStatus::Unknown  => sb_warn_row.set_subtitle("Unknown"),
            }
        }
    };

    // ─────────────────────────────────────────────────────────────────────────
    //  Load system info
    // ─────────────────────────────────────────────────────────────────────────

    let load_sysinfo = {
        let gpu_row = gpu_row.clone();
        let driver_row = driver_row.clone();
        let kernel_row = kernel_row.clone();
        let dkms_row = dkms_row.clone();
        let secureboot_row = secureboot_row.clone();
        let disk_row = disk_row.clone();
        let reboot_row = reboot_row.clone();
        let reboot_banner = reboot_banner.clone();
        let driver_badge = driver_badge.clone();
        let state = state.clone();
        let update_config_tab = update_config_tab.clone();
        let log_fn = log_fn.clone();
        let list_box = list_box.clone();
        let search_entry = search_entry.clone();

        move || {
            gpu_row.set_subtitle("Detecting…");
            driver_row.set_subtitle("Detecting…");
            log_fn("Querying system info…".to_string());

            let log_fn = log_fn.clone();
            let gpu_row = gpu_row.clone();
            let driver_row = driver_row.clone();
            let kernel_row = kernel_row.clone();
            let dkms_row = dkms_row.clone();
            let secureboot_row = secureboot_row.clone();
            let disk_row = disk_row.clone();
            let reboot_row = reboot_row.clone();
            let reboot_banner = reboot_banner.clone();
            let driver_badge = driver_badge.clone();
            let state = state.clone();
            let update_config_tab = update_config_tab.clone();
            let list_box = list_box.clone();
            let search_entry = search_entry.clone();

            spawn_async(
                async move { tokio::task::spawn_blocking(query_system).await.unwrap_or_default() },
                move |info| {
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

                    // Kernel
                    kernel_row.set_subtitle(&info.kernel_version);

                    // DKMS
                    if info.dkms_status.is_empty() {
                        dkms_row.set_subtitle("No NVIDIA DKMS modules registered");
                    } else {
                        let summary = info.dkms_status.iter()
                            .map(|e| format!("{}/{} [{}] {}", e.module, e.version, e.kernel, e.status))
                            .collect::<Vec<_>>()
                            .join("\n");
                        dkms_row.set_subtitle(&summary);
                    }

                    // Secure boot
                    secureboot_row.set_subtitle(&info.secure_boot.to_string());

                    // Disk
                    match info.free_disk_bytes {
                        Some(free) => disk_row.set_subtitle(&format!("{} free on /", format_bytes(free))),
                        None       => disk_row.set_subtitle("Could not determine"),
                    }

                    // Reboot
                    if info.reboot_required {
                        reboot_row.set_subtitle("Yes — new driver installed, reboot to activate");
                        reboot_banner.set_revealed(true);
                    } else {
                        reboot_row.set_subtitle("No");
                        reboot_banner.set_revealed(false);
                    }

                    state.borrow_mut().sysinfo = info;
                    // Update configure tab with new disk/secureboot/version info
                    update_config_tab();
                    // Re-populate browse list with version badges now that driver is known
                    let versions = state.borrow().versions.clone();
                    if !versions.is_empty() {
                        let installed = state.borrow().sysinfo.installed_driver.clone();
                        populate_list(&list_box, &versions, &search_entry.text(), installed.as_deref());
                    }
                },
            );
        }
    };

    load_sysinfo();

    { let ls = load_sysinfo.clone(); refresh_sysinfo_btn.connect_clicked(move |_| ls()); }

    // ─────────────────────────────────────────────────────────────────────────
    //  Load versions
    // ─────────────────────────────────────────────────────────────────────────

    let load_versions = {
        let list_box = list_box.clone();
        let list_spinner = list_spinner.clone();
        let download_btn = download_btn.clone();
        let selected_label = selected_label.clone();
        let banner = banner.clone();
        let state = state.clone();
        let log_fn = log_fn.clone();
        let search_entry = search_entry.clone();

        move || {
            list_spinner.start();
            list_spinner.set_visible(true);
            banner.set_revealed(false);
            while let Some(c) = list_box.first_child() { list_box.remove(&c); }
            download_btn.set_sensitive(false);
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
                        state.borrow_mut().versions = versions.clone();
                        let installed = state.borrow().sysinfo.installed_driver.clone();
                        populate_list(&list_box, &versions, &search_entry.text(), installed.as_deref());
                    }
                }
            });
        }
    };

    load_versions();

    { let lv = load_versions.clone(); refresh_btn.connect_clicked(move |_| lv()); }

    // ─────────────────────────────────────────────────────────────────────────
    //  Row selection — connected exactly once. The version is looked up by the
    //  row's title (the version string), never by row index: when the search
    //  filter is active the visible rows are a subset, so an index into the
    //  full versions vec would select the wrong driver.
    // ─────────────────────────────────────────────────────────────────────────
    {
        let state = state.clone();
        let download_btn = download_btn.clone();
        let selected_label = selected_label.clone();
        list_box.connect_row_selected(move |_, row| {
            let selected = row
                .and_then(|r| r.downcast_ref::<ActionRow>().map(|ar| ar.title().to_string()))
                .and_then(|title| {
                    state.borrow().versions.iter().find(|v| v.version == title).cloned()
                });

            match selected {
                Some(ver) => {
                    selected_label.set_label(&format!("Selected: {}", ver.version));
                    state.borrow_mut().selected_version = Some(ver);
                    download_btn.set_sensitive(true);
                }
                None => {
                    download_btn.set_sensitive(false);
                    selected_label.set_label("No version selected");
                    state.borrow_mut().selected_version = None;
                }
            }
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
    {
        let state = state.clone();
        let progress = progress.clone();
        let log_fn = log_fn.clone();
        let update_config_tab = update_config_tab.clone();
        let stack = stack.clone();
        let toast_overlay = toast_overlay.clone();
        let cancel_dl_btn = cancel_dl_btn.clone();
        let window = window.clone();

        download_btn.connect_clicked(move |btn| {
            let ver = { state.borrow().selected_version.clone() };
            let Some(ver) = ver else { return };

            // Check disk space before starting
            let free = state.borrow().sysinfo.free_disk_bytes;
            if let Some(f) = free {
                if f < MIN_DISK_BYTES {
                    toast_overlay.add_toast(Toast::new(
                        "Low disk space — install may fail"));
                }
            }

            btn.set_sensitive(false);
            cancel_dl_btn.set_visible(true);
            progress.set_visible(true);
            progress.set_fraction(0.0);
            progress.set_text(Some(&format!("Downloading {}…", ver.version)));

            // Cancel token
            let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            state.borrow_mut().download_cancel = Some(cancel_flag.clone());

            // XDG_DOWNLOAD_DIR normally lives only in ~/.config/user-dirs.dirs,
            // not the environment — ask GLib, which reads that file.
            let dest_dir = glib::user_special_dir(glib::UserDirectory::Downloads)
                .unwrap_or_else(|| {
                    std::path::PathBuf::from(
                        std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
                    )
                    .join("Downloads")
                });

            // Progress channel — sent from the Tokio side, drained on the GTK
            // main context. The loop exits when the sender drops at download
            // end, so nothing is leaked (the old timer ran forever).
            let (prog_tx, mut prog_rx) =
                tokio::sync::mpsc::unbounded_channel::<(u64, Option<u64>)>();
            let download_start = std::time::Instant::now();

            {
                let progress = progress.clone();
                glib::MainContext::default().spawn_local(async move {
                    let mut last_ui = std::time::Instant::now();
                    while let Some(mut msg) = prog_rx.recv().await {
                        // Coalesce bursts — jump to the newest queued update
                        while let Ok(newer) = prog_rx.try_recv() { msg = newer; }
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
                            } else { "calculating…".to_string() };
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
            let state = state.clone();
            let progress = progress.clone();
            let log_fn = log_fn.clone();
            let update_config_tab = update_config_tab.clone();
            let stack = stack.clone();
            let btn = btn.clone();
            let toast_overlay = toast_overlay.clone();
            let cancel_dl_btn = cancel_dl_btn.clone();
            let cancel_flag2 = cancel_flag.clone();
            let window = window.clone();

            spawn_async(
                async move {
                    // Keep fetch errors distinct from "no checksum published":
                    // neither may silently turn into an unverified install.
                    let (checksum, checksum_err) = match fetch_checksum(&ver).await {
                        Ok(c) => (c, None),
                        Err(e) => (None, Some(format!("{:#}", e))),
                    };
                    let tx = prog_tx;
                    let result = download_run_file(
                        &url, &dest_dir, &filename,
                        move |dl, total| { let _ = tx.send((dl, total)); },
                        cancel_flag2,
                    ).await;
                    (checksum, checksum_err, result)
                },
                move |(checksum, checksum_err, result)| {
                    btn.set_sensitive(true);
                    cancel_dl_btn.set_visible(false);
                    state.borrow_mut().download_cancel = None;

                    match result {
                        Err(e) => {
                            let msg = e.to_string();
                            if msg.contains("cancelled") {
                                log_fn("Download cancelled.".to_string());
                                progress.set_text(Some("Cancelled"));
                            } else {
                                log_fn(format!("Download failed: {}", e));
                                progress.set_text(Some("Download failed"));
                                toast_overlay.add_toast(Toast::new("Download failed"));
                            }
                        }
                        Ok(path) => {
                            log_fn(format!("Download complete: {}", path.display()));
                            progress.set_fraction(1.0);

                            if let Some(ref cs) = checksum {
                                log_fn("Verifying SHA256…".to_string());
                                progress.set_text(Some("Verifying…"));
                                let cs_clone = cs.clone();
                                let path_clone = path.clone();
                                let log_fn2 = log_fn.clone();
                                let progress2 = progress.clone();
                                let toast2 = toast_overlay.clone();
                                let state2 = state.clone();
                                let cs2 = checksum.clone();
                                let update2 = update_config_tab.clone();
                                let stack2 = stack.clone();
                                spawn_async(
                                    async move { verify_sha256(&path_clone, &cs_clone).await },
                                    move |verify_result| match verify_result {
                                        Ok(()) => {
                                            log_fn2("SHA256 verified OK".to_string());
                                            progress2.set_text(Some("Verified"));
                                            let path_str = path.to_string_lossy().to_string();
                                            { let mut s = state2.borrow_mut(); s.downloaded_path = Some(path_str); s.expected_checksum = cs2; }
                                            update2();
                                            stack2.set_visible_child_name("configure");
                                            toast2.add_toast(Toast::new("Download complete"));
                                        }
                                        Err(e) => {
                                            log_fn2(format!("SHA256 FAILED: {}", e));
                                            match std::fs::remove_file(&path) {
                                                Ok(()) => {
                                                    log_fn2(format!("Deleted corrupt file: {}", path.display()));
                                                    progress2.set_text(Some("Checksum mismatch — file deleted"));
                                                }
                                                Err(del_err) => {
                                                    log_fn2(format!("Could not delete file: {}", del_err));
                                                    progress2.set_text(Some("Checksum mismatch — delete the file manually"));
                                                }
                                            }
                                            toast2.add_toast(Toast::new("Checksum mismatch — do not install"));
                                        }
                                    },
                                );
                            } else {
                                let reason = match &checksum_err {
                                    Some(e) => {
                                        log_fn(format!("Could not fetch the SHA256 checksum: {}", e));
                                        format!("The SHA256 checksum could not be fetched ({}).", e)
                                    }
                                    None => {
                                        log_fn("NVIDIA publishes no SHA256 checksum for this version".to_string());
                                        "NVIDIA does not publish a SHA256 checksum for this version.".to_string()
                                    }
                                };
                                progress.set_text(Some("Downloaded — not verified"));
                                let dialog = libadwaita::AlertDialog::new(
                                    Some("Install without verification?"),
                                    Some(&format!(
                                        "{}\n\nThe download cannot be verified against NVIDIA's published hash. \
                                         Only continue if you trust the network you downloaded it over.",
                                        reason
                                    )),
                                );
                                dialog.add_responses(&[("cancel", "Discard Download"), ("use", "Use Unverified")]);
                                dialog.set_response_appearance("use", libadwaita::ResponseAppearance::Destructive);
                                dialog.set_default_response(Some("cancel"));
                                dialog.set_close_response("cancel");
                                let state = state.clone();
                                let log_fn = log_fn.clone();
                                let progress = progress.clone();
                                let update_config_tab = update_config_tab.clone();
                                let stack = stack.clone();
                                let toast_overlay = toast_overlay.clone();
                                dialog.choose(Some(&window), gio::Cancellable::NONE, move |response| {
                                    if response == "use" {
                                        log_fn("User chose to continue WITHOUT checksum verification".to_string());
                                        progress.set_text(Some("Complete (unverified)"));
                                        let path_str = path.to_string_lossy().to_string();
                                        { let mut s = state.borrow_mut(); s.downloaded_path = Some(path_str); s.expected_checksum = None; }
                                        update_config_tab();
                                        stack.set_visible_child_name("configure");
                                        toast_overlay.add_toast(Toast::new("Download complete (unverified)"));
                                    } else {
                                        let _ = std::fs::remove_file(&path);
                                        log_fn(format!("Discarded unverified download: {}", path.display()));
                                        progress.set_text(Some("Discarded (not verified)"));
                                        toast_overlay.add_toast(Toast::new("Unverified download discarded"));
                                    }
                                });
                            }
                        }
                    }
                },
            );
        });
    }

    // Cancel download button
    {
        let state = state.clone();
        let cancel_dl_btn = cancel_dl_btn.clone();
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

            dialog.open(Some(&window), gio::Cancellable::NONE, move |result| {
                if let Ok(file) = result {
                    if let Some(path) = file.path() {
                        let path_str = path.to_string_lossy().to_string();
                        log_fn(format!("Opened local file: {}", path_str));
                        { let mut s = state.borrow_mut(); s.downloaded_path = Some(path_str); s.expected_checksum = None; }
                        update_config_tab();
                        stack.set_visible_child_name("configure");
                    }
                }
            });
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Switches
    // ─────────────────────────────────────────────────────────────────────────
    { let s = state.clone(); dkms_switch.connect_active_notify(move |sw| { s.borrow_mut().use_dkms = sw.is_active(); }); }
    { let s = state.clone(); hold_switch.connect_active_notify(move |sw| { s.borrow_mut().hold_packages = sw.is_active(); }); }

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

        install_btn.connect_clicked(move |btn| {
            let s = state.borrow();
            let Some(ref run_file) = s.downloaded_path else { return };

            if let Some(free) = s.sysinfo.free_disk_bytes {
                if free < MIN_DISK_BYTES {
                    toast_overlay.add_toast(Toast::new(
                        "Warning: low disk space — install may fail"));
                }
            }

            let opts = InstallOptions {
                use_dkms: s.use_dkms,
                hold_packages: s.hold_packages,
                run_file: run_file.clone(),
                sha256: s.expected_checksum.clone(),
            };
            drop(s);

            btn.set_sensitive(false);
            progress.set_visible(true);
            progress.set_text(Some("Installing… the desktop stays up; this takes a few minutes"));
            stack.set_visible_child_name("log");
            log_fn(format!("Starting install: {}", opts.run_file));
            log_fn("The new driver installs alongside the running one; reboot afterward to switch over.".to_string());

            // Pulse the bar while the install runs; the timer stops itself
            // once the completion callback re-enables the button.
            {
                let progress = progress.clone();
                let btn = btn.clone();
                glib::timeout_add_local(std::time::Duration::from_millis(150), move || {
                    if btn.is_sensitive() {
                        glib::ControlFlow::Break
                    } else {
                        progress.pulse();
                        glib::ControlFlow::Continue
                    }
                });
            }

            let log_fn = log_fn.clone();
            let toast_overlay = toast_overlay.clone();
            let progress = progress.clone();
            let btn = btn.clone();
            let load_sysinfo = load_sysinfo.clone();

            spawn_async(
                async move {
                    tokio::task::spawn_blocking(move || run_privileged_install(&opts)).await
                },
                move |result| {
                    btn.set_sensitive(true);
                    progress.set_visible(false);
                    match result {
                        Ok(Ok(())) => {
                            log_fn("Install completed successfully.".to_string());
                            log_fn("Reboot to switch to the new driver.".to_string());
                            toast_overlay.add_toast(Toast::new("Install complete — reboot to activate"));
                            load_sysinfo();
                        }
                        Ok(Err(e)) => {
                            log_fn(format!("Install failed: {}", e));
                            toast_overlay.add_toast(Toast::new("Install failed — see Log tab"));
                        }
                        Err(e) => {
                            log_fn(format!("Task error: {}", e));
                        }
                    }
                },
            );
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
                .website("https://github.com/labj1987/GreenLight")
                .issue_url("https://github.com/labj1987/GreenLight/issues")
                .comments(format!(
                    "GTK4 + Rust GUI for installing NVIDIA drivers from official .run files.\n\nGPU: {}\nDriver: {}\nKernel: {}",
                    gpu, driver, kernel
                ))
                .build();
            dialog.add_acknowledgement_section(Some("Built with"), &["Claude Code (Anthropic)"]);
            dialog.present(Some(&window));
        });
    }

    // Clear log
    { let lv = log_view.clone(); clear_btn.connect_clicked(move |_| { lv.buffer().set_text(""); }); }

    window.present();
}

// ─────────────────────────────────────────────────────────────────────────────
//  Populate version list with version-status badges
// ─────────────────────────────────────────────────────────────────────────────

fn populate_list(
    list_box: &ListBox,
    versions: &[DriverVersion],
    filter: &str,
    installed: Option<&str>,
) {
    while let Some(c) = list_box.first_child() { list_box.remove(&c); }
    let filter = filter.to_lowercase();

    for ver in versions {
        if !filter.is_empty() && !ver.version.contains(&filter) { continue; }

        let subtitle = if let Some(inst) = installed {
            match compare_versions(inst, &ver.version) {
                VersionRelation::Newer  => format!("{} · Upgrade available", ver.filename),
                VersionRelation::Same   => format!("{} · Currently installed", ver.filename),
                VersionRelation::Older  => format!("{} · Older than installed", ver.filename),
                VersionRelation::Unknown => ver.filename.clone(),
            }
        } else {
            ver.filename.clone()
        };

        let row = ActionRow::builder()
            .title(&ver.version)
            .subtitle(&subtitle)
            .activatable(true)
            .build();

        // Highlight the currently installed version
        if installed.map(|i| i == ver.version).unwrap_or(false) {
            row.add_css_class("success");
        }

        // Branch badge (Production/New Feature/LTS/Legacy) — omitted rather
        // than guessed when the version doesn't match a known branch range.
        if let Some(branch) = ver.branch {
            let badge = Label::new(Some(branch.label()));
            badge.add_css_class("caption");
            badge.add_css_class("pill");
            badge.add_css_class(branch.css_class());
            badge.set_valign(Align::Center);
            row.add_suffix(&badge);
        }

        list_box.append(&row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn compare_unknown_on_unparseable() {
        assert!(compare_versions("", "595.84") == VersionRelation::Unknown);
        assert!(compare_versions("595.84", "abc") == VersionRelation::Unknown);
    }
}
