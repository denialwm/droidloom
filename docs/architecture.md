# Architecture

Droidloom runs one Android userspace per configured Linux user. Android supplies
Bionic, ART, Binder, Zygote and system_server. The privileged supervisor creates
namespaces, image mounts and networking; the desktop presenter and app catalog
run as the desktop user. This is shared-kernel isolation, not a VM.

Each top-level Android task has a host-managed logical display and one ordinary
Wayland `xdg_toplevel`. Android Composer and the Rust graphics bridge exchange
task buffers, lifecycle and input over private Unix sockets. The presenter uses
DMA-BUFs and explicit synchronization. Compositor geometry is sent back to Android;
there is no combined Android desktop window or compositor plugin.

Host focus, resize, close and minimize use the existing authenticated Composer
task-control endpoint to reach the Rust backend inside system_server. ATMS/WMS validate the
registered task incarnation and apply normal Android window/lifecycle operations
directly. A bounded acknowledged queue coalesces obsolete updates; socket waits
stay outside the WMS lock. A task's first resize skips the 80 ms debounce: it
still has Android's provisional freeform launch bounds, and SurfaceFlinger does
not present a new task until its layer bounds match the window's configured
size. That initial wait is bounded at 250 ms, then the task is presented anyway
and the mismatch is logged. Later resizes, including interactive desktop
resizing, are not gated. InputFlinger owns the native input listener; the
framework policy handles power changes. IME and SystemUI use authenticated
descriptor delegation for direct host channels, without a standalone input bridge.

Minimal Android SystemUI supplies system UI support and clipboard/notification
adapters. The catalog exports Android activities as XDG desktop launchers.
Launching uses an already-running cell. See [desktop integration](desktop-integration.md).

## Build and source boundaries

`droidloom-package` runs the Rust builder in rootless Arch Podman and produces
runtime and image packages. `droidloom-update` owns source preparation, modified
Android builds, host compilation and assembly, plus a separate developer installer.

The build reuses pinned Android base partitions and rebuilds modified components.
Source locks live in `android/manifest/`; modules are declared in `Android.bp`
and Android product files. Unified packaging currently targets x86_64; specialist
ARM64 workflows remain in `tools/`.

- `runtime/droidloom-supervisor`: privileged lifecycle, control protocol and CLI.
- `runtime/droidloom-applications`: desktop launcher catalog.
- `runtime/droidloom-package-support`: first-user setup and package lifecycle.
- `graphics/droidloom-wayland`: windows, input and desktop integration.
- `graphics/`: Composer, private transport and synchronization libraries.
- `android/framework/`: framework API adapters, native transport, IME and minimal system apps.
- `android/device/`: product configuration, overlays and image contents.

The development supervisor constructs private device nodes from the host's
actual device geometry. Loop-device minors account for `loop.max_part` rather
than assuming consecutive whole-device minors. The selected DRM render node
uses a private inode with Android-accessible permissions and the same device
number; host node permissions remain unchanged, and no card/KMS node is exposed.

The ARM64 product builds native Freedreno and Panfrost rendering, plus Zink/Turnip
for split display/KGSL hosts. The cell's optional `graphics_backend` defaults
to `drm`. Explicit `kgsl_dma_heap` selection exposes private character nodes
for `/dev/kgsl-3d0` and `/dev/dma_heap/system`, verified against their kernel
identities. It selects Zink GLES and minigbm's linear DMA-heap image allocator
for Android children. It does not rename the display driver or grant KMS access.
Explicit `drm_dma_heap` selection exposes only `/dev/dma_heap/system` alongside
the selected render node and uses the same linear image allocator. Mesa retains
native driver selection; this supports Panthor hosts without forcing Zink.
With `drm`, the cell gets `/dev/dma_heap/system` for Codec2 media buffers when
the host has it, verified the same way. A host without it still starts the
cell; apps then cannot decode audio or video.

The image backend preserves minigbm's native-handle ABI, AIDL allocator and
stable-C mapper. RGB, HDR, YUV and blob layouts share DMA-BUFs directly with
the GPU; CPU locks retain DMA-BUF cache synchronization. Imports require one
shared allocation, linear modifiers, valid strides and non-overlapping planes.
Images are bounded to 16384 pixels per dimension and 1 GiB per allocation; R8
blobs may exceed the image width limit. Protected allocations are rejected.
The host presenter still allocates final Wayland targets. Linear images add no
required CPU copy, but their bandwidth cost needs workload-specific measurement.
Passing host feedback/syncobj checks alone does not establish guest GPU support.

## Input ownership and recovery

Android verifies injected touchscreen state per display/device, not per task.
The native policy therefore permits one active task gesture on each display;
other tasks' contacts on that display stay suppressed until their physical
UP or CANCEL, even if the owner releases first. Separate displays retain
independent gestures. Pointer IDs and actions are validated before mutation.

Motion state advances only after InputDispatcher accepts the injected event.
CANCEL carries Android's `FLAG_CANCELED` and the exact held pointer set; the
native adapter preserves these flags. Injection uses asynchronous dispatch
(`NONE`), whose return still includes synchronous input-verifier acceptance,
not eventual delivery to an application. A rejected cancellation retains its
route token and contacts for retry, blocks fresh input until recovery succeeds,
and defers route retirement. This applies to reconnect, task replacement and
display-off recovery, rather than assuming an empty local map resets Android.

The presenter retires shared-display contacts, mouse-as-touch ownership and
deferred clipboard touches on task unmap/removal, route replacement, seat or
touch-capability removal and disconnect. Display-off and invalid input also
clear contact bookkeeping. Compositor cancellation must reach the client even
after its last touch frame was delivered; Denial retains the client grab for
that retirement. Completed-frame, cross-task and rejected-cancellation paths
have executable regressions. These contracts do not establish physical device
or application-level delivery qualification.

## Wayland and Denial

The presenter requires `xdg-shell`, `linux-dmabuf` v4 feedback and
`linux-drm-syncobj` synchronization. Each task has a bounded, generation-scoped
target pool for composition fallback. The default task path first resolves
SurfaceFlinger's ordinary layer occlusion and output geometry, then exports
eligible original Android buffers as synchronized Wayland subsurfaces. Denial
samples those buffers in its desktop composition, removing the intermediate
SurfaceFlinger task render. This policy applies to every task and supports
multiple layers; it never checks application names.

