# GPU forwarding

NVIDIA's driver has no Android build, so on an NVIDIA GPU Android cannot run
its own Vulkan driver. Droidloom gives it one that forwards: every Vulkan call
an Android process makes is replayed by `droidloom-gpu-server` on the host's
NVIDIA driver. OpenGL ES runs as ANGLE on top of that Vulkan.

## Pieces

| Piece | Where | Runs |
|---|---|---|
| `vulkan.droidloom` | `android/gpu/droidloom-vulkan` | in each Android process, as the vendor Vulkan HAL |
| `droidloom-gpu-server` | `runtime/droidloom-gpu-server` | on the host as the desktop user, one process per Android client |
| Shared layout and commands | `graphics/droidloom-gpu-protocol` | both |
| Code generator | `tools/droidloom-gpu-gen` | at development time |
| Desktop test client | `tools/droidloom-gpu-triangle` | on the host |

The driver also runs on the desktop as an ordinary Vulkan ICD, which is how most
of it is tested: `VK_DRIVER_FILES` pointing at a manifest for
`libdroidloom_vulkan.so`, then `droidloom-gpu-triangle --headless` or
`vkmark --winsys headless`.

## Choosing it

One setting picks the GPU Android renders on: `"gpu"` in
`/etc/droidloom/cell.json`, absent for `auto`.

| Choice | Renders on |
|---|---|
| `auto` | the first NVIDIA GPU, else the first AMD or Intel one |
| `nvidia`, `amd`, `intel` | the first GPU of that vendor |
| a PCI address, `0000:04:00.0` or `04:00.0` | that GPU |

```sh
droidloomctl gpu            # list the GPUs and what the choice resolves to
droidloomctl gpu amd        # choose; asks for administrator authentication
droidloomctl restart
```

An NVIDIA GPU renders through forwarding, AMD and Intel natively with Mesa.
With forwarding, the cell's render node stays on an AMD or Intel GPU when the
host has one. Setup (`runtime/droidloom-supervisor/src/host_gpu.rs`) names
GPUs by PCI address, because render-node numbers follow driver load order. It
writes the result into `cell.json` (`render_node`, `graphics_backend`) and the
forwarded GPU into `/etc/droidloom/runtime.env` as `DROIDLOOM_GPU`, which
`droidloom-gpu-server` reads. Package installs and every `droidloomctl start`
resolve the choice again and rerun setup when the host's GPUs changed.

The `forwarding` pairing:

- sets `ro.hardware.vulkan=droidloom` and `ro.hardware.egl=angle` in the
  product properties, which Android loads after the vendor image's;
- binds `%t/droidloom/gpu.sock` at `/dev/socket/droidloom/gpu`;
- allocates buffers from the system DMA heap, so both GPUs can import them;
- sets `vendor.minigbm.vram=vulkan` for Android: RGB buffers that are only
  drawn, sampled and shown come from the Vulkan driver instead, in VRAM, in
  one of the modifiers the server lists. Window buffers are such buffers.
  With a compositor on another GPU the server lists none, and every buffer
  stays in the DMA heap.

`droidloom-gpu.socket` belongs to `droidloom.service`. The first Android
Vulkan client starts `droidloom-gpu.service`, which forks a server for each
connection: a crash stays inside one Android process.

## Reading the logs

- `droidloomctl start` prints the line `Droidloom graphics: ...`, saying which
  GPU Android renders on and whether forwarding is on.
- The cell's start in `journalctl -u droidloomd` has
  `droidloom-supervisor: graphics: GPU forwarding ...` or
  `graphics: native ...; GPU forwarding is off`.
- With forwarding, `journalctl --user -u droidloom-gpu` names the GPU and every
  Android process it serves:

  ```text
  droidloom-gpu-server: forwarding Android Vulkan to NVIDIA GeForce RTX 5060 Ti at 0000:04:00.0 (chosen by Droidloom setup) on /dev/dri/renderD128, ...
  droidloom-gpu-server: serving Android pid 2131652 (/system/bin/surfaceflinger)
  droidloom-gpu-server: serving Android pid 2133040 (com.android.vending)
  ```

  Without forwarding the server never starts.

## Transport

Client and server share one memory region and one Unix socket.

- Calls are records in a ring inside the region. The client writes, the
  server replays in order, and replies land in a per-thread area of the same
  region. Waiting is a short spin, then a futex.
- Command buffers are record streams in fixed-size chunks. A full chunk goes
  to the server at once, so the host replays while the application records.
- The socket carries only the handshake and file descriptors: DMA-BUFs,
  sync files and syncobjs pass as descriptors, never as copies.
- Host-visible memory is shared: the client maps the very pages the GPU
  works on, so mapping needs no call. System memory is a memfd the server
  imports with `VK_EXT_external_memory_host`. Mappable VRAM, which the
  driver offers on a card with resizable BAR, stays the driver's own
  allocation and reaches the client as a DMA-BUF. The GPU works on a VRAM
  buffer without crossing the PCIe link; the CPU reads system memory far
  faster. Applications choose between the two as they do on the driver
  itself.

