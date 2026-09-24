# Green Light (formerly NVI / NVIDIA Driver Installer)

GTK4 + libadwaita GUI, written in Rust, for browsing and installing
official NVIDIA `.run` drivers from download.nvidia.com. Distributed as
a single AppImage — **AppImage-only; no `.deb` packaging should ever be
reintroduced** (it was deliberately removed, along with the DBus service
file).

Install model is repo-style: the new driver goes to disk while the
current one keeps running (`--allow-installation-with-running-driver`),
and the switch happens at the next reboot — no session teardown, no
black screens.

## Naming convention

Display name is "Green Light". The binary, crate, repo, AppImage filename,
`.desktop` and icon filenames, helper and installed paths use hyphenated
lowercase `green-light`. The application ID `io.github.labj1987.GreenLight`
and the polkit action ids stay PascalCase and must NOT be renamed (changing
them breaks existing installs' policy/settings).

## Module layout (`src/`)

- `main.rs` — entry point, sets up the shared Tokio runtime and wires up
  the GTK application.
- `ui.rs` — the GTK4/libadwaita UI: browse/configure/install/system tabs.
- `versions.rs` — talks to download.nvidia.com/XFree86/Linux-x86_64/:
  lists available driver versions, fetches the SHA256 checksum for a
  version.
- `download.rs` — downloads the `.run` file with progress, cancel,
  retries, and SHA256 verification.
- `system.rs` — queries the local system: GPU, installed driver, kernel,
  DKMS status, Secure Boot state, free disk space, reboot-required state.
- `install.rs` — invokes `scripts/privileged-install.sh` via `pkexec`
  with `InstallOptions` (DKMS, version hold, etc).

## Build process

`build-appimage.sh` builds the AppImage — `appimagetool`-direct,
not `linuxdeploy` (an earlier version of this doc claimed otherwise;
the script itself never has):
1. Installs build deps via apt (cargo, rustc, gtk4/adwaita dev headers,
   `wget`, `zsync`, `desktop-file-utils`).
2. `cargo build --release`.
3. Assembles the AppDir (binary, privileged script, polkit policy,
   appdata, desktop file, icon, generated `AppRun`).
4. Downloads `appimagetool` (continuous build) and packs the AppDir into
   `green-light-$VERSION-x86_64.AppImage`, with `UPDATE_INFORMATION` set
   for `gh-releases-zsync` delta updates.
5. Runs `zsyncmake` directly on the built AppImage to produce the
   `.zsync` sidecar (see gotcha below).

**Gotcha (fixed in v2.5.6):**
`appimagetool`'s own built-in zsync generation silently no-ops on the
GitHub Actions runner even when `UPDATE_INFORMATION` is set and
`zsync`/`zsyncmake` are installed and working. Do not rely on
`appimagetool` to generate the `.zsync` — call `zsyncmake "$OUT"`
directly right after packing, as the script does now. Keep that call
non-fatal (the AppImage is valid without the sidecar).

## Release process

1. Bump `version` in `Cargo.toml`.
2. Add a `CHANGELOG.md` entry.
3. Commit, push to `main`.
4. `git tag vX.Y.Z && git push origin vX.Y.Z`.
5. The tag push triggers `.github/workflows/release.yml` ("Build and
   Release"), which runs `build-appimage.sh` and uploads the AppImage
   (+ `.zsync`) to a GitHub Release via `softprops/action-gh-release`.
   The release-asset glob must match both files — check it whenever the
   output filename pattern changes.

## Conventions

- Don't use `sed`/`awk` to edit files — use direct file writes/edits.
  `tee` is fine for one-off terminal inspection, but Claude Code sessions
  should edit files directly rather than shelling through it.
- Repo lives at `/home/alex/Projects/green-light`, owned by user `alex` — if
  operating as root, run git commands as `alex`
  (`su -s /bin/bash alex -c '...'`) to keep authorship and file
  ownership correct.
