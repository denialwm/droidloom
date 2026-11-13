# Unified Droidloom installer and updater

`droidloomctl update` builds and installs a complete runtime, restarts Droidloom,
and checks Android readiness. Automatic startup remains disabled. The updater,
including source preparation, bundle verification, installation, and rollback,
is implemented in Rust. It does not invoke Droidloom's Bash deployment scripts
or its Python bundle verifier.

To compile the audio HAL APEX and `libaudiobase.so` in a prepared workspace,
without installing or restarting services, use:

```console
cargo run --locked -j 1 -p droidloom-update -- -j 4 audio-build --work .work/arch/build
```

Add `--product droidloom_arm64` for ARM64. The builder retains source locks,
projection recovery, build exclusion and the two-CPU reserve. This focused
operation leaves translator development sources and artifact provenance intact.

To rebuild the paired `services.jar` and `libinputflinger.so` containing the
Rust WindowManager policy in a prepared workspace, use:

```console
cargo run --locked -j 1 -p droidloom-update -- --jobs 1 framework-build --work .work/arch/build
```

Add `--product droidloom_arm64` for ARM64. This command uses the same source
projection/recovery and CPU reserve, preserves translator development
provenance, and performs no installation or runtime restart. Bundle verification
requires matching compiled window-policy ABI metadata in both artifacts.
Add `--tasks` to also compile and stage IME, SystemUI, Composer and SurfaceFlinger.
The launch/catalog adapter is part of services; the standalone launcher and
catalog JAR are retired. This validates authoritative ActivityTaskManager
publication, direct framework focus/resize/close handling and layer commits
without building images or installing anything. Their Droidloom shared-library
dependencies are rebuilt and staged with them. All task-control peers
(services, InputFlinger and Composer) must carry task-backend ABI 4.
Services, InputFlinger and Composer also require native input ABI 2; services,
SystemUI, IME and the presenter require host channels ABI 1. The standalone input
bridge process, launcher and byte-copy relays are removed.
Services, InputFlinger, the host daemon and catalog require framework control
ABI 1; SystemUI and the presenter require notification ABI 3.
SurfaceFlinger and the presenter negotiate asynchronous layer commits and bounded
allocation imports separately.

For an ARM64 phone, build everything on the x86_64 workstation as a candidate
of the aarch64 package pair. These four commands are the whole recipe, which
the workspace's `tools/build` runs (see `AGENTS.md`). Every cache is persistent,
so repeating them after a change compiles only what changed:

```console
cargo run --locked -j 1 -p droidloom-update -- --jobs 4 android-build \
  --work .work/arch/build --product droidloom_arm64
cargo run --locked -j 1 -p droidloom-update -- host-build \
  --work .work/arch/build --sysroot /path/to/arm64-root
cargo run --locked -j 1 -p droidloom-update -- candidate-stage \
  --work .work/arch/build --product droidloom_arm64 \
  --host .work/arch/build/cargo-aarch64-unknown-linux-gnu/aarch64-unknown-linux-gnu/release \
  --base /path/to/pinned-base/images --candidate N --destination OUT/payload
cargo run --locked -j 1 -p droidloom-package -- candidate \
  --payload OUT/payload --destination OUT --work .work/arch
```

`host-build` cross-compiles the Linux programs:
- It uses `aarch64-linux-gnu-gcc`, and takes native libraries and pkg-config
  files from `--sysroot`, an ARM64 root filesystem such as an Arch Linux ARM
  root with the target's `-devel` dependencies.
- Its Cargo output is kept in `cargo-aarch64-unknown-linux-gnu` below the work
  directory.

`candidate-stage` lays out the published packages' payload from those builds:
`packaging/cell-spec-arm64.json` is the recipe, images, init and the Android
file mappings, and the package's `cell-template.json` carries it without any
device field. The phone's own setup adds its user, UID, data directory and
GPU, so every phone gets the same packages. `package.json` names the release
and the candidate N; `droidloom-package candidate` packs them with the
release's recipe as pkgrel `R.N`, without compiling anything.

