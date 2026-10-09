# Changelog

One `## <version> — <date>` heading per released version, newest first. A release's page
carries its section as written, and the AppStream `<releases>` list is generated from these
headings. Versions before 2.3.0 predate this repository and have no dates; they are kept
under 2.3.0.

## 2.9.0 — 2026-10-08

**Driver signing for Secure Boot.** Green Light can sign the NVIDIA driver it installs with
your machine's module-signing key, the same key DKMS uses, so the driver can be used with
Secure Boot on.

- **Set Up Signing** on the System tab does the one-time setup. It creates a signing key if
  the machine has none and queues it for enrollment. You choose a one-time password and
  type it once at the blue MOK Manager screen on the next reboot.
- After that, every driver install is signed. A driver installed before setup stays
  unsigned until you reinstall it once; the System tab says when that's needed.
- The System tab's new Module signing row shows whether there is a key, whether it is
  enrolled or waiting for the reboot, and who signed the installed driver.
- If you never set up signing, installs work as before.

## 2.8.1 — 2026-10-02

- The About dialog and README credit Codex (OpenAI) again, alongside Claude Code (Anthropic).

## 2.8.0 — 2026-10-02

Kernel module flavor
- New "Kernel Module" option on the Configure tab: Automatic (default, the
  installer picks from your GPUs), Open, or Proprietary, passed to NVIDIA's
  installer as `--kernel-module-type`. Choices that cannot work are refused
  before the password prompt: open modules on a pre-Turing GPU or a driver
  older than 515, proprietary modules on Blackwell and newer.
- The System tab shows which flavor is loaded and your GPU generation.
- Fixed the driver version fallback misreading `/proc/driver/nvidia/version`
  on the open kernel module.

Pre-install checks
- Lists the distro driver packages (apt or rpm, including Pop!_OS
  `system76-driver-nvidia*`) that the install will remove.
- With Secure Boot on, checks whether a DKMS signing key is enrolled and shows
  the exact `mokutil --import` steps when it is not.

Post-install verification
- The install script now checks the nouveau blacklist, the rebuilt initramfs,
  the DKMS build for the running kernel, and the module signature under Secure
  Boot. Results appear in the Log tab and `/var/log/green-light.log`. These
  checks only report; they never fail an install.
- When the installer or the DKMS build fails, the tail of the DKMS `make.log`
  is added to the log.

Other
- The About dialog credits Claude (Anthropic) for help building the app.

## 2.7.5 — 2026-09-21

Naming
- Display name is "Green Light"; the binary, crate, repo (now
  `labj1987/green-light`), AppImage (`green-light-VERSION-x86_64.AppImage`),
  `.desktop`/icon files, helper (`green-light-setup`) and installed paths
  (`/usr/lib/green-light/`, `/var/log/green-light.log`) are hyphenated
  lowercase. The application ID and polkit action ids are unchanged.
- Upgrading from an older install: the first launch asks for your password
  once to install the helper at its new path. The old `/usr/lib/greenlight/`
  directory is left behind and can be deleted by hand. Older AppImages'
  built-in delta update looks for `greenlight-*` assets, so download this
  release manually once.

## 2.7.4 — 2026-09-21

Security
- The privileged install script now copies the `.run` file into a root-only
  directory and verifies a caller-supplied SHA256 against that copy before
  running anything, and refuses symlinks and files owned by other users or
  writable by group/others (closes a swap-the-file-for-root-exec window).
- Polkit `allow_active` is `auth_admin` instead of `auth_admin_keep`, so the
  authorization is no longer cached for other processes.
- A checksum fetch failure (or a version with no published checksum) no
  longer silently skips verification; you must explicitly choose to use the
  unverified download, and discarding it is the default.
- Fixed a crash (RefCell double borrow) when typing in the search box with a
  version selected.

Robustness
- Install script runs under `set -euo pipefail`; modprobe.d and initramfs
  failures now fail the install instead of reporting success.
- Distro-package cleanup only purges driver packages (no more `libcuda*` /
  `libcudnn*`) and no longer falls back to `dpkg --purge --force-all`.
- Truncated downloads are detected and retried; HTTP 4xx is no longer
  retried.
- Local `.run` files with extra suffixes (e.g. `-vulkan`) show "Unknown"
  instead of being mislabeled; the Downloads folder comes from
  `glib::user_special_dir`.
