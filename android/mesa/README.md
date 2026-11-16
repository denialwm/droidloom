# Pinned Mesa Android build

Droidloom builds Mesa 26.3.0 from the archive and checksums in the source lock.
It uses Mesa's Android Meson wrapper with the pinned AOSP/Bionic compiler and
Android API 37, rather than AOSP's incomplete generated Mesa Soong modules.

`droidloom-mesa prepare` verifies the archive, applies the ordered `patches/`,
and records the resulting tree's identity. The maintained Rust Android builder
in `tools/droidloom-update/src/android.rs` mirrors that tree into its temporary
AOSP projection, applies the runtime patches below, and restores projected
sources after building. The cached ARM64 build uses the same projection.

Panfrost v11 (Mali-G615/G715) is part of the pinned release: its hardware
descriptions, architecture dispatch, command-stream decoder and model entries
need no patch.

The ARM64 product builds Gallium Freedreno, Panfrost and Zink, Vulkan Freedreno
(Turnip), and both MSM and KGSL kernel interfaces. Native DRM remains the cell
default. Droidloom's product does not enable the Mali Vulkan driver.
Explicit `graphics_backend: "kgsl_dma_heap"` selects Zink over Turnip and the
portable minigbm allocator; see `docs/architecture.md` for the device boundary.
No software renderer is included.

Runtime patches under `android/aosp-patches/`:

- `0010-minigbm-dma-heap-images.patch`: opt-in bounded linear image allocation,
  import validation and matching allocation/capability-query checks. Every
  pitch is a multiple of 128 bytes, NVIDIA's linear import requirement and
  twice Turnip's. Planar YVU420/YV12 uses a 256-byte luma pitch alignment so
  both half-width chroma pitches keep it. Android YV12's exact chroma-stride
  rule and unpadded height remain intact.
- `0021-minigbm-vram-images.patch`: with `vendor.minigbm.vram=vulkan`, RGB
  buffers that are only drawn, sampled and shown come from the Vulkan driver:
  an image from its DRM modifier list, device-local memory exported as a
  DMA-BUF, cleared. Mapping one fails. Formats and sizes the driver lists no
  modifier for stay linear DMA-heap images.
- `0011-mesa-zink-kgsl.patch`: when ordinary DRM matching fails on `msm_drm`,
  select a unique Turnip device without DRM identity only if external DMA-BUF
  memory and image modifier extensions are present. Other DRM drivers keep
  their existing matching behavior.
- `0036-libdrm-2.4.134.patch`: AOSP's libdrm is a 2.4.124 snapshot, and
  Mesa's RadeonSI and RADV need libdrm_amdgpu 2.4.133. The patch brings the
  libraries, headers and data to the signed upstream 2.4.134 release and keeps
  AOSP's build files and Bionic changes; its header lists what it covers.
- `0037-drm-ffi-syncobj-point.patch`: the new kernel headers add a timeline
  `point` to `drm_syncobj_handle`; the Rust `drm-ffi` crate sets it to zero.
- `0015-libdrm-empty-version-strings.patch`: applied after the upgrade,
  preserve empty kernel driver version strings as owned empty strings.
  Panthor advertises an empty date; libdrm otherwise returns a NULL date
  (2.4.124 passed the NULL kernel pointer to `strdup` and crashed
  SurfaceFlinger during EGL initialization). This applies to libdrm,
  retaining the real driver name and version without changing the kernel.
- `0016-mesa-turnip-hwui-queues.patch`: two queues in Turnip's VM_BIND graphics
  family, which HWUI's Vulkan pipeline requires (render and upload queues).
  The SET_IOVA family keeps one queue: it recycles freed addresses after queue
  0's fence alone. Shader barrier removal stops at workgroup scope.
- `0018-mesa-ir3-alias-array-sources.patch`: when alias.tex replaces a
  collect with its sources, an array element keeps its array, element and
  mask. The copy used to name array 0, element 0: a wrong read, or a crash in
  DCE when array 0 was already gone (Wuthering Waves on the Pad 4).
- `0019-mesa-turnip-a722-no-bv.patch`: the A722 has no BV pipe, so Turnip
  registers no BV bin preamble and keeps concurrent binning off on it. The BV
  amble replaced the BR one, which restores the CCU's GMEM state after a
  preemption; resumed render passes then wrote tiles to address 0. Same
  change as the Moto's host Mesa (Phonewave recipe `mesa-denial`).

Validate a new host using real Android allocations, raw DMA-BUF and native
Android EGL imports, GPU rendering/readback, native fences and synchronized CPU
visibility. EGL initialization alone does not prove the complete graphics path.
Full Android boot and application presentation remain separate checks; linear
buffer bandwidth and frame rate must be measured under the intended workload.

With the pinned AOSP checkout and host libdrm headers available, validate the
actual allocator and minigbm layout helpers without hardware:

```sh
cargo test --locked -j 1 -p droidloom-update --test dma_heap_layout -- --ignored
cargo test --locked -j 1 -p droidloom-update --test libdrm_version -- --ignored
cargo test --locked -j 1 -p droidloom-update --test panfrost_model -- --ignored
```

This covers the reported 576x1024 YV12 import failure, even widths through
4096, plane alignment/overlap, Android stride requirements, and unchanged
RGB/NV12/P010 layouts. GPU import and app behavior require device validation.
The libdrm test compiles its actual pinned implementation with both patches and
injects DRM version replies covering Panthor's empty date, an empty description,
all-empty strings and ordinary nonempty strings. It opens no device.
The model test compiles the prepared Mesa lookup and checks the tablet's exact
G615 ID and variant, G715, an existing G610, and rejection of unknown variants
and models. The full cross-build separately compiles the v11 rendering paths.

`tests/yv12_import.rs` is an offscreen Android diagnostic for actual
AHardwareBuffer allocation and native EGL import, including 576x1024 YV12.
Build it with the pinned Android Rust toolchain and Bionic libraries. For the
KGSL cell, run it with the same environment as Android init:

```sh
env MESA_LOADER_DRIVER_OVERRIDE=zink vendor.minigbm.allocator=dma_heap_images \
  /data/local/tmp/droidloom-yv12-import
```

It opens no window and does not manipulate apps. Run only within authorized
device validation; successful import does not prove video sampling or app UI.

## Background worker placement

Patch `0014-mesa-background-cpu-placement.patch` adds optional platform policy
at minimum-priority queue entry, after Mesa's full-affinity reset. Zink cache-put
is background work; cache lookup, compilation and submission keep their existing
classification. `MESA_BACKGROUND_CPUS` supplies a Linux CPU list, and
`MESA_BACKGROUND_CPUSET` supplies an existing Android cgroup tasks file. Only
configured background queues are moved and demoted from inherited FIFO/RR to
SCHED_BATCH. Without these variables, the placement hook does nothing; the
cache-put queue's new minimum-priority classification still applies.

Android applies the patch through the maintained source projection. A host Mesa
build needs the same patch to honor the host presenter's worker exception. The
runtime topology/role boundary and Android controller requirements are described
in `docs/architecture.md`.

Compile the non-UI worker test against the patched Mesa source:

```sh
cc -Wall -Wextra -Werror -pthread -I /path/to/patched/mesa/src/util \
  android/mesa/tests/background_cpu.c -o /tmp/droidloom-mesa-cpu-test
/tmp/droidloom-mesa-cpu-test
```

The test checks CPU-list rejection, creator/worker affinity separation and
background scheduling. It does not create a graphics context or UI event.
