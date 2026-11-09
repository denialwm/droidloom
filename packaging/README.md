# Packaging internals

For installation and source builds, use the [Arch Linux guide](arch/README.md).

The native package split is `droidloom-runtime` and `droidloom-image`, with
optional hardware-service packages. `droidloom-runtime` owns the supervisor,
CLI, user lifecycle service, private Android endpoint, and ordinary Wayland
presenter. Denial has no Droidloom package dependency, plugin, service drop-in,
or product-specific environment.

The development endpoint accepts a dynamic peer PID because Android Composer's
host PID is learned only after the cell starts. Kernel `SO_PEERCRED` UID and
GID remain exact. Production packaging must replace the temporary PID wildcard
with supervisor-to-presenter PID registration.

Package lifecycle tests must use a disposable root filesystem and compare
files, units, mounts, namespaces, cgroups, links, addresses, routes, nftables
objects, processes, and devices with the pre-install baseline. Normal removal
retains user Android data. Data deletion must be a separate explicit user operation.

Paths and ownership are frozen in
[`docs/contracts/runtime-layout-v1.md`](../docs/contracts/runtime-layout-v1.md).

## Lifecycle units

`systemd/system/droidloomd.service` owns the privileged, signal-aware Rust
daemon. The daemon holds the exact namespace child process and cleans its
network objects and ephemeral mount paths on normal stop. The namespace owner
also has a parent-death signal, so a daemon crash cannot leave Android running;
the next start removes only deterministically named Droidloom residue before
constructing the cell again.

`systemd/user/droidloom.service` owns the complete graphical runtime. Packaging
leaves it disabled by default. `droidloomctl start` starts the complete runtime
on demand; `droidloomctl stop` stops the presenter, catalog, and privileged
daemon. Application launches use the already-running runtime.
Its main process is `droidloom-wayland`. The service waits for the Wayland
session, connects, binds its private endpoint, and reports readiness before
`ExecStartPost` starts Android. Stopping or crashing the presenter stops the
cell as part of the same unit. The compositor remains untouched.

`systemd/user/droidloom-applications.service` is a non-privileged companion.
After the cell is ready it asks Android for enabled
`MAIN`/`LAUNCHER` activities, labels, and rendered adaptive icons, then
atomically reconciles ordinary XDG entries below
`~/.local/share/applications/` with collision-resistant `droidloom-` names. It periodically checks for package
changes while retaining the last good catalog across temporary Android
restarts. It also sets Android's night mode from the desktop portal's
`color-scheme`, at start and on each change, so Android follows the host's
dark or light theme. The privileged daemon never writes into a user's home
directory.

The user-facing lifecycle is:

```text
droidloomctl start
droidloomctl status
droidloomctl applications
droidloomctl launch org.mozilla.firefox
droidloomctl dpi 175
droidloomctl window-mode fit-output
droidloomctl window-mode windowed --width 900 --height 1200
droidloomctl restart
droidloomctl stop
```

`droidloomctl start`, `stop`, and `restart` control the complete user service.
The internal `--cell` flag bypasses service orchestration for unit hooks and
explicit recovery. Normal application launch requires the session-managed cell,
waits for its exact PID-namespace init, and executes the packaged Android task
launcher through that namespace; it never starts Droidloom. `SO_PEERCRED`
permits only root or the exact
`host_uid` in the validated cell specification. Non-root callers can select
only root-owned, non-group/world-writable specifications below
`/etc/droidloom`; root retains an explicit development override path.

`droidloomctl dpi DPI` applies Android's standard built-in-display density
override. WindowManager persists the
value in the per-user Android data directory, which the installer retains across
Droidloom upgrades and host restarts. The display's pixel canvas remains the
host output's oriented physical resolution; DPI changes UI sizing, not render
resolution.

`droidloomctl window-mode fit-output` is the default mobile policy. For a
size-less initial XDG configure, the presenter waits for the standard
`xdg_toplevel.configure_bounds` event and creates Android's first target pool
at that logical size. There is no small provisional window or visible resize.
If the compositor does not advertise bounds, the current logical `wl_output`
extent is used. The portable 480x800 fallback is used only when neither source
exists, before any buffer is published.

`droidloomctl window-mode windowed --width WIDTH --height HEIGHT` selects a
persistent desktop-style default. Add `--package PACKAGE` to set an explicit
per-application override. Stable compositor-assigned sizes are remembered for
windowed applications and constrained to later XDG bounds. Policy is read at
application creation, never from a render or input loop. Configuration lives
at `$XDG_CONFIG_HOME/droidloom/window-policy-v1.json`; automatically remembered
sizes live at `$XDG_STATE_HOME/droidloom/window-sizes-v1.json`.

## Build and installation ownership

The Rust package builder produces `droidloom-runtime` and `droidloom-image`.
Pacman owns their system files. Rust package support handles first-user setup,
service permissions and transaction hooks. Upgrades stop Droidloom and leave it
stopped; removal retains application data. See the [package guide](arch/README.md)
for commands and exact administrator requirements.

The [Rust updater](../tools/droidloom-update/README.md) also supports a separate
developer installation with versioned releases, transactional activation and
rollback. It refuses to overwrite pacman-managed files. Moving an older developer
installation to pacman needs an explicit migration, not blanket overwrite flags.

## Component installation helpers

`install-runtime`, the component installers and `verify-bundle` support focused
developer installation workflows. The `ime/setup` and `home/setup` scripts run
inside Android and are also used by the Rust assembly.

## Component compatibility

The Rust builder checks required components, executable permissions, architecture
and compiled input ABI metadata, including alternate input-bridge copies. It
accepts compatible byte differences and extra files; pinned upstream inputs
retain their integrity checks. The pacman workflow uses this builder to produce
a matched runtime/image pair.

The retained ARM64 installer uses a sealed manifest with file hashes, modes,
symlinks and input ABI checks. This is specific to that installer, not an added
gate for the private pacman preview. Its Python regressions run from the Rust
repository-contract suite.

Compiled input markers do not cover every private ABI. Package lifecycle checks
must be complemented by live input and resize validation; successful Android boot
or catalog enumeration alone does not establish interaction compatibility.
