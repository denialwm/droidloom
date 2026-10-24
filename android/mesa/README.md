# Pinned Mesa Android build

Droidloom builds Mesa 26.1.7 from the archive and checksums in the source lock.
It uses Mesa's Android Meson wrapper with the pinned AOSP/Bionic compiler and
Android API 37, rather than AOSP's incomplete generated Mesa Soong modules.

`droidloom-mesa prepare` verifies the archive, applies the ordered `patches/`,
and records the resulting tree's identity. The maintained Rust Android builder
in `tools/droidloom-update/src/android.rs` mirrors that tree into its temporary
AOSP projection, applies the runtime patches below, and restores projected
sources after building. The cached ARM64 build uses the same projection.

`patches/0005-panfrost-v11-support.patch` backports the GLES portion of
[Mesa MR 41803](https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/41803)
(ending at `73210bf69dfa4cfb5d5c5b3b7c6c91a05209e2b6`) to the pinned release.
It retains the tablet's working `c35c76d76250cd089baa9f52f79d1ad2a45468e1`
v11 XML so the shared decoder and format fields match this release. It adds
the v11 hardware descriptions, compiled architecture
dispatch and Mali-G615/G715 model entries. The tablet's native Mesa already
uses an earlier revision of this series. `0006-panfrost-v11-cs-decode.patch`
retains its two command-stream diagnostic dispatch fixes. The changes apply
to the prepared source used by both native shader tools and Android libraries.

The ARM64 product builds Gallium Freedreno, Panfrost and Zink, Vulkan Freedreno
(Turnip), and both MSM and KGSL kernel interfaces. Native DRM remains the cell
default. Droidloom's product does not enable the Mali Vulkan driver.
Explicit `graphics_backend: "kgsl_dma_heap"` selects Zink over Turnip and the
portable minigbm allocator; see `docs/architecture.md` for the device boundary.
No software renderer is included.

Runtime patches under `android/aosp-patches/`:

- `0010-minigbm-dma-heap-images.patch`: opt-in bounded linear image allocation,
  import validation and matching allocation/capability-query checks. Planar
  YVU420/YV12 uses a 128-byte luma pitch alignment so both half-width chroma
  pitches satisfy Turnip's 64-byte linear import requirement. Android YV12's
  exact chroma-stride rule and unpadded height remain intact.
- `0011-mesa-zink-kgsl.patch`: when ordinary DRM matching fails on `msm_drm`,
  select a unique Turnip device without DRM identity only if external DMA-BUF
  memory and image modifier extensions are present. Other DRM drivers keep
  their existing matching behavior.
- `0012-mesa-adreno722.patch`: upstream device entry from Mesa commit
  `803731f7ed4aa8e8265fe0aff08b85efb3f3c7e9`, also used by the validated host
  Turnip build. It retains upstream's initial A730-derived register settings.

- `0013-mesa-texture-upload-span.patch`: preserve the threaded staging layout
  while copying only the valid source span, excluding trailing row/layer
  padding. Prevents guard-page overreads during Skia glyph-atlas uploads on
  the Zink render-pass path; keeps threading and GPU buffer-to-image copies.
- `0015-libdrm-empty-version-strings.patch`: preserve empty kernel driver
  version strings as owned empty strings. Panthor advertises an empty date;
  the pinned libdrm otherwise passes its NULL kernel pointer to `strdup` and
  crashes SurfaceFlinger during EGL initialization. This applies to libdrm,
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
  change as the Moto's host Mesa (Phonewave `mesa-a722` 0002).

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
The libdrm test compiles its actual pinned implementation with the patch and
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
