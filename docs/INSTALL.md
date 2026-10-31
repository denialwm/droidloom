# Install Droidloom

This guide covers the preview packages for x86_64 Arch Linux and aarch64 Arch
Linux ARM. Prebuilt packages for both are in the
[latest release](https://github.com/denialwm/droidloom/releases/latest). Read
[known issues](KNOWN_ISSUES.md) before installing. To produce the packages yourself,
follow [BUILDING.md](BUILDING.md), then return here.

## 1. Check prerequisites

- An x86_64 machine running Arch Linux, or an aarch64 machine running Arch
  Linux ARM, with a Wayland desktop.
- A GPU other than NVIDIA: Droidloom is incompatible with NVIDIA graphics. On a
  dual-GPU laptop, setup selects the other GPU's render node.
- A working systemd user session and administrator access through sudo or polkit.
- At least 20 GiB free for packages, installed files and Android app data.
  The package pair is about 975 MiB to download and 2.5 GiB installed, before apps.

Run the desktop commands below in a terminal **inside your Wayland session**, as
your ordinary user. Do not run `droidloomctl start` or app launches with sudo.

## 2. Install both packages

Install or update from the published pacman repository:

```console
curl -fLO https://denialwm.github.io/droidloom/install.sh
less install.sh
sh install.sh
```

The script adds `/etc/pacman.d/droidloom.conf` to pacman's configuration and runs
`pacman -Syu --needed droidloom-runtime droidloom-image`, with pacman's normal
confirmation. This performs a full system upgrade to avoid partial Arch upgrades.
Run the same script again to update, or use your normal `sudo pacman -Syu`.
The initial repository publishes **unsigned packages over HTTPS**; `SigLevel =
Never` applies only to `[droidloom]`. Other repositories keep their signing policy.
Pages serves the package database; GitHub Releases serves the package archives.
On Arch Linux ARM (aarch64) the same script points pacman at the aarch64
repository.
Google apps are not included.

For locally built packages, put one matching pair in a directory and run:

```console
sudo pacman -U ./droidloom-runtime-*.pkg.tar.zst ./droidloom-image-*.pkg.tar.zst
```

Sudo allows pacman to install system files and dependencies and update its package
database. Both packages are required and must have the same version and release.
Installation leaves Droidloom stopped. No compilation runs on the recipient's
machine when installing prebuilt packages.

## 3. Complete first-time setup and start Android

```console
droidloomctl start
```

On first start, Droidloom requests administrator authentication to create
configuration under `/etc/droidloom`, Android data under
`/var/lib/droidloom/users/<uid>/data`, and the permission rule for managing
`droidloomd.service`. Approve the prompt in your desktop authentication agent.
Subsequent starts do not need sudo.

If the authentication prompt is unavailable, run the setup helper explicitly
from your ordinary user's terminal, then retry start:

```console
sudo /usr/lib/droidloom/droidloom-package-helper setup --uid "$(id -u)" \
  --data-home "${XDG_DATA_HOME:-$HOME/.local/share}" \
  --state-home "${XDG_STATE_HOME:-$HOME/.local/state}"
droidloomctl start
```

This sudo command performs the same privileged setup described above. First boot
can take longer while Android initializes its data. `start` and `restart` wait
for Android readiness before printing `Droidloom is running; Android is ready`.
They show the observed boot stage: Android init, boot completion (including
runtime and boot-animation service states when available), and Droidloom's input
and task services. These are readiness observations, not a percentage estimate.

Boot readiness has a 120-second deadline. While a command waits, progress is
repeated every 10 seconds with elapsed time and the remaining request budget.
Application listing, launch and installation identify their operation after
boot, with a separate bounded execution time. A lifecycle request, including
time queued behind another request, stops waiting after 250 seconds. Service
startup is a separate step bounded at 160 seconds; authentication prompts wait
for your response.

Stopping `droidloom.service` stops the lifecycle daemon directly. A pending
Android readiness wait or framework reply is cancelled before cell teardown,
so logout does not wait for its 120-second request deadline. Disconnecting a
control client also cancels those waits; Android itself keeps running unless
a stop was requested. Graceful cell teardown can still take about ten seconds.

If Android does not become ready, the command exits with `Android looks stuck`,
the last observed boot stage, and journal commands. A readiness timeout does not
stop Android. After inspecting the logs you can follow its progress again:

```console
droidloomctl wait
```

Use `droidloomctl start --no-wait` when you only want to start the services;
its success message explicitly says Android readiness has not been checked.
`--json` keeps a single JSON result on stdout and suppresses progress messages.
If a request times out while an app operation is in progress, it may still finish
in the background; check its result before retrying.

To start Android automatically with your graphical session, run:

```console
systemctl --user enable droidloom.service
```

While Droidloom is running, an event-driven recovery service watches the Android
cell owner. If the cell or lifecycle daemon crashes, it starts Android again
after five seconds; presenter crashes use the main service’s restart policy.
Five rapid recovery failures within five minutes stop further attempts to avoid
a crash loop. Inspect `journalctl --user -u droidloom-recovery.service`, then use
`systemctl --user reset-failed droidloom-recovery.service` and
`droidloomctl restart` after correcting the cause. An intentional
`droidloomctl stop` stops recovery too. Recovery restores Android, not unsaved
application state. This watches process exits; a live but hung Android process
still needs diagnosis.

Each start shows a “Droidloom is starting” notification. Recovery follows
Android's boot and closes it once Android is ready; if Android does not start,
the notification says why and stays until dismissed.

## 4. Try a built-in app

```console
droidloomctl applications
droidloomctl launch com.android.settings
```

A separate Settings window should appear. Optionally set Android's display
density; 160 DPI was used for the desktop preview tests:

```console
droidloomctl dpi 160
```

The density persists. Desktop tiling determines the window's available size.

## 5. Install an APK

The installation command accepts a **single, standalone APK**. Native x86_64
APKs and apps without native libraries remain preferred. Revision 15 adds
experimental ARM64 support through the source-built Digitalis/Berberis translator.
ARM32 apps are unsupported; ARM64 compatibility and performance vary by app.

Download the APK from its publisher or another source you trust. Keep it on the
Linux host; you do not need to copy it into Android first.

Start Droidloom, then install the APK as your ordinary desktop user:

```console
droidloomctl start
droidloomctl install "$HOME/Downloads/application.apk"
```

No sudo is needed for installation. The client opens the APK with your permissions
and passes its file descriptor to the authenticated runtime daemon, which feeds
it to Android's package manager. The command waits for boot readiness, updates
an existing app while retaining its data, and reports success only when Android
confirms installation. The desktop catalog refreshes automatically.

Use `--user <id>` to select an Android user (default: `0`), or `--json` for a
machine-readable response. Files must be regular APK archives no larger than
2 GiB. Both `droidloomctl` and `droidloomd` must include the install command;
older preview packages need an update.

If installation reports `device is still booting`, wait and retry. An ABI error
usually means the APK needs an unsupported CPU architecture. A `.apkm`, `.apks`
or `.xapk` bundle is not a standalone APK: renaming it will not work. Split-package
and additional game-data installation are not covered by this preview guide.

Android writable storage uses ordinary host directories: `data_dir/data` is
mounted at `/data`, and `data_dir/metadata` at `/metadata`. App installations,
accounts, game downloads and emulated storage share the host filesystem's free
space. There is no writable disk image, fixed capacity or automatic resizing.
The parent is private to root; Android retains its normal inner ownership and
permissions. Read-only Android system/APEX artifacts remain image based.

An inherited syscall filter blocks filesystem-wide shutdown, freeze, trim,
quota changes and superblock remount/reconfiguration before Android starts.
It applies to Android descendants, including native and compat processes;
ordinary per-file I/O, FUSE and graphics operations remain available. This is a
specific shared-filesystem guard, not a claim of a complete container sandbox.

Existing image-backed installations must be migrated explicitly, with Droidloom
stopped. The runtime refuses legacy images rather than creating empty app data:

```console
systemctl --user stop droidloom.service
sudo systemctl stop droidloomd.service
sudo droidloom-supervisor migrate-storage --source /var/lib/droidloom/users/1000/data --destination /var/lib/droidloom/users/1000/host-storage
```

Use the actual selected `data_dir` and an unused destination. The migration needs
`rsync`, `e2fsprogs` and `util-linux`; it refuses attached images, checks them
read-only, copies through read-only mounts in a private namespace, and verifies
file checksums, ownership, permissions, hard links and xattrs. It publishes the
new directory only after verification and retains the source images. Back up
`/etc/droidloom/cell.json`, change only `data_dir` to the new destination, then
start `droidloom.service`. Verify apps and accounts before retiring the old
backup. A rollback needs the previous runtime and configuration together.

## 6. Launch the installed app

The desktop catalog updates automatically. Find the app in your desktop launcher
or list its package and launcher activity:

```console
droidloomctl applications
droidloomctl launch com.whatsapp
```

Replace `com.whatsapp` with your app's package name. If explicit activity selection
is needed, use the component shown in the catalog:

```console
droidloomctl launch com.whatsapp --component com.whatsapp/com.whatsapp.Main
droidloomctl launch com.halfbrick.fruitninjafree \
  --component com.halfbrick.fruitninjafree/com.halfbrick.mortar.MortarGameLauncherActivity
```

These examples assume you installed WhatsApp or the tested Fruit Ninja 2.8.9
x86_64 APK. APKs are not bundled with Droidloom. Complete app registration and
first-run prompts yourself. Revision 11 handles Fruit Ninja's launcher and
age-screen handoffs without requiring an activity-specific retry; earlier
revisions are affected by the [known launch issue](KNOWN_ISSUES.md).

For games that choose their rendering size only at startup, set the initial
Android task resolution in pixels:

```console
droidloomctl launch com.hottagames.nte \
  --component com.hottagames.nte/com.epicgames.unreal.SplashActivity \
  --resolution 2560x1440
```

`--resolution WIDTHxHEIGHT` restarts that application and supplies its task
bounds before Android starts the activity. Each dimension must be between 1 and
16384. Ordinary launches keep their existing behavior. This sets the Android
launch size; game-specific rendering-scale settings remain under the app's control.

To keep a resolution in a generated `.desktop` launcher, append the option to
its `Exec=` line and add `X-Droidloom-Resolution=2560x1440` in the `[Desktop Entry]`
section. The catalog validates and preserves that key during refreshes, including
app updates, and regenerates the matching command-line option.

NTE 1.3.1 also needs its Unreal mobile render scale overridden: a 2560x1440
Android window alone still produced approximately 1440x792 game buffers. On the
tested installation, its existing Unreal launch-file reader accepts
`-mcsf=0 -mobileresx=2560 -mobileresy=1440` in
`/data/user/0/com.hottagames.nte/files/UnrealGame/HT/UECommandLine.txt`, appended
to the original project/map command line. Preserve an existing launch file before
changing it and restart only NTE afterward. Do not replace its encrypted
`GameUserSettings.ini` or `Engine.ini` with plain-text Unreal settings.
The native-resolution launch produced 2536x1392 game buffers inside the desktop's
2542x1397 decorated work area; fullscreen removes that work-area constraint.
This is an NTE-specific setup, separate from the general `--resolution` option.

## ADB access

While Droidloom runs, it automatically connects Android to your ordinary user's
local ADB server. The runtime package includes `android-tools`. Run these
commands as the same user who started Droidloom:

```console
adb devices -l
adb -s device:droidloom shell
adb -s device:droidloom install application.apk
```

The device appears as `10.177.0.2:5555`, with `model:Droidloom` and
`device:droidloom` in the detailed list. Android reports `ro.serialno=droidloom`;
standard ADB uses the connection address for a TCP transport's serial column.
Use `-s device:droidloom` for a stable selector, including when other Android
devices are attached. If Droidloom is your only device, plain `adb shell` works.

The connection is authenticated with your host's normal `~/.android/adbkey`.
Only its public key is projected into Android. The endpoint uses Android's
private network; Droidloom adds no LAN listener or inbound port forwarding.
It reconnects automatically after an ADB server restart and disconnects when
Droidloom stops, leaving your ADB server and other devices running.

Developer installations also need `/usr/bin/adb`, a writable home directory,
and a running systemd user manager for the configured cell owner. A new ADB
server runs in a separate `droidloom-adb-server.service` user unit, so stopping
Droidloom preserves that server. An existing ADB server is reused.
If these prerequisites are unavailable, Android still starts
and `journalctl -b -u droidloomd.service` reports `automatic ADB unavailable`.
Install `android-tools`, fix the owner's home permissions, then restart
Droidloom to prepare the key and connection. Custom ADB server addresses and
custom key directories are not selected by the system daemon.

## Stop, restart, upgrade and remove

```console
droidloomctl stop
droidloomctl start
```

Startup is manual: app launch does not currently start a stopped runtime.
Stopping releases the runtime's resources and closes its Android windows.

To upgrade, install the new matching package pair with the same `pacman -U`
command, then run `droidloomctl start`. The transaction stops Droidloom and retains
Android data. Installing an older pair restores software, not changes to app data.

To uninstall:

```console
sudo pacman -R droidloom-runtime droidloom-image
```

Sudo lets pacman stop the system service, remove package-owned files and update
its database. Removal cleans exported desktop launchers and the generated service
permission. **Android app data and personal configuration are retained**, so a
reinstall is not a factory reset. Back them up before any deliberate data reset.

For launch failures, collect diagnostics before restarting:

```console
droidloomctl status
droidloomctl logs -n 500
droidloomctl crashes com.whatsapp
journalctl --user -u droidloom.service -n 100 --no-pager
journalctl -b -u droidloomd.service -n 200 --no-pager
```

These commands do not require sudo. Replace the example package in crash reports
with the failing app. See [KNOWN_ISSUES.md](KNOWN_ISSUES.md) for current limits.
