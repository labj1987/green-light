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
  lists available driver versions (an HTTP error or an empty listing is an
  error, not "0 versions"), fetches the SHA256 checksum for a version from
  `<file>.run.sha256sum` (NVIDIA publishes no `.manifest`). Only a 404 means
  "no checksum published" and may lead to the Use Unverified question; any
  other failure is an error the UI offers to retry. Checksums are validated
  as 64 hex characters.
- `branches.rs` — the Browse tab's branch labels, from NVIDIA's own data:
  the Unix driver page (nvidia.com/en-us/drivers/unix/) carries the driver
  lookup service URL and its Production / New Feature / Legacy queries;
  the labels are the branches those return, plus "Older Production" for
  older production branches released since the current one began. Never
  hardcode branch numbers; with no data, nothing is labeled.
- `download.rs` — downloads the `.run` file with progress, cancel,
  retries, and SHA256 verification. Bytes go to `<file>.part`, created mode
  0644 explicitly (the umask would leave it group-writable, which the
  install script refuses), and are renamed into place only when complete;
  every failure deletes the `.part`.
- `system.rs` — queries the local system: GPU, installed driver, kernel,
  DKMS status, Secure Boot state, free disk space, reboot-required state.
- `preflight.rs` — pure, unit-tested decision logic: open/proprietary
  module choice and validation, distro-package conflict filtering, DKMS
  signing-config parsing, signing-key pair/enrollment state, module signer,
  MOK password rules, Secure Boot advice, the .run file ownership/mode rule.
  `system.rs` gathers the facts it consumes. Constants mirrored from the
  install script (package globs, block file paths, signing key pairs) are
  compared with the script by the `install_script_agrees` test.
- `install.rs` — invokes `scripts/privileged-install.sh` via `pkexec`
  with `InstallOptions` (DKMS, distro-driver block, module flavor), and
  `run_privileged_setup_signing` for `--setup-signing` (password on stdin).
  `check_run_file` applies the script's file rule before the password
  prompt. The script's stdout/stderr stream into the Log tab as it runs, and
  the failure message comes from its exit code and first `ERROR:` line, so it
  always describes this run.
- `ui.rs` runs one long operation at a time (`Busy`: download — including
  checksum verification and the unverified/retry questions — install, or
  signing setup); `actions_enabled` derives every button's sensitivity from
  that.

## Module signing

The privileged script signs with the machine's existing key; Green Light
has no key location of its own. Lookup order (mirrored by
`SIGNING_KEY_PATHS`/`signing_cert_candidates` in `preflight.rs` and
`SIGNING_KEY_PAIRS`/`resolve_signing_key` in the script — keep them in
step): `mok_signing_key`/`mok_certificate` from `/etc/dkms/framework.conf`
then `framework.conf.d/*.conf` (last wins), else
`/var/lib/shim-signed/mok/MOK.{priv,der}` (Ubuntu DKMS's default), else
`/var/lib/dkms/mok.{key,pub}`, else Fedora's akmods pair. A default pair
counts only when key AND certificate exist; a config-named pair is used as
named even when incomplete (the app shows it as incomplete, the script
then signs nothing). `resolve_signing_pair` in `preflight.rs` is the app's
copy of that logic.

- Install path: never creates a key. With no key it must log exactly the
  same steps as before signing existed. With a key: DKMS installs add
  nothing (DKMS signs at build time); non-DKMS installs pass
  `--module-signing-secret-key` (PEM) and `--module-signing-public-key`
  (DER; a PEM cert is converted into the private temp dir). A `pkcs11:`
  key can't be passed to nvidia-installer, so a non-DKMS install with one
  is unsigned; the Secure Boot advice says to turn on DKMS. Step 6b
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

## Install script (`scripts/privileged-install.sh`)

- Order: validate arguments and the .run file (exit 2), copy it into a
  root-only dir and verify the SHA256 on the copy, `--check`, install
  prerequisites (allowed to fail), then **check** the module can be built
  for `uname -r` (`/lib/modules/$KVER/build` with `Makefile` and
  `include/generated/autoconf.h`, gcc — or clang/ld.lld when autoconf.h
  says the kernel was built with them —, make, dkms with `--dkms`) and
  abort with "No changes made" (exit 1) if not. Only then the destructive
  part: unhold, purge, boot config, nvidia-installer.
- Before the destructive part it records the installed driver packages
  with versions, their apt holds, the distro-driver block, and the
  modprobe files / alternate-install marker, and sets `ROLLBACK_ARMED`.
  Any failure from there until nvidia-installer succeeds (including an
  unexpected `set -e` exit) runs `rollback` from the EXIT trap: restore the
  files, reinstall the packages (exact versions, else current), re-hold,
  re-block, rebuild the initramfs. Exit 3 = rolled back, 4 = rollback
  incomplete (the UI says not to reboot), 5 = installed but initramfs
  failed. The script never exits 126/127 (pkexec's dismissed / not
  authorized); the EXIT trap maps a stray one to 1. Same for
  `green-light-setup`.
- File rule: owner root or the caller; never other-writable;
  group-writable only in the owner's private group (named after the
  owner, its primary group, no other members, no other account with it as
  primary group). `run_file_problem` in `preflight.rs` mirrors it.
