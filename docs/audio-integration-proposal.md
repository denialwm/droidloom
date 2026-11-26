# Audio integration proposal

See [Host service integration](architecture.md#host-service-integration)
for the audio ownership model and Linux-generic backend boundaries.

Status: shared-pool bridge implemented; host loopback validated and activated
in the local x86_64 test cell. Fruit Ninja playback through Bluetooth headphones
is user-confirmed clear with acceptable latency. Microphone capture and wider
device validation remain required.

## Implemented bridge and host checks

`runtime/droidloom-audio` is a per-user PipeWire broker. The Android client in
`android/audio/droidloom-audio-client` links statically into the default audio
HAL. Its PCM16, 48 kHz BUS streams use four dedicated size-sealed memfds; output
FMQ transactions copy directly into the acquired pool slot, and input copies
directly from that slot into FMQ. Capture grants use read-only descriptors.
The broker registers those same allocations with PipeWire's application-buffer
API and performs no PCM copy. Android's master volume and mute affect only
Droidloom PCM; its media volume is the host's output volume (see the runtime
layout contract).

Playback remains clocked when no host sink exists: PipeWire's dummy driver
paces the stream, while WirePlumber's `node.linger` policy waits for an output
without sending a fatal no-target error. This does not retain the node after
client close. When output routing recreates PipeWire's buffer wrappers, the
same sealed sample allocations remain live. An Android write that was already
borrowed can finish; its invalidated credit is discarded and counted as a
discontinuity instead of queuing stale samples. Capture still fails closed on
renegotiation, since its producer could otherwise overwrite a live reader.
The isolated integration test covers no sink, sink arrival/removal, a borrowed
credit during port reconfiguration, monotonic progress and node cleanup.

The separate `droidloom-audio.socket` user unit owns
`%t/droidloom/audio.sock`; the supervisor projects its inode at
`/dev/socket/droidloom/audio`. Socket activation preserves this inode across
broker restarts. New connections receive fresh generations. The current preview
authenticates Android audioserver's host UID 1041; production subordinate UID
mapping will need the supervisor to provide the mapped identity.

The broker must retain a user namespace in which the HAL's UID is visible.
Filesystem namespacing options in a systemd user service implicitly create a
user namespace mapping only the host user; `SO_PEERCRED` then reports the HAL
as the overflow UID. The audio unit therefore omits `ProtectSystem`,
`ProtectHome` and `PrivateTmp`, while retaining `NoNewPrivileges`,
`RestrictSUIDSGID` and exact peer authentication. A startup UID-map check rejects
an unmapped configured peer with a descriptive error.

Position queries are valid before graph linking and while paused. During those
states, the broker retains the last observed frame count instead of treating
PipeWire's non-streaming time-query result as a broken connection. Playback
credits follow the caller's burst and the sealed allocation capacity; the
graph's suggested quantum does not truncate synchronous Android writes.
`PW_STREAM_FLAG_EARLY_PROCESS` wakes the producer when PipeWire recycles a
buffer, allowing a refill before the queue runs empty. The default callback
only asks for more data on starvation, which inserted silence when acquisition
was limited by the queue budget. The callback stays on the locked stream loop
and only signals waiting control work; it performs no PCM copy or socket I/O.
Playback acquisition limits queued/converter data to one caller burst or host
graph period, whichever is larger, before granting the next. A host graph period
larger than an Android burst is filled through successive credits at increasing
offsets in the same memfd, without copying PCM. Drain queues a partial final
block; flush and standby discard it. All four immutable allocations remain
available; device delay does not consume credits.

The optional `BURST_POSITION` feature carries a coherent host position snapshot
in each acquire/submit reply. The HAL's existing position queries reuse that
snapshot for at most one 256-frame quantum, preserving its original monotonic
timestamp. Standalone queries after expiry still contact the host, and every
stream control invalidates cached timing. A normal burst therefore needs two
control exchanges without increasing buffer size or changing release ownership.
Legacy brokers remain supported through a fresh legacy OPEN after feature
rejection; legacy clients retain their original replies on a new broker.

