# Droidloom Android-to-host protocol version 1.8

Status: accepted transport contract  
Byte order: little endian  
Transport: connected Unix `SOCK_SEQPACKET` socket

## Scope

One authenticated connection represents one Android cell belonging to the
logged-in Linux user. Each Android top-level task is a separate protocol object
and one ordinary Wayland toplevel. There is no root object representing an
Android desktop, emulator display, or phone screen.

Version 1 supports the SurfaceFlinger client-target correctness path, cached
DMA-BUF import, explicit DRM syncobj timeline synchronization, presentation
timing, native task state, touch/key/navigation input, and task teardown. It
does not support protected content, HDR presentation, delegated Android layer
trees, capture, or a software-rendering fallback.

This protocol does not redefine Linux allocation or synchronization. DMA-BUF,
DRM FourCC/modifiers, DRM syncobj timelines, `sync_file`, Unix credentials, and
`SCM_RIGHTS` remain the underlying kernel ABIs.

## Connection and authentication

The host presenter creates the socket below the user's mode-0700 runtime
directory. The supervisor bind-mounts only that socket into the Android cell.
The presenter validates the peer credentials and associates them with the
expected cell before parsing `ClientHello`.

Task and package IDs received from Android are claims. The host reconciles them
with the trusted session service before exposing launcher, close, focus, or
input authority. Authentication never depends on a package string in a packet.

A connection begins with `ClientHello` and `ServerHello`. Version 1 requires
all of these capabilities:

- independent native task windows;
- cached DMA-BUF imports;
- shared DRM syncobj timelines;
- presentation timing;
- native input routing;
- one SurfaceFlinger client target per task display.

Unknown capability bits are ignored during negotiation. Missing required bits
are fatal. This private connection complements the presenter's public Wayland
connection; it never represents a combined Android display.

## Packet header

Every sequenced record contains exactly one header and payload. Partial records
are impossible at the protocol layer; truncation (`MSG_TRUNC` or
`MSG_CTRUNC`) is fatal.

| Offset | Type | Field | Rule |
| ---: | --- | --- | --- |
| 0 | `[u8; 4]` | magic | ASCII `DLOM` |
| 4 | `u16` | major | `1` |
| 6 | `u16` | minor | `3` for existing records; `4` activation, `5` power, `6` direct input, `7` target demand, `8` scene presentation |
| 8 | `u16` | opcode | direction-specific value |
| 10 | `u16` | descriptor count | must equal ancillary descriptors |
| 12 | `u32` | flags | zero in version 1 |
| 16 | `u64` | object ID | zero for connection messages, otherwise non-zero |
| 24 | `u32` | payload bytes | exact remaining record length |
| 28 | `u32` | reserved | zero |

The complete record is at most 65,536 bytes. Integers are little endian.
Strings are a `u16` byte length followed by valid UTF-8 without a trailing NUL.
Unknown opcodes, unknown enum values, non-zero reserved bits, trailing bytes,
and descriptor-count mismatches are fatal protocol errors.

Descriptors attached with `SCM_RIGHTS` have message-defined roles and order.
The receiver closes the connection and every received descriptor when a record
is malformed. Descriptors are never inferred from payload integers.

## Android-to-host messages

| Opcode | Message | Object | Attached descriptors |
| ---: | --- | --- | --- |
| `0x0001` | `ClientHello` | connection | none |
| `0x0002` | `CreateTask` | new task object | none |
| `0x0003` | `DestroyTask` | task | none |
| `0x0004` | `BindTimelines` | task | acquire timeline, release timeline |
| `0x0005` | `AckConfigure` | task | none |
| `0x0006` | `RegisterBuffer` | task | one DMA-BUF FD per dense plane |
| `0x0007` | `UnregisterBuffer` | task | none |
| `0x0008` | `Present` | task | none |
| `0x0009` | `SetContentState` | task | none |
| `0x000a` | `SetFrameRate` | task | none |
| `0x000b` | `Pong` | connection | none |
| `0x000c` | `BindTask` | task | none |
| `0x000d` | `BindLayerStream` | task | request socket, completion socket |
| `0x000e` | `RequestActivation` | bound task | none |
| `0x000f` | `BindInputChannel` | connection | connected input socket |
| `0x0010` | `BindInputRoute` | bound task | none |
| `0x0011` | `RequestRenderTargets` | task | none |

`CreateTask` carries a non-zero dedicated logical display ID and a package
identity of at most 255 bytes. `BindTask` subsequently supplies the real Android
task ID. Object, task, and
display identities cannot be reused while live.

