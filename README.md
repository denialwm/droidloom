# Droidloom

Droidloom runs Android apps in separate Linux Wayland windows. It uses a shared
kernel Android userspace with real Android APIs, GPU acceleration and a native
desktop application catalog.

<p align="center">
  <img src="assets/screenshots/whatsapp.png" alt="WhatsApp running in a separate Linux desktop window through Droidloom, with its Android process visible in the terminal below" width="640">
</p>

This is an early preview. Prebuilt pacman packages are published for x86_64
Arch Linux and aarch64 Arch Linux ARM. Download them from the
[latest release](https://github.com/denialwm/droidloom/releases/latest), or
install them through the repository as below. WhatsApp and Phone have been
tested on Hyprland, including tiled resizing.

> [!NOTE]
> Droidloom is incompatible with NVIDIA graphics.

## Install the preview

Follow the **[step-by-step installation guide](docs/INSTALL.md)**, including
first-time setup and **[installing APKs](docs/INSTALL.md#5-install-an-apk)**.

1. Download and review the installer:

   ```console
   curl -fLO https://denialwm.github.io/droidloom/install.sh
   less install.sh
   ```

2. Run `sh install.sh` to add the pacman repository and install or update both
   packages. It asks for administrator access and keeps pacman's confirmation
   before a full system upgrade. Droidloom packages are currently unsigned over
   HTTPS; this policy is scoped to the Droidloom repository. Later updates also
   arrive through normal `sudo pacman -Syu`.
3. From your normal Wayland desktop terminal, run `droidloomctl start`. First start
   requests administrator authentication to create Android data, runtime
   configuration and the system-service permission. The guide includes an explicit
   setup command if the authentication prompt is unavailable.
4. Try `droidloomctl launch com.android.settings`, then follow the guide to install
   your APKs with `droidloomctl install /path/to/application.apk`.

Startup is manual; launching an app does not start a stopped runtime. Read
**[known issues](docs/KNOWN_ISSUES.md)** for compatibility limits and workarounds.

## Build it yourself

The **[build guide](docs/BUILDING.md)** covers prerequisites, disk space and package
checks. One Rust command builds the modified Android components, host binaries
and both pacman packages, without host sudo:

```console
cargo run --locked -j 1 -p droidloom-package -- build
```

## Use Droidloom

```console
droidloomctl start
droidloomctl applications
droidloomctl launch com.android.settings
droidloomctl stop
```

The desktop catalog exports app launchers. Clipboard sharing and desktop
notifications are implemented; compatibility depends on the app and compositor.
See [integration and limitations](docs/desktop-integration.md) before trying the preview.

## Development

- [Documentation](docs/README.md): architecture, desktop integration and contracts.
- [Tools](tools/README.md): package builder, source updater and specialist helpers.
- `runtime/`: supervisor, CLI, application catalog and installation support.
- `graphics/`: Wayland presenter, input, Composer and private host transport.
- `android/`: Android components, patches and pinned upstream inputs.
- `packaging/`: pacman metadata, service units and lifecycle integration.

Run `cargo check --locked --workspace -j 1` to check host Rust code. This does not
build Android or install anything. Generated sources, images and packages are
not tracked in Git.

Droidloom's original code is **GPL-3.0-or-later**. See [LICENSE](LICENSE),
[the complete GPLv3 text](LICENSES/GPL-3.0-or-later.txt) and
[third-party licenses](THIRD_PARTY.md).