- Build: `apt-get update` runs before installing zsync, `appimagetool` is
  pinned to 1.9.1 with a SHA256 check, and AppRun installs system components
  through a dedicated `greenlight-setup` helper with its own polkit action
  and checked exit status.

Other
- Added CI (build, test, clippy, shellcheck) on every push/PR; appdata
  release list completed and auto-extended from `Cargo.toml`; removed unused
  `serde`/`serde_json`; added unit tests.
- Display name is now "Green Light"; credits use "Linnard Alex Brown Jr.";
  the About dialog shows the AGPL license, website and issue tracker.

## 2.7.3 — 2026-09-17

- Fixes the distro-package cleanup step purging `nvidia-container-toolkit`
  and `libnvidia-container*` — these match the `nvidia-*`/`libnvidia-*`
  purge glob but are Docker's GPU-passthrough plumbing, not the display
  driver. Removing them doesn't touch the running driver but breaks every
  GPU container the moment its runtime next restarts (confirmed: took down
  a running Frigate NVR container this way). They're now excluded.
- Fixes the same cleanup step missing `xserver-xorg-video-nvidia-<ver>`,
  which doesn't match the `nvidia-*`/`libnvidia-*` glob but is a
  reverse-dependency of `nvidia-support-<ver>`. Leaving it installed made
  dpkg silently refuse to remove `nvidia-support-<ver>` (every removal
  command was `|| true`), leaving its
  `/usr/lib/nvidia/alternate-install-present` marker file in place — which
  makes the `.run` installer itself abort with "please use the Debian
  packages instead," on a machine that's mid-purge of those exact
  packages. Now included in the purge, and the marker is removed
  explicitly afterward regardless of whether the purge reported success,
  and a failed `apt-get purge` now retries once with
  `dpkg --purge --force-all` instead of silently giving up.

## 2.7.2 — 2026-09-09

- Credits Claude Code (Anthropic) in the About dialog's acknowledgements.

## 2.7.1 — 2026-08-12

- Fixes the 2.7.0 release build, which failed in CI: its gtk4/libadwaita
  feature flags (`v4_22`/`v1_9`) required a newer system GTK4/libadwaita
  than the GitHub Actions runner's `libgtk-4-dev` provides (4.14.5).
  Dialed the feature flags back to `v4_12`/`v1_4,v1_5` — nothing in this
  release actually needs the newer APIs.

## 2.7.0 — 2026-08-12

- Browse list now labels each driver version's release branch (Production,
  New Feature, Long Term Support, or Legacy) with a small tinted badge,
  classified from the version number against NVIDIA's current branch
  ranges. A version that doesn't match a known branch is left unlabeled
  rather than guessed at.
- Dependency bump: gtk4 0.11, libadwaita 0.9, glib/gio 0.22, reqwest 0.13,
  scraper 0.27 — matching the GTK4/libadwaita versions actually shipping
  on Ubuntu 26.04.
- CI: the release workflow no longer sets `GITHUB_TOKEN` explicitly (picked
  up automatically via the `contents: write` permission) and now generates
  release notes automatically.
- The `screenshots/` directory is now wired into the AppStream metadata.

## 2.6.1 — 2026-08-05

- Fixes the AppImage's self-update pointer, which still referenced the
  pre-rename `NVI` repo — it now points at `GreenLight`, matching where
  the 2.6.0 release actually lives.

## 2.6.0 — 2026-08-05

**Rebrand to GreenLight, new icon set.** NVI is now GreenLight. Full rename — crate/package name, application ID
(`io.github.labj1987.GreenLight`), prgname, window title, About dialog,
desktop file, appdata, polkit policy, install-script/log paths
(`/usr/lib/greenlight/`, `/var/log/greenlight.log`), and the HTTP user
agent. Pure rebrand, no behavior change.

- Replaces the icon set with the approved green circuit-board chip
  design, rendered natively at 16/32/48/64/128/256/512px (not just
  downscaled from one size) and verified legible at the small sizes
  used in the GNOME dock/app grid.
- Corrects `CLAUDE.md`'s build-process section, which described
  `build-appimage.sh` as using `linuxdeploy` — that stopped being true
  as of 2.5.10's switch to bare `appimagetool`; the doc just never
  caught up.
- Fixes `.gitignore`: it excluded `/AppDir/`, but `build-appimage.sh`
  has always nested the AppDir at `build-appimage/AppDir`, so that
  pattern never matched anything.