- `--block-distro-driver` writes an apt pin (`Pin-Priority: -1` on the
  `src:nvidia-graphics-drivers*` and `src:nvidia-settings` source packages
  plus Ubuntu's prebuilt module packages and system76-driver-nvidia) or, on
  dnf5, an `excludepkgs` drop-in in `/etc/dnf/libdnf5.conf.d/` (dnf4 has no
  drop-in: unsupported, the switch is greyed out). Each install removes the
  block before touching packages and sets it again only when asked, so
  turning the switch off removes it; the switch starts in the current
  state. The container toolkit (`nvidia-container*`, `libnvidia-container*`)
  is never purged or blocked.
- Tests: `scripts/tests/privileged-install-test.sh` runs the real script
  unprivileged in bubblewrap with scratch `/etc`, `/var`, `/boot`, `/tmp`
  and module tree and a PATH of stubs only. Add a case there for every
  behaviour change. CI runs it.

## Build process

`build-appimage.sh` builds the AppImage — `appimagetool`-direct,
not `linuxdeploy` (an earlier version of this doc claimed otherwise;
the script itself never has):
1. Runs as an ordinary user and writes only inside the checkout, without
   changing tracked files. It installs packages via apt (cargo, rustc,
   gtk4/adwaita dev headers, `pkg-config`, `zsync`, `wget`, `file`,
   `desktop-file-utils`) only on a machine with no toolchain (cargo or the
   GTK4 headers missing), the one case that needs root (through `sudo`
   unless already root). In CI the workflow installs the headers and the
   packaging tools; the script then fails early if `wget`, `file` or
   `desktop-file-validate` is missing.
2. `cargo build --release --locked`.
3. Assembles the AppDir (binary, privileged script, `green-light-setup`
   helper, polkit policy, appdata, desktop file, icon, generated
   `AppRun`) and runs `desktop-file-validate` on the desktop file. The
   appdata's `<releases>` are generated from the changelog straight into
   the AppDir (`sync_appdata_releases.py --output`); CI checks the tracked
   copy separately. AppRun stages the setup files in a `mktemp -d` dir and,
   on first run, runs the staged helper as `pkexec "$BASH" <helper>` so a
   noexec `/tmp` doesn't matter.
4. Downloads `appimagetool` (pinned 1.9.1) and the type2 runtime it
   puts in front of the squashfs (pinned 20251108, passed with
   `--runtime-file`), both SHA256-verified and cached in `.cache/`, and
   packs the AppDir into
   `green-light-$VERSION-x86_64.AppImage`, with `UPDATE_INFORMATION` set
   for `gh-releases-zsync` delta updates.
5. Runs `zsyncmake` directly on the built AppImage to produce the
   `.zsync` sidecar (see gotcha below).

**Gotcha (fixed in v2.5.6):**
`appimagetool`'s own built-in zsync generation silently no-ops on the
GitHub Actions runner even when `UPDATE_INFORMATION` is set and
`zsync`/`zsyncmake` are installed and working. Do not rely on
`appimagetool` to generate the `.zsync` — call `zsyncmake "$OUT"`
directly right after packing, as the script does now. A missing or
failing `zsyncmake` is fatal when `CI` is set (the update information
points at a `.zsync`, so a release without one cannot update); a local
build only warns.

Rust is pinned in `rust-toolchain.toml`; bump it there and in both
workflows together (each checks the two agree).

## Release process

1. Bump `version` in `Cargo.toml`.
2. Add a `CHANGELOG.md` entry (see Changelog below).
3. Run `python3 scripts/sync_appdata_releases.py` to regenerate the
   appdata `<releases>` list; CI fails if it is out of date.
4. Commit, push to `main`.
5. `git tag vX.Y.Z && git push origin vX.Y.Z`.
6. The tag push triggers `.github/workflows/release.yml` ("Build and
   Release"). Its `checks` job runs `ci.yml` (a reusable workflow) on the
   tagged commit, so nothing is published that fails CI's checks. Its
   `build` job, after them, has a read-only token: it checks the tag
   against the `Cargo.toml` version, runs `build-appimage.sh`, and hands
   the AppImage, `.zsync` and release notes on as a workflow artifact.
   Both workflows have `timeout-minutes` on every job and a concurrency
   group (CI cancels superseded runs; a release never cancels). A separate `publish` job, the only
   one with write access, attaches them to a GitHub Release via
   `softprops/action-gh-release`, with that version's changelog section
   as the release text. Actions in both workflows are pinned to commit
   SHAs; Dependabot (`.github/dependabot.yml`) proposes the updates.
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
