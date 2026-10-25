# Android apps as ordinary Linux applications

Status: architecture proposal with the PhoneWindowManager replacement,
authoritative task publication, direct host window controls and native input
reception inside InputFlinger implemented in the checkout. The broader
integrations below remain proposed work, not preview guarantees. Existing released behavior is documented in
[desktop integration](desktop-integration.md) and [known issues](KNOWN_ISSUES.md).

## Implementation status

The first slice substitutes `DroidloomWindowManagerPolicy` at the existing
SystemServer injection point when `ro.vendor.droidloom.surfaceflinger_tasks`
is enabled. It implements Android's `WindowManagerPolicy` directly and never
constructs or delegates to PhoneWindowManager on that path.

The decision engine is Rust in
[`droidloom-android-policy`](../android/framework/droidloom-window-policy/src/lib.rs).
It owns screen readiness and callback generations, application/system-key
routing, and overlay authorization decisions. A Java adapter supplies framework
objects and permission facts, calls Android's keyguard, and executes the Rust
engine's effects. The policy JNI carries primitive arguments inside `system_server`;
it adds no service process or socket proxy. Rust links into the existing
`libinputflinger.so`, paired with the rebuilt `services.jar`. InputFlinger already
loads inside system_server and owns the native input callbacks into this policy;
this uses its existing Droidloom build and file override. The runtime
overrides both existing partition files, and bundle verification rejects a
missing or incompatible native policy. Other Android products retain their
PhoneWindowManager selection.

Android still owns AppOps, permissions, credentials, trust and display sleep
tokens. Android keyguard callbacks and drawn-window waits remain part of boot
and wake. Draw timeouts complete display sequencing; they do not authenticate
or unlock a user. Host shell keys cannot invoke the phone's HOME, recents or
power UI. Back and ordinary application/IME keys use Android dispatch; unhandled
media and volume keys reach Android's existing media-session machinery.

The next implemented slice moves task selection and lifecycle publication into
ActivityTaskManager. Its dedicated worker calls the Rust backend directly,
using the existing authenticated Composer task-control endpoint. Host launch
requests use a root-authenticated native RPC inside system_server; ActivityStarter and its launch
completion callbacks supply the exact task, including reuse and trampoline
handoffs. The external TaskObserver and the launcher's stack-list parsing,
discovery loops and `--bind-task` operation are removed. Launch/setup and density
call existing framework APIs through a small Java adapter; Rust owns native
transport and authentication. The launcher executable and command children are
retired. The diagnostic shell command remains available for authorized callers.

The backend copies Android task facts under WMS's lock, then performs bounded
socket I/O outside it. Registrations carry framework task incarnations;
stale removals cannot delete a newer registration. Reconnects revalidate cached
bindings. Protocol 3.1 separates lifecycle publication from activation, so
restoring background tasks does not request focus. Background tasks retain their
windows until detached; HOME, recents,
organizer containers and other users/displays are excluded. A five-second
connection-health check does not enumerate tasks while connected and idle.

Host focus, resize, close, hide and show now travel over a dedicated
subscription on the existing root/system task-control endpoint (protocol 3.3). Rust validates the
native window object and the incarnation captured when the command was queued
against the current registration. ATMS rechecks
that incarnation, attachment, user and display under WMS's global lock, then
calls its normal focus, resize/transition and task-removal paths directly.
The Java input bridge no longer receives these commands or queries task lists
for resizing. Socket waits and acknowledgements run outside the WMS lock.

The bounded Rust command queue preserves the existing 80 ms resize debounce,
coalesces obsolete sizes/focus transitions and retains commands until acknowledged.
Close supersedes its window's pending work; reconnects retry unacknowledged
commands against their original object identity. Idle subscriptions sleep until
an event. Host-originated focus changes do not echo activation requests back to
the compositor. Paired task-backend ABI 3 metadata rejects old/new mixtures.

Keep screen on follows Android: a shown window with `FLAG_KEEP_SCREEN_ON` keeps
the screen on, here per task. Surface placement of the default display reports
those tasks to the backend, which republishes a task when its state changes
(task-control 3.4). Composer sends `SetKeepScreenOn` (protocol 1.10, capability
`KEEP_SCREEN_ON`) and the presenter holds a `zwp_idle_inhibitor_v1` on the
task's surface. The host decides visibility: a minimized or hidden window keeps
nothing on. Android's own obscuring order is ignored.

Task destruction has two phases: request host window destruction once, retaining
the binding to route the displayed frame's final buffer release; then remove the
drained Composer display and binding. A busy teardown keeps the publication
connection and other task identities valid. Reconciliation continues publishing
live apps while a closed task drains, so closing one app cannot strand another
app's resize or focus controls.

Native input owns its listening socket inside
`libinputflinger`. Rust validates incarnation-tagged records and owns gesture,
repeat and modifier state; a small C++ adapter constructs events and calls the
existing targeted InputDispatcher path. WMS publishes bounds and application
tokens on changes, outside its global lock. This removes per-event task-bounds
Binder queries, Java event conversion and the bridge-to-InputFlinger event-parcel
transaction. Task/token replacement and disconnects cancel active input before
releasing the old Binder token. Input ABI 7 and native input ABI 2 are checked
across the compiled peers. The framework policy starts the listener and handles
infrequent power effects on its existing handler.