The GitHub repo itself (`labj1987/NVI`) is intentionally left unrenamed
for now — `build-appimage.sh`'s `UPDATE_INFORMATION` and the appdata/
policy URLs still point at NVI.

## 2.5.10 — 2026-08-04

**Switch packaging to bare appimagetool (drop bundled GTK).**

- NVI was built with `linuxdeploy` +
  `linuxdeploy-plugin-gtk`, which bundles its own copy of GTK4/libadwaita
  into the AppImage from whatever the CI runner's apt repo offers — Ubuntu
  24.04's `libadwaita-1-0 1.5.0-1ubuntu2`. That's the earliest release to
  support `Adw.Dialog`, and its floating-dialog presentation (used by the
  About dialog since 2.5.8) lacks the border/backdrop-dim styling refined
  in later libadwaita releases, making it look visually flat compared to
  apps that dynamically link the host's libadwaita instead of bundling
  one.
  Switched `build-appimage.sh` to a bare-`appimagetool` approach: the
  binary now links against
  the host's system GTK4/libadwaita at runtime instead of a bundled copy,
  giving it the host's modern dialog styling and running natively on
  Wayland instead of under XWayland as a side effect. AppDir layout,
  AppRun's privileged-install
  staging logic, and the polkit policy/appdata paths are unchanged.

## 2.5.9 — 2026-08-04

**Align prgname/StartupWMClass with the application ID.**

- NVI never exhibited the phantom-taskbar-entry bug that a mismatched
  `app_id` causes on Wayland, because it runs under XWayland (the bundled
  linuxdeploy GTK stack falls back to X11), where WM_CLASS comes from
  `prgname`, which already matched `StartupWMClass`. On Wayland, though,
  GTK4 announces the GApplication ID as the toplevel's `app_id`, not
  `prgname` — so this was latent, not fixed. Set both `prgname` and
  `StartupWMClass` to the application ID (`io.github.labj1987.NVI`) to
  remove the latent risk should
  NVI's packaging ever move off linuxdeploy's bundled GTK.

## 2.5.8 — 2026-08-04

**Fix phantom taskbar window from the About dialog.**

- The About dialog used `gtk4::AboutDialog`, a `Gtk.Window` subclass that
  creates a real separate top-level Wayland surface, showing as a
  second, unnamed window in the dock. Switched to `libadwaita::AboutDialog`
  (`Adw.Dialog` subclass, requires the `v1_5` feature, now enabled),
  which renders as a sheet inside the main window's own surface.

## 2.5.7 — 2026-08-04

**Fix UPDATE_INFORMATION to reference .zsync sidecar.**

- Per the AppImage update spec, the GitHub Releases zsync transport string
  must end in the `.zsync` sidecar filename, not the AppImage filename.
  `UPDATE_INFORMATION` in `build-appimage.sh` ended in
  `-x86_64.AppImage` instead of `-x86_64.AppImage.zsync`, which broke
  update detection in tools like Gear Lever even though the `.zsync`
  sidecar itself was already being generated and published correctly.
  Packaging-only fix, no application behavior changes.

## 2.5.6 — 2026-07-17

**Fix orphaned .zsync sidecar.**

- build-appimage.sh renamed only the built .AppImage to its final
  versioned filename; a same-named .zsync sidecar produced by linuxdeploy
  was left under linuxdeploy's original output filename and was never
  renamed or moved. CI's release glob only matches the versioned
  filename pattern, so the .zsync silently never got uploaded even
  after 2.5.5 fixed zsync not being installed. The .zsync is now
  renamed alongside the AppImage.

## 2.5.5 — 2026-07-17

**Fix missing .zsync file.**

- The build runner never had zsync installed, so linuxdeploy silently
  skipped generating the .zsync file even though UPDATE_INFORMATION was
  already set in 2.5.4 — update-aware tools had nothing to delta-update
  against. zsync is now installed alongside the other build dependencies.

## 2.5.4 — 2026-07-17

**Enable update checking.**

- Embedded UPDATE_INFORMATION in the AppImage so update-aware tools
  (Gear Lever, AppImageUpdate) can check GitHub Releases for newer
  versions and delta-update via zsync. CI now also uploads the .zsync
  file alongside the AppImage.

## 2.5.3 — 2026-07-16

**Bug fixes and version-string consolidation.**

- Fixed wrong version being selected when the search filter is active:
  the selection handler indexed the full version list by visible row
  position, so filtering could select (and download) a different driver
  than the one clicked. Selection now looks up the version by the row's
  title.