Playback uses a 256-frame HAL minimum and bursts at 48 kHz (approximately
5.3 ms), with a 5 ms nominal buffer hint. The explicit frame minimum avoids
the generic conversion of 5 ms into 240 frames, which does not align with the
host's common power-of-two periods.
Both products set `persist.audio.normal_playback_period_ms=5` so AudioFlinger
does not expand those bursts back to its default 20 ms mixer period. Capture
retains its 1024-frame bursts and 20 ms hint. Playback allocations have capacity
for 8192 frames to accommodate coarser host graph periods; capacity does not
set the valid sample count or bypass playback backpressure. No global host
quantum is changed.

`android/audio/0001-host-buffered-playback.patch` updates AudioSystem's minimum
playback-buffer calculation in `libaudiobase.so`. On `ro.hardware=droidloom`,
two mixer periods cover producer scheduling; PipeWire's downstream device delay
is not duplicated in each application's minimum queue. Other Android platforms
retain upstream sizing, and apps retain explicitly requested larger queues.
Actual output-latency reports and presentation timestamps remain unchanged.
Both products build and project the audio client library with the HAL.
The module explicitly retains the platform prebuilt's cross-DSO CFI
instrumentation. The local replacement matches all 591 dynamic exports and data
sizes, its SONAME and dependency list; its added `property_get` import is supplied
by the already installed `libcutils.so`.

The standalone `android/audio/tests/AudioBufferProbe.java` queries the real
Android API without starting playback or a GUI. Compile it against the pinned
public `android.jar`, convert with the pinned D8, and run it through
`app_process` with the dex on `CLASSPATH`. An optional maximum duration argument
(milliseconds) makes an oversized minimum queue fail the check. With the
256-frame mixer period, a 15 ms limit allows the expected two periods and
resampling margin at 44.1, 48 and 96 kHz.

The local x86_64 API check measured approximately 235 ms before the change and
42.8 ms after activation at all three sample rates. Both system_server and
audioserver map the verified replacement. These are minimum app-buffer sizes;
they do not measure physical end-to-end playback latency. A freshly reopened
Fruit Ninja track used 1888 frames at 44.1 kHz instead of 9440, with no reported
underruns. The observed estimated track latency fell from approximately 472 ms
to 286 ms; this is a diagnostic snapshot, not a physical latency benchmark.
The user still perceived approximately a second of delay after that first
change, while native Linux audio through the same Bluetooth headphones felt
immediate. Shortening the HAL and normal mixer periods addresses another local
queue. The first 240-frame short-period preview reduced the real API minimum
to approximately 10.1 ms; the user found its delay acceptable but reported
crackling, with no Android track underruns in the diagnostic snapshot. The
aligned 256-frame HAL and early-refill broker address period boundaries and
host starvation. Their active HAL and normal mixer both report 256 frames;
the API minimum is 10.839 ms at 44.1 kHz and 10.750 ms at 48 and 96 kHz.
The user confirmed that the final build sounds good and considers playback
resolved. This confirms acceptable audible latency and clear playback on the
tested Bluetooth route; physical end-to-end latency has not been measured.

Microphone capture is on by default, as on a phone: Android's own per-app
microphone permission asks the user the first time an app records, and the
desktop shows the broker's PipeWire recording stream like any other. To keep
the whole cell off the microphone, set `DROIDLOOM_AUDIO_CAPTURE=0` in
`~/.config/droidloom/audio.env` and restart `droidloom-audio.service`; that stops
existing capture and invalidates its generation, and reopening the Android
recording stream obtains the new policy. This switch applies to the cell, not
to individual Android apps. Android records only from microphone device types,
so the host input is declared as a built-in microphone; as a BUS input, every
recording failed in the policy engine before reaching the HAL.
Missing endpoints and denied capture retain the
timed stub; fallback input is explicitly zero-filled, replacing upstream random
bytes. Transport failures after opening a live stream terminate that stream.