The presenter now writes input directly to InputFlinger through a delegated
socket. Composer only handles connection handoff and route metadata. System_server
recreates its own listener after restart. Native ownership
preserves InputManager's original class layout for its prebuilt JNI allocator.
The standalone support process and byte-copy relays are removed. System_server
authenticates IME/SystemUI and delegates connected host descriptors; payloads then
travel directly to the adapters. IMMS annotates the validated editor's exact task
ID, replacing IME task-list queries. The catalog adapter lives in services.jar;
package/user/locale broadcasts wake a native subscription whose descriptor is
delegated to the host catalog. Initial/reconnect snapshots and batched missing
icons replace the 30-second polling loop and per-query app_process startup.
SystemUI sends notification snapshots at connection, then only changed posts
and versioned removals. Unchanged entries do not re-encode icon pixels.
The local x86_64 build boots without the retired support process
or init service, with verified direct input/SystemUI socket ownership and catalog
access. Unauthorized descriptor requests and direct host path access are rejected.
Manual launch/editor flows, clipboard/notification actions, recovery and host
display cycling still need acceptance on each target before shipping; latency
and memory gains have not been measured.

## Direction

Build Linux integration into the Android services that already own application
state. Apps retain Android APIs, ART, Binder, permissions and lifecycle behavior;
the Linux desktop owns presentation, physical devices and host policy.

The objective is to make launching, switching, opening documents, recording,
receiving notifications and recovering from failures behave like ordinary Linux
applications. Measure that through complete user workflows and latency, rather
than the number of components removed.

The existing builder already rebuilds `services`, `libinputflinger` and
`surfaceflinger`, and Droidloom already replaces SystemUI. Direct framework
integration fits the current modified-component build model. It will expand
the maintained Android changes and their compatibility tests.

## Move integration to its owner

| Area | Proposed integration point | Result |
| --- | --- | --- |
| Tasks and windows | ActivityTaskManager and WindowManager backends | Publish authoritative task creation, geometry, focus and destruction; retire launcher subprocesses, command-output parsing and discovery polling. |
| Input | Native host receiver alongside InputDispatcher | Feed the existing Android dispatch machinery without routing input through Composer, the standalone Java bridge and additional event-parcel transactions. |
| Clipboard | Backend inside ClipboardService | Preserve Android clipboard policy and URI grants while retiring SystemUI monitoring and the root byte-stream relay. |
| Notifications | Backend inside NotificationManagerService | Export changes after Android policy, preserving PendingIntents, channels and dismissal semantics; retire full listener snapshots and relay hops. |
| Application catalog | PackageManager events and authoritative metadata | Update launchers for package, activity, user and locale changes; keep full snapshots for initial connection and recovery. |
| Audio | AudioFlinger track integration and AudioPolicy backend | Preserve stream identity before mixing so the host can expose individual Android applications in audio controls. |

## Concrete components to replace

The two main complete service-rewrite candidates are SurfaceFlinger for
graphics and AudioFlinger for audio. The most useful first policy substitution
is PhoneWindowManager, whose initial Rust replacement is described above.
The complete SurfaceFlinger and AudioFlinger rewrites remain architectural
options; the framework task integration above retains the current graphics path.

These are different projects with different outcomes. Replacing a policy object,
an input source and a complete Binder service should not be described as the
same operation.

| Component | Proposed substitute | Scope and outcome |
| --- | --- | --- |
| `PhoneWindowManager` | `DroidloomWindowManagerPolicy` implementing `WindowManagerPolicy` | A complete policy implementation inside `system_server`, coordinating Android system-key, display, power and keyguard behavior with host policy. |
| SurfaceFlinger's presentation backend, or eventually the complete `SurfaceFlinger` service | An embedded Wayland backend, or a compatible service using Wayland | Own Android layer transactions and host presentation in one service; can absorb the custom Composer and presenter graphics roles after their contracts are covered. |
| InputFlinger's host-input source | Native host receiver wired into InputManager/InputDispatcher | Retire Composer input forwarding and the standalone Java input-conversion path while preserving Android dispatch, gestures, cancellation and input channels. |
| `AudioFlinger` | Compatible Android audio service backed by PipeWire | A full service rewrite can expose individual tracks to Linux and delegate supported graph processing to PipeWire. Adapt AudioPolicyService/AudioPolicyManager for routes and devices; retain coordination with Java AudioService's focus policy. |
| `DocumentsUI` picker activities for supported requests | Host picker activities plus a scoped DocumentsProvider/content provider | Native file dialogs with ordinary Android URI grants and activity results. This does not require replacing PackageManager or all of the storage framework. |

The policy replacement has an actual insertion point in the pinned source:
`frameworks/base/services/java/com/android/server/SystemServer.java` supplies
`new PhoneWindowManager()` to `WindowManagerService.main(...)`. The replacement
implements the existing interface from
`services/core/java/com/android/server/policy/WindowManagerPolicy.java`.
It must satisfy the policy's boot, screen, input and credential contracts and
account for remaining concrete `PhoneWindowManager` references.