- The row-selected handler is now connected once instead of on every
  refresh, so handlers no longer accumulate.
- Replaced the 600-second total request timeout with connect and read
  timeouts, so a slow but healthy download of a large .run file is no
  longer killed at the 10-minute mark. Stalled connections still time
  out after 60 seconds without data.
- A file that fails SHA256 verification is now actually deleted, as the
  UI already claimed.
- Removed the Skip X Server Check switch: since the 2.3.0 repo-style
  install the script always passes --no-x-check to the installer, so
  the switch did nothing.
- Fixed the Fedora path never clearing dnf versionlock entries before
  package removal (broken plugin detection).
- Version now lives only in Cargo.toml: the About dialog and HTTP user
  agent read CARGO_PKG_VERSION at compile time, and build-appimage.sh
  parses Cargo.toml. The About dialog previously reported 2.4.0 in the
  2.5.x releases because the hardcoded copies were missed.

## 2.5.2 — 2026-07-13

**New application ID.** Application ID moved to io.github.labj1987.NVI (polkit action, appdata,
application_id); all machine-specific references removed; .deb packaging
and DBus service file deleted. AppImage is the only distribution format.

## 2.5.1 — 2026-07-13

- Packaging fix for the app's description metadata. No change to the app itself.

## 2.5.0 — 2026-07-13

- The AppImage now carries its description metadata, so AppImage managers and
  software centers can show what the app is.
- The README has screenshots.

## 2.4.0 — 2026-07-08

**Fedora and dnf-based distro support.** The install script now detects the package manager (apt vs dnf) and
branches every distro-specific step accordingly: kernel header packages,
clearing conflicting driver packages, initramfs rebuild (`dracut` instead
of `update-initramfs`), and the optional version hold (`dnf versionlock`
instead of `apt-mark hold`). No changes needed to the GUI itself — it
was already package-manager agnostic. Less tested than the Ubuntu path;
if something doesn't work right on Fedora, open an issue.

## 2.3.0 — 2026-07-07

**Repo-style install.** The big one. Rethought the install model entirely: instead of tearing down
the graphical session to unload the live kernel module, the installer now
runs with `--allow-installation-with-running-driver` and installs the new
driver to disk while the old one keeps running — exactly like a distro
package upgrade. The switch happens at the next reboot.

- No session teardown, no display manager stops, no black screens
- Reboot Required detection now compares the on-disk module (`modinfo`)
  against the running driver, so pending installs are reported correctly
- Privileged script simplified from ~230 lines of session management to
  ~120 straightforward lines
- Fully verified end to end with a live install

### Before this repository

Versions 1.x to 2.2.x were released before this repository existed and have no dates.

#### 2.2.x — Detached-install experiments (superseded)

Attempted to survive session teardown by re-executing the privileged script
into a detached `systemd-run` scope with `IgnoreOnIsolate=true`. Worked, but
2.3.0 made the entire problem unnecessary. Kept in history for reference.

#### 2.1.x — Feature releases

- **2.1.4** — Fixed AppImage first-run: root cannot read another user's
  FUSE mount, so privileged files are now staged through /tmp before
  `pkexec` installs them
- **2.1.3** — Module unload retries with process diagnostics (superseded)
- **2.1.2** — Create the download directory if missing
- **2.1.1** — Code-review hardening: `Cargo.lock` pinned for reproducible
  builds; async bridge rewritten from polling timers to
  `tokio::sync::oneshot` + `glib spawn_local`; SHA256 verification streams
  in 1 MiB chunks instead of loading the whole file; DKMS status parser
  rewritten to handle all output formats; archive integrity check moved
  before any destructive step
- **2.1.0** — System info tab (GPU, driver, kernel, DKMS, Secure Boot,
  disk, reboot state); version comparison badges in the browse list;
  pre-install checks; download cancel, speed and ETA; About dialog;
  custom NVI icon set; single-instance fix (Wayland WM_CLASS must match
  the full application ID); AppImage packaging

#### 2.0.0 — Rust rebuild

Complete rewrite from Python/GTK4 to Rust + GTK4 + libadwaita. Single
static binary, Tokio async runtime bridged to the glib main loop, `.deb`
packaging.

#### 1.x — Original Python version

GTK4 Python GUI with polkit-authorized install script, version browsing,
download with progress, and SHA256 verification (added in 1.2.0).