When the listener starts, a probe exports a page of each mappable VRAM type,
writes through the DMA-BUF's mapping and reads back through the driver's
own. Types that pass are offered as mappable; the start-up line in
`journalctl --user -u droidloom-gpu` lists them beside the system-memory
types. `droidloom-gpu-triangle --headless --check-memory` checks every
mappable type both ways through the forwarding.

`droidloom-gpu-triangle --headless --check-target` renders into an exported
buffer, imports its DMA-BUF a second time the way a compositor does, and
compares the two after each of two frames. With `--vram` the buffer lies in
VRAM in one of the GPU's own DRM modifiers instead of LINEAR system memory;
with a window, `--vram` shows such buffers in a compositor that renders on
the same GPU, which then reads them where they are.

`graphics/droidloom-gpu-protocol/api.txt` lists every forwarded command and
structure. `droidloom-gpu-gen` turns it and `vk.xml` into the encoders, the
server's decoders and the command tables. Flags in `api.txt` mark commands with
hand-written halves: `client`, `server`, `local` (answered without the host)
and `sync` (the caller waits for a reply). The protocol hash covers `api.txt`
and `layout.rs`; the two ends refuse each other when built from different
trees.

## What the server adds

The host driver is not an Android driver, so the server fills in what Android
expects.

- **Window and AHardwareBuffer images.** Gralloc buffers are linear, and
  NVIDIA cannot draw into a linear colour image while a depth or stencil
  attachment is bound. The application therefore draws into an optimal image
  in VRAM, and the buffer is a second image beside it (`shadow.rs`). A window
  buffer gets the frame at `vkQueueSignalReleaseImageANDROID`. An
  AHardwareBuffer gets it when the application releases the image to the
  foreign queue, and gives it back on acquire. A buffer whose handle names
  one of the GPU's own modifiers lies in VRAM: it is the application's image
  itself, with no second image and no copy.
- **The GPU's modifiers.** `VkDrmFormatModifierPropertiesListEXT` answers
  with the host driver's list, so a client can allocate a buffer in VRAM
  with plain Vulkan: an image created from that list, device-local memory
  exported as a DMA-BUF, and the modifier the driver chose read back. The
  DMA-BUF keeps the memory after the client frees its allocation. Android's
  allocator does exactly this (`0021-minigbm-vram-images.patch`), in its own
  process, for buffers nobody maps. Any such buffer may end up in a window, so the list holds only what the compositor
  imports (`layouts.rs`): the server asks it once, at start. A compositor
  that renders on the same GPU takes the GPU's modifiers, and window buffers
  lie in VRAM. A compositor on another GPU takes none of them, the list is
  LINEAR alone, and buffers stay in system memory. The start-up line says
  which. `--all-layouts` skips the question, for tests without a window.
- **Video buffers.** Decoders fill YUV AHardwareBuffers: NV12, YV12 and
  P010. The driver reports them as external formats, which ANGLE samples
  through a YCbCr conversion; a named format would also have to be
  renderable, which linear YUV is not. NVIDIA imports no linear DMA-BUF into a
  multi-planar image, so each plane is a linear image of its own over the
  buffer, with its own import, and the application samples an optimal YUV
  image in VRAM (`shadow.rs`). Taking the buffer from the foreign queue copies
  each plane in; handing it back copies nothing.
- **ETC2, EAC and ASTC.** Android requires them, NVIDIA's desktop driver has
  none. A compressed image is a decoded image plus a buffer holding the
  original blocks (`compressed.rs`). Uploads write the blocks and decode them
  with a compute shader in the application's own command buffer; downloads
  and copies read the blocks, so they stay exact.
- **Timeline semaphores** are kernel syncobjs on the client, fed by the sync
  files of the server's submissions.
- **Presentation** follows `VK_ANDROID_native_buffer`: gralloc DMA-BUFs in,
  sync files out.

## Checking it in the cell

`android/gpu/droidloom-vulkan/tests/yuv_sample.c` samples NV12, YV12 and
P010 buffers through ANGLE the way SurfaceFlinger shows video, including a
frame rewritten under the same texture, and reads the colours back. It opens
no window. Against the installed stack:

```sh
cargo test --locked -p droidloom-update --test gpu_yuv_sample -- --ignored --nocapture
```

The test leaves the probe at `/data/local/tmp/droidloom-yuv-sample`. To try
a driver before it is packaged, run the probe in a mount namespace of its own
with the build bound over the installed driver. Make the namespace's
mounts private first: Android's `unshare -m` keeps them shared, and the bind
then reaches every process in the cell.

```sh
pkexec /usr/local/bin/droidloom-dev shell 'unshare -m sh -c "mount -o rprivate none / &&
  mount --bind /data/local/tmp/vulkan.droidloom.so /vendor/lib64/hw/vulkan.droidloom.so &&
  /data/local/tmp/droidloom-yuv-sample"'
```

A server build replaces the installed one for new clients only through a
user drop-in on `droidloom-gpu.service` that sets its `ExecStart` and
`KillMode=process`: a restart then swaps the listener, and running clients
keep their forked servers. Remove the drop-in together with the next
Droidloom restart, since without `KillMode=process` restarting the service
ends every client's server.