`candidate-stage` verifies pinned base-image hashes and the complete compiled
ABI pairing without reading the workstation's device configuration or
activating either machine. Compatibility declarations are image contents, not device
configuration:
- the vendor image carries the touchscreen and orientation features,
  `ro.opengles.version` and `dalvik.vm.dex2oat64.enabled` (the 64-bit-only base
  has no `dex2oat32`);
- `android.hardware.droidloom_app_compatibility.xml` declares the features Play
  infers from ordinary permissions: Bluetooth, location, Wi-Fi, camera,
  microphone, the common sensors and the standard phone software features.
  The XML explains why telephony, USB, NFC and biometrics are left out.
- the SettingsProvider overlay keeps Bluetooth off on fresh data. Data created
  before that overlay needs `settings put global bluetooth_on 0` once;
- the pinned base disables `android.hardware.location.network` at compile time;
  Play implies it only for apps targeting API 20 or older;
- the compiled `services.jar` reports the detected Vulkan level and version.

Check them in a staged payload with `debugfs -R 'cat /build.prop'` on
`image/usr/lib/droidloom/images/images/vendor.raw.img`.

`android-build` also compiles the system-server jars that depend on Droidloom's
`services.jar`, using the host `dex2oat64` that `android-build` produces from the
base's own source. It reads each base odex for the boot
image, class-loader context, compiler filter, profile and app image to use,
then checks that the new odex records the same boot image checksums and
context. The release's `framework/oat/<isa>` replaces the base directory in the
cell; see the [runtime layout](../../docs/contracts/runtime-layout-v1.md).

On the phone, install both packages with `pacman -U` as root; their hooks stop
Droidloom first and keep its data and setup. Google apps are the
`droidloom-gapps` package, packed for the candidate (`AGENTS.md`).
After a capability change, clear the Play Store cache, as described in
`AGENTS.md`. The ARM64 vendor includes Freedreno and Panfrost GLES; native
ARM execution omits the x86 ISA translator. `graphics_backend=drm_dma_heap`
selects linear DMA-heap images for DRM drivers without a native minigbm backend,
while retaining Mesa's native render-node selection. Kernel and
device activation still follow that target's workspace rules.
Check the live compositor as well as `droidloom-doctor`: the presenter requires
`wp_linux_drm_syncobj_manager_v1`, and Denial's Smithay integration requires
`DRM_IOCTL_SYNCOBJ_EVENTFD` on its synchronization device. Timeline capability
alone is insufficient. On split display/render systems, that device must support
syncobjs; a display-only KMS device can report no support while the GPU render
node supports timelines. TB373FU's initial `6.1.138-tb373fu-droidloom1` passed
the basic host probe but lacked the eventfd ioctl; Denial also selected the
display-only device for protocol initialization. Its matching
`6.1.138-tb373fu-syncobj1` kernel release and Denial's render-node selection
resolve those prerequisites. A successful basic probe does not establish
graphical readiness. Panthor also exposes an
empty driver date, covered by the pinned libdrm patch and regression test in
`android/mesa/README.md`.
The ARM build also prepares Panfrost's shader compiler as a native workstation
tool and passes it explicitly to Meson; no target executable runs on the tablet
during compilation. Optional Mesa dependencies do not trigger fallback downloads.

Bootstrap from the checkout as the desktop user:

```console
cargo run --locked --release -p droidloom-update
```

After installation, from any directory:

```console
droidloomctl update
droidloomctl start                 # desktop by default
droidloomctl start --mode mobile   # fill the available work area
droidloomctl stop
```

The checkout location and desktop owner are remembered during installation.
Updates preserve the mode selected for the current session.
The command builds the current checkout, including uncommitted changes; it does
not fetch or reset the user's Git branch. Administrator authentication is used
for system package installation when necessary and for the final activation.