Normal host routing requires WirePlumber or another active PipeWire session
manager. The loopback fixture links ports explicitly and does not validate
desktop session-manager policy or physical device availability.

Run the focused host tests with PipeWire tools and development headers installed:

```sh
cargo test --locked -j 1 -p droidloom-audio -p droidloom-audio-client -p droidloom-audio-protocol
cargo run --locked -j 1 -p droidloom-update -- -j 1 audio-build --work .work/arch/build
```

The tests launch an isolated PipeWire server and null-sink monitor, preserving
the desktop audio session. Tagged PCM crosses playback and capture through the
real conversion graph in mono and stereo, checking sample and channel order.
Tests check partial capture retention, pause/flush/standby,
playback drain, denied capture, peer credentials, stale generations, invalid
credits, sealed mappings, backend death, socket activation, startup/paused
positions and repeated 1024-frame bursts across a 256-frame graph quantum.
Short 256-frame playback bursts are checked against 64-, 256-, 512- and
1024-frame graphs, including tagged sample order, no inserted interior silence,
a partial drain tail and
discarding a partial block across pause/flush/resume. Buffer-return success
includes positive PipeWire queue counts, so a successful flush cannot retain
the old partial block.
They establish shared-pool
transport behavior, not physical-device latency or Android AudioTrack behavior.

The audio APEX compiles and links for both x86_64 and ARM64 against the pinned
sources, including the Rust FFI client. Linker configuration, host APEX verification
and APEX SELinux checks pass for both products. The x86_64 test override now
activates the matching HAL, host broker and socket unit together. Before this
activation, that machine had no audio units and AudioFlinger was still using
the silent 4096-frame stub. Verification checks the live HAL bytes and the
identity of the socket projected into Android. The local Fruit Ninja test
confirmed clear output through the host's Bluetooth headphones after the
namespace/startup fixes and raising Android's low media level. That route
reports approximately 214 ms of device latency; this is separate from bridge
queuing and Android track buffering. No host volume, codec or graph quantum was
changed. ARM64 runtime and microphone validation remain required.

Remaining work includes wider Android playback/recording and audio core VTS,
device-change notifications to Android, per-app host attribution, measured
presentation timestamps, real xrun accounting and latency/CPU comparisons.
Timing uses PipeWire queue/conversion/device-delay estimates. The fourth status
field counts explicit discontinuities from flush/standby, not hardware xruns.
The playback period is provisional; graph delay is reported separately and no
global PipeWire quantum is forced. Unsupported pool negotiation
fails explicitly; a measured copying fallback and AAudio MMAP remain later work.

## Design direction

The Android AIDL audio HAL adapter is backed by the host PipeWire service.
Keep Android audio policy and app APIs intact; let the host own physical devices.
Transport PCM through bounded shared-memory buffers, separating stream control
from sample transfer and avoiding extra queues or copies where practical.

Start with playback and microphone capture, including routing, mute, disconnect
recovery and access policy. Validate latency, underruns, CPU cost and concurrent
Linux/Android playback before tuning buffer sizes. Direct ALSA ownership would
require a separate device-ownership and isolation contract for dedicated hosts.

## Baseline source audit and gaps

The source audit uses the revisions in
[`m2-sparse-source-lock.json`](../android/manifest/m2-sparse-source-lock.json):
`hardware/interfaces` at `0162af698935100a590b7359581ac8b1b80693e5`,
`frameworks/av` at `475269ec44acec82792be6b01fb8e16357d4d2c8`, and
`system/libfmq` at `d525961b46fbebd138018a102e4ab983c9582045`.
The table describes the baseline before the implementation above.

