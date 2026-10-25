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
  prompt (off the GTK thread), ACLs included. The script's messages reach
  the Log tab by following `/var/log/green-light.log` from where it ended
  when the run started (the script writes nothing to pipes), and the
  failure message comes from its exit code and first `ERROR:` line, so it
  always describes this run. `install_running` reads the script's lock;
  `recovery_pending` sees a journal with no install running,
  `recovery_note` reads the script's note, `run_privileged_recover` runs
  `--recover` and `run_privileged_discard_recovery` `--discard-recovery`;
  exit 6 comes back as a `StaleRecovery` error (the UI offers Discard and
  Install).
- `ui.rs` runs one long operation at a time (`Busy`: download — including
  checksum verification and the unverified/retry questions — install, or
  signing setup); `actions_enabled` derives every button's sensitivity from
  that, and Install also waits for the system info (the block switch starts
  from it). A `BusyGuard` holds the operation and ends it when dropped, so a
  panicking task can't leave the buttons disabled. The install result is
  shown only after the last streamed output line. Closing the window during
  an install or signing setup asks first; an install from an earlier run
  (lock held) shows a banner and blocks new actions until it ends; a
  journal with no install running shows a banner whose action follows the
  note: Restore Previous Driver, Try Again, or (refused) Retry Recovery /
  Discard Recovery Data. The note is re-evaluated by every recovery run,
  so at every boot. The
  version list shows at once; branch labels are added to the rows when
  `branches.rs` answers (20 s limit).

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
  file, then one `mokutil --import <ders...> --hash-file`. The password
  must never reach argv or a log. Setup doesn't touch the installed driver.
- shim keeps ONE pending request (MokNew) with ONE password (MokAuth);
  `mokutil --import` appends to it but replaces the password, and skips an
  already-queued key without updating it. So setup rebuilds the whole
  request: the certificates already waiting (parsed out of the MokNew EFI
  variable, e.g. another app's kernel-signing key) plus this cert, minus
  anything enrolled, under the new password (`--revoke-import`, then one
  `--import`). A request holding non-certificate entries is left alone.
- MokNew is root-only, so `mokutil --list-new` and `--test-key` run as the
  user cannot see a queued key. Setup writes the queued SHA1 fingerprints to
  `/var/lib/green-light/mok-pending` (0644); the app counts the cert as
  pending when MokNew exists and the `sha1sum` of the DER cert is listed.
- Never run `mokutil --import/--delete/--reset` or the privileged script
  against a real machine while developing; test helpers in a temp dir.

## Install script (`scripts/privileged-install.sh`)

- It must finish or roll back whatever happens to the app, the session or
  the machine:
  - Without a terminal it writes nothing to stdout/stderr (pipes to the app,
    which may be closed: SIGPIPE); everything goes to
    `/var/log/green-light.log`, each line tagged `[green-light <PID>]`, and
    the app follows the log showing only its own run's lines
    (`run_pkexec`, `LogTail`). Run by systemd (the boot unit,
    `INVOCATION_ID` set) stdout/stderr stay on: they go to the journal and
    console. SIGPIPE keeps its default for child commands; SIGHUP is ignored.
  - It re-execs into its own transient scope (`systemd-run --scope`, same
    PID; a trial `systemd-run --scope true` first, and without a working
    one it goes on unscoped with a warning; skipped under a systemd
    service; guarded by `GREEN_LIGHT_SCOPED`) so logout doesn't kill it,
    then into its own session (`setsid`).
  - SIGTERM/SIGINT set `STOP_REQUESTED`, honoured only at safe points:
    before anything changes it exits "No changes made"; after the purge and
    before nvidia-installer it rolls back; a signal during nvidia-installer
    is taken when it ends: success finishes the install, failure rolls back.
    The EXIT trap ignores TERM/INT first thing.
  - While it changes the system (armed, and during any recovery),
    `systemd-inhibit --what=shutdown:sleep --mode=block` blocks shutdown and
    suspend requests (an administrator can still force them; `tail
    --pid=$$` as the held command, so it ends with the script).
  - `flock` on `/run/green-light-install.lock` (fd 9): one install, recovery
    or signing setup at a time; logging starts once it is held. The app
    uses it to see an install that outlived its window. Every long or
    package-manager command runs through `nolock` (a subshell with fd 9
    closed and TERM/INT back at their defaults), so background jobs
    (akmods from an rpm scriptlet, DKMS) neither keep the lock nor inherit
    the rollback's ignored signals.
- Order: validate arguments and the .run file (exit 2), copy it into a
  root-only dir and verify the SHA256 on the copy, `--check`, install
  prerequisites (allowed to fail), **check** the module can be built for
  `uname -r` (headers in `/lib/modules/$KVER/build`, the kernel's compiler,
  make, dkms with `--dkms`), then **test-build** it: `--extract-only` and
  `make -C <flavor> KERNEL_UNAME=$KVER modules` with the compiler chosen as
  DKMS does (`LLVM=1` for clang kernels, the gcc named in
  `CONFIG_CC_VERSION_TEXT` when installed), for the chosen flavor or both.
  Any of these failing is "No changes made" (exit 1).
- **Journal** (`/var/lib/green-light/journal`, root-only, parent 0755 so the
  app can see it; written as `journal.new`, every file synced, renamed):
  before the first change it records a snapshot of every installed package
  at exact versions, the removal plan (`apt-get -s purge` / `dnf remove
  --assumeno` listing; only plan ∩ actually-removed is ever reinstalled),
  which of them were automatically installed, holds, the block state, file
  backups, the earlier .run version, the module's version and signer, the
  .run version being installed (`TARGET_RUN_VER`, from the verified copy's
  makeself `label=`, not the file name), the kernels that had an nvidia
  module (`MOD_KERNELS_BEFORE`) and the boot ID, and copies of the packages to be removed (apt's cache or
  `apt-get download`; `dnf download`; akmods' cached kmod rpm; index
  written atomically) so the rollback works offline. Phase: `armed` →
  `installer-started` → `installer-done`; the journal is removed when the
  installer has succeeded or a rollback has been verified, kept when a
  rollback fails. `recovery-status` next to it (0644) tells the app why
  recovery data is waiting (`failed: …` / `refused: …`).
- **Rollback** (EXIT trap while armed, or recovery): `nvidia-uninstall` if
  the failed installer registered a new .run driver, restore files,
  reinstall the removed set — dnf: the kept copies alone with
  `--disablerepo='*'` (works offline with expired metadata), then only what
  has no copy from the repositories (`--skip-unavailable` /
  `--setopt=strict=0`); apt: the kept .debs plus exact versions; else
  current versions; akmods' local `kmod-nvidia-<kernel>` never from a
  repository — restore auto marks, holds and block. For each kernel that
  matters — the journal's and, at a boot into a newer one, `uname -r` — it
  makes sure the module exists (`akmods --force --kernels`, Ubuntu's
  `linux-modules-nvidia-<branch>-<kernel>` when the setup used prebuilt
  modules, else `dkms autoinstall -k`) and rebuilds that kernel's
  initramfs. Then `verify_restored`: the earlier .run driver, every removed
  package, a module for each of those kernels, and under Secure Boot a
  signer that is unchanged or the enrolled key; a missing module for another
  kernel the previous driver didn't cover either (not in
  `MOD_KERNELS_BEFORE`) is only a warning. Exit 3 only when that passes,
  else 4. What the rollback installs is recorded after every step
  (`record_reinstalled`, synced), so an interrupted recovery still knows
  its own work.