PhoneWindowManager does not own every window-management decision. Task bounds,
activity lifecycle, per-display layout and transitions also live in
WindowManagerService, ActivityTaskManagerService and DisplayPolicy. Pair the new
policy with explicit integration there to make host geometry/focus authoritative
and retire task discovery helpers. Replacing PhoneWindowManager alone does not
remove those helpers or implement native presentation. Preserve Android secure
credential/Keystore semantics even when the host supplies desktop locking.

A complete SurfaceFlinger replacement is the clearest large graphics-service
rewrite. Its Android side must implement the pinned `ISurfaceComposer` and
`ISurfaceComposerClient` behavior used by framework/libgui clients: surface/layer
creation, SurfaceControl transactions, buffer handoff, completion/release
callbacks, display metadata, scheduling and input-window information. Its host
side uses Wayland toplevels/subsurfaces, DMA-BUFs and real synchronization. Reuse
working Android graphics primitives where useful. Keeping Android's public API
names without their transactional and lifetime semantics would break apps.
See [AOSP's SurfaceFlinger and WindowManager description](https://source.android.com/docs/core/graphics/surfaceflinger-windowmanager).

Replacing Composer alone changes the HWC backend, which Droidloom already
customizes; it leaves SurfaceFlinger and the current host boundary above it.
An embedded SurfaceFlinger backend is a smaller route toward the same host
presentation ownership than a complete service rewrite. The presenter's
clipboard, notification and IME responsibilities must be moved separately before
its executable can disappear.

The current direct-layer export path already avoids the intermediate
SurfaceFlinger task render for eligible scenes. A replacement's additional
benefit would come from presentation ownership, fewer process/IPC boundaries and
recovery behavior. It is not a new guarantee of fewer composition passes or
higher performance; both need comparison with the existing direct-layer path.

A complete AudioFlinger replacement must preserve Android AudioTrack/AudioRecord
contracts, shared control/data buffers, track lifetime, timestamps, capture
policy and required effects behavior. It can provide separate PipeWire streams
before Android mixing would erase app separation. Audio policy integration is
necessary alongside it; replacing only the HAL cannot provide that result.

ClipboardService and NotificationManagerService are good places for direct host
backends, but rewriting each entire policy engine is optional and has a much
larger compatibility cost than replacing its host-facing portion. SystemUI is
already replaced in Droidloom. ART, Zygote and the entire ActivityManager or
PackageManager are not prerequisite rewrite targets for this proposal.

For ordinary desktop behavior, prioritize the policy replacement plus WMS/ATMS
integration and native input. For a deliberate whole-service rewrite, choose
SurfaceFlinger for graphics or AudioFlinger for audio, with their neighboring
contracts explicitly included in the scope. Neither rewrite alone supplies
native document handling, desktop permission presentation and app lifecycle
integration.

## Existing paths to retire

### Next overhead reductions

The presenter now connects directly to InputFlinger. Asynchronous direct-layer
commits also remove the normal per-scene acknowledgement wait, with one scene
awaiting acceptance per task, coalesced compositions and separate GPU release
dependencies. Asynchronous allocation imports remove registration waits in
batches of up to 16 per task. Pending or failed imports use composed fallback;
completion schedules the latest scene, whose sources must all be confirmed.
GPU release and composition-mode transitions retain their barriers, and older
peers keep synchronous imports.
The presenter also schedules task work from socket/fence readiness and explicit
resize/close changes, avoiding idle socket and timeline probes on unrelated
wakeups. Poll descriptor enumeration remains, and GPU release ownership is
unchanged. Kernels lacking eventfd support keep the bounded release fallback.
Window geometry is coalesced after each batch of Wayland callbacks: only the
final size and fractional scale reach Android. Negotiated demand allocation
creates the fallback pool only if composition needs it.
Unspecified axes use the latest queued size, and close cancels queued resizes.
In-flight targets retain their real GPU release dependencies.
Direct-layer presentation also skips attachment, damage and commit requests for
children whose effective geometry and buffer/read-ticket or solid color are
unchanged. Parent commits still apply the scene atomically. New read tickets,
geometry/color changes and returning from composed fallback remain observable.
Partial-damage transport is deferred for the current Denial target; changed
layers still receive full damage. The demonstrated saving is protocol work,
with frame-time gains unmeasured. Clipboard changes below remain a lower priority.

Composition targets are now allocated on demand with negotiated protocol 1.7.
SurfaceFlinger tries direct layers from geometry-only output state before
borrowing a target. A fallback dequeue requests three buffers for the current
configure; resize and close races cannot allocate stale pools. Bootstrap uses
the same demand path when a caller dequeues a target; older peers remain eager.
Existing pools and in-flight retired buffers retain their GPU ownership rules.
Import delays and unsupported scenes still use composition, so net memory and
latency savings need workload measurements.
See [render/input fast-path work](architecture.md#renderinput-fast-path-work).

The target for further runtime work is ordinary ARM64 Android phones running
Linux, preserving Android multitasking and background services. Memory-pressure
policy is deferred; reducing cached-app retention is not a performance target.
Partial-damage work remains deferred for Denial. The following runtime targets
preserve that policy; items 1, 2 and 4 are implemented:

1. **Skip unchanged tasks before SurfaceFlinger prepares them — implemented.**
   The [task work gate](../android/surfaceflinger/0014-droidloom-task-work-gate.patch)
   checks ordered layer/buffer/frame identities and Android change flags before
   copying snapshots, preparing output or submitting composed fallback. Host
   configuration and asynchronous recovery increment a work revision; acceptance
   cannot consume a wakeup that arrived during preparation. Front-buffer,
   sideband, metadata/effects and geometry changes still prepare. Input-token,
   lifecycle and release processing continues. Pure C++ tests cover unchanged
   work, order changes, failed work and racing revisions. Optional task counters
   expose actual skipped preparations; phone CPU/frame-time gains remain to be
   measured with a static app beside an animating app, plus resize/dialog tests.
2. **Batch direct-layer scene transport — implemented.** The
   [sender](../android/surfaceflinger/0015-droidloom-scene-batches.patch) and
   [presenter](../graphics/droidloom-wayland/src/layers.rs) negotiate `SCENE_BATCH`.
   A chunk holds up to sixteen layers and four indexed producer fences. The last
   chunk includes the commit, reducing small scenes to one packet; reusable
   sender storage avoids rebuilding chunk allocations. The complete scene still
   has one asynchronous acceptance, at most 64 layers and one commit in flight.
   Configure serials, source read tickets, real producer/release fences and
   fallback ordering barriers retain their existing checks. Older peers retain
   individual staging. C++ socket and Rust codec tests cover chunk boundaries,
   descriptor identity, malformed indices and scene overflow. Measure CPU and
   latency on the target; protocol counts alone do not establish a speedup.
3. **Remove synchronous audio buffer-control exchanges.** The
   [Android audio client](../android/audio/droidloom-audio-client/src/lib.rs)
   performs request/reply exchanges for acquire, submit and position. The
   [HAL adapter](../android/aosp-patches/0002-audio-default-only-service.patch)
   also requests position inside transfer and when refining position. PCM already
   uses four shared buffers and the [PipeWire bridge](../runtime/droidloom-audio/src/pipewire.c)
   performs no PCM copy. Target the control path: generation-scoped shared slot
   credits, readiness notification and coherent timing snapshots, leaving the
   socket for setup/lifecycle/errors. Avoid filling all credits just because they
   exist: that would add buffering delay. Keep current latency goals and Android
   audio policy, with no socket waits or allocations in the real-time process
   callback. Validate playback/capture, underruns, timestamps, pause/drain,
   reconnect and device changes. [PipeWire stream scheduling](https://docs.pipewire.org/page_streams.html)
   explains buffer ownership and callback timing; moving queue work needs actual
   graph-latency measurements.
4. **Remove the lifecycle daemon's idle health timer — implemented.** The
   [management loop](../runtime/droidloom-supervisor/src/control.rs) waits on its
   listener, an exact-child pidfd and a sticky shutdown eventfd. It has no idle
   timeout; explicit startup/readiness operations retain their deadlines. Tests
   cover shutdown requested before waiting, child exit and replacement. The
   prior local daemon made sixty voluntary context switches over three idle
   seconds, consistent with its 50 ms poll; the activated replacement made
   zero voluntary or involuntary context switches over the same interval.
   This targets idle power, with no frame-time claim.

These changes and candidates are not measured phone speedups. Compare
frame-time distributions, CPU time, wakeups and sustained thermal behavior on
the same device and workload before accepting performance claims. Do not obtain
lower overhead by changing app background policy or weakening GPU ownership.

Lower-priority follow-ups remain:

1. **Keep clipboard synchronization from blocking ordinary input.** Restrict
   clipboard ordering to operations that need clipboard contents. Preserve key
   and gesture ordering under overload, and retain timestamps from receipt.
   Validate with a stalled clipboard peer while delivering clicks, gestures and
   keys; the current presenter can queue input for ten seconds and allow newer
   events to bypass a full 512-event queue.
2. **Measure the event-driven catalog.** The native subscription, in-process
   adapter and batched icons are implemented. Measure idle wakeups and refresh
   cost across catalog sizes and verify package/user/locale changes on each target.
3. **Measure notification deltas and consider icon identities.** Changed-entry
   posts and versioned removals are implemented, with snapshots on reconnect.
   Changed entries still include their icon pixels; reusable icon identities
   could reduce that traffic further. Measure updates among many unchanged entries.

These are source-backed candidates; their performance impact needs measurement.

### Retirement map

Retirement is conditional on the replacement covering the existing contracts
and passing acceptance. Components with several responsibilities may remain
until their other roles have moved.

| Replacement work | Existing Droidloom path it can supersede |
| --- | --- |
| PhoneWindowManager policy replacement plus direct WindowManager/ActivityTaskManager integration | The standalone task launcher, external TaskObserver and input-bridge focus/bounds/close handling are retired. Host launch/setup/density use the existing system_server through authenticated native RPC. |
| Native input acquisition inside InputFlinger | The [`presenter`](../graphics/droidloom-wayland/src/input.rs) writes directly to InputFlinger. Java event conversion, per-event bounds queries, Composer event forwarding and the standalone support process are retired. Rust owns the listener; framework hooks handle power and authenticated descriptor delegation. |
| SurfaceFlinger with direct Wayland presentation | The graphics roles of the custom [`Composer service`](../graphics/droidloom-composer-service/src/main.rs), [`host presenter`](../graphics/droidloom-wayland/src/main.rs) and current SurfaceFlinger-to-host presentation transport. Non-graphics presenter duties require separate replacements. |
| AudioFlinger with PipeWire streams and corresponding audio-policy integration | The HAL-level PCM transport for supported streams. Retiring the [`host audio broker`](../runtime/droidloom-audio/src/main.rs) additionally requires an equally constrained replacement for host access policy and resource delegation. |
| Direct ClipboardService/NotificationManagerService backends | Their SystemUI export adapters, while retaining Android policy and scoped content access. The root relay is already removed through direct descriptor delegation. |

Current paths illustrating the opportunity:

- The [framework task backend](../android/framework/droidloom-window-policy/java/com/android/server/wm/DroidloomTaskBackend.java)
  now selects and publishes tasks inside ActivityTaskManager, with the
  [Rust registry and transport](../android/framework/droidloom-window-policy/src/tasks.rs).
- The [native input receiver](../android/framework/droidloom-window-policy/src/input_ffi.rs)
  receives the presenter's records directly inside InputFlinger. The private
  graphics protocol only carries socket handoff and route publication, negotiated
  through DIRECT_INPUT capability in protocol 1.6.
- The [framework channel hook](../android/framework/droidloom-window-policy/java/com/android/server/policy/DroidloomHostChannels.java)
  authenticates the installed IME/SystemUI UIDs and delegates connected descriptors.
  It performs no payload copying and runs inside the existing system_server.

The pinned AOSP ClipboardService already selects an `ArcClipboardMonitor` backend
for ARC. A Droidloom backend could use a similar insertion point while retaining
the existing access checks. This is an implementation precedent, not an API
compatibility guarantee for other services. See the
[upstream ClipboardService](https://android.googlesource.com/platform/frameworks/base/+/refs/heads/android16-release/services/core/java/com/android/server/clipboard/ClipboardService.java).

Use small interfaces inside the owning services, backed by shared transport and
validation libraries. A framework component may coordinate endpoint lifecycle
and session metadata. Avoid making it a call-through facade for all services or
the sole queue for unrelated work. Keep required SystemUI boot and policy
callbacks until their replacements are complete.

## Direct graphics and audio backends

A more ambitious graphics backend would let SurfaceFlinger create Wayland
surfaces directly. ActivityTaskManager/WindowManager would supply task and window
semantics; SurfaceFlinger would retain Android layer transactions, BufferQueues,
composition fallback and real producer/release dependencies.

This could absorb much of the separate presenter and custom graphics transport.
First extract reusable presentation logic; compare the existing and embedded
backends before retiring either. Preserve DMA-BUF import checks, explicit
synchronization, fractional scaling, atomic scene changes and fallback rendering.
Moving the same code into SurfaceFlinger alone does not prove a latency or memory
improvement. A compositor disconnect must invalidate old host objects without
inventing releases for buffers still in use.

Audio requires a different insertion point. The existing HAL bridge sees streams
after AudioFlinger may have mixed applications. Per-app PipeWire nodes need track
and attribution plumbing before mixing, with Android audio policy preserved.
Begin with a backend within AudioFlinger/AudioPolicy, covering supported mixed
playback and capture, rather than immediately reimplementing their entire Binder
contracts. Direct/offload/MMAP paths, effects, timestamps, drain, focus/ducking and
recording policy need explicit support or an honest fallback. A copying or mixed
fallback can remain useful where the direct path is unavailable.

The existing [audio bridge](audio-integration-proposal.md) remains the baseline
for comparison. Physical routing stays with PipeWire and its session manager.
An Android volume or mute operation must not silently change unrelated Linux
applications. See [AOSP audio architecture](https://source.android.com/docs/core/audio).

## Shared identity and independent service lifetimes

Use a consistent identity model across windows, audio, notifications, documents
and permissions. Illustrative identities are:

| Identity | Lifetime and use |
| --- | --- |
| Installed application | Host user/cell, Android user and package, with installation/signing provenance tracked separately. Updates preserve the desktop identity; uninstall/replacement invalidates incompatible grants. |
| Desktop entry | Stable exported launcher identity, with an explicit rule for grouping multiple launcher activities belonging to one package. |
| Runtime task | Cell boot generation, Android task ID and task incarnation; a recycled numeric task ID cannot receive an old action. |
| Host operation | Endpoint generation, request ID, owning application/task and any scoped grant; stale replies cannot affect a later session. |

One application can have several windows and processes. A task may temporarily
contain another package's permission or authentication activity. Retain task
ownership and the immediate caller's attribution separately. Shared Android
services cannot be assigned exclusively to one application's resource total.

Keep authoritative state with its existing owner: Android owns activities and
application policy; the compositor owns host focus and geometry; PipeWire owns
physical audio routing. Versioned events connect those states without creating
competing copies of each policy. Reconnects use a snapshot plus ordered changes,
with sequence-gap detection and idempotent reconciliation.

Android and Linux remain separate processes and security domains. A small,
authenticated transport is still required. Compile common codecs and state
machines into existing services and retain separate bounded channels for input,
graphics, audio and slower operations. Never perform host I/O while holding
WindowManager/ActivityManager locks, on InputDispatcher's dispatch thread, or in
real-time audio callbacks. Backpressure in clipboard or desktop services must
not stall input, rendering or audio.

Framework-reported package names and Wayland `app_id` values are attribution,
not independent host credentials. Host policy authenticates the cell and its
delegated capabilities; it must not assume that package labels prove identity
after a cell compromise. Preserve the boundaries in the
[threat model](threat-model-v2.md). New implemented authority requires its stated
threat-model update before shipping.

## Make application identity native to the desktop

Treat each installed Android application as a desktop application served by a
shared runtime. Keep its host identity stable while the cell is stopped or
restarting. Use that identity consistently for launcher filenames, Wayland
`app_id`, icons, notification desktop-entry hints, media players and permission
records. Establish a canonical package entry and explicit aliases for additional
launcher activities. Include Android user/cell identity to avoid collisions and
reserve a Droidloom namespace so packages cannot claim unrelated host apps.

There is a concrete mismatch in the current source: the
[catalog](../runtime/droidloom-applications/src/lib.rs) generates
`droidloom-<id>.desktop` filenames, while the
[presenter](../graphics/droidloom-wayland/src/main.rs) sets Wayland `app_id` to the
package name. `StartupWMClass` also uses the package name. Establishing one mapping
would give desktops a consistent basis for grouping, pinning and icon lookup.
Validate this on each supported desktop; do not assume every shell uses the same
fallback matching. Migrate existing generated entries and pinned identities
deliberately.

Labels and task titles should come from Android's authoritative metadata and
window state, including locale changes. A task containing an authentication
activity retains its owner while exposing the appropriate transient title.

### Portal identity without a process for every app

Investigate a separate host D-Bus connection per logical application, managed
inside an existing unprivileged host component. Where supported, register each
connection with `org.freedesktop.host.portal.Registry` before making portal
requests. Its registration associates a connection with a desktop application
ID, is limited to one registration per connection and applies to callers the
portal considers unsandboxed. Re-register after portal restart, and detect
availability instead of assuming this interface exists on every target.

This provides a concrete path to per-app dialog attribution without adding a
host helper process for every Android application. Registration supplies desktop
attribution; it does not establish isolation between apps within a compromised
cell. The host component must constrain requested identities to its catalog and
delegated grants. Never let an arbitrary package string select another host
application's identity. See the
[portal Registry contract](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.host.portal.Registry.html).

## Launch, installation and removal through normal desktop actions

An exported launcher should start its app from a cold runtime. The existing
supervisor should coalesce concurrent starts, retain each original launch intent,
wait for readiness and then deliver it once. Carry the desktop activation token
through that operation where it remains valid; the compositor decides focus.
Provide ordinary startup feedback and an actionable error if boot or launch
fails. Initial administrator provisioning remains explicit; normal launches
after provisioning should not need a terminal or administrator access.

Preserve deep-link data, URI grants, activity-result relationships and task flags
when launching. Reconnection must distinguish a request that never ran from one
whose outcome is unknown; do not blindly replay external actions.

Publish supported MIME/scheme handlers from authoritative Android resolution
metadata so Linux's Open With UI can launch an Android app. Bridge supported
outbound intents to host handlers, preserving explicit Android targets and
avoiding recursive host-to-Android routing. Respect user defaults on both sides
and retain a chooser when resolution is ambiguous. Export only implemented
operations; advertising every Android intent filter is insufficient. See the
[Desktop Entry Specification](https://specifications.freedesktop.org/desktop-entry/latest/)
and [OpenURI portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.OpenURI.html).

Offer install, update and remove operations through the desktop application
experience, including split-package sessions and necessary game assets. Update
the catalog after PackageManager commits. Removal revokes exported handlers and
grants; retaining or deleting app data is an explicit choice. A failed update
must not leave a launcher pointing at a partially installed package.

## Windows follow the host desktop

Host maximize, tile, fullscreen, minimize, resize and close operations must feed
the appropriate Android task/window transitions. Treat host activation and
keyboard focus, Android lifecycle/resume, visibility and audio focus as related
but distinct state. Several visible Android apps must continue to work while
only one has keyboard focus. Validate the pinned product's multi-resume behavior
instead of deriving visibility solely from Android's globally focused task. See
[AOSP multi-resume](https://source.android.com/docs/core/display/multi_display/multi-resume).

Map Android dialogs, menus and permission windows using their actual owner,
modality and input semantics. Some remain child layers within a task; others may
need host transient surfaces. Exporting every Android window as an independent
top-level or automatically turning every popup into `xdg_popup` would lose those
semantics. Compositor decorations and Android content insets must agree so there
are no duplicate bars or inaccessible content edges.

Reflect output changes, available work area, scale and orientation through
Android configuration and real layout. Test whether the pinned shared-display
design can provide the necessary per-task density overrides; mixed-DPI behavior
may require further WindowManager work. Use host-provided insets where supported
and keep an explicit fallback elsewhere. The current
[fixed inset policy](contracts/navigation-insets.md) is a baseline, not the final
desktop geometry contract.

## Documents, drag and drop, and sharing

Use the host file chooser for supported Android document-picker requests and
expose selected objects through a scoped Android DocumentsProvider/content
provider. Preserve MIME filters, multi-selection, cancellation, activity results
and the distinction between read, write and persistable grants. A host file URI
cannot simply be handed to an Android app whose namespace cannot resolve it.
Persistable Android permissions require a durable backing grant, not a temporary
descriptor disguised as permanent access. See the
[FileChooser portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.FileChooser.html)
and [Documents portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Documents.html).

Connect Wayland drag-and-drop offers to Android's drag lifecycle with correct
MIME negotiation, action selection, completion and URI access. Preserve app
support limits: receiving a drag does not make an app understand a new format.
Large files should use streaming descriptors or scoped document handles instead
of being copied wholesale through control messages. Explicitly shared storage
collections remain useful and are separate from individual document grants.

Route supported Android share intents into host applications and allow native
apps to share into Android targets. Cross-runtime requests need original caller,
target, lifetime and grant information, with loop detection and cancellation.
Reconcile downloads and recent-document metadata through their owning services;
avoid exposing Android's private databases or mounting the whole host home.
Wayland's [data-transfer interfaces](https://wayland.freedesktop.org/docs/html/apa.html#protocol-spec-wl_data_device)
provide the host drag-and-drop contract.

## Media, devices and permission presentation

Connect Android MediaSession state to per-app MPRIS players, including supported
transport actions and seeking. Host media keys then reach the active Android
player, and Android lock-screen media UI becomes unnecessary for desktop use.
Use notifications for notifications and media sessions for player state. The
[MPRIS specification](https://specifications.freedesktop.org/mpris/latest/)
defines the host-facing contract; apps exposing no useful session need a
documented fallback.

Camera, location and screen capture should use backends in their owning Android
services with narrowly granted host resources. Use portal/device capability
discovery and host revocation, preserving Android API behavior and app policy.
For example, investigate a CameraService/provider backend consuming a permitted
PipeWire camera stream; screen capture needs the MediaProjection consent and
session lifecycle as well as host capture permission. A PipeWire connection
alone supplies neither contract. See the
[Camera portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Camera.html).

Native permission UI can render an Android permission transaction through the
existing PermissionController, while the Android permission engine continues to
decide and record the result. Coordinate host resource consent so users are not
asked twice for the same effective operation. Android permission and the host's
capability grant must both permit access; revoking either stops affected use.
Host dialogs need the verified application attribution and the correct parent
window. Host unlock or biometric success cannot substitute for Android Keystore
authentication tokens or hardware attestation.

## Input, accessibility and desktop preferences

Complete host IME integration with cursor rectangles, selection, supported
surrounding-text context and editor actions. Preserve sensitive-field policy,
Unicode boundaries, composition and session generation checks. Deliver real
mouse buttons, hover, wheel scrolling and pointer shapes when the Android app
supports them. Preserve compositor shortcuts and Android key semantics rather
than globally rewriting keys such as Escape or Ctrl. The
[text-input contract](contracts/text-input-v1.md) records the current baseline.

Investigate an AccessibilityManagerService backend exporting Android's semantic
accessibility tree through AT-SPI, with roles, focus, text, bounds and supported
actions tied to the correct host window. This enables desktop assistive tools
to navigate Android content through semantic objects. Use event subscriptions
and lazy bounded queries instead of full-tree polling; accessibility access
needs its own policy and sensitive-content treatment. Custom app rendering may
provide little semantic information. The relevant interfaces are
[Android AccessibilityNodeInfo](https://android.googlesource.com/platform/frameworks/base/+/refs/heads/main/core/java/android/view/accessibility/AccessibilityNodeInfo.java)
and [AT-SPI Accessible](https://gnome.pages.gitlab.gnome.org/at-spi2-core/libatspi/class.Accessible.html).

Map host appearance, locale, time zone, font scale and reduced-motion preferences
where Android has meaningful equivalents. Keep updates event driven and avoid
recreating every activity unnecessarily. Apps retain their own layouts and
branding; system integration cannot automatically redesign a touch-only app
into a desktop UI. Match system dialogs, decorations and interaction behavior
first.

## Per-app resource management and recovery

Investigate supervisor-owned per-app host cgroup accounting and controls,
coordinated with Android process lifecycle and existing task-profile placement.
Resolve actual processes with stable handles and mapped identities; a package
label is insufficient authority to move or kill a host process. Display app
processes under the same desktop identity in task management where the desktop
supports it. Report shared runtime overhead separately and handle shared-UID or
shared-service cases honestly.

**Deferred:** replace the development LMKD compatibility endpoint with an Android
memory policy informed by the cell's real host budget and pressure. This is a
memory-pressure correctness task, separate from the runtime optimizations above;
preserve normal Android multitasking and avoid artificially tight app limits.
Coordinate process
importance, foreground audio, visible tasks and host limits so pressure relief
does not treat every Android app as an indistinguishable background workload.
This needs measured behavior under pressure, not just a service that acknowledges
requests.

Make background operation an explicit host policy. Apps allowed to receive push,
play audio or perform work may keep the shared runtime available; closing the
last window does not necessarily stop it. An idle desktop profile can quiesce the
runtime when no required work remains. Coordinate Android alarms/jobs, Doze,
host suspend, notifications and permitted sleep inhibitors. Do not freeze the
whole cell while a background obligation still needs it or promise network push
during host suspend without a working wake mechanism.

An app crash should be recoverable through its normal launcher without a cell
restart. Optional integration failures must remain local to their service.
System-server or shared runtime failures can still affect multiple Android apps;
the design must acknowledge that shared failure domain. Restore host surfaces
from current Android state after reconnection, expire stale handles and avoid
duplicating notifications or external operations. Runtime/image updates must
preserve app data with an explicit backup/migration policy; reverting software
alone does not revert mutable Android data.

## Further research: app-side native protocol backends

A still more aggressive graphics experiment would place a Wayland backend in
`libgui` so selected app-owned buffers reach the host from application processes.
It would require preserving SurfaceControl/BufferQueue transactions, cross-process
layer ownership, WindowManager policy, overlays and buffer-release semantics.
This is a different architecture from the SurfaceFlinger backend and a larger
compatibility project. It should be an isolated experiment with representative
apps and a measured benefit before becoming a product direction.

Native-looking desktop behavior does not require applications to share the host
mount, Binder or device namespaces. Keep containment while making framework
backends understand native desktop objects. Port required protocol clients for
Android's Bionic environment or retain a narrow host endpoint; host glibc shared
libraries cannot simply be loaded into Android processes.

## Behavioral acceptance

Use complete workflows on supported compositors, with cold and warm starts and
native Linux apps present. Record app/version, architecture, compositor, backend
and runtime revision alongside latency, failures, idle cost and resource usage.

| Workflow | Required observable result |
| --- | --- |
| Launch from search or a pinned icon with Android stopped | One startup transaction, the intended app/intent, normal startup feedback and the correct grouped window. |
| Open a document from the native file manager | Android appears in Open With, receives only the selected document grant and can save changes where permitted. |
| Open a link from Android | The configured native handler opens it once; return/cancellation and focus remain correct. |
| Tile two Android apps beside a Linux app | Geometry, lifecycle, focus, shortcuts, fullscreen and scale changes behave consistently. |
| Use permission dialogs and an authentication handoff | Correct parenting, caller identity, Android result delivery and return to the original task. |
| Type with a host IME and navigate with assistive tools | Composition, candidate placement, focus and semantic actions reach the correct editor/window. |
| Drag a file or share content across runtimes | Supported formats transfer with scoped grants, bounded memory and correct completion. |
| Play media while using another app | Separate attributable streams, working media keys, Android focus behavior and no unrelated global-volume changes. |
| Record or capture, then revoke access | Native consent and Android policy agree; active access ends according to the API's revocation contract. |
| Click a notification after focus changes or reconnection | The original action reaches the intended task once, without a duplicate launcher activity. |
| Stall clipboard, a portal or an input peer | Unrelated input, graphics control, audio and communications continue within measured bounds. |
| Crash one app, restart a host endpoint and suspend/resume | Recovery preserves app data, task/grant generations and action identity; failures do not silently trigger unrelated app restarts. |
| Update or remove an app | Desktop entries, handlers and grants follow the committed package state, with explicit app-data handling. |

Set performance budgets from measured reference workflows before making claims
about improvements. Include click-to-first-frame, input-to-display, frame pacing,
audio timing, cold-start memory, background wakeups and suspend behavior. Keep
translator correctness and long-running app compatibility in the release matrix.

## First implementation slice

The policy substitution, authoritative task publication and direct host
focus/bounds/close operations are implemented with Rust engines and Android
framework adapters. Task discovery helpers are removed. Native input reception
inside InputFlinger now owns event conversion and dispatch, using task bounds
and application tokens published by WMS. The presenter sends input directly over
a dedicated socket delegated through Composer at startup or recovery. Socket
writes are nonblocking and ordered, with a bounded queue; exhaustion closes the
connection so native cancellation precedes recovery. Stale handoff generations
are rejected. Rust owns listener startup/recovery; the existing framework handles
power and descriptor delegation. The standalone support bridge is removed.
Validate reconnect, slow receivers, output scale and live task retirement.

Acceptance requires actual visible windows, correct task ownership, permission
dialogs, reused tasks, activity-result handoffs, ordered gestures and continued
graphics progress while an input peer stalls. Compare launch-to-first-frame and
input-to-display latency against the existing path. Unit or package checks alone
cannot establish these results.

This work is compatible with the Linux ownership model in
[host service integration](architecture.md#host-service-integration).

Alongside that first slice, unify desktop identity and add normal cold launch.
Then complete document/link handling, clipboard and IME behavior; add native
media/resource consent and accessibility; evaluate deeper graphics/audio
backends against the resulting behavioral and performance baselines. Each slice
should retire the superseded path after end-to-end acceptance, with rollback
artifacts and protocol compatibility accounted for.

If choosing a major whole-service rewrite, take SurfaceFlinger as the graphics
project or AudioFlinger with its policy integration as the audio project.
Neither is required to precede the first policy/input slice. Record which
alternative is selected before implementation so an embedded backend and a full
replacement do not become competing implementations by accident.