| Component | Present | Missing for real audio |
| --- | --- | --- |
| Android service | Vendor audio APEX, init services and VINTF audio core/effect V4 declarations | Host-backed `IModule/default` input/output stream implementation |
| Android policy | 48 kHz PCM16 stereo BUS output and mono built-in microphone input | Operational host routes, microphone description, availability and permission handling |
| Sample processing | Upstream primary HAL, patched to select its stub for current BUS types | Playback delivery and real capture; the upstream stub discards output and generates synthetic input bytes |
| Host backend | Architectural proposal | Rust per-user PipeWire service, graph negotiation and stream lifecycle |
| Cell transport | Graphics FD-passing infrastructure | Audio-specific protocol, endpoint delegation, bounded buffers and ownership transitions |
| Integration | Both ARM64 and x86_64 products include the audio APEX | Backend packaging, dependencies, startup/reconnect/teardown and diagnostics |

The baseline policy's “Droidloom Silent Input” label does not establish zero-filled
capture: the pinned `DriverStubImpl::transfer` fills connected stub input with
`std::rand()` bytes. A denied or unavailable real microphone needs explicit,
tested behavior rather than inheriting this simulation.

The existing patch is
[`0002-audio-default-only-service.patch`](../android/aosp-patches/0002-audio-default-only-service.patch).
Replacing its BUS-to-stub branch with a host driver is necessary, but retaining
the example stream worker unchanged retains its intermediate sample buffer.

## Copy budget and practical first implementation

FMQ is shared memory, but its ordinary `read`/`write` operations copy samples.
In the pinned framework, `StreamHalAidl::transfer` copies playback into the
data FMQ and capture out of it. In the example HAL, `StreamOutWorkerLogic::write`
copies FMQ data into `mDataBuffer`; `StreamInWorkerLogic::read` fills that buffer
and copies it into FMQ. Simply adding another shared ring and copying it into
PipeWire would add more sample transfers.

Target this playback path for the first optimized bridge:

```text
AudioFlinger mix buffer
  -- copy 1 --> Android data FMQ
  -- copy 2 --> dedicated shared PipeWire sample buffer
  -- same backing memory --> host audio service / PipeWire stream
```

Have the HAL transfer directly between FMQ and a dedicated pool accepted by
PipeWire, eliminating `mDataBuffer` as a separate staging copy. Share descriptors
once during registration; steady-state messages carry buffer IDs, valid frame
counts, generations and credits. This targets one copy at the FMQ-to-host-buffer
boundary and no additional sample copy in the host broker. The framework's FMQ
copy remains. Mixing, conversion, resampling and the device path are outside
that count and must be measured separately.