- **Recovery**: a journal left by a cut-off run (power loss, SIGKILL) or a
  failed rollback is handled by `--recover` (the app's Restore Previous
  Driver), at the start of the next install, and at boot by
  `data/green-light-recover.service` (ConditionPathExists on the journal;
  after akmods/dkms, before the display manager; output to journal and
  console, progress on plymouth via `plymouth display-message`;
  `SuccessExitStatus=3 6`; `TimeoutStartSec=20min` — a timeout leaves the
  journal for the next try). Phase `installer-done` only clears the journal.
  First `journal_stale_reason`: if the driver was changed by other means
  since (a .run driver that is neither the earlier nor the target one, or
  driver packages neither in the snapshot nor put back by a rollback),
  nothing is touched: exit 6, note `refused: …`, until `--discard-recovery`
  (a mode for the app's Discard Recovery Data, and an install option for its
  Discard and Install). Exit codes of `--recover`: 0 nothing to put back, 3
  restored and verified, 4 failed, 6 refused.
- At the start of an install, a journal whose rollback fails again is
  carried over: the new journal is built on it (its snapshot, plan, copies,
  holds, block, backups and driver state stay the baseline; this install's
  removals are added; the earlier target joins `KNOWN_TARGETS` and an
  earlier `installer-started` is kept as `EARLIER_INSTALLER_STARTED`), so
  rolling back the new install restores the setup from before the first
  one. A stale journal stops the install (exit 6).
- The distro-driver block is set right after nvidia-installer succeeds, so
  every later exit keeps it as asked. Exit 5 = installed but initramfs
  failed. The script never exits 126/127 (pkexec's dismissed / not
  authorized); the EXIT trap maps a stray one to 1. Same for
  `green-light-setup`.
- File rule: owner root or the caller; never other-writable;
  group-writable only in the owner's private group (named after the
  owner, its primary group, no other members, no other account with it as
  primary group — the last part only sees accounts NSS can enumerate); an
  ACL (`ls` "+" mark) may not grant write to anyone else, checked with
  `getfacl` (refused when it can't be checked). `run_file_problem` and
  `acl_writers` in `preflight.rs` mirror it.
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
  unprivileged in bubblewrap with scratch `/etc`, `/var`, `/boot`, `/run`,
  `/tmp`, `/usr/lib/nvidia` and module tree, and a PATH of stubs that keep a
  fake dpkg/rpm database (purges take reverse dependencies, downloads and
  reinstalls bring packages and modules back, `$GLT/offline` cuts the
  network and makes dnf's metadata fail). It covers signals at each stop
  point, SIGKILL in each journal phase with `--recover` and the boot unit's
  ExecStart, offline recovery, a newer boot kernel (prebuilt, DKMS, and
  missing), stale journals and discarding, carried baselines, dnf with an
  akmods kmod, the lock (signing setup too), PID-tagged log lines, ACLs,
  partial and earlier .run installs, Secure Boot verification, inherited
  signals and fds, and `systemd-analyze verify` of the unit. Add a case
  there for every behaviour change. CI runs it.

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
   copy separately. The setup helper also installs
   `data/green-light-recover.service` to `/etc/systemd/system` and enables it
   (hash-verified like the rest; AppRun reinstalls when it differs or isn't
   enabled). AppRun
   stages the setup files in a `mktemp -d` dir and,
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