Protocol minor 4 adds optional capability bit 8 (`TASK_ACTIVATION`) and
`RequestActivation`. Both peers must advertise support. The message carries
no payload or descriptors and names an already bound live task. It requests
native activation after Android has accepted an activity launch; it never
asserts focus or synthesizes input. Native endpoint users opt in only when
they handle the corresponding action. The Wayland presenter maps it to
`xdg_activation_v1`, attaches the recent source input serial when available,
and waits for a new window's first buffer commit before applying its token.
Stale tokens and tokens for closed windows are discarded. Old peers remain
compatible and receive no new opcode. Initial negotiation and all existing
records retain minor-3 headers, so a minor-3 decoder can reach the handshake.
At that revision, ServerHello selected minor 4 only with mutual activation
support. Later power and direct-input extensions raise the negotiated minor.

Protocol minor 7 adds optional bit 11 (`LAZY_RENDER_TARGETS`). Only when both
peers advertise it may the host publish a configure without a fallback pool.
`RequestRenderTargets` carries one nonzero `u32` configure serial and no FDs;
the task object fixes allocation ownership and the host's current dimensions.
Composer sends it only from an actual composition dequeue, at most once per
generation after a successful send. Existing busy buffers wait for real release
and do not trigger another allocation. Stale requests and requests for closing
windows allocate nothing. Targets arrive through the existing registration
messages; geometry acknowledgement and GPU ownership rules remain unchanged.
The presenter selects minor 7 with this capability and minor 6 for current
direct-input peers without it. Existing records retain their original headers.
SurfaceFlinger prepares geometry and tries direct layers before dequeue, so
direct presentation needs no composition target. Its bootstrap dequeue can
request targets too when the normally elided hidden draw is needed.

Protocol minor 8 adds optional bit 12 (`SCENE_PRESENTATION`). Both peers must
advertise it before the host sends `ScenePresented` (`0x8010`). ServerHello
selects minor 8 with mutual scene-presentation support; peers without it keep
their previously negotiated minor. Existing records retain their original
headers so older decoders can complete negotiation.

Protocol minor 9 adds optional bit 13 (`HOST_CLOCK`). Both peers must
advertise it before the host sends `FrameGrant` (`0x8011`, connection object
zero): three `u64` CLOCK_MONOTONIC values, the latch (a host vblank by which
content must be ready), the predicted presentation (after the latch), and the
granting output's nominal period. Composer then delivers vsync only from
grants and runs no timer. The host sends grants only while Android draws.
ServerHello selects minor 9 with mutual host-clock support.

`BindTimelines` transfers exactly two DRM syncobj opaque descriptors. The first
exports the acquire timeline; the second exports the release timeline. Both
sides retain imported handles for the lifetime of the task. Rebinding is a
protocol error.

Protocol minor 3 adds `BindLayerStream`. Composer delegates exactly two connected
Unix sequenced-packet sockets after its task allowlist selects the presentation
channel. A stream is bound once, cannot address other tasks, and has no authority
to create windows. The `DLL1` stream codec in
`graphics/droidloom-surface-bridge/src/layers.rs` is authoritative: each record has
a 16-byte little-endian header (magic, opcode, payload size, descriptor count),
with a 1260-byte record bound and at most four descriptors. Legacy records
remain at most 128 bytes.

The stream requests are Config (1), Register (2), Unregister (3), Stage (4),
Commit (5), Abort (6), optional StageAsync (7), SubscribeConfig (8),
CommitAsync (9), RegisterAsync (10), StageBatch (11), CommitBatch (12), and
SubscribePlacement (13).
Config returns capabilities,
configure serial and physical dimensions. Register has one FD per image plane;
Stage and StageAsync have at most
one original producer sync-file. RegisterAsync has Register's image planes;
batch records have up to four indexed producer sync-files. Remaining requests
have no descriptors.
Commit validates the entire layer list and current configure before publishing
it. Ack (`0x8001`) reports acceptance or a negative error; Config (`0x8002`)
returns four words; Released (`0x8003`) names one 64-bit completed read ticket.
ReleaseFence (`0x8004`) names one 64-bit read ticket and carries exactly one
`sync_file` FD exported from that source's actual compositor release point.
It is sent when the fence becomes available, without waiting for it to signal;
Released follows only after completion. A read that completes before fence
export may send Released alone. Both events travel on the second socket
and different tickets may complete out of order. Config capability bits 0
(layers/alpha) and 1 (release fences) are both required for direct layers;
older peers select composition fallback. Fence delivery permits asynchronous
Android callbacks, but only Released permits reuse of the host import and its
timeline. Source allocations and read tickets are separate from legacy host
render-target IDs. Missing fences on disconnect never imply completion.