PipeWire supports application-allocated buffers through
`PW_STREAM_FLAG_ALLOC_BUFFERS`; negotiate `SPA_DATA_MemFd` and populate buffer
data during `add_buffer`. The isolated host tests prove this for their graph,
not every audio adapter, capture route or target graph. Prove buffer
negotiation in both directions. If an independent PipeWire buffer is required,
use a bounded copy fallback and expose its extra copy in diagnostics.
[PipeWire stream flags](https://docs.pipewire.org/group__pw__stream.html),
[MemFd allocation example](https://docs.pipewire.org/video-src-alloc_8c-example.html).

Playback slots move from free to HAL-owned, ready, PipeWire-owned and back to
free only after consumption. Capture slots must remain retained until Android
has consumed their contents. Handle partial bursts, pool exhaustion, format
changes and buffer removal without premature reuse. PipeWire's `remove_buffer`
and stream destruction invalidate mappings; reconnect must establish a new
generation. Never block the PipeWire real-time callback waiting for Android;
prepare buffers outside that callback and define underrun/overrun behavior.
[PipeWire buffer lifecycle](https://docs.pipewire.org/page_streams.html).

A separate memfd ring alone does not satisfy this target if the broker then
copies every period into an unrelated PipeWire pool. Likewise,
`PW_STREAM_FLAG_MAP_BUFFERS` only maps buffers; it does not eliminate sample
copies. Ordinary CPU PCM needs shared RAM and ownership synchronization; the
graphics DMA-BUF/DRM-syncobj machinery is not a prerequisite.

At the current policy format, stereo playback is 192,000 bytes/s and mono
capture is 96,000 bytes/s. These rates make correctness, wakeups and queue
latency at least as relevant as memcpy cost. Use the copy fallback as a measured
baseline while developing pool sharing.

## Why full zero-copy needs more work

The pinned FMQ library has direct memory transactions (`beginRead`/`beginWrite`
and corresponding commits), and its C++ AIDL queue can use an externally
supplied buffer FD. Those provide useful implementation primitives, but do not
change the audio transfer contract. `StreamDescriptor.AudioBuffer.fmq` is
temporary storage for each BURST: the consumer must empty it before completing
the transfer. Queueing an FMQ pointer into PipeWire and immediately completing
the BURST permits Android to overwrite samples still awaiting playback.

An FMQ-backed zero-copy experiment must finish downstream access within that
BURST or establish another contract preserving buffer ownership. It also needs
ring-wrap handling, frame-aligned regions, graph quantum adaptation and safe
backpressure. Direct FMQ transactions can avoid a HAL scratch copy, but cannot
by themselves remove the framework's copy into/out of FMQ.

AAudio MMAP is a separate possible optimization for eligible apps. It needs real
MMAP profiles/policy, V4 `IStreamCommon.createMmapBuffer`, start/stop behavior and
accurate position timestamps. The example's `StreamMmapStub` only simulates a
DSP; enabling its profile does not implement audio. Buffer access must be safe
before advertising application-shareable memory. Ordinary AudioTrack/media
playback still needs the AudioFlinger path, and AAudio shared mode may mix.
[AAudio and MMAP](https://source.android.com/docs/core/audio/aaudio).

Consequently, a zero-copy Android/host transport is a reasonable prototype
target; general app-to-speaker zero-copy is not an established capability.

## Control, timing and isolation work

- Run the PipeWire client as the assigned host user in a separate audio process.
  The supervisor delegates its endpoint and owns cleanup. Audio must work
  independently of the presenter and its rendering loop.
- Specify format/channel layout, pool size, credits, frame counters, generations,
  discontinuities and all stream operations: open, start, pause, drain, flush,
  standby and close. Both directions need bounded disconnect and restart paths.
- Reuse or extract the low-level `SeqPacket`/`SCM_RIGHTS`/peer-credential code
  from `droidloom-denial-ipc`; its typed protocol and limits are graphics-specific.
  Authenticate the actual cell/session through supervisor delegation and peer
  credentials. Package names supplied by Android are not host authentication.
- Allocate dedicated, size-sealed sample memory for each authorized stream;
  validate descriptor roles, offsets, lengths, frame alignment and resource
  limits. Retain host-owned bounds separately from cell-writable memory. Do not
  pass PipeWire graph metadata or buffers containing unrelated user audio.
- Give Android only the audio endpoint and its authorized sample descriptors.
  Keep physical ALSA nodes, the unrestricted PipeWire socket and host session
  bus outside the cell. Microphone grants must be revocable on the host; stop
  capture and invalidate the old pool on revocation. Disabling an endpoint
  cannot erase samples Android already received.
- Android's music volume and mute are the host's default output volume
  (see the runtime layout contract, "Media volume and screen brightness");
  other Android streams keep their own volume. Let host session policy choose
  devices and reflect route loss, headset/Bluetooth transitions and capture
  availability back into Android.
  HAL mix streams do not automatically provide per-Android-app controls or
  trustworthy microphone attribution; that requires framework identity work.
  [PipeWire session policy](https://docs.pipewire.org/page_session_manager.html),
  [PipeWire access control](https://docs.pipewire.org/page_access.html).
- Report presentation/capture frames and monotonic timestamps from actual graph
  progress. Account for queued frames, conversion buffers and device delay;
  frames merely accepted into a pool are not frames already played. Handle
  graph-rate changes, suspend and xruns without drifting Android's video clock.
  [PipeWire timing](https://docs.pipewire.org/structpw__time.html).
- Replace the example `ModulePrimary::getNominalLatencyMs` normal-stream value
  of 85 ms and its resulting minimum buffer requirement with measured values
  appropriate to the bridge. Negotiate the graph quantum rather than forcing
  global host settings. At 48 kHz, 128 and 256 frames span about 2.67 and 5.33 ms;
  these are period durations, not end-to-end latency guarantees.
- Update the runtime contract and threat-model version before shipping the new
  device-service boundary. Add host build/runtime dependencies and ARM64/x86_64
  packaging. Keep host and protocol/state code in Rust; the existing C++ HAL
  offers an initial integration seam. The pinned AIDL Rust defaults and FMQ Rust
  wrapper are available source primitives, but a Rust replacement still needs
  Soong/vendor-APEX integration and full stream-contract validation.

## Implementation order and acceptance

1. Implement the audio protocol/state machine and a Rust PipeWire backend with
   testable playback/capture. Validate on a Linux machine with working audio;
   keep a copy baseline and prove shared-pool negotiation separately.
2. Connect the Android HAL and policy for the current PCM profiles. Verify real
   playback and microphone input, correct clock reporting, permissions and
   simultaneous native/Android streams before advertising extra capabilities.
3. Remove redundant staging, instrument each sample-copy boundary, and compare
   latency, xruns, CPU and idle wakeups. Test format/quantum changes, partial
   transfers, pause/drain/flush, unplug, PipeWire restart, cell death and suspend.
   Use loopback correlation for end-to-end latency and tagged sample patterns
   for order, corruption and premature-reuse checks.
4. Prototype MMAP or deeper FMQ/framework changes only against that baseline.
   Add Android audio core VTS coverage and malformed-descriptor, stale-generation,
   resource-exhaustion and microphone-revocation tests to each implemented path.

The last recorded Moto inspection in
[the architecture guide](architecture.md#current-evidence-from-the-moto)
found no ALSA cards and only PipeWire `auto_null`, without a capture source.
That is historical evidence and was not rechecked for this source audit. Real
Moto playback/capture needs host device support as well as this portable bridge;
null-sink tests cannot prove speaker or microphone operation. Echo cancellation
remains a separate milestone. Cellular call audio belongs to native telephony,
outside Droidloom.

## Pinned source entry points

Paths below are relative to the pinned AOSP checkout under
`.work/arch/build/aosp-source`:

| Source | Relevant behavior |
| --- | --- |
| `hardware/interfaces/audio/aidl/default/ModulePrimary.cpp` | Stream creation, MMAP placeholder and nominal latency |
| `hardware/interfaces/audio/aidl/default/primary/StreamPrimary.cpp` | Primary driver selection, transfer and simulated timing |
| `hardware/interfaces/audio/aidl/default/Stream.cpp` | FMQ worker state machines and scratch-buffer copies |
| `hardware/interfaces/audio/aidl/default/stub/DriverStubImpl.cpp` | Synthetic sample/timing behavior |
| `hardware/interfaces/audio/aidl/android/hardware/audio/core/StreamDescriptor.aidl` | Commands, replies and FMQ lifetime rules |
| `hardware/interfaces/audio/aidl/android/hardware/audio/core/IStreamCommon.aidl` | V4 MMAP buffer creation |
| `frameworks/av/media/libaudiohal/impl/StreamHalAidl.cpp` | Framework FMQ copies and timing queries |
| `system/libfmq/include/fmq/AidlMessageQueue.h` and `MessageQueueBase.h` | External buffer FD and direct memory transactions |
| `system/libfmq/libfmq.rs` and `Android.bp` | Rust FMQ transactions and build dependencies |