The authenticated Composer broker delegates a pair of task-scoped sequenced
sockets. Image planes are registered once, imports are confirmed asynchronously,
and a complete staged layer list commits atomically under the current configure.
The root keeps the XDG role and task-relative input. A constant dark gray (#0d1117) XRGB SHM
buffer supplies an opaque backing; its reduced dimensions preserve the task's
aspect without allocating a full-window image. This matches the gray fill in
SurfaceFlinger's composed task output, including gaps between producer layers.
Switching back to a composed target damages the whole root: Android's damage
rectangles describe differences between app frames, not replacement of the
presenter's constant backing. Hidden children stay mapped and show one
transparent pixel instead: a compositor may show a newly mapped surface's first
buffer a frame late, and an empty full-window layer reads as a black flash.
Already-hidden children are not hidden again on subsequent composed frames.
Child input regions are empty. Source rectangles, orthogonal transforms, stacking,
opaque coverage, per-layer alpha and constant-color layers are preserved. An
unchanged scene retains its existing attachments instead of generating new work.
Opaque ARGB/ABGR Android layers use an XRGB/XBGR import view of the same planes
and modifier, preserving Android's ignored-alpha semantics without a copy. A
blend-mode change retires the old view only after its reads complete; rejected
format reinterpretations select normal composition fallback.

Each source allocation owns an independent Wayland syncobj timeline. The original
producer fence gates the host read. Only a signalled compositor release point
completes the corresponding Android read ticket. As soon as the compositor
materializes a release point, the presenter exports its real `sync_file` without
waiting for it to signal. SurfaceFlinger merges that fence with Android's release
dependencies for the exact GraphicBuffer ID and producer frame number, including
layer destruction. Until a fence exists, only dependent callbacks are deferred;
no host-read wait occupies the shared transaction callback worker. Completion
callbacks retain order within each listener while other apps continue. Commit
callbacks always retain surface/frame metadata and never depend on release.
Socket acknowledgements and presentation timestamps do not release a source.
Disconnects leave missing fences unresolved; delivered kernel fences continue
protecting producer reuse independently of the socket.

Scenes fall back as a whole for unsupported formats, failed imports, protected
or secure content, HDR/LUT/color operations, effects, front buffers, duplicate
source attachments, or geometry that cannot be represented exactly. Fractional
host scales must preserve every layer edge in integer Wayland surface coordinates.
The fallback renders into a host target and hides the direct children in the
same root commit. Receiving a direct commit first drains older Composer records
to preserve ordering across both sockets. The stream bounds scenes to 64 layers
and caches to 128 source allocations per task.

The duplicate headless bootstrap draw is also disabled by default; its HWC
scheduling remains active. `persist.vendor.droidloom.elide_bootstrap=false`
restores that draw for comparison.

The same invisible output can also complete without acquiring or submitting a
discarded client target. One private fence-only request transfers the actual task
completion sync-file to Composer before HWC validation, which can itself present
the frame. This also covers frames without client composition. An already-complete
frame signals a fresh point on a dedicated timeline, preserving per-frame
completion timestamps while reusing the DRM objects. It advances the display
surface, keeps the actual task completion fence on the normal client LayerFEs,
and retains HWC validation, presentation, power-hint fences and layer release
processing. Prediction and
already-dequeued targets retain buffer-backed completion. Ordinary displays and
capture outputs do not select this path. Set
`persist.vendor.droidloom.bootstrap_bufferless=false` to restore the bootstrap's
buffer round trips without enabling its duplicate GPU draw. Fence handoff failure selects the
buffer-backed path. The private bridge is version 4; SurfaceFlinger and Composer
must be updated together.

`persist.vendor.droidloom.direct_layers=false` selects composition fallback for
all tasks. Missing host alpha-modifier support also selects fallback. Existing
host targets remain subject to the original configure and release rules.
SurfaceFlinger reads these switches once, at its first frame; a change takes a
Droidloom restart.

Opaque coverage belongs to each rendered frame, independently of its ARGB
allocation format. SurfaceFlinger's normal task output now includes an opaque
gray fill below the app layers. The private
`PresentWithContent` request carries that opacity beside the buffer ID and
original ready fence. Legacy `Present` requests conservatively mean unknown
opacity. Unknown content bits and nonzero reserved fields are rejected.

Composer sends the existing `SetContentState::OPAQUE` metadata before the
corresponding host `Present` on the ordered task transport, updating it when
opacity changes. The Wayland presenter applies an opaque region in logical
surface coordinates to that commit and clears it for transparent successors;
resizing invalidates the cached region. An ARGB buffer with opaque pixels
therefore need not trigger unnecessary compositor backdrop work. Transparency
within a normal app blends over its other layers and gray backing; legacy
targets still honor their supplied opacity metadata. This requires no application-specific override,
pixel readback, extra composition pass or compositor modification.

Each target owns its own Wayland syncobj timeline. Acquire and release points
alternate only across reuses of that same buffer, after its preceding release
has completed. A newer buffer's acquire fence or out-of-order release must not
advance another buffer's release state. This follows linux-drm-syncobj's
per-buffer timeline recommendation and preserves GPU fencing without a CPU copy.
On kernels with syncobj eventfd support, completion of the buffer's release
point wakes the presenter directly. Older kernels retain a bounded polling
fallback. Waking alone never releases a buffer: the actual release timeline
point must have completed. Presentation feedback is independent and never
holds a reusable target. At most 64 outstanding feedback requests per task
retain frame identities and timing metadata, without retaining buffers or
fences. In portable mode, full queues omit the optional timing request until
capacity returns. Exact timeline mode waits for capacity so every consumed
grant has a completion record. Late `Presented` events remain valid after
`BufferReleased`. The optional [frame timeline](contracts/frame-timeline-v1.md)
targets ordinary root and changed child buffer commits with compositor-issued
identities; missing globals retain the portable presentation path.

Negotiated protocol 1.8 `SCENE_PRESENTATION` extends timing feedback to direct
layer scenes. The presenter requests feedback on one changed, nontransparent
child before its synchronized commit; an occluded carrier root is not used as
the timing source. Up to eight callbacks per task retain only scene/configure
identities and feedback proxies. Portable mode omits optional observations when
full; exact mode waits for callback capacity. In either mode,
unchanged layers acquire no extra commits. Discarded, unmapped, stale-configure
and departed-output observations are dropped. `ScenePresented` feeds Composer's
host timing observer using the real monotonic display timestamp; it neither
creates a composed frame nor releases a buffer. The existing unreliable-present-
fence declaration remains necessary because GPU completion is not display time.

With negotiated protocol 1.7 demand allocation, geometry changes publish a new
configure without allocating a composition pool. SurfaceFlinger prepares output
geometry and tries direct layers before dequeuing any target. Only an actual
composition dequeue requests the current generation's three host targets; one
request per generation bounds retries. Bootstrap uses the same demand path when
a caller dequeues a target. Older peers retain eager allocation.
Stale requests and closing windows allocate nothing. Resizing retires older
targets under their existing GPU release rules; a pool already needed for the
current geometry remains available for later fallback. Pending source imports
can still require fallback, so savings depend on the workload. This removes
unnecessary allocations and target borrowing, not producer buffers or GPU fences.

The presenter sleeps until a Wayland, Android transport, text-input, clipboard,
or release-eventfd event arrives. It has no periodic idle poll. A 4 ms timeout
is used only while a busy target lacks kernel release notifications; queued
input waiting for clipboard synchronization retains its existing ten-second
deadline. Pending Wayland callbacks are dispatched before sleeping, and output
backpressure waits for socket writability. These waits preserve frame and
clipboard ordering without checking sockets and GPU timelines 250 times per
second on a still screen.

Diagnostics are off by default. Each process reads them once, when it first
checks: `DROIDLOOM_TRACE` in the host environment and
`persist.vendor.droidloom.trace` inside Android, each a comma-separated list of
`frames`, `audit` and `input`. A change takes a Droidloom restart. Without them
a check costs one load of a constant, and no diagnostic state is kept.

`audit` logs aggregates at most once a second: the presenter's
commit-to-reported-presentation, feedback-delivery and buffer-release timings,
its clock audit, Composer's vsync audit, SurfaceFlinger's task work counters and
release backlog reports. Presentation timings exclude Android's earlier
input/render work, and buffer-release ages can include idle time while a
compositor retains the current image. No input or application content is
logged.

`frames` enables correlated per-frame records in both the
Android Composer service and `droidloom-wayland`. Every eighth task-local frame
is sampled with its existing object/frame/buffer IDs; no wire protocol or
release policy changes. The SurfaceFlinger broker additionally records its
display ID, which the `android_submit` record maps to the same host object.
All event times use `CLOCK_MONOTONIC` nanoseconds. Android and the host must
share that clock (as in the standard cell); journal delivery time is not a
rendering timestamp.

The recorded path is:

1. `sf_bridge`: first output-target acquisition request, successful target
   reply, retry count, receipt of SurfaceFlinger's rendered target, and ACK.
2. `android_submit`: Composer submission entry, acquire-fence import/export,
   and socket-send interval.
3. `host_receive`, `native_present_begin`, `wayland_commit`, `wayland_flushed`:
   host receipt, fence export, surface commit and successful connection flush.
4. `android_gpu_complete`: original RenderEngine fence's kernel signal time,
   queried without waiting when the buffer returns. `gpu_complete` is the
   host-side fence snapshot when a real timestamp survives DRM transfer.
   Placeholder/stub fences do not establish a GPU completion timestamp.
5. `presented`: compositor-reported display timestamp and local callback time,
   refresh period and display sequence; `discarded` identifies skipped frames.
6. `release_observed`, `buffer_return_sent`, `android_reusable`: independent
   observation of the release point, return-message completion, and Android's
   target becoming reusable. `feedback_pending` records whether presentation
   feedback was outstanding when release was observed; it no longer gates
   reuse. Observation times are upper bounds on the
   corresponding kernel readiness/delivery events, not invented fence times.

The earliest event is the broker receiving SurfaceFlinger's output-target
request. App input, Java/UI work and app RenderThread work before that request
are outside this trace; do not call the total input-to-display latency. The
target-reply-to-present-receipt interval includes SurfaceFlinger submission
and IPC, and is not exclusively GPU execution. Buffer reuse may include idle
time while a compositor retains the current image.

Log writing uses a bounded 256-record channel and a separate thread; producers
use nonblocking sends. Per-process output is capped at 65,536 records, and
sequence numbers plus `dropped_total` expose loss. Original sampled fences
are retained only within the existing bounded in-flight target pool. Query
errors produce unavailable-timestamp records, never zero-duration work.
Tracing is disabled by default and does not log input contents or app pixels.

The shared Rust timing library also records sampled stage durations in
SurfaceFlinger and the direct-layer presenter. These use `CLOCK_MONOTONIC`
elapsed time and `CLOCK_THREAD_CPUTIME_ID` CPU time. CPU time covers the measured
thread, including its system calls; it excludes worker threads and GPU execution.
Nested stages overlap, so their durations must not be added to the composite
total. See [frame-stage measurement](BUILDING.md#frame-stage-timing-baselines)
for capture, interpretation and comparison.

Denial handles Droidloom windows through ordinary Wayland focus, input, tiling
and resize paths. No Droidloom compositor plugin or alternate window role is
required. Mobile integrations use the same surfaces and seat; the optional
[keyboard-dismissal extension](contracts/text-input-v1.md) provides feedback to
Android without changing ordinary text-input semantics.

The user service starts Android after presenter readiness and stops the cell
when the presenter stops. See [security requirements](threat-model-v2.md) for
the shared-kernel trust boundary.

Droidloom retains each direct task's CompositionEngine, capture output, and
LayerFE identities across buffer-only frames. A target rotation swaps the
render surface's buffer; changes to size, crop, layer stack or pacesetter
recreate the output. Layer membership changes and interrupted submissions
invalidate its geometry history. Snapshots are refreshed in place and retired
layer identities are removed.

Fallback SurfaceFlinger presents carry bounded buffer-coordinate damage through
Composer to Wayland. Unscaled, uncropped buffer updates preserve producer
damage; other content changes use the changed layer's visible coverage.
Geometry changes, recovery, and effects that sample outside that coverage
retain full-target damage. Every rotated host target is still completely
rendered, so damage is relative to the preceding submitted frame and never
assumes a buffer age. Legacy private presents retain full damage; explicit
empty damage means unchanged content.

Each native task's refresh follows its entered Wayland outputs, with the
presentation-feedback output taking precedence when a window spans monitors.
Before feedback, the most recently entered known output supplies cadence;
unmapped tasks default to 60 Hz. Enter, leave, hotplug, and mode changes update
cadence without replacing the configure's target allocation generation.
Android's headless bootstrap uses the fastest active task's cadence to support
the shared scheduler without selecting an unrelated faster monitor.
The Composer refresh path also updates the enabled vsync period and
reannounces the existing Android display configuration, so SurfaceFlinger
reloads its cached mode period. Disabled clocks remain disabled.

The same output picks the task's density. The presenter reads
`$XDG_CONFIG_HOME/droidloom/density-v1.json`, which gives dots per inch per unit
of output scale: `dpiPerScale` for every output and `outputs` by connector name
for one. A task on an output of scale 1.5 with 230 per scale gets 345 dpi, so
its contents keep their logical size on every screen. The private protocol
carries it as `Density` (minor 11, capability `TASK_DENSITY`), sent before the
configure it belongs to; the framework sets it as the task's requested
`densityDpi` override, or drops the override for 0 or the display's own
density. The file is watched, so a change reaches open windows at once.
Without it, every task keeps Android's display density.

### Vsync

With Denial's frame timeline, Android has no clock of its own. The presenter
subscribes to the pacesetter output, the one whose refresh the bootstrap
display takes, and forwards each grant to Composer (`FrameGrant`, protocol
1.9). Composer reports the vblank before the grant's latch to SurfaceFlinger
as a vsync, moved early by SurfaceFlinger's 1 ms phase offset plus a 2 ms
budget. SurfaceFlinger's predictor fits Denial's phase-locked grid, and its
frame wakes 2 ms before Denial latches, so it is targeted at that latch.
Grants for other periods are not delivered.

Every timeline subscription is armed only while Android draws. After 250 ms
without a frame the presenter disarms them all at once; Denial, the presenter,
Composer and SurfaceFlinger then get no wakeups, and SurfaceFlinger keeps its
learned model. The next frame is committed untargeted at once and arms them
again. Powering the Android display off also ends its vsync. Without the
frame timeline global, Composer runs a timer seeded from presentation feedback.
With the `audit` diagnostic, each second with grants, Composer logs
`Droidloom vsync audit` (grants delivered as `ticks`, delivery after Denial's
tick as `wake_late`) and the presenter logs `Droidloom clock audit` (frames,
untargeted frames, frames late for their grant, and how long before the latch
targeted frames arrived).

ActivityTaskManager now owns task publication inside system_server. Its task
callbacks enqueue a coalesced worker; the worker copies attached task identities
under WMS's lock and uses the Rust registry/transport after releasing that lock.
Host launches use a root-authenticated Rust RPC inside system_server, whose
ActivityStarter completion callbacks identify the exact resulting task. Setup
and density use existing framework APIs. The external observer, launcher
executable, command children, discovery loops and task-dump parsing are removed.
Host focus/resize/close commands use a separate subscription on
the existing Composer task-control endpoint and run directly inside ATMS/WMS;
the existing framework owns input setup, power and authenticated host-channel
delegation. The standalone input bridge and byte-copy relays are removed.
Package/user/locale broadcasts wake a native catalog subscription whose descriptor
is delegated to the host catalog. It takes snapshots at connection and changes,
with batched icon fetches and no healthy idle polling or app_process startup.
SystemUI sends an initial notification snapshot followed by changed posts and
versioned removals; unchanged entries do not re-encode their icons.
The lifecycle daemon blocks on its control listener, a pidfd for the exact
owned namespace child and a sticky eventfd written by SIGTERM/SIGINT. There is
no periodic idle health timeout. Child exit still triggers teardown, and
shutdown requested before entering the wait remains observable. Explicit
startup/readiness deadlines are unchanged. Isolated binary checks observed
zero idle wakeups over one second and clean shutdown for both signals; tests
also cover child replacement and cancellation races.

SurfaceFlinger retries unregistered tasks every 250 ms initially, then every
five seconds after 24 attempts. Delayed composition callbacks allow an idle
task to recover after late registration. The broker's explicit export allowlist
continues to exclude unsolicited HOME and system tasks.

## Render/input fast-path work

The Moto two-app stall capture found 112,239 target-acquisition failures in a
roughly 15-minute host log, amplified into over 900,000 log lines. Sampled
buffers remained unavailable for up to 1.73 seconds after observed compositor
release while waiting for presentation feedback. The presenter now separates
those lifetimes as described above. A task's `dequeueBuffer` returns
`WOULD_BLOCK` on pool exhaustion; SurfaceFlinger preserves the previous image,
invalidates that task for a full retry, schedules the next composition and
continues processing other tasks. Normal backpressure no longer logs an error
on every attempt. The bootstrap window retains its blocking contract. These
changes require user-owned device validation; they do not establish how much
of first-image loading is app decoding, allocation or GPU upload work.

Render and input receive loops reuse caller-owned packet storage. Direct-layer
requests use a 1260-byte buffer matching their bounded scene-chunk limit; general task and
SurfaceFlinger connections retain maximum-sized storage across requests.
The transport still rejects truncation and closes rejected descriptors.

Direct-layer views cache their applied geometry independently of buffer IDs and
read tickets. Buffer-only frames retain stacking, position, viewport, opacity,
alpha and transform state, avoiding repeated opaque-region creation. Geometry
changes still update that state; root state is reapplied after composition
fallback. Layer staging retains its allocation across commits. Idle cached
source imports no longer incur an eventfd read on every presenter wakeup.
Acquire/release dependencies and per-buffer attachment rules remain unchanged.

SurfaceFlinger retires imports from removed layers or obsolete allocation sizes
on the next scene, instead of keeping every resize allocation until the 128-entry
protocol cap. Compatible producer pools retain up to three recent spare imports
per layer, with a task-wide spare budget of 12 imports and 96 MiB of mapper-reported
allocation storage. Current and host-busy allocations are exempt from eviction;
only a successful host unregister permits dropping the owning GraphicBuffer.
Busy or pending imports retry after completion, including when the visible scene
is otherwise unchanged. The existing protocol cap still bounds total admission.
These limits bound spare retention per task, not all GPU memory or all clients'
descriptor usage. Focused regressions cover repeated resizing, delayed releases,
removed layers, count/byte pressure, actual descriptor closure and stable rotating
producer pools; they do not substitute for device resize validation.


The timeline bridge now retains one private binary transfer syncobj per timeline,
created lazily and reused under exclusive mutable access. After the first use,
each fence import/export avoids a syncobj create/destroy pair (four fewer DRM
ioctls for one import plus one export). The scratch handle is never shared with
the compositor. Linux exports a reference to the current fence into a sync_file;
replacing the scratch object's fence does not change previously exported files.
This follows the kernel's [syncobj import/export semantics](https://raw.githubusercontent.com/torvalds/linux/master/drivers/gpu/drm/drm_syncobj.c).
The opt-in DRM regression also checks scratch-handle stability across 128 buffer
cycles; it remains hardware-dependent and was not run in the host test suite.

Each direct-layer source retains its availability eventfd across reads. Rearming
drains the previous notification before registering the new timeline point, so
an immediate new notification cannot be consumed accidentally. An unsupported
kernel is remembered for that source rather than retried on every frame. Host
tests use real eventfds to cover reuse, stale/immediate notifications, unsupported
registration and error propagation. Availability still never proves completion.

The presenter retains its poll descriptor and layer-task vectors between wakeups.
Layer and text-input descriptor enumeration uses iterators rather than temporary
vectors, retaining the existing poll readiness and idle-sleep behavior.

Poll readiness now schedules work for the owning task and reason: layer requests,
layer fence progress or composed-frame releases. Unrelated input and clipboard
wakeups no longer probe every task's layer socket or DRM timelines. Configuration
publication, window unmapping and deferred destruction are scheduled explicitly;
configuration changes consumed during a cross-socket barrier are published in the
same turn. Pending DMA-BUF imports pause request polling and explicitly resume
queued commands when their Wayland callback completes.

Poll still enumerates registered stream descriptors and outstanding reads; this
change removes idle per-task processing rather than replacing poll with epoll.
Only signalled eventfds are drained. Real timeline queries continue to govern
buffer reuse. Kernels without the required eventfd support retain a four-millisecond
fallback for affected tasks, using an absolute deadline so unrelated traffic cannot
postpone it. Closing windows retain their outstanding release watches until safe
destruction. Host tests cover real socket/eventfd readiness among 64 streams,
disconnects, work coalescing, import resumption, cross-window configuration work,
fallback deadlines and watch removal. CPU and latency gains remain unmeasured.

Window configure and fractional-scale callbacks retain the latest requested
geometry per task. After dispatching the queued Wayland events, the presenter
applies each task's final size and scale once, before draining Android frames
or publishing direct-layer configuration. SCTK still acknowledges every XDG
configure. Unspecified axes inherit the latest queued size; scale events cannot
overwrite it with the previous applied size. Input scaling and refresh-only
updates use the applied scale until the new geometry is installed.

Intermediate sizes allocate no fallback targets and send no Android geometry
update. A batch that returns to the applied size and scale retains its pool;
otherwise the presenter publishes the final geometry and retires the previous
pool. With demand allocation, a new three-target GBM pool is created only when
composition requests it; older peers allocate it immediately. Busy retired
targets still wait for their real GPU releases. Close cancels queued geometry
before allocation, and removal/disconnect clears its scheduled work. Window-size
persistence follows the final XDG state in the batch. The hidden Android bootstrap
canvas retains immediate output configuration. Host tests cover independent
windows, unspecified axes, scale/configure ordering, scale-only changes, return
to applied geometry and close cancellation. The user confirmed the local desktop
resize build works; allocation savings and latency gains remain unmeasured.

Direct-layer views cache both effective Wayland geometry and attached content:
the buffer allocation/read-ticket pair or a solid color. An unchanged child
issues no attachment, damage or commit requests when a sibling changes. The
root still commits each accepted scene to apply synchronized child updates and
stacking atomically. Reusing an allocation for a new read ticket, changing
geometry or color, and switching between buffered and solid content still
commit. Detach clears the content identity so returning from composed fallback
reattaches unchanged solids as well as buffered layers. GPU reads and releases
remain independently tracked; skipping a child commit never releases a read.

When Android removes a layer, the presenter detaches its buffer through the
synchronized child state and retains the subsurface for reuse. Destroying a
subsurface unmaps it immediately, so destruction after queuing a parent commit
could expose the backing while replacement buffers wait on GPU fences.
Reassigned views refresh geometry and stacking. The retained pool is bounded
by the largest accepted scene (at most 64 layers) and is destroyed when the
task retires.

When a direct scene contains a fully opaque layer covering the entire task,
the root retains its last composed image beneath the synchronized children.
This avoids exposing a blank root while the compositor switches textures and
child layouts, and avoids the SHM attachment and upload on that transition.
Partial or transparent scenes still attach the constant gray backing, as do
initial direct scenes without a preceding composed image. The retained root
uses the existing render target and normal release fences; no copy, additional
target or early release is introduced.

An asynchronous scene that needs an allocation whose previous GPU read is
still pending waits in the task's existing bounded staging area. The presenter
keeps the preceding scene visible and retries on real release events; it sends
the commit result only after attachment or explicit rejection. Configuration,
cancellation and disconnect handling remain active while it waits. Resize and
close revalidate the pending scene. Unsupported content and legacy synchronous
peers retain composed fallback. No additional render targets or timer are used.

SurfaceFlinger also gates each task before copying its layer snapshots or
preparing its output. Retained ordered layer/buffer/frame identities and Android
change flags detect task work. Geometry, visibility, effects, front-buffer and
sideband updates remain active. Host configuration, drained imports, deferred
or rejected commits and fallback recovery advance an asynchronous work revision
before scheduling composition. Successful direct or fallback work accepts only
the revision captured before preparation, preserving racing wakeups. Static
fallback tasks skip dequeue/render/queue as well. Global lifecycle and release
processing still runs. With the `audit` diagnostic, counters report skipped
tasks, copied snapshots and prepared tasks without an idle timer.

Private-socket wire tests observe zero requests for two static children across
100 updates to an animated sibling, while the changed child and root each
commit 100 times. Regressions also cover new tickets on reused allocations,
geometry changes, solid/buffer transitions, detach/reappearance and failed
updates. This reduces Wayland protocol and state-processing work; no frame-time
gain has been measured. Partial-damage transport is deferred for the current
Denial target, and changed layers retain full-surface damage.

The layer stream advertises optional `ASYNC_STAGE` capability bit 2. Updated
SurfaceFlinger sends opcode 7 for each layer without waiting for individual
acknowledgements, then waits for the existing complete-scene Commit result.
For a changed scene with cached imports, staging alone reduces synchronous
replies from N+2 to two (Config and Commit); allocation registration is
separately confirmed according to negotiated import capabilities.
Peers lacking the capability keep opcode 4 and its per-layer replies. Scene
overflow is sticky through Commit and releases all rejected staging resources;
Abort and disconnect discard staged metadata without releasing existing reads.
A lost Commit reply still leaves submitted source dependencies unresolved.

Optional `SCENE_BATCH` bit 6 replaces the individual stage records with chunks
of at most sixteen layers and four indexed producer fences. The last chunk
shares a packet with CommitAsync, so a small cached scene needs one send and
one receive dispatch. C++ encoding storage survives between frames. The host
retains the existing bounded staging area, atomic acceptance, read-ticket
ownership and fallback ordering barriers. Without bit 6, the legacy staging
and commit path remains available. Tests cover chunk limits, descriptor identity,
invalid indices and overflowing scenes; operation counts are not latency claims.

Optional `CONFIG_EVENTS` capability bit 3 removes the remaining per-frame Config
query after initial subscription. The host publishes revisioned configuration
changes on the existing completion socket; the sender caches them and schedules
composition when they change, including when an app is idle. The revision orders
the initial reply against events on the other socket and covers visibility
changes without a new configure serial. The host publishes after draining all
layer barriers, including configuration changes consumed from the Composer
socket during that drain. The repaint callback is created once with the stream.
Commit still checks the current host
configuration. With these two extensions and cached imports, a changed scene
requires one synchronous reply (Commit); an unchanged scene sends no layer requests.
Legacy hosts retain per-frame Config queries. These are protocol operation
counts, not measured end-to-end latency improvements.

Optional `ASYNC_COMMIT` capability bit 4 removes that last normal-frame wait.
Opcode 9 carries a nonzero, increasing 64-bit scene ID followed by the existing
Commit configuration and layer count. Acceptance or rejection arrives as event
0x8006 with the same ID and a status; it never means GPU completion. The sender
allows one unacknowledged scene per task. Further compositions coalesce until
acceptance schedules another composition using the latest Android snapshot.
Rejected scenes release only their newly tracked, never-attached reads and force
a composed fallback; reused reads retain their original GPU dependencies.
Status -95 means the host cannot place the arrangement at all, such as a layer
edge off its logical pixel grid: the sender composes it without asking again
until its layers, size or configure serial change. Status -11 rejects only that
scene, and the next composition asks again. Once a frame is composed, the
sender asks again only after the scene's shape (layers, placement, size and
configure serial) has held for 100 ms, so a moving scene stays composed instead
of switching paths every frame.
Invalid, duplicate or mismatched results fail the stream without releasing
ambiguous reads. Transitions to composed targets wait for outstanding acceptance
before sending on Composer's socket, preserving ordering across both sockets.
Cached, eligible changed scenes now need no synchronous reply; legacy registration
and mode-transition barriers remain synchronous. Peers without bit 4 retain Commit
replies. End-to-end latency and coalescing under host contention remain unmeasured.

Optional `ASYNC_IMPORT` capability bit 5 removes the per-allocation registration
wait. Opcode 10 prefixes the existing allocation metadata with a strictly
increasing, nonzero 64-bit attempt ID; event 0x8007 carries that attempt, allocation
ID and signed status. Each task permits 16 pending imports and retains at most
128 cached allocations, including failed attempts. The host continues reading
requests during asynchronous imports and matches out-of-order Wayland callbacks
to their original attempts. Legacy opcode 2 still pauses request processing until
its import callback replies on the request socket.

SurfaceFlinger stages no source reads until every allocation required by the
current scene has imported successfully. Pending or failed imports select composed
fallback so other tasks keep progressing. Draining an import batch schedules one
new composition from the latest Android snapshot; no old frame is retained for
later submission. Failed imports remain cached until ordinary eviction, avoiding
repeated retries. Pending imports cannot be retired or reinterpreted; stale,
duplicate, mismatched or malformed results fail the stream. Success proves only
import usability, and never releases any producer buffer. GPU read tracking and
the existing cross-socket composition-mode barrier remain independent. Older
hosts retain synchronous registration. Startup and resize gains remain unmeasured.

The Android sender's packet helper uses a stack header plus payload iovecs,
avoiding its former packet vector allocation and payload copy. A host C++ test
applies the actual 0007/0008/0011/0012 patch chain and exercises the resulting transport:
64 queued layers with descriptors before any reply, Commit rejection, the
legacy acknowledgement path, and send failure. It also checks configuration
subscription, a newer resize event racing the initial reply, visibility changes,
duplicate/invalid revisions, callbacks outside the cache lock, and 1,000 cached
configuration reads that send no requests. Async regressions cover submission
before any result, bounded outstanding scenes, coalesced wakeups, strict result
identity, rejection ownership, duplicate results and disconnected sends. Import
regressions cover 16 registrations with descriptors before any reply, out-of-order
results, strict attempt/allocation identity, scene gating, cache bounds, failed
import retention, legacy acceptance/rejection and transport failure. Rust tests
cover the wire contract, bounded callback tracking, legacy polling suspension and
pending-resource cleanup. Run the C++ tests with:

```console
cargo test --locked -j 1 -p droidloom-update --test release_queue
```

Rust regressions cover both stage opcodes, descriptor and flag validation,
sticky overflow, resource cleanup and staging-capacity reuse. These tests do
not replace a full Android build or on-device rendering acceptance.

Configuration-event validation passed 15 surface-bridge tests, 39 Wayland tests
(including integration tests), and two C++ regressions. The complete nine-patch
SurfaceFlinger sequence also applies to the local upstream checkout in an
isolated temporary directory. `cargo check` passes for the presenter; Clippy
completes with existing warnings. The full x86_64 package build for 0.1.0-14
compiled the modified Android targets, including SurfaceFlinger and Composer.
That build caught and corrected explicit AOSP weak-pointer construction and
Rust 2021 syntax compatibility in the optional Composer key tracing. Device
latency measurements remain pending.

The Moto ARM64 build was deployed as `fastpath1` at
`/var/lib/moto70-droidloom/fastpath1`. Activation verified Android boot completion,
the running input bridge, a 14-app catalog, all staged Android files as mounted
inside the cell, and running presenter/daemon/catalog executable hashes. The
phone boot ID and Denial PID remained unchanged. GApps, userdata and the existing
private host Mesa configuration were retained. The prior cell configuration and
service definitions remain available for rollback. Local deployment evidence is
under `.work/moto-fastpath-deploy/`; this readiness check does not establish
touch-to-display latency or visual acceptance.

Input logs are off by default. The `input` diagnostic logs every input record
the presenter sends, with its event time and its age at receipt, and the text
input editor's state changes. Composer no longer forwards input records; the
native receiver replaces the Java bridge's per-key path.

Host regression tests cover reusable record boundaries, truncated-FD cleanup,
geometry stability across buffer rotation, and resize/opacity/solid transitions.
The explicit local benchmark is:

```console
cargo test --locked --release -j 1 -p droidloom-denial-ipc benchmark_reused_record_storage -- --ignored --nocapture
```

Six alternating local trials of 50,000 112-byte socket records measured median
send/receive cost of 2.56 microseconds with the allocating receiver and 2.04 with
reused storage (about 20% lower). This isolates local socket/buffer work; it is
not a frame-rate or touch-to-display measurement. The x86_64 Android module build
has passed; visual acceptance and latency measurements on the target remain
necessary to validate the deployed behavior.

## CPU placement and Android task profiles

The host presenter and Android graphics services are separate scheduling
boundaries. A placement policy inside Denial affects neither the user-managed
`droidloom.service` nor the cell owned by the system `droidloomd.service`.
Restricting the entire cell to one CPU class would also restrict Android init,
Zygote, application processes and unrelated services.

### Implemented policy

`runtime/droidloom-cpu-placement` separates worker roles from Linux topology
discovery and Android controller placement. Roles are declared at process or
worker creation; there is no thread-name scanner or polling daemon.

| Role | CPU domain | Owned work |
| --- | --- | --- |
| Graphics | Every capacity tier above the smallest | Presenter event loop and render/submission dependencies; Composer; SurfaceFlinger, RenderEngine and timing workers; native input receiver |
| Normal | Original inherited CPU domain | Lifecycle helpers before exec; ordinary Android foreground work |
| Background | Smallest capacity tier | Lifecycle daemon; application catalog; clipboard/notification transfer workers; frame-log writers; explicitly minimum-priority Mesa queues |

The host backend reads `cpu_capacity` for the CPUs allowed at startup. It keeps
prime cores in the graphics class, respects the inherited domain, and does not
guess topology from CPU numbers or frequency. Homogeneous domains need no
partition. Missing topology produces a diagnostic and preserves inherited
masks; a platform can supply the paired `DROIDLOOM_BIG_CPUS` and
`DROIDLOOM_LITTLE_CPUS` overrides. `DROIDLOOM_CPU_PLACEMENT=off` disables the
capacity partition. No Motorola model, serial, or CPU-number condition is part
of the runtime policy.

The lifecycle daemon captures its original domain before moving to background.
Its child-command wrapper restores that domain with one affinity syscall before
exec, so Android init and command helpers cannot inherit little-only placement
accidentally. The presenter initializes its background notification reactor
before restoring graphics placement and initializing GBM/EGL. Other owned
workers declare their role at entry, before spawning dependencies or doing work.

The Android backend in `runtime/droidloom-supervisor/src/cpu_placement.rs`
prepares private legacy cpuset and blkio subtrees before entering the cgroup
namespace. Creating a new legacy hierarchy after `CLONE_NEWCGROUP` failed with
`EPERM` on the tested kernel. The backend mounts each controller in the private
mount namespace, creates a cell subtree below the caller's existing group,
joins it, and retains only that subtree at a private holding path. The temporary
full-hierarchy discovery mount is detached before Android starts. Declared
`/dev/cpuset` and `/dev/blkio` mount points remain unmounted until Android's
`CgroupSetup` runs: an already-mounted v1 descriptor otherwise makes AOSP return
early and skip its v2 subtrees. Android mounts its own group root normally. The separate v2 freezer domain is retained. Cleanup removes only
empty, cell-owned legacy groups.

Android is told only about controllers the cell provides: blkio and cpuset when
their subtrees are ready, and the v2 freezer. The legacy cpu hierarchy cannot be
created from the cell's cgroup namespace, and v2 memory cannot be delegated below
the namespace root, which holds Android init. The supervisor removes profile
actions and attributes on any other controller. Otherwise each would fail, and
log, at every process start and state change.

Android retains its native task-profile API. Generated init policy assigns
top-app to graphics, foreground to normal, and background/system-background/
restricted to little. Existing CPU `JoinCgroup` actions are translated to these
cpuset groups; non-placement actions and aggregate profiles are preserved.
Blkio is prepared because Android's scheduling aggregates include I/O actions.
This supplies their existing private policy, without adding frequency boosts or
utilization-clamp tuning. A host which reserves these controllers for cgroup v2
or disables them receives an explicit backend diagnostic; the implementation
does not take controllers away from host services. Such platforms need another
Android backend. This is not a claim of compatibility with every Linux kernel.

Composer requests `MaxPerformance`. SurfaceFlinger's main,
RenderEngine and timing workers use the graphics group. Runtime generation
merges the existing SurfaceFlinger service stanza, preserving other options and
bootstrap imports. Owned Java notification and clipboard workers enter Android's
background service group and use background priority. Arbitrary app internals,
ART/GC/JIT workers, and unrelated services retain Android's process-state policy.

Mesa patch `android/aosp-patches/0014-mesa-background-cpu-placement.patch`
applies a platform's optional background policy at queue entry, after Mesa's
affinity reset and before the first job. It affects only queues explicitly
marked minimum priority. Zink cache-put is marked accordingly; cache lookup,
compilation and submission remain on their creator's graphics domain. The hook
also removes inherited FIFO/RR scheduling from these background queues.
`MESA_BACKGROUND_CPUS` selects the host mask; `MESA_BACKGROUND_CPUSET` selects an
Android tasks file. The Android builder applies this patch automatically, and
SurfaceFlinger's generated service supplies the cell path. Host platforms must
provide the same hook in their Mesa build to obtain the Mesa worker exception;
an unpatched host Mesa does not consume these variables. No global loader
interposition or thread-name inference is used.

Android application process-state transitions can override per-thread cpuset
requests, including those of the replacement SystemUI's bridges. Their explicit
background priority remains useful, but startup role assignment alone is not a
persistent little-only guarantee for AMS-managed app processes. This is an
observed limit; no thread-name sweep or repeated affinity repair hides it. The
hard graphics/background separation is verified for the host presenter and
init-managed Composer and SurfaceFlinger. Input now runs inside system_server;
there is no standalone input bridge process to place.

Focused native tests cover topology partitioning, invalid masks, worker and
exec inheritance, profile translation, and service-stanza preservation. The
non-UI `cpu-placement-probe` example checks real controller remount isolation,
big/little/normal transitions, inherited masks, and unchanged v2 membership.
Run it as root only inside a disposable private mount namespace; it is not a
visual test. The Mesa worker test is documented in `android/mesa/README.md`.

### Performance hint sessions

Android's hint sessions name the threads that produce a client's frames and
report how long each frame's work took against a target. HWUI opens one per
window for its UI thread and RenderThread when `debug.hwui.use_hint_manager`
is set, which the vendor image does. The AOSP example power HAL accepted the
reports and did nothing, so frame threads started every burst of work at
whatever utilization the kernel last measured: often on a little CPU at its
lowest frequency. On the Moto, Telegram's dropped frames were "slow issue
draw commands": RenderThread and driver work on CPUs often at 480 MHz. Fixed
floors of 512 on those threads made it smooth.

`android.hardware.power-service.droidloom` implements Power AIDL V7. The
host-testable controller in `android/power/droidloom-power` turns sessions into
`sched_util_min` floors, which act only while a thread is runnable:

| Event | Floor |
| --- | --- |
| Session opened, CPU_LOAD_RESET | At least the baseline, just above the smallest tier |
| Work over its target | Raised by half the overrun's fraction of the target, 16 to 256 units |
| Work under 70% of its target | Decays by 4 units per report |
| CPU_LOAD_UP / CPU_LOAD_DOWN | Plus or minus 128 |
| CPU_LOAD_SPIKE | Ceiling for the next work, which is not learned from |
| User interaction (PowerManager INTERACTION boost) | At least midway between baseline and ceiling, and no decay below that until 1.5 s after the last input, as Denial holds its own floors |
| No report for 10 target periods (100 ms to 2 s), pause | Removed; the learned floor returns with the next report or hint |
| POWER_EFFICIENCY, close, thread removed | Removed |
| Battery saver (LOW_POWER mode) | Capped at the baseline |

The ceiling is 80% of the largest capacity tier with at least two CPUs:
schedutil then runs that tier at its highest frequency. A higher floor would
push both of an application's frame threads onto a single prime CPU. On the
Moto (capacities 364/829/1024) baseline, interaction and ceiling are 428, 545
and 663. A thread in several sessions takes the highest of their floors.
Real-time threads never go below the kernel's RT default.

Mesa's gallium driver queue (`<process>:gdrvN`) performs the driver half of
every GL frame, but no Android API can name it. Each session therefore also
floors the driver queue threads of its own process, found by that name at
session creation, on RESET/RESUME hints and at most once per second while the
session has a floor. This is the only thread-name match.

The service runs as `system` with `CAP_SYS_NICE` and `readproc`, and accepts
sessions only from root or `system` callers, which have validated the threads.
It works because the development cell has no user namespace, so Android's
`CAP_SYS_NICE` is the host's. The production UID map will need a host-side
actuator behind the same `Actuator` trait. The service advertises no message
queue channel, CPU/GPU headroom or composition data, and returns a channel
without an event flag, which HintManagerService treats as "Binder reports".
`dumpsys android.hardware.power.IPower/default` prints sessions, floors and
the applied clamps.

### Implementation validation on Moto

The final `cpu-placement2` bundle booted Android successfully with all three
Droidloom host services active. Running executable/APK/JAR hashes matched the
staged manifest. Denial's PID and the phone's boot ID stayed unchanged. A final
passive snapshot found presenter 3 graphics / 10 background threads,
SurfaceFlinger 23 / 4, and Composer 9 / 1. Graphics allowed CPUs 3-7 and
background 0-2; Android init retained 0-7. Both host Gallium and Turnip mapped
from Droidloom's private prefix. Minimum-priority Mesa workers were SCHED_BATCH,
nice 19. SystemUI's four bridge workers had nice 10 but retained the observed
Android-managed 0-7 cpuset limitation described above. Existing timer-slack
writes still report read-only procfs errors; this does not imply CPU placement
failed or establish complete support for every task-profile action.

The focused native tests, Mesa worker test, ARM64/Android builds, APK signer
verification and disposable controller integration probes passed. The raw logs,
exact artifact manifest and rollback instructions are retained outside Git at
`/mnt/development/moto70edge/droidloom-prep/cpu-placement-implementation/`.
No UI test event was generated, and no frame-time gain is claimed from these
health and affinity checks.

### Initial inspection (before implementation)

A passive Moto Edge 70 inspection on 2026-09-08 found a concrete integration
gap. The kernel exposes `cpu` and `cpuset`, but Android's `/dev/cpuctl` and
`/dev/cpuset` are ordinary tmpfs directories. Their foreground/top-app/background
subdirectories have no controller files. The private cgroup2 mount is rooted at
the host's `/system.slice/droidloomd.service`; its available controllers were
only `memory pids`, with no CPU or cpuset controller enabled for the cell.

`runtime/droidloom-supervisor/assets/development-cgroups.json` marks the legacy
CPU, cpuset and blkio controllers optional, and declares only freezer and memory
in its v2 descriptor. `bind_development_cgroups` in `development.rs` installs
this descriptor over Android's `system/etc/cgroups.json`. This lets Android
start on the unified host, but does not supply the missing CPU policy backend.
The live journal contains failed `HighPerformance`, `ProcessCapacityHigh` and
`MaxPerformance` profile applications. Other actions such as thread priority
can still work; this is not evidence that every Android scheduling mechanism
is inactive.

The installed Android `task_profiles.json` still maps capacity profiles through
`JoinCgroup` to the missing cpuset controller: `ProcessCapacityLow` to
`background`, `ProcessCapacityHigh` to `foreground`, `ProcessCapacityMax` to
`top-app`, and `ServiceCapacityLow` to `system-background`. CPU performance
profiles similarly refer to the absent `cpu` controller. Android already
provides the profile abstraction and foreground/background transitions; its
backend needs to be connected to the actual delegated host hierarchy. See
[AOSP's cgroup abstraction contract](https://source.android.com/docs/core/perf/cgroups).
Profile actions must be audited individually so a placement repair does not
silently introduce frequency boosts or unrelated resource changes.

The installed Composer init service and repository
`android/device/droidloom_arm64/droidloom-composer.rc` declare
`task_profiles ServiceCapacityLow`. That currently does not enforce little
placement because the controller is missing. Once capacity groups work, blindly
mapping `system-background` to little would classify this graphics-critical
service incorrectly. Give Composer an explicit graphics placement role and
separate its housekeeping work.

### Initial proposed allocation boundaries

| Work | Initial placement and implementation boundary |
| --- | --- |
| `droidloom-wayland` event loop, input delivery, DMA-BUF/fence transport, Mesa submission `zfq` and mixed compilation `zcfq` | Big default before GBM/library initialization, with explicit worker exceptions |
| Presenter notification bridge, clipboard transfer workers, `dl-frame-log`, Mesa cache-put `zcq` and minimum-priority disk/trace queues | Little at owned worker entry; bounded compatibility handling for Mesa |
| Presenter async/D-Bus helpers and unidentified threads | Retain big until their full responsibilities are established |
| Android Composer Binder work, `droidloom-vsync`, `droidloom-denial-events`, SurfaceFlinger listener and `droidloom-sf-task` handlers | Big; they carry timing, buffer and release dependencies, including direct-layer transport |
| Composer task-control listener/clients | Keep big initially: configure/registration/activation are visible launch dependencies |
| Composer `dl-frame-log` | Little |
| SurfaceFlinger, RenderEngine, scheduler/timing lanes and dependent Mesa rendering/submission workers | Big graphics domain; classify housekeeping separately |
| Android app UI/RenderThread, input dispatcher and Droidloom input/text bridge | Use Android state/explicit roles with a functioning backend; do not restrict all of system_server or Zygote |
| `droidloom-applications` launcher/icon catalog | Little; reconciliation runs on events and recovery |
| `droidloomd` management work | Medium initially; little is possible only after restoring the original domain before cell/helper launch |
| Other Android apps/services, mixed ART worker pools and unknown workers | Keep Android's state-aware policy; no blanket little classification from daemon/JIT/GC names |

The host clipboard code has four unnamed `std::thread::spawn` sites in
`graphics/droidloom-wayland/src/clipboard/mod.rs` (native receive, Android
receive, Android send and offer writing). Add explicit role/name registration
there before treating the identically named live threads as proven clipboard
workers. Notification and frame-log workers are already named. Java workers
remain in `HostInputMethod` and the SystemUI adapters. The standalone bridge's
text and byte-copy workers are removed; framework delegation handles setup only.

There is also a priority-inheritance concern distinct from placement:
SurfaceFlinger's live `surfacefl:zcq0` cache-put worker had `SCHED_FIFO` priority
2, as did rendering workers. The disk and trace queues had `SCHED_BATCH`, nice
19. Thus neither ordinary nice 0 nor an inherited RT policy alone identifies
work that deserves a big core. Do not extend Denial's priority demotion to
arbitrary Android threads; retain known graphics/real-time roles and explicitly
handle any proven background inheritance.

`start_development_cell` launches `ip` -> `unshare` -> supervisor -> Android init
without resetting affinity. Launch/catalog now use native RPC and retain
system_server's existing worker placement. A future little-only lifecycle daemon must
restore the original application domain before cell launch, otherwise it
can inadvertently confine the entire Android workload to little. Zygote-spawned
apps have their own Android process-state policy; process ancestry is not a
sufficient role declaration.

### Inspection evidence and limits

Boot `ecbda8eb-af2e-49ae-952c-437c8efbae34`, Moto serial `ZY22MMG59D`.
The initial process listing contained 89 processes and 1301 threads across the
three Droidloom roots and their descendants. These counts include the Android
system and apps, not just Droidloom-owned workers. A later affinity inspection
of 427 tasks across 12 selected processes found every sampled mask at `0-7`.
Snapshots are not atomic: workers appear/disappear, so counts differ between
the initial listing and the per-process affinity read.

Key host PIDs and initial thread counts were presenter 1135 (12), lifecycle
daemon 1133 (1), catalog 1345 (1), Android init 1346 (3), Composer 2254 (11),
input bridge 2288 (18), SurfaceFlinger 6590 (32), and system_server 6675 (208).
Android namespace PIDs differ: init is 1, Composer 650 and SurfaceFlinger 4059.
SurfaceFlinger main, RenderEngine and Mesa submission/GL/driver workers were
last observed on little CPUs despite FIFO priority 2. Last CPU is a snapshot,
not a residence-time or frame-cost measurement.

Presenter SHA-256
`57efaf0da2e57891ec3c087992360426e7ed043d13a1d81bf62627d1f480e68b`
matches the retained `root-aspect/stage/droidloom-wayland` artifact.
Composer SHA-256 is
`ed29841ad0a65bdaa4bf82ceaa84d81adbb317341785a14cf8d7010842e96fed`;
SurfaceFlinger SHA-256 is
`ce495505acb8a0057b73033c4513cb6a95bd729effc783a1883c2a1ddeb4e1ac`.
The current checkout has existing development edits; this inspection does not
assert that its complete source tree matches every installed binary.

Raw process, affinity, installed profile/mount and journal evidence is retained
outside Git at `/mnt/development/moto70edge/droidloom-prep/cpu-placement-audit/`.
No runtime code, affinity, controller configuration, service, Android image or
frequency policy was changed during this inspection. No UI event was generated.
No performance gain is claimed until the user exercises the Android frame path
with comparable before/after timing data.

The implementation should first establish the host presenter's owned worker
roles and reuse the native affinity backend, then repair the Android cell's
controller/profile integration and add explicit graphics roles. The current
kernel already exposes the relevant controllers; this finding does not justify
a kernel upgrade. Verify effective masks, profile success, child inheritance,
foreground/background transitions and background exceptions before treating the
Android integration as complete.

## Host service integration

Proposed 2026-09-12 for SMS, calls and audio; narrowed to audio on 2026-10-21.
This section describes ownership, not implemented capability. The audio bridge's
status is in the [audio proposal](audio-integration-proposal.md).

### Telephony is out of scope

Decision of 2026-10-21: Droidloom does not handle telephony (calls, SMS, MMS or
the modem). Core telephony stays native. The Phonewave modem service
(`phonewave/services/modem/`) owns the modem and offers its own API to the
native Dialer and Messages apps. Droidloom only runs optional third-party
Android apps, and its image declares no telephony or telecom features.

This withdraws the earlier plans for a Droidloom radio HAL, a host
communications service with a shared message store, and a ModemManager
backend.

### Product contract and ownership

Droidloom should make Android a client of Linux-owned services. Android apps
retain Android APIs, permissions and familiar behavior; native Linux clients
can access the same underlying devices without Android running. The lifecycle
supervisor continues to construct cells; it must not become the audio engine
and notification server in one privileged process.

| Responsibility | Authority |
| --- | --- |
| Physical audio devices and stream graph | PipeWire with host session-manager policy |
| Android app permissions and audio APIs | Existing Android framework plus narrowly scoped adapters |
| User assignment, grants, privacy and presentation ownership | Host policy plus Android app permissions |
| Cell construction, authenticated endpoint delegation and teardown | Existing Droidloom supervisor |

### Service boundaries

Use Rust for new host services, transport codecs/state machines, and Android
AIDL adapters where supported by the pinned product. Retain the existing
Android framework. PipeWire and the underlying Linux libraries remain their
existing implementations; this is not a rewrite of all dependencies in Rust.

- `runtime/droidloom-audio`: per-user PipeWire integration and negotiated stream
  control in its own process and failure domain.
- Android audio service: implements the stable AIDL audio contracts on private
  Binder and speaks the delegated host transport. Keep APKs unmodified.

Use existing Linux APIs southbound: PipeWire, WirePlumber/session policy,
desktop notifications and logind where available. Do not expose the
unrestricted host system/session bus inside Android.

Session agents handle user audio. A mobile installation may keep an assigned
user's audio session alive by policy; a desktop must not silently create a root
PipeWire instance.

Reuse protocol/authentication primitives from graphics, but give audio its own
endpoint. PCM must not pass through JSON or D-Bus method payloads. Use bounded
shared-memory audio buffers and negotiated formats with control messages on a
separate channel. Measure the actual copy count and latency; zero-copy is not
established by choosing shared memory alone.

### Audio

The Android audio HAL should expose negotiated host outputs/capture to ordinary
Android apps. PipeWire carries audio; WirePlumber or the selected session manager
owns device selection and access policy. Linux routing changes, headset removal,
Bluetooth profile changes, mute and microphone revocation must be reflected
back into Android. Android audio focus and host stream policy need an explicit
mapping, so a native call or other host stream can interrupt or duck Android
media without two independent managers fighting over global volume. Android
master mute must not silently become system-wide mute.

The initial bridge maps Android mix streams to PipeWire through a dedicated
shared sample pool. Host loopback tests validate pool negotiation in both
directions; see [implementation and tests](audio-integration-proposal.md#implemented-bridge-and-host-checks).
Per-Android-app volume,
routing and microphone attribution are a later explicit milestone: AudioFlinger
may mix apps before the HAL, so creating PipeWire nodes at HAL level alone does
not expose each app. Additional framework stream/identity plumbing may be needed.
App labels supplied by a compromised cell are not trusted host identities.

### What Linux-generic means

The first supported host profile is Linux with PipeWire and the relevant
session services. Other backends can implement the same contracts; do not
implement speculative ALSA backends before needed. Generic does not mean every
distribution already exposes the required media APIs.

No Moto model names, Qualcomm nodes, driver commands or kernel release checks
belong in Droidloom core. Those remain in host drivers and distribution/device
packages. Source should be buildable for ARM64 and x86_64 with separate
dependencies for optional audio. Capabilities distinguish unsupported, absent,
temporarily unavailable and ready; feature presence is not operational
readiness.

### Policy and validation

Host permission bounds apply in addition to Android permissions. Grant a cell
only its assigned operations, with microphone grants separate from playback.
The current preview's root identities and incomplete isolation are not
production patterns. New device-service authority requires updating the threat
model as specified there before shipping an implemented boundary.

Validate protocol state machines with fake backends, malformed packets, lost
responses, queue limits and crash/replay tests on ordinary development hosts.
Validate PipeWire playback/capture on an ordinary working Linux audio machine
before attributing Moto hardware-driver gaps to the Android bridge. Include
native/Android coexistence, session changes, Android stopped, suspend, reconnect
and actual battery measurements.

### Current evidence from the Moto

Read-only inspection on 2026-09-12 found PipeWire and WirePlumber active, but
`/proc/asound/cards` reported no sound cards and PipeWire exposed only
`auto_null`, with no capture source. The new host-backed HAL has been tested
against an isolated host graph. Real Moto audio therefore needs host hardware bring-up as
well as the portable Android adapter.

References: [audio proposal](audio-integration-proposal.md),
[desktop integration](desktop-integration.md), [threat model](threat-model-v2.md),
[PipeWire session policy](https://docs.pipewire.org/page_session_manager.html),
[PipeWire access control](https://docs.pipewire.org/page_access.html),
[Android AIDL audio interfaces](https://source.android.com/docs/core/audio/aidl-hidl-comp?hl=en).