Config capability bit 2 (`ASYNC_STAGE`) permits StageAsync. Its payload and
descriptor rules match Stage, but it produces no acknowledgement. The sender
can queue the bounded scene and then wait for Commit's single result. Stage
overflow invalidates the whole staged scene until Commit rejection or Abort;
the host must never accept an overflowing prefix even if the Commit count
matches that prefix. Rejection, Abort and disconnect drop staged descriptors
without completing any earlier submitted read. Malformed records still close
the transport. A missing Commit acknowledgement remains an ambiguous submission,
not proof of rejection. Without bit 2 the sender uses Stage and consumes each
acknowledgement. Config and Commit retain their cross-socket ordering barriers.

Config capability bit 3 (`CONFIG_EVENTS`) permits SubscribeConfig, an empty,
descriptor-free request. Its initial reply (`0x8005`) contains six words:
revision low, revision high, capabilities, configure serial, physical width,
physical height. Later changes use the same record on the event socket. The
host emits events only after subscription and when the configuration changes;
the nonzero 64-bit revision also increments for visibility/capability changes
that leave the configure serial unchanged. Revision exhaustion closes the
transport rather than wrapping. The sender ignores older revisions, rejects
conflicting snapshots at the same revision, and schedules composition on change.
This orders events against the initial reply even across the two sockets.
SubscribeConfig retains the same cross-socket barrier as Config. Cached values
replace frame-by-frame Config queries; Commit still validates current host
geometry and serial, rejecting stale submissions. Peers without bit 3 continue
querying Config. No configuration events are sent to unsubscribed peers.

Config capability bit 6 (`SCENE_BATCH`) permits StageBatch and CommitBatch,
used together with asynchronous staging, configuration and commit acceptance.
StageBatch contains a u32 chunk count followed by 1–16 entries. Each entry is
the existing 72-byte Stage payload followed by a u32 descriptor index, or
`0xffffffff` for no fence. Indices are packet-local, unique and exhaustive;
solid layers cannot carry fences. CommitBatch starts with a u64 scene ID and
u32 configure serial, width, height and total scene count, followed by the same
chunk encoding. It appends the last chunk and uses the existing CommitAsync
validation and event `0x8006`. Earlier chunks remain invisible until acceptance.
The complete scene remains bounded to 64 layers and one unacknowledged commit;
chunk overflow invalidates the whole scene. Final batches preserve the same
cross-socket barrier as Commit. Older peers use StageAsync and CommitAsync.

Config capability bit 7 (`PLACEMENT_GRID`) permits SubscribePlacement, which is
SubscribeConfig whose snapshots append two words: logical width and logical
height. A layer edge is placeable only where edge × logical size is a multiple
of the physical size; the sender composes any other scene without submitting
it. Scene rejection -95 means the host cannot place that arrangement at all, and
the sender composes it until its layers, size or configure serial change; -11
rejects that scene only. Older peers subscribe with SubscribeConfig.

`RegisterBuffer` carries an allocation ID, dimensions, explicit DRM FourCC and
modifier, and one through four dense plane records containing index, offset,
and stride. Its ancillary descriptors contain those planes in index order. A
buffer ID cannot be registered with different immutable metadata.

`Present` carries a unique frame ID, registered buffer ID, acknowledged
configure serial, acquire point, release point, and no more than 256 damage
rectangles. It is descriptor-free. Points are non-zero and monotonically
increase on their respective task timelines.

Before sending `Present`, the Android adapter imports SurfaceFlinger's acquire
`sync_file` into the named acquire point and allocates a release point. After
receiving `Present`, the host imports that operation into the matching Wayland
surface timeline. It signals the Android-facing release only after the Wayland
compositor signals its release point. If it discards a headless frame, it
signals release explicitly. Android waits for point *availability*, not fence
completion, and then exports the still-unsignalled point as the present fence
returned to SurfaceFlinger. The handoff uses shared kernel objects and requires
no synchronous protocol reply or per-frame socket descriptor.

## Host-to-Android messages

