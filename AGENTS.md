# Working on Droidloom

## Updating Droidloom

An Android image-only update needs a Droidloom restart, not a reboot. ARM64
hosts need the ARM64 packages; the x86_64 desktop packages do not apply.

When Droidloom still runs on Android storage created by an earlier release,
start it on fresh storage rather than carrying that storage forward, even for
the direct `data`/`metadata` directories without images. Point `data_dir` in
`/etc/droidloom/cell.json` at a new `host-storage-YYYYMMDD` directory;
`droidloom-update apply` creates and prepares it. Leave the old directory on
disk untouched. The user signs in to their Google account again inside Android.
On 2026-10-19 storage from the 2026-10-16 release could not unlock user 0:
Keystore rejected the synthetic-password key as an invalid key blob, and
system_server restarted every 36 s.

## Play Store catalog reset

After changing Android capabilities, restarting Droidloom alone may leave stale
Play Store compatibility results. Inside Android, run
`/system/bin/cmd package clear --user 0 --cache-only com.android.vending`, then
`/system/bin/cmd activity force-stop com.android.vending` and reopen Play Store.
Clear only the cache; preserve Google account and app data.
The cache-only command needs Android root/system permissions. On the pinned
Android 17 build, plain ADB shell lacks `INTERNAL_DELETE_CACHE_FILES`; the
request is silently ignored and the command hangs. In that case, use Android
Settings → Apps → Google Play Store → Storage & cache → **Clear cache**, verify
the cache reaches zero, then force-stop Play Store and reopen the app listing.
Do not select **Clear storage**. See the exact recovery sequence in
`docs/BUILDING.md` under “Play Store catalog refresh”.

## Commands

- Build pacman packages: `cargo run --locked -j 1 -p droidloom-package -- build`
- Check packages: `cargo run --locked -j 1 -p droidloom-package -- check --packages dist/arch/<version-release>`
- Check Rust code: `cargo check --locked --workspace -j 1`
- Test packaging: `cargo test --locked -j 1 -p droidloom-update -p droidloom-package -p droidloom-package-support`
- Test runtime contracts: `cargo test --locked -j 1 -p droidloom-contracts`

Use focused tests for changed components. Keep two logical CPUs free across all
compiler processes; the Rust builder enforces this for nested Android builds.
Use Rust for new tooling; shell is reserved for required upstream interfaces and
PKGBUILD functions. Explain why administrator access is needed before requesting
it. Package builds use rootless Podman and need no host sudo.

## Phone release loop

ARM64 phones (aston, roadstr, iceland) get a release built on this workstation
from main's last commit, only through the workspace's build tool: merge into
main first (workspace AGENTS.md, "Building for a phone"), then
`/mnt/development/denial-os/tools/build start droidloom DEVICE` and
`tools/build wait DEVICE`. It runs android-build, host-build and bundle-stage
with this checkout's `.work` caches, so repeats are incremental. Never run
those steps by hand. STAGE is `/mnt/development/denial-os/builds/DEVICE/droidloom/latest`,
the newest build that verified.

Send `STAGE/` (the slash follows the link) over the phone's one stage,
`/var/lib/droidloom/stage/current`, with
`rsync -a --sparse --delete --compress --compress-choice=zstd`: the previous
content is the delta basis, so only changed files travel. Run
`STAGE/usr/bin/droidloom-update verify STAGE` there before activating.

Activate on the phone as root:
1. Stop `droidloom.service` and `droidloom-applications.service` in the desktop
   user's manager, then `droidloomd.service`. Wait until neither
   `droidloom-andro` nor `droidloomd` runs.
2. `STAGE/usr/bin/droidloom-update apply STAGE --uid UID --no-start`, with the
   UID from `packaging/devices/DEVICE.json`.
3. Activation disables autostart. Link `/usr/lib/systemd/user/droidloom.service`
   into the user's `~/.config/systemd/user/graphical-session.target.wants/`,
   then reload both managers.
4. Start `droidloomd.service`, then the user's `droidloom.service`, and wait for
   `sys.boot_completed`.

The installed releases under `/usr/lib/droidloom/releases` are the rollback;
the stage is only the transfer copy. The user's manager is reached as root with
`sudo -u USER XDG_RUNTIME_DIR=/run/user/UID DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/UID/bus systemctl --user`.
Android tools run with
`nsenter -t $(pgrep -o -f '^/init second_stage') -a /system/bin/TOOL`.

## AOSP patches

The folders under `android/` hold patches applied in the order of `PATCHES` in
`tools/droidloom-update/src/android.rs`, each to one project pinned in
`android/manifest/m2-sparse-source-lock.json`. `.work/arch/build/aosp-source/PROJECT`
is the unpatched checkout. To change a patch, add a detached worktree of that
checkout, apply the patch there and commit it, make the change and amend, then
write `git diff HEAD~1 HEAD` back over the patch. Check it with `git apply --check`
on a clean worktree, and remove both worktrees.

## Translation benchmarks

“Translation benchmarks” means `droidloom-translation-bench` for Digitalis/Berberis
ARM64-to-x86_64: correctness stress followed by seven workloads across three modes.
See [run/build/compare instructions](docs/BUILDING.md#translation-stress-and-performance-benchmarks).
Rebuild host tests after translator edits; compare saved runs, accounting for noise.
Keep at least two cores available; pin benchmarks to one logical CPU (currently 6).
Local reference: `.work/translation/bench-reference/result.json`; keep results out of Git.

## Useful files

- `README.md`: overview and starting points.
- `packaging/arch/README.md`: installation and source builds.
- `docs/README.md`: architecture, integration and contracts.
- `tools/README.md`: maintained entry points and specialist helpers.
- `tools/droidloom-package/src/main.rs`: container build and pacman packaging.
- `tools/droidloom-update/src/`: Android build, assembly and source updater.
- `runtime/`: lifecycle, CLI, catalog and package setup.
- `graphics/`: Wayland presentation and private Android transport.
- `android/manifest/`: pinned upstream inputs.
- `LICENSE` and `THIRD_PARTY.md`: licensing and upstream exceptions.

Keep packages, APKs, build outputs, caches and local test journals out of Git.
Update existing guides instead of adding dated plans. Documentation and ordinary
development tasks do not authorize installing anything or restarting the user's
graphical session.

## Shipping

The basic pipeline is `.github/workflows/ship.yml`. Before an authorized push to
`main`, run `cargo run --locked -j 1 -p droidloom-package -- runner start` to arm
the one-job worker under `/mnt/puck/logix/droidloom`. After the job, verify it is
stopped with `runner status`; use `runner stop` to remove leftover registration.
Preserve its caches. Increment `packaging/arch/version.json` before shipping
changed package bytes. See `docs/BUILDING.md` for setup and recovery.
