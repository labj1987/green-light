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
- `preflight.rs` — pure, unit-tested decision logic: open/proprietary
  module choice and validation, distro-package conflict filtering, DKMS
  signing-config parsing, signing-key/enrollment state, module signer,
  MOK password rules, Secure Boot advice. `system.rs` gathers the facts it
  consumes.
- `install.rs` — invokes `scripts/privileged-install.sh` via `pkexec`
  with `InstallOptions` (DKMS, version hold, etc), and
  `run_privileged_setup_signing` for `--setup-signing` (password on stdin).

## Module signing

The privileged script signs with the machine's existing key; Green Light
has no key location of its own. Lookup order (mirrored by
`SIGNING_KEY_PATHS`/`signing_cert_candidates` in `preflight.rs` and
`SIGNING_KEY_PAIRS`/`resolve_signing_key` in the script — keep them in
step): `mok_signing_key`/`mok_certificate` from `/etc/dkms/framework.conf`
then `framework.conf.d/*.conf` (last wins), else
`/var/lib/shim-signed/mok/MOK.{priv,der}` (Ubuntu DKMS's default), else
`/var/lib/dkms/mok.{key,pub}`, else Fedora's akmods pair.

- Install path: never creates a key. With no key it must log exactly the
  same steps as before signing existed. With a key: DKMS installs add
  nothing (DKMS signs at build time); non-DKMS installs pass
  `--module-signing-secret-key` (PEM) and `--module-signing-public-key`
  (DER; a PEM cert is converted into the private temp dir). Step 6b
  compares `modinfo -F signer` with the cert's subject CN, report-only.
- `--setup-signing` (first argument): password from stdin, installs
  `mokutil`/`openssl` if missing, `ensure_signing_key` (never overwrites;
  `update-secureboot-policy --new-key`, else `dkms generate_mok`), then
  `mokutil --generate-hash` (password piped twice) into a root-only hash
  file and `mokutil --import <der> --hash-file`. The password must never
  reach argv or a log. Setup doesn't touch the installed driver.
- `mokutil --test-key` says "is already in the enrollment request" for a
  queued key; the app also matches `mokutil --list-new` SHA1 fingerprints
  against `sha1sum` of the DER cert.
- Never run `mokutil --import/--delete/--reset` or the privileged script
  against a real machine while developing; test helpers in a temp dir.

## Build process

`build-appimage.sh` builds the AppImage — `appimagetool`-direct,
not `linuxdeploy` (an earlier version of this doc claimed otherwise;
the script itself never has):
1. Installs the tools the script itself uses via apt, unconditionally
   (`zsync`, `wget`, `file`, `desktop-file-utils`), and the toolchain
   (cargo, rustc, gtk4/adwaita dev headers, `pkg-config`) only when
   cargo or the GTK4 headers are missing.
2. `cargo build --release --locked`.
3. Assembles the AppDir (binary, privileged script, `green-light-setup`
   helper, polkit policy, appdata, desktop file, icon, generated
   `AppRun`) and runs `desktop-file-validate` on the desktop file.
4. Downloads `appimagetool` (pinned 1.9.1, SHA256-verified, cached in
   `.cache/`) and packs the AppDir into
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
2. Add a `CHANGELOG.md` entry (see Changelog below).
3. Run `python3 scripts/sync_appdata_releases.py` to regenerate the
   appdata `<releases>` list; CI fails if it is out of date.
4. Commit, push to `main`.
5. `git tag vX.Y.Z && git push origin vX.Y.Z`.
6. The tag push triggers `.github/workflows/release.yml` ("Build and
   Release"), which checks the tag against the `Cargo.toml` version,
   runs the tests, runs `build-appimage.sh` and uploads the AppImage
   (+ `.zsync`) to a GitHub Release via `softprops/action-gh-release`,
   with that version's changelog section as the release text.
   The release-asset glob must match both files — check it whenever the
   output filename pattern changes.

## Changelog

- One `## X.Y.Z — YYYY-MM-DD` heading per released version, newest
  first. No entries for builds that were never released.
- Write each entry for the people using the app: what changed for them
  and anything they need to do. Leave out implementation detail (file
  paths, flags, internal names, CI and packaging changes) unless a user
  needs it to act.
- The release page is the version's section written out in full
  (`scripts/release_notes.py`), never a link to the changelog. The
  release fails if the section is missing.
- The appdata `<releases>` list is generated from the headings
  (`scripts/sync_appdata_releases.py`). Don't edit it by hand.

## Conventions

- Don't use `sed`/`awk` to edit files — use direct file writes/edits.
  `tee` is fine for one-off terminal inspection, but Claude Code sessions
  should edit files directly rather than shelling through it.
- Repo lives at `/home/alex/Projects/green-light`, owned by user `alex` — if
  operating as root, run git commands as `alex`
  (`su -s /bin/bash alex -c '...'`) to keep authorship and file
  ownership correct.