| Opcode | Message | Object | Attached descriptors |
| ---: | --- | --- | --- |
| `0x8001` | `ServerHello` | connection | none |
| `0x8002` | `FormatFeedback` | task | none |
| `0x8003` | `Configure` | task | none |
| `0x8004` | `Insets` | task | none |
| `0x8005` | `Visibility` | task | none |
| `0x8006` | `Close` | task | none |
| `0x8007` | `BufferReleased` | task | none |
| `0x8008` | `Presented` | task | none |
| `0x8009` | `Input` | task | none |
| `0x800a` | `Error` | connection or task | none |
| `0x800b` | `Ping` | connection | none |
| `0x800c` | `RegisterRenderTarget` | task | DMA-BUF plane descriptors |
| `0x800d` | `UnregisterRenderTarget` | task | none |
| `0x800e` | `DisplayPower` | connection | none |
| `0x800f` | `RequestInputChannel` | connection | none |
| `0x8010` | `ScenePresented` | task | none |

`FormatFeedback` atomically replaces the accepted explicit FourCC/modifier set
for its generation. At most 256 pairs are sent. `DRM_FORMAT_MOD_INVALID` is not
accepted because implicit modifier negotiation is outside the version-1 path.

`Configure` supplies a serial, non-zero buffer size no larger than 16,384 by
16,384 pixels, rational scale, output transform, and refresh rate. Android must
acknowledge the latest serial before presenting the initial or resized buffer.

After waiting for the acquire point, the Wayland compositor may sample,
promote, or directly scan out the buffer. When all GPU/KMS use finishes, it
signals the Wayland release point; the presenter then signals the named
Android-facing release point and sends `BufferReleased`. Android uses that
event to retire its internal in-flight frame.

`Presented` is independent from release because scanout timing and safe buffer
reuse are different events. It carries a monotonic timestamp, refresh period,
sequence, and whether the frame was composed or directly scanned out.
It may arrive after `BufferReleased`; its timing metadata must not delay safe
buffer reuse. The Wayland presenter bounds outstanding timing requests to 64
per task and omits optional requests when full, without blocking presentation.

`ScenePresented` reports a displayed asynchronous direct-layer scene without
accessing composed-frame ownership or signalling a release timeline. Its
32-byte payload contains four `u64` values in order: nonzero accepted scene ID
(not a `FrameId`), nonzero CLOCK_MONOTONIC timestamp in nanoseconds, nonzero
refresh period in nanoseconds, and host presentation sequence. It has no
descriptors or presentation flags. The presenter bounds outstanding scene
feedback to eight per task and drops observations from stale configures or
departed outputs. Android uses the observation as timing input independently
of layer read completion, which remains on the layer stream.

`Input` carries a monotonic host sequence and timestamp plus a typed touch, key,
or navigation event. Android never receives the host's physical input
device descriptors.

## Required state ordering

For one task, the minimum successful sequence is:

```text
CreateTask
  -> BindTimelines(acquire timeline FD, release timeline FD)
  <- FormatFeedback(generation, formats/modifiers)
  <- Configure(serial, size, scale, transform, refresh)
  -> AckConfigure(serial)
  -> RegisterBuffer(buffer metadata, DMA-BUF plane FDs)  [once per allocation]
  -> Present(buffer ID, frame ID, acquire point, release point, damage)
  <- Presented(frame ID, timing)                         [when displayed]
  <- BufferReleased(frame ID, buffer ID, release point) [after all use]
```

No buffer may be presented before timeline binding, compatible feedback, and
the latest configure acknowledgement. A buffer cannot be presented again
while an earlier frame using it remains unreleased. A task cannot be destroyed
while a frame is in flight. Disconnect closes all task windows and imported
descriptors through presenter-owned bounded teardown.

## Bounds and failure policy

Version 1 admits at most 4,096 task objects per cell, 256 registered buffers
per task, four planes per allocation, 256 format/modifier pairs per feedback
generation, and 256 damage rectangles per frame. Implementations may negotiate
smaller task, buffer, and damage limits in `ServerHello`.

Malformed packets, stale configuration, descriptor ambiguity, duplicate IDs,
point regression, unsupported formats, protected content, resource exhaustion,
and release-before-use violations fail closed. The host may send one bounded
`Error` record and then disconnect; correctness never falls back to pixel
readback, implicit synchronization, or a nested Android desktop.

## Performance contract

The protocol design makes three testable claims:

1. Each allocation's DMA-BUF descriptors cross the socket once per import
   lifetime, not once per frame.
2. Timeline descriptors cross once per task; a normal `Present` and its release
   notification are descriptor-free.
3. All state needed to latch a client-target frame is contained in one bounded
   `Present` record, with no synchronous host response in the Composer thread.

These properties keep the private Android boundary efficient while the host
uses ordinary Wayland for presentation. Release evidence must measure bridge
CPU time, bytes, messages, syscalls, wakeups, imports, scheduling latency,
copies, and frame misses under the same workload.