```console
droidloom-update plan
droidloom-update --build-only
droidloomctl update --clean
droidloom-update verify .work/arch/build/release-<build-id>
```

Compilation leaves two logical CPUs outside the compiler process affinity,
including nested Ninja and JVM workers. Parallel jobs are also capped at
`nproc - 2`, even if a larger `--jobs` value is requested.

Every invocation requests all host programs and the complete Android target
set, including Composer, IME, framework services and its in-process catalog, patched
native platform services, Mesa and the vendor image. Cargo, Soong, Ninja and
Meson determine which outputs need compilation. `--clean` deletes only this
updater's build outputs and compiler cache. Pinned downloaded sources and base
images are retained and verified again. Existing Android application data is
preserved.

The updater rebuilds itself first and hands off to that executable. All mutable
Android artifacts come from the updater-owned product output, after requesting
the full component set. The Git revision is recorded for information; locally
built files do not need to match predetermined checksums. Previously installed
system, system_ext and product partitions can be reused when they match the
pinned base download. Missing base images are downloaded and verified.
Compatibility APEX files are extracted from that base. A base vendor image with
the retired input bridge init service must be rebuilt or have that service
definition replaced as part of a matched development override.

The prepared payload records required files and executable permissions. The
framework input receiver must declare the input ABI matching Composer; the
framework services JAR must contain the compiled navigation policy, and ELF
components must have the expected architecture. Missing or empty components
and incompatible peers reject the update. Retired bridge artifacts also reject
the bundle. Services must contain the compiled application-catalog adapter class.
Each DEX 035–040 member in `services.jar` and the Droidloom HOME, IME and SystemUI
archives must have consistent header sizes and a valid Adler-32 checksum,
matching ART's integrity check. This also rejects damaged compiler outputs whose ZIP CRC and transfer
hashes are valid; rebuild the affected artifact instead of rewriting its checksum.
Compatible byte differences and additional files do not reject a bundle.
Predetermined checksums remain for pinned third-party inputs.

Activation makes a private administrator-owned copy and verifies it again
before stopping Droidloom. Versioned releases live under
`/usr/lib/droidloom/releases`; one `active` symlink selects the complete release.
Stable binary, image and service paths resolve through it. The configuration
is installed as a regular file below `/etc/droidloom`, as required by the
supervisor, and participates in the same rollback transaction. A
persistent transaction journal also records the pre-existing installation's
files and symlinks, so the first migration can be rolled back. A failed start or
empty application catalog restores the previous installation. Readiness also
requires `sys.droidloom.framework_ready=1`, set after native input and authenticated
channel setup. An interrupted
activation is recovered after updater bootstrap on the next update, before
compiling Android again. Rollback restores
programs/configuration; it does not undo writes Android made to application
data while starting.

Build source projections are journaled and restored on success or failure.
Interrupted projections are recovered on the next invocation. Unchanged projected
content retains its modification time, so applying patches again does not
invalidate compiled Android outputs. Build children
inherit the source lock, preventing recovery while an orphaned compiler is
still running. No command restarts the compositor, logs out the desktop user,
changes a kernel, or flashes a device.

The initial source-building implementation targets x86_64 Linux with systemd
and AMD/Intel graphics. Pure-Python build dependencies are downloaded into a private cache from
`android/manifest/build-python-lock.json`, with checksums verified. MarkupSafe
and PyYAML use their supported pure-Python implementations. Missing native
build dependencies are installed automatically on Arch-family hosts. Other distributions need equivalent build dependencies
already available. ARM64 packaging and signed public release distribution
remain separate work; this command does not claim to bootstrap every device.
Catalog readiness is a nonvisual check. It does not establish that mouse,
touchscreen, and resize behavior have passed the user's visual validation.

Run updater regression tests with:

```console
cargo test --locked -p droidloom-update
```

They exercise mixed ABI rejection, missing components, acceptance of compatible
byte changes and additional files,
architecture mismatch, source restoration and recovery, inherited build locks,
and filesystem rollback of both legacy and versioned installations.
