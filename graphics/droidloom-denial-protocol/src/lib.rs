//! Denial-native wire contract for Droidloom task windows and presentation.
//!
//! This crate deliberately contains no Wayland types and performs no socket
//! syscalls. It defines and validates one packet per Unix `SOCK_SEQPACKET`
//! record. The platform adapters attach the descriptor roles returned by this
//! codec with `SCM_RIGHTS`, preserving descriptor ownership at that boundary.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use droidloom_transport::TransportError;
pub use droidloom_transport::{
    BufferId, BufferMetadata, Damage, FormatModifier, FrameId, PlaneMetadata,
};
use thiserror::Error;

/// Four-byte little-endian marker at the start of every packet.
pub const MAGIC: [u8; 4] = *b"DLOM";
/// Protocol major implemented by this crate.
pub const PROTOCOL_MAJOR: u16 = 1;
/// Protocol minor implemented by this crate.
pub const PROTOCOL_MINOR: u16 = 12;
/// Wire version retained by existing records, including initial negotiation.
pub const BASE_PROTOCOL_MINOR: u16 = 3;
/// Wire version introducing task activation.
pub const ACTIVATION_PROTOCOL_MINOR: u16 = 4;
/// Fixed wire-header size.
pub const HEADER_BYTES: usize = 32;
/// Maximum complete sequenced packet, including its header.
pub const MAX_PACKET_BYTES: usize = 65_536;
/// Maximum ancillary descriptors carried by any version-1 packet.
pub const MAX_PACKET_DESCRIPTORS: usize = 4;
/// Maximum task/package identity length in bytes.
pub const MAX_PACKAGE_BYTES: usize = 255;
/// Maximum diagnostic error text length in bytes.
pub const MAX_ERROR_BYTES: usize = 1_024;
/// Maximum format/modifier pairs in one feedback generation.
pub const MAX_FORMATS: usize = 256;
/// Maximum damage rectangles in one present.
pub const MAX_DAMAGE_RECTS: usize = 256;
/// Maximum task objects admitted by the v1 protocol.
pub const MAX_TASK_OBJECTS: u32 = 4_096;
/// Maximum registered client-target allocations per task.
pub const MAX_BUFFERS_PER_TASK: u32 = 256;

/// Feature bits exchanged during the connection handshake.
pub mod capability {
    /// Each top-level Android task is one independent native Denial task.
    pub const TASK_WINDOWS: u64 = 1 << 0;
    /// DMA-BUF planes are registered once and subsequently named by ID.
    pub const CACHED_DMABUF_IMPORT: u64 = 1 << 1;
    /// Acquire/release synchronization uses shared DRM syncobj timelines.
    pub const SYNCOBJ_TIMELINES: u64 = 1 << 2;
    /// Denial sends monotonic presentation timing for submitted frames.
    pub const PRESENTATION_TIMING: u64 = 1 << 3;
    /// Denial can route typed touch, key, and navigation input to Android.
    pub const INPUT: u64 = 1 << 4;
    /// Legacy path accepting a `SurfaceFlinger` client target.
    pub const CLIENT_TARGET: u64 = 1 << 5;
    /// Denial allocates and lends the final native-window render targets.
    pub const HOST_RENDER_TARGETS: u64 = 1 << 6;
    /// Droidloom composes Android task layers into the host render targets.
    pub const DROIDLOOM_COMPOSITION: u64 = 1 << 7;
    /// The host accepts task activation requests under its own focus policy.
    pub const TASK_ACTIVATION: u64 = 1 << 8;
    /// The host publishes whether any physical display is powered on.
    pub const DISPLAY_POWER: u64 = 1 << 9;
    /// Delegate a connected `InputFlinger` socket and framework task incarnations.
    pub const DIRECT_INPUT: u64 = 1 << 10;
    /// Allocate composition targets only after a configure-scoped request.
    pub const LAZY_RENDER_TARGETS: u64 = 1 << 11;
    /// Real presentation observations for direct layer scenes, independent of targets.
    pub const SCENE_PRESENTATION: u64 = 1 << 12;
    /// The host forwards its compositor frame grants as Android's vsync.
    pub const HOST_CLOCK: u64 = 1 << 13;
    /// Android tells the host which tasks keep the screen on.
    pub const KEEP_SCREEN_ON: u64 = 1 << 14;
    /// The host gives each task the density of the output it is on.
    pub const TASK_DENSITY: u64 = 1 << 15;
    /// Android names the task a dialog-like task opened over.
    pub const TASK_PARENTS: u64 = 1 << 16;

    /// Capabilities required for every protocol-v1 session.
    pub const REQUIRED_V1: u64 = TASK_WINDOWS
        | CACHED_DMABUF_IMPORT
        | SYNCOBJ_TIMELINES
        | PRESENTATION_TIMING
        | INPUT
        | HOST_RENDER_TARGETS
        | DROIDLOOM_COMPOSITION
        | DISPLAY_POWER;
}

/// Per-connection object naming one native Denial task window.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TaskObjectId(pub u64);

/// Android framework task identity; treated as an untrusted claim by Denial.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AndroidTaskId(pub u64);

/// Host-managed Android logical display dedicated to one task.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AndroidDisplayId(pub u64);

/// Required meaning and ordering of an attached `SCM_RIGHTS` descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescriptorKind {
    /// Connected writer endpoint for the native `InputFlinger` input receiver.
    InputChannel,
    /// One DMA-BUF plane, ordered by its dense plane index.
    DmabufPlane,
    /// Authenticated task-scoped layer request/release socket.
    LayerStream,
    /// Opaque descriptor exporting the task's acquire syncobj timeline.
    AcquireTimeline,
    /// Opaque descriptor exporting the task's release syncobj timeline.
    ReleaseTimeline,
}

/// A validated packet ready for the platform's sequenced-socket adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedPacket {
    /// Complete packet bytes, including the fixed header.
    pub bytes: Vec<u8>,
    /// Descriptor roles to attach in exactly this order.
    pub descriptors: Vec<DescriptorKind>,
}

/// A decoded message and the validated roles of received descriptors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedPacket<T> {
    /// Typed protocol message.
    pub message: T,
    /// Descriptor roles received in exactly this order.
    pub descriptors: Vec<DescriptorKind>,
}

/// Android-to-Denial protocol messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AndroidMessage {
    /// Request fallback composition buffers for an existing task configuration.
    RequestRenderTargets {
        /// Existing task object; allocation is restricted to its current size.
        object: TaskObjectId,
        /// Configure generation needing composition targets.
        configure_serial: u32,
    },
    /// Complete one host request with a connected native input socket.
    BindInputChannel {
        /// Host request generation; stale handoffs must be discarded.
        generation: u64,
        /// Input record wire ABI supported by the receiver.
        wire: u16,
    },
    /// Publish a framework-authorized route for an already-bound task object.
    BindInputRoute {
        /// Existing native task object.
        object: TaskObjectId,
        /// Actual Android logical display, independent of the export display.
        display: u32,
        /// Framework incarnation of the bound Android task.
        incarnation: u64,
    },
    /// Start version and capability negotiation for one authenticated cell.
    ClientHello {
        /// Oldest major version the client understands.
        min_major: u16,
        /// Newest major version the client understands.
        max_major: u16,
        /// Feature bits supported by the client.
        capabilities: u64,
    },
    /// Request one native Denial task window and no shared desktop surface.
    CreateTask {
        /// New per-connection task object.
        object: TaskObjectId,
        /// Dedicated Android logical display ID.
        display: AndroidDisplayId,
        /// Requested Android package identity.
        package: String,
    },
    /// Bind the real `ActivityTaskManager` ID after launch on the configured
    /// display. The create phase deliberately carries no synthetic task ID.
    BindTask {
        /// Existing per-connection task object.
        object: TaskObjectId,
        /// Nonzero task ID reported by the privileged Android controller.
        task: AndroidTaskId,
    },
    /// Tear down one task object after every buffer is released.
    DestroyTask {
        /// Task to destroy.
        object: TaskObjectId,
    },
    /// Share acquire and release DRM syncobj timelines for one task.
    BindTimelines {
        /// Task receiving the two timeline descriptors.
        object: TaskObjectId,
    },
    /// Acknowledge the latest Denial size/configuration serial.
    AckConfigure {
        /// Task being configured.
        object: TaskObjectId,
        /// Latest configure serial.
        serial: u32,
    },
    /// Register immutable DMA-BUF metadata and plane descriptors once.
    RegisterBuffer {
        /// Owning task.
        object: TaskObjectId,
        /// Allocation metadata paired with the attached plane descriptors.
        buffer: BufferMetadata,
    },
    /// Retire an idle registered DMA-BUF.
    UnregisterBuffer {
        /// Owning task.
        object: TaskObjectId,
        /// Allocation to retire.
        buffer: BufferId,
    },
    /// Atomically submit damage and syncobj points for a registered buffer.
    Present {
        /// Owning task.
        object: TaskObjectId,
        /// Unique frame identity.
        frame: FrameId,
        /// Previously registered allocation.
        buffer: BufferId,
        /// Configure serial governing this buffer's dimensions.
        configure_serial: u32,
        /// Point Denial must wait for on the acquire timeline.
        acquire_point: u64,
        /// Point Denial must signal on the release timeline.
        release_point: u64,
        /// Buffer-coordinate damage.
        damage: Vec<Damage>,
    },
    /// Delegate two private sockets for layer requests and completion events.
    BindLayerStream {
        /// Existing, authorized task object.
        object: TaskObjectId,
    },
    /// Update opaque/secure/HDR state for following presents.
    SetContentState {
        /// Owning task.
        object: TaskObjectId,
        /// Bitwise [`content_state`] values.
        flags: u32,
    },
    /// Submit a task frame-rate vote; zero withdraws it.
    SetFrameRate {
        /// Owning task.
        object: TaskObjectId,
        /// Requested rate in millihertz.
        millihz: u32,
    },
    /// Ask the host to present an existing task after an Android intent launch.
    /// Requires negotiated `TASK_ACTIVATION`; focus remains host-authoritative.
    RequestActivation {
        /// Existing task to activate.
        object: TaskObjectId,
    },
    /// A shown window of the task has Android's `FLAG_KEEP_SCREEN_ON`.
    /// Requires negotiated `KEEP_SCREEN_ON`.
    SetKeepScreenOn {
        /// Owning task.
        object: TaskObjectId,
        /// Whether the host keeps its displays on while the task is visible.
        keep: bool,
    },
    /// The task opened over `parent` and leaves it running, as a dialog does:
    /// the host keeps the parent going while the task is in front.
    /// Requires negotiated `TASK_PARENTS`.
    SetParent {
        /// Child task.
        object: TaskObjectId,
        /// Parent task, or `None` when the task has none.
        parent: Option<TaskObjectId>,
    },
    /// Answer a host liveness probe.
    Pong {
        /// Cookie copied from the matching host ping.
        cookie: u64,
    },
}

/// Content-state bits understood by protocol v1.
pub mod content_state {
    /// The submitted client target is fully opaque.
    pub const OPAQUE: u32 = 1 << 0;
    /// The content requests a protected end-to-end path.
    pub const SECURE: u32 = 1 << 1;
    /// The content contains HDR values requiring an HDR-capable path.
    pub const HDR: u32 = 1 << 2;
    /// All content-state bits understood by protocol v1.
    pub const KNOWN: u32 = OPAQUE | SECURE | HDR;
}

/// Denial-to-Android protocol messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DenialMessage {
    /// Ask for a new native input connection at startup or after disconnect.
    RequestInputChannel {
        /// Monotonic host request generation.
        generation: u64,
    },
    /// Select one version and publish authoritative connection limits.
    ServerHello {
        /// Selected protocol major.
        major: u16,
        /// Selected protocol minor.
        minor: u16,
        /// Feature bits active for this connection.
        capabilities: u64,
        /// Maximum simultaneously live task objects.
        max_task_objects: u32,
        /// Maximum registered allocations per task.
        max_buffers_per_task: u32,
        /// Maximum damage rectangles per present.
        max_damage_rects: u32,
    },
    /// Publish whether the host currently has any powered physical display.
    DisplayPower {
        /// True while at least one host output is powered on.
        powered: bool,
    },
    /// Replace the authoritative DMA-BUF import feedback generation.
    FormatFeedback {
        /// Task whose renderer/output constraints this feedback describes.
        object: TaskObjectId,
        /// Monotonic feedback generation.
        generation: u32,
        /// Accepted explicit DRM format/modifier pairs.
        formats: Vec<FormatModifier>,
    },
    /// Lend one Denial-owned DMA-BUF render target to Droidloom.
    RegisterRenderTarget {
        /// Task allowed to render into this target.
        object: TaskObjectId,
        /// Configure serial whose dimensions this target satisfies.
        configure_serial: u32,
        /// Immutable allocation metadata paired with DMA-BUF plane FDs.
        buffer: BufferMetadata,
    },
    /// Revoke one idle Denial-owned render target.
    UnregisterRenderTarget {
        /// Owning task.
        object: TaskObjectId,
        /// Target that Droidloom must stop using.
        buffer: BufferId,
    },
    /// Configure native task size, scale, transform, and refresh policy.
    Configure {
        /// Task being configured.
        object: TaskObjectId,
        /// Monotonic configure serial.
        serial: u32,
        /// Required client-target width in pixels.
        width: u32,
        /// Required client-target height in pixels.
        height: u32,
        /// Logical-scale numerator.
        scale_numerator: u32,
        /// Logical-scale denominator.
        scale_denominator: u32,
        /// Host-authoritative output transform.
        transform: Transform,
        /// Current refresh rate in millihertz.
        refresh_millihz: u32,
    },
    /// The density Android lays the task out at, for the output it is on.
    Density {
        /// Task being laid out.
        object: TaskObjectId,
        /// Dots per inch; 0 follows Android's display density.
        dpi: u32,
    },
    /// Update Denial-owned system-bar and cutout insets.
    Insets {
        /// Owning task.
        object: TaskObjectId,
        /// Left logical inset.
        left: u32,
        /// Top logical inset.
        top: u32,
        /// Right logical inset.
        right: u32,
        /// Bottom logical inset.
        bottom: u32,
    },
    /// Update authoritative task visibility and keyboard focus.
    Visibility {
        /// Owning task.
        object: TaskObjectId,
        /// Current shell visibility.
        visibility: Visibility,
        /// Whether this task owns keyboard focus.
        focused: bool,
    },
    /// Request that Android finish the named task.
    Close {
        /// Task Android should finish.
        object: TaskObjectId,
    },
    /// Confirm that Denial has signalled the frame's release timeline point.
    BufferReleased {
        /// Owning task.
        object: TaskObjectId,
        /// Released frame.
        frame: FrameId,
        /// Buffer now eligible for reuse.
        buffer: BufferId,
        /// Release timeline point that was signalled.
        release_point: u64,
    },
    /// Report host presentation timing independently of buffer release.
    Presented {
        /// Owning task.
        object: TaskObjectId,
        /// Presented frame.
        frame: FrameId,
        /// Monotonic presentation timestamp.
        timestamp_nanos: u64,
        /// Actual refresh period.
        refresh_period_nanos: u64,
        /// Denial presentation sequence.
        sequence: u64,
        /// Presentation flags from [`presentation_flag`].
        flags: u32,
    },
    /// Report a direct layer scene's display time without releasing any buffer.
    ScenePresented {
        /// Owning task.
        object: TaskObjectId,
        /// Accepted asynchronous layer scene identity, not a composed FrameId.
        scene: u64,
        /// Actual CLOCK_MONOTONIC presentation timestamp.
        timestamp_nanos: u64,
        /// Actual refresh period.
        refresh_period_nanos: u64,
        /// Host display sequence.
        sequence: u64,
    },
    /// Route one typed native input event to an Android task.
    Input {
        /// Target task.
        object: TaskObjectId,
        /// Host input sequence.
        serial: u64,
        /// Monotonic event timestamp.
        timestamp_nanos: u64,
        /// Typed event payload.
        event: InputEvent,
    },
    /// Fatal protocol error; Denial closes the connection after sending it.
    Error {
        /// Optional task related to the failure.
        object: Option<TaskObjectId>,
        /// Stable protocol error code.
        code: ProtocolErrorCode,
        /// Opcode rejected by Denial, or zero for connection-level failures.
        offending_opcode: u16,
        /// Bounded diagnostic text, never used for machine decisions.
        message: String,
    },
    /// Probe client liveness without polling.
    Ping {
        /// Opaque cookie the client must return unchanged.
        cookie: u64,
    },
    /// One presentation opportunity granted by the host compositor.
    FrameGrant {
        /// CLOCK_MONOTONIC time by which content must be ready; a host vblank.
        latch_nanos: u64,
        /// Predicted CLOCK_MONOTONIC presentation, one period after the latch.
        target_nanos: u64,
        /// Nominal refresh period of the granting output.
        period_nanos: u64,
    },
}

/// Native output transform selected by Denial.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Transform {
    /// No rotation.
    Normal = 0,
    /// Clockwise quarter turn.
    Rotate90 = 1,
    /// Half turn.
    Rotate180 = 2,
    /// Clockwise three-quarter turn.
    Rotate270 = 3,
}

/// Denial-authoritative task visibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Visibility {
    /// Not present in the visible scene.
    Hidden = 0,
    /// Visible to the user.
    Visible = 1,
    /// Present but fully obscured.
    Obscured = 2,
}

/// Presentation-result bits reported by Denial.
pub mod presentation_flag {
    /// The frame reached a physical display.
    pub const DISPLAYED: u32 = 1 << 0;
    /// The allocation was directly scanned out.
    pub const DIRECT_SCANOUT: u32 = 1 << 1;
    /// Denial sampled the buffer into its scene composition.
    pub const COMPOSITED: u32 = 1 << 2;
    /// All presentation flags understood by protocol v1.
    pub const KNOWN: u32 = DISPLAYED | DIRECT_SCANOUT | COMPOSITED;
}

/// Typed input routed from Denial; coordinates are signed 16.16 logical units.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputEvent {
    /// One multitouch contact transition or motion sample.
    Touch {
        /// Contact operation.
        action: TouchAction,
        /// Stable contact identity until the matching up/cancel.
        pointer_id: u32,
        /// Logical x coordinate in signed 16.16 fixed point.
        x_fixed: i32,
        /// Logical y coordinate in signed 16.16 fixed point.
        y_fixed: i32,
        /// Normalized pressure from zero through 65,535.
        pressure: u16,
    },
    /// Linux input key transition.
    Key {
        /// Press/release operation.
        action: KeyAction,
        /// Linux evdev key code.
        keycode: u32,
        /// Host-generated repeat count.
        repeat: u16,
    },
    /// Denial shell navigation mapped to Android task APIs.
    Navigation {
        /// Host shell action.
        action: NavigationAction,
    },
}

/// Touch contact operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TouchAction {
    /// New contact.
    Down = 0,
    /// Existing contact movement.
    Motion = 1,
    /// Contact lifted.
    Up = 2,
    /// Gesture stream cancelled by host policy.
    Cancel = 3,
}

/// Key operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum KeyAction {
    /// Key press.
    Down = 0,
    /// Key release.
    Up = 1,
}

/// Denial mobile-shell navigation action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NavigationAction {
    /// Navigate within or finish the current Android activity.
    Back = 0,
    /// Move to Denial home while updating Android visibility.
    Home = 1,
    /// Open Denial overview backed by Android task enumeration.
    Overview = 2,
}

/// Stable fatal error codes sent before disconnect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum ProtocolErrorCode {
    /// Handshake version ranges or capabilities do not intersect.
    Incompatible = 1,
    /// A message or identity is malformed.
    InvalidMessage = 2,
    /// A task, display, frame, or buffer identity conflicts.
    DuplicateObject = 3,
    /// A message references an object that does not exist.
    UnknownObject = 4,
    /// A bounded resource limit was exceeded.
    ResourceLimit = 5,
    /// Configure, buffer, frame, or release ordering is invalid.
    InvalidState = 6,
    /// The requested content cannot use a secure path.
    ProtectedContent = 7,
    /// Denial cannot import the requested format/modifier.
    UnsupportedFormat = 8,
}

/// Invalid packet shape or typed payload.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum WireError {
    /// Packet is shorter than the fixed header.
    #[error("packet is shorter than the {HEADER_BYTES}-byte header")]
    ShortHeader,
    /// Packet exceeds the hard connection bound.
    #[error("packet exceeds the {MAX_PACKET_BYTES}-byte bound")]
    PacketTooLarge,
    /// Packet does not carry the Droidloom magic.
    #[error("invalid protocol magic")]
    InvalidMagic,
    /// Packet targets a version this codec cannot interpret.
    #[error("unsupported protocol version {major}.{minor}")]
    UnsupportedVersion {
        /// Received major version.
        major: u16,
        /// Received minor version.
        minor: u16,
    },
    /// Packet payload length differs from the header.
    #[error("packet payload length does not match its header")]
    LengthMismatch,
    /// Header flags or reserved fields contain unknown non-zero bits.
    #[error("packet contains unsupported header bits")]
    UnsupportedHeaderBits,
    /// Opcode is not defined for this direction.
    #[error("unknown or wrong-direction opcode {0:#06x}")]
    UnknownOpcode(u16),
    /// The packet carried the wrong number of descriptors.
    #[error("expected {expected} descriptors but received {actual}")]
    DescriptorCount {
        /// Count implied by the typed message.
        expected: usize,
        /// Count observed by the socket adapter.
        actual: usize,
    },
    /// Payload ended before its typed fields were complete.
    #[error("truncated message payload")]
    Truncated,
    /// Payload has bytes after the typed message.
    #[error("message payload contains trailing bytes")]
    TrailingBytes,
    /// A string was not bounded valid UTF-8.
    #[error("invalid {field} string")]
    InvalidString {
        /// Name of the rejected field.
        field: &'static str,
    },
    /// A task-scoped opcode carried the reserved connection object zero.
    #[error("task object zero is reserved for connection messages")]
    InvalidObject,
    /// A typed enum or bit field contains an unknown value.
    #[error("invalid {field} value {value}")]
    InvalidEnum {
        /// Name of the rejected field.
        field: &'static str,
        /// Rejected numeric value.
        value: u64,
    },
    /// A count or numeric field exceeds a versioned bound.
    #[error("{field} exceeds its protocol bound")]
    Limit {
        /// Name of the rejected field.
        field: &'static str,
    },
    /// Existing transport invariants rejected buffer metadata.
    #[error(transparent)]
    Transport(#[from] TransportError),
}

/// Frame accepted by the Denial-side task receiver after all state checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedWireFrame {
    /// Task receiving the frame.
    pub object: TaskObjectId,
    /// Unique frame identity.
    pub frame: FrameId,
    /// Cached allocation identity.
    pub buffer: BufferId,
    /// Acquire timeline point Denial must wait for.
    pub acquire_point: u64,
    /// Release timeline point Denial must signal after all use.
    pub release_point: u64,
    /// Validated buffer-coordinate damage.
    pub damage: Vec<Damage>,
}

/// Invalid transition in one Denial-native task presentation stream.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TaskStateError {
    /// A message addressed a different task object.
    #[error("message addressed task object {actual:?}; expected {expected:?}")]
    WrongObject {
        /// Receiver's task object.
        expected: TaskObjectId,
        /// Message task object.
        actual: TaskObjectId,
    },
    /// Timeline descriptors were bound more than once.
    #[error("task timelines are already bound")]
    TimelinesAlreadyBound,
    /// Present arrived before timeline descriptors were bound.
    #[error("task timelines are not bound")]
    TimelinesUnbound,
    /// Feedback generation repeated or moved backwards.
    #[error("format feedback generation {actual} is not newer than {previous}")]
    FeedbackRegression {
        /// Last accepted generation.
        previous: u32,
        /// Rejected generation.
        actual: u32,
    },
    /// Present arrived before any host configure.
    #[error("task has no configure")]
    MissingConfigure,
    /// An acknowledgement used an unknown future serial, or a present did not
    /// use the latest configure serial.
    #[error("configure serial {actual} does not match latest serial {expected}")]
    StaleConfigure {
        /// Latest host serial.
        expected: u32,
        /// Rejected serial.
        actual: u32,
    },
    /// Present arrived before the current configure was acknowledged.
    #[error("latest configure is not acknowledged")]
    UnacknowledgedConfigure,
    /// A buffer ID was registered twice.
    #[error("buffer {0:?} is already registered")]
    DuplicateBuffer(BufferId),
    /// The per-task registration table is full.
    #[error("registered buffer limit reached")]
    BufferLimit,
    /// A message named an unregistered allocation.
    #[error("buffer {0:?} is not registered")]
    UnknownBuffer(BufferId),
    /// An allocation is still sampled, scanned out, or awaiting release.
    #[error("buffer {0:?} is still in flight")]
    BufferInFlight(BufferId),
    /// A frame identity was reused before release.
    #[error("frame {0:?} is already in flight")]
    DuplicateFrame(FrameId),
    /// A release named a frame not owned by this task.
    #[error("frame {0:?} is not in flight")]
    UnknownFrame(FrameId),
    /// A release event did not match the accepted frame tuple.
    #[error("release tuple does not match the accepted frame")]
    ReleaseMismatch,
    /// Acquire or release point repeated or moved backwards.
    #[error("{timeline} timeline point {actual} is not newer than {previous}")]
    TimelineRegression {
        /// Timeline whose point regressed.
        timeline: &'static str,
        /// Last accepted point.
        previous: u64,
        /// Rejected point.
        actual: u64,
    },
    /// Registered allocation does not match the latest configured dimensions.
    #[error("buffer dimensions do not match the latest configure")]
    SizeMismatch,
    /// Current feedback does not admit the allocation's explicit format.
    #[error("buffer format/modifier is absent from current feedback")]
    UnsupportedFormat,
    /// Damage is outside the registered allocation.
    #[error("damage rectangle exceeds the registered buffer")]
    InvalidDamage,
    /// Wire-level metadata validation failed.
    #[error(transparent)]
    Wire(#[from] WireError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingWireFrame {
    buffer: BufferId,
    release_point: u64,
}

/// Denial-side state for one Android task object.
///
/// The socket adapter calls these methods only after the packet codec has
/// validated record framing and descriptor roles. Real descriptor imports and
/// timeline operations remain outside this safe state machine.
#[derive(Debug)]
pub struct TaskPresentationState {
    object: TaskObjectId,
    timelines_bound: bool,
    feedback_generation: u32,
    feedback: BTreeSet<FormatModifier>,
    configure: Option<(u32, u32, u32)>,
    acknowledged_serial: Option<u32>,
    buffers: BTreeMap<BufferId, BufferMetadata>,
    in_flight: BTreeMap<FrameId, PendingWireFrame>,
    busy_buffers: BTreeSet<BufferId>,
    last_acquire_point: u64,
    last_release_point: u64,
}

impl TaskPresentationState {
    /// Construct state for one non-zero task object.
    ///
    /// # Errors
    ///
    /// Rejects connection object zero.
    pub fn new(object: TaskObjectId) -> Result<Self, TaskStateError> {
        valid_object(object)?;
        Ok(Self {
            object,
            timelines_bound: false,
            feedback_generation: 0,
            feedback: BTreeSet::new(),
            configure: None,
            acknowledged_serial: None,
            buffers: BTreeMap::new(),
            in_flight: BTreeMap::new(),
            busy_buffers: BTreeSet::new(),
            last_acquire_point: 0,
            last_release_point: 0,
        })
    }

    /// Return this stream's task object.
    pub fn object(&self) -> TaskObjectId {
        self.object
    }

    /// Mark the task's acquire and release timeline descriptors imported.
    ///
    /// # Errors
    ///
    /// Rejects descriptor rebinding.
    pub fn bind_timelines(&mut self) -> Result<(), TaskStateError> {
        if self.timelines_bound {
            return Err(TaskStateError::TimelinesAlreadyBound);
        }
        self.timelines_bound = true;
        Ok(())
    }

    /// Replace authoritative format/modifier feedback with a newer generation.
    ///
    /// # Errors
    ///
    /// Rejects generation regression, implicit modifiers, empty feedback, and
    /// the version-1 format count bound.
    pub fn replace_feedback(
        &mut self,
        generation: u32,
        formats: impl IntoIterator<Item = FormatModifier>,
    ) -> Result<(), TaskStateError> {
        if generation <= self.feedback_generation {
            return Err(TaskStateError::FeedbackRegression {
                previous: self.feedback_generation,
                actual: generation,
            });
        }
        let feedback = formats.into_iter().collect::<BTreeSet<_>>();
        if feedback.is_empty() || feedback.len() > MAX_FORMATS {
            return Err(WireError::Limit {
                field: "format feedback",
            }
            .into());
        }
        if feedback.iter().any(|format| format.modifier == u64::MAX) {
            return Err(WireError::InvalidEnum {
                field: "DRM modifier",
                value: u64::MAX,
            }
            .into());
        }
        self.feedback_generation = generation;
        self.feedback = feedback;
        Ok(())
    }

    /// Record the latest Denial configure and invalidate an older ack.
    ///
    /// # Errors
    ///
    /// Applies the same version-1 dimension and refresh bounds as the codec.
    pub fn configure(
        &mut self,
        serial: u32,
        width: u32,
        height: u32,
        refresh_millihz: u32,
    ) -> Result<(), TaskStateError> {
        validate_configure(serial, width, height, 1, 1, refresh_millihz)?;
        if self.configure.map(|value| value.0) != Some(serial) {
            self.acknowledged_serial = None;
        }
        self.configure = Some((serial, width, height));
        Ok(())
    }

    /// Acknowledge a configure serial that Android has consumed.
    ///
    /// A host may send a newer configure while Android's acknowledgement for
    /// the previous one is already in flight. Such superseded
    /// acknowledgements are valid but do not acknowledge the latest
    /// configure. A future serial remains a protocol error.
    ///
    /// # Errors
    ///
    /// Rejects missing or fabricated future serials.
    pub fn acknowledge_configure(&mut self, serial: u32) -> Result<(), TaskStateError> {
        let Some((expected, _, _)) = self.configure else {
            return Err(TaskStateError::MissingConfigure);
        };
        if serial == expected {
            self.acknowledged_serial = Some(serial);
            return Ok(());
        }
        // Configure serials are monotonically increasing non-zero u32 values.
        // Use wrapping sequence arithmetic so an acknowledgement immediately
        // before a u32 wrap is still recognized as older after the wrap.
        if serial_precedes(serial, expected) {
            return Ok(());
        }
        Err(TaskStateError::StaleConfigure {
            expected,
            actual: serial,
        })
    }

    /// Cache one successfully imported DMA-BUF allocation.
    ///
    /// # Errors
    ///
    /// Rejects malformed metadata, duplicate IDs, and resource exhaustion.
    pub fn register_buffer(&mut self, buffer: BufferMetadata) -> Result<(), TaskStateError> {
        buffer.validate().map_err(WireError::from)?;
        if buffer.id.0 == 0 || buffer.format.modifier == u64::MAX {
            return Err(WireError::InvalidEnum {
                field: "buffer identity or modifier",
                value: buffer.id.0,
            }
            .into());
        }
        if self.buffers.contains_key(&buffer.id) {
            return Err(TaskStateError::DuplicateBuffer(buffer.id));
        }
        if self.buffers.len() >= usize::try_from(MAX_BUFFERS_PER_TASK).unwrap_or(usize::MAX) {
            return Err(TaskStateError::BufferLimit);
        }
        self.buffers.insert(buffer.id, buffer);
        Ok(())
    }

    /// Retire one idle cached DMA-BUF import.
    ///
    /// # Errors
    ///
    /// Rejects unknown or in-flight allocations.
    pub fn unregister_buffer(
        &mut self,
        buffer: BufferId,
    ) -> Result<BufferMetadata, TaskStateError> {
        if self.busy_buffers.contains(&buffer) {
            return Err(TaskStateError::BufferInFlight(buffer));
        }
        self.buffers
            .remove(&buffer)
            .ok_or(TaskStateError::UnknownBuffer(buffer))
    }

    /// Validate and reserve one descriptor-free steady-state present.
    ///
    /// # Errors
    ///
    /// Enforces timeline/configure ordering, cached import compatibility,
    /// monotonic points, unique frames, damage bounds, and release-before-reuse.
    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments exactly match the bounded Present wire transaction"
    )]
    pub fn present(
        &mut self,
        object: TaskObjectId,
        frame: FrameId,
        buffer: BufferId,
        configure_serial: u32,
        acquire_point: u64,
        release_point: u64,
        damage: Vec<Damage>,
    ) -> Result<AcceptedWireFrame, TaskStateError> {
        self.require_object(object)?;
        if !self.timelines_bound {
            return Err(TaskStateError::TimelinesUnbound);
        }
        let Some((expected_serial, width, height)) = self.configure else {
            return Err(TaskStateError::MissingConfigure);
        };
        if configure_serial != expected_serial {
            return Err(TaskStateError::StaleConfigure {
                expected: expected_serial,
                actual: configure_serial,
            });
        }
        if self.acknowledged_serial != Some(expected_serial) {
            return Err(TaskStateError::UnacknowledgedConfigure);
        }
        let metadata = self
            .buffers
            .get(&buffer)
            .ok_or(TaskStateError::UnknownBuffer(buffer))?;
        if (metadata.width, metadata.height) != (width, height) {
            return Err(TaskStateError::SizeMismatch);
        }
        if !self.feedback.contains(&metadata.format) {
            return Err(TaskStateError::UnsupportedFormat);
        }
        if self.busy_buffers.contains(&buffer) {
            return Err(TaskStateError::BufferInFlight(buffer));
        }
        if self.in_flight.contains_key(&frame) {
            return Err(TaskStateError::DuplicateFrame(frame));
        }
        require_newer_point("acquire", self.last_acquire_point, acquire_point)?;
        require_newer_point("release", self.last_release_point, release_point)?;
        if damage.len() > MAX_DAMAGE_RECTS
            || damage.iter().any(|rect| {
                rect.width == 0
                    || rect.height == 0
                    || rect
                        .x
                        .checked_add(rect.width)
                        .is_none_or(|right| right > width)
                    || rect
                        .y
                        .checked_add(rect.height)
                        .is_none_or(|bottom| bottom > height)
            })
        {
            return Err(TaskStateError::InvalidDamage);
        }
        self.last_acquire_point = acquire_point;
        self.last_release_point = release_point;
        self.busy_buffers.insert(buffer);
        self.in_flight.insert(
            frame,
            PendingWireFrame {
                buffer,
                release_point,
            },
        );
        Ok(AcceptedWireFrame {
            object,
            frame,
            buffer,
            acquire_point,
            release_point,
            damage,
        })
    }

    /// Consume and immediately release a frame produced for a configure which
    /// the host superseded while Android was rendering it.
    ///
    /// Interactive resize is asynchronous in both directions: Android may
    /// reserve a target for configure N immediately before Denial publishes
    /// configure N+1. The completed N frame is no longer useful, but it is not
    /// a protocol violation and its timeline points must still be consumed so
    /// the producer can retire the old target. Future/current serials and all
    /// timeline regressions remain hard errors.
    ///
    /// Unlike [`Self::present`], this path deliberately does not require the
    /// old buffer to remain registered. Denial may already have revoked the
    /// superseded target before its late `Present` packet reaches the endpoint.
    ///
    /// # Errors
    ///
    /// Rejects an unknown object, missing timeline/configure state, a frame
    /// that is not actually superseded, or regressing frame/timeline points.
    pub fn discard_superseded_present(
        &mut self,
        object: TaskObjectId,
        frame: FrameId,
        buffer: BufferId,
        configure_serial: u32,
        acquire_point: u64,
        release_point: u64,
    ) -> Result<DenialMessage, TaskStateError> {
        self.require_object(object)?;
        if !self.timelines_bound {
            return Err(TaskStateError::TimelinesUnbound);
        }
        let Some((expected_serial, _, _)) = self.configure else {
            return Err(TaskStateError::MissingConfigure);
        };
        if !serial_precedes(configure_serial, expected_serial) {
            return Err(TaskStateError::StaleConfigure {
                expected: expected_serial,
                actual: configure_serial,
            });
        }
        if self.busy_buffers.contains(&buffer) {
            return Err(TaskStateError::BufferInFlight(buffer));
        }
        if self.in_flight.contains_key(&frame) {
            return Err(TaskStateError::DuplicateFrame(frame));
        }
        require_newer_point("acquire", self.last_acquire_point, acquire_point)?;
        require_newer_point("release", self.last_release_point, release_point)?;
        self.last_acquire_point = acquire_point;
        self.last_release_point = release_point;
        Ok(DenialMessage::BufferReleased {
            object: self.object,
            frame,
            buffer,
            release_point,
        })
    }

    /// Complete a frame after Denial has signalled its release timeline point.
    ///
    /// # Errors
    ///
    /// Rejects unknown frames and tuple mismatches without releasing the buffer.
    pub fn release(
        &mut self,
        frame: FrameId,
        buffer: BufferId,
        release_point: u64,
    ) -> Result<DenialMessage, TaskStateError> {
        let pending = self
            .in_flight
            .get(&frame)
            .copied()
            .ok_or(TaskStateError::UnknownFrame(frame))?;
        if pending.buffer != buffer || pending.release_point != release_point {
            return Err(TaskStateError::ReleaseMismatch);
        }
        self.in_flight.remove(&frame);
        self.busy_buffers.remove(&buffer);
        Ok(DenialMessage::BufferReleased {
            object: self.object,
            frame,
            buffer,
            release_point,
        })
    }

    /// Roll back a present which failed before Denial submitted or otherwise
    /// began using its buffer.
    ///
    /// Timeline point numbers remain consumed; later presents must continue
    /// with newer values.
    ///
    /// # Errors
    ///
    /// Rejects unknown frames and tuple mismatches.
    pub fn cancel_unsubmitted(
        &mut self,
        frame: FrameId,
        buffer: BufferId,
        release_point: u64,
    ) -> Result<(), TaskStateError> {
        let pending = self
            .in_flight
            .get(&frame)
            .copied()
            .ok_or(TaskStateError::UnknownFrame(frame))?;
        if pending.buffer != buffer || pending.release_point != release_point {
            return Err(TaskStateError::ReleaseMismatch);
        }
        self.in_flight.remove(&frame);
        self.busy_buffers.remove(&buffer);
        Ok(())
    }

    /// Number of imported allocations cached for this task.
    pub fn registered_buffer_count(&self) -> usize {
        self.buffers.len()
    }

    /// Number of frames still owned by Denial.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    fn require_object(&self, object: TaskObjectId) -> Result<(), TaskStateError> {
        if object == self.object {
            Ok(())
        } else {
            Err(TaskStateError::WrongObject {
                expected: self.object,
                actual: object,
            })
        }
    }
}

fn require_newer_point(
    timeline: &'static str,
    previous: u64,
    actual: u64,
) -> Result<(), TaskStateError> {
    if actual > previous {
        Ok(())
    } else {
        Err(TaskStateError::TimelineRegression {
            timeline,
            previous,
            actual,
        })
    }
}

fn serial_precedes(actual: u32, expected: u32) -> bool {
    actual != expected && expected.wrapping_sub(actual) < (1_u32 << 31)
}

const OP_CLIENT_HELLO: u16 = 0x0001;
const OP_CREATE_TASK: u16 = 0x0002;
const OP_DESTROY_TASK: u16 = 0x0003;
const OP_BIND_TIMELINES: u16 = 0x0004;
const OP_ACK_CONFIGURE: u16 = 0x0005;
const OP_REGISTER_BUFFER: u16 = 0x0006;
const OP_UNREGISTER_BUFFER: u16 = 0x0007;
const OP_PRESENT: u16 = 0x0008;
const OP_SET_CONTENT_STATE: u16 = 0x0009;
const OP_SET_FRAME_RATE: u16 = 0x000a;
const OP_PONG: u16 = 0x000b;
const OP_BIND_TASK: u16 = 0x000c;
const OP_BIND_LAYER_STREAM: u16 = 0x000d;
const OP_REQUEST_ACTIVATION: u16 = 0x000e;
const OP_BIND_INPUT_CHANNEL: u16 = 0x000f;
const OP_BIND_INPUT_ROUTE: u16 = 0x0010;
const OP_REQUEST_RENDER_TARGETS: u16 = 0x0011;
const OP_SET_KEEP_SCREEN_ON: u16 = 0x0012;
const OP_SET_PARENT: u16 = 0x0013;

const OP_SERVER_HELLO: u16 = 0x8001;
const OP_FORMAT_FEEDBACK: u16 = 0x8002;
const OP_CONFIGURE: u16 = 0x8003;
const OP_INSETS: u16 = 0x8004;
const OP_VISIBILITY: u16 = 0x8005;
const OP_CLOSE: u16 = 0x8006;
const OP_BUFFER_RELEASED: u16 = 0x8007;
const OP_PRESENTED: u16 = 0x8008;
const OP_INPUT: u16 = 0x8009;
const OP_ERROR: u16 = 0x800a;
const OP_PING: u16 = 0x800b;
const OP_REGISTER_RENDER_TARGET: u16 = 0x800c;
const OP_UNREGISTER_RENDER_TARGET: u16 = 0x800d;
const OP_DISPLAY_POWER: u16 = 0x800e;
const OP_REQUEST_INPUT_CHANNEL: u16 = 0x800f;
const OP_SCENE_PRESENTED: u16 = 0x8010;
const OP_FRAME_GRANT: u16 = 0x8011;
const OP_DENSITY: u16 = 0x8012;

/// Encode one Android-to-Denial message.
///
/// # Errors
///
/// Rejects malformed identities, metadata, bounds, and unknown bit values.
#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive message encoder stays visibly aligned with the wire opcode table"
)]
pub fn encode_android(message: &AndroidMessage) -> Result<EncodedPacket, WireError> {
    let mut payload = Writer::default();
    let (opcode, object, descriptors) = match message {
        AndroidMessage::RequestRenderTargets {
            object,
            configure_serial,
        } => {
            nonzero("configure serial", u64::from(*configure_serial))?;
            payload.u32(*configure_serial);
            (
                OP_REQUEST_RENDER_TARGETS,
                valid_object(*object)?,
                Vec::new(),
            )
        }
        AndroidMessage::BindInputChannel { generation, wire } => {
            nonzero("input generation", *generation)?;
            nonzero("input wire ABI", u64::from(*wire))?;
            payload.u64(*generation);
            payload.u16(*wire);
            (OP_BIND_INPUT_CHANNEL, 0, vec![DescriptorKind::InputChannel])
        }
        AndroidMessage::BindInputRoute {
            object,
            display,
            incarnation,
        } => {
            validate_input_route(*display, *incarnation)?;
            payload.u32(*display);
            payload.u64(*incarnation);
            (OP_BIND_INPUT_ROUTE, valid_object(*object)?, Vec::new())
        }
        AndroidMessage::ClientHello {
            min_major,
            max_major,
            capabilities,
        } => {
            if *min_major == 0 || min_major > max_major {
                return Err(WireError::InvalidEnum {
                    field: "version range",
                    value: u64::from(*min_major),
                });
            }
            payload.u16(*min_major);
            payload.u16(*max_major);
            payload.u64(*capabilities);
            (OP_CLIENT_HELLO, 0, Vec::new())
        }
        AndroidMessage::CreateTask {
            object,
            display,
            package,
        } => {
            let object = valid_object(*object)?;
            if display.0 == 0 || !valid_package(package) {
                return Err(WireError::InvalidString {
                    field: "task identity",
                });
            }
            payload.u64(display.0);
            payload.string(package, MAX_PACKAGE_BYTES, "package")?;
            (OP_CREATE_TASK, object, Vec::new())
        }
        AndroidMessage::BindTask { object, task } => {
            nonzero("Android task identity", task.0)?;
            payload.u64(task.0);
            (OP_BIND_TASK, valid_object(*object)?, Vec::new())
        }
        AndroidMessage::DestroyTask { object } => {
            (OP_DESTROY_TASK, valid_object(*object)?, Vec::new())
        }
        AndroidMessage::BindTimelines { object } => (
            OP_BIND_TIMELINES,
            valid_object(*object)?,
            vec![
                DescriptorKind::AcquireTimeline,
                DescriptorKind::ReleaseTimeline,
            ],
        ),
        AndroidMessage::AckConfigure { object, serial } => {
            if *serial == 0 {
                return Err(WireError::InvalidEnum {
                    field: "configure serial",
                    value: 0,
                });
            }
            payload.u32(*serial);
            (OP_ACK_CONFIGURE, valid_object(*object)?, Vec::new())
        }
        AndroidMessage::RegisterBuffer { object, buffer } => {
            buffer.validate()?;
            if buffer.id.0 == 0 || buffer.format.modifier == u64::MAX {
                return Err(WireError::InvalidEnum {
                    field: "buffer identity or modifier",
                    value: buffer.id.0,
                });
            }
            encode_buffer(&mut payload, buffer)?;
            (
                OP_REGISTER_BUFFER,
                valid_object(*object)?,
                vec![DescriptorKind::DmabufPlane; buffer.planes.len()],
            )
        }
        AndroidMessage::UnregisterBuffer { object, buffer } => {
            nonzero("buffer ID", buffer.0)?;
            payload.u64(buffer.0);
            (OP_UNREGISTER_BUFFER, valid_object(*object)?, Vec::new())
        }
        AndroidMessage::Present {
            object,
            frame,
            buffer,
            configure_serial,
            acquire_point,
            release_point,
            damage,
        } => {
            nonzero("frame ID", frame.0)?;
            nonzero("buffer ID", buffer.0)?;
            nonzero("configure serial", u64::from(*configure_serial))?;
            nonzero("acquire point", *acquire_point)?;
            nonzero("release point", *release_point)?;
            if damage.len() > MAX_DAMAGE_RECTS {
                return Err(WireError::Limit { field: "damage" });
            }
            payload.u64(frame.0);
            payload.u64(buffer.0);
            payload.u32(*configure_serial);
            payload.u64(*acquire_point);
            payload.u64(*release_point);
            payload.u16(
                u16::try_from(damage.len()).map_err(|_| WireError::Limit { field: "damage" })?,
            );
            for rect in damage {
                validate_damage(*rect)?;
                payload.u32(rect.x);
                payload.u32(rect.y);
                payload.u32(rect.width);
                payload.u32(rect.height);
            }
            (OP_PRESENT, valid_object(*object)?, Vec::new())
        }
        AndroidMessage::BindLayerStream { object } => (
            OP_BIND_LAYER_STREAM,
            valid_object(*object)?,
            vec![DescriptorKind::LayerStream; 2],
        ),
        AndroidMessage::SetContentState { object, flags } => {
            known_bits(
                "content state",
                u64::from(*flags),
                u64::from(content_state::KNOWN),
            )?;
            payload.u32(*flags);
            (OP_SET_CONTENT_STATE, valid_object(*object)?, Vec::new())
        }
        AndroidMessage::SetFrameRate { object, millihz } => {
            if *millihz > 1_000_000 {
                return Err(WireError::Limit {
                    field: "frame rate",
                });
            }
            payload.u32(*millihz);
            (OP_SET_FRAME_RATE, valid_object(*object)?, Vec::new())
        }
        AndroidMessage::RequestActivation { object } => {
            (OP_REQUEST_ACTIVATION, valid_object(*object)?, Vec::new())
        }
        AndroidMessage::SetKeepScreenOn { object, keep } => {
            payload.u32(u32::from(*keep));
            (OP_SET_KEEP_SCREEN_ON, valid_object(*object)?, Vec::new())
        }
        AndroidMessage::SetParent { object, parent } => {
            let parent = match parent {
                Some(parent) if parent == object => {
                    return Err(WireError::InvalidEnum {
                        field: "parent task",
                        value: parent.0,
                    });
                }
                Some(parent) => valid_object(*parent)?,
                None => 0,
            };
            payload.u64(parent);
            (OP_SET_PARENT, valid_object(*object)?, Vec::new())
        }
        AndroidMessage::Pong { cookie } => {
            payload.u64(*cookie);
            (OP_PONG, 0, Vec::new())
        }
    };
    finish_packet(opcode, object, &payload.bytes, descriptors)
}

/// Decode one Android-to-Denial sequenced packet.
///
/// `received_descriptors` is the exact number extracted from the record's
/// ancillary data by the socket adapter.
///
/// # Errors
///
/// Rejects malformed framing, wrong-direction opcodes, descriptor mismatches,
/// and invalid typed fields.
#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive message decoder stays visibly aligned with the wire opcode table"
)]
pub fn decode_android(
    packet: &[u8],
    received_descriptors: usize,
) -> Result<DecodedPacket<AndroidMessage>, WireError> {
    let header = parse_header(packet, received_descriptors)?;
    let object = TaskObjectId(header.object);
    let mut payload = Reader::new(header.payload);
    let (message, descriptors) = match header.opcode {
        OP_REQUEST_RENDER_TARGETS => {
            valid_object(object)?;
            let configure_serial = payload.u32()?;
            nonzero("configure serial", u64::from(configure_serial))?;
            (
                AndroidMessage::RequestRenderTargets {
                    object,
                    configure_serial,
                },
                Vec::new(),
            )
        }
        OP_CLIENT_HELLO => {
            require_connection(header.object)?;
            let min_major = payload.u16()?;
            let max_major = payload.u16()?;
            if min_major == 0 || min_major > max_major {
                return Err(WireError::InvalidEnum {
                    field: "version range",
                    value: u64::from(min_major),
                });
            }
            (
                AndroidMessage::ClientHello {
                    min_major,
                    max_major,
                    capabilities: payload.u64()?,
                },
                Vec::new(),
            )
        }
        OP_CREATE_TASK => {
            valid_object(object)?;
            let display = AndroidDisplayId(payload.u64()?);
            let package = payload.string(MAX_PACKAGE_BYTES, "package")?;
            if display.0 == 0 || !valid_package(&package) {
                return Err(WireError::InvalidString {
                    field: "task identity",
                });
            }
            (
                AndroidMessage::CreateTask {
                    object,
                    display,
                    package,
                },
                Vec::new(),
            )
        }
        OP_BIND_TASK => {
            valid_object(object)?;
            let task = AndroidTaskId(payload.u64()?);
            nonzero("Android task identity", task.0)?;
            (AndroidMessage::BindTask { object, task }, Vec::new())
        }
        OP_DESTROY_TASK => {
            valid_object(object)?;
            (AndroidMessage::DestroyTask { object }, Vec::new())
        }
        OP_BIND_TIMELINES => {
            valid_object(object)?;
            (
                AndroidMessage::BindTimelines { object },
                vec![
                    DescriptorKind::AcquireTimeline,
                    DescriptorKind::ReleaseTimeline,
                ],
            )
        }
        OP_ACK_CONFIGURE => {
            valid_object(object)?;
            let serial = payload.u32()?;
            nonzero("configure serial", u64::from(serial))?;
            (AndroidMessage::AckConfigure { object, serial }, Vec::new())
        }
        OP_REGISTER_BUFFER => {
            valid_object(object)?;
            let buffer = decode_buffer(&mut payload)?;
            let descriptors = vec![DescriptorKind::DmabufPlane; buffer.planes.len()];
            (
                AndroidMessage::RegisterBuffer { object, buffer },
                descriptors,
            )
        }
        OP_UNREGISTER_BUFFER => {
            valid_object(object)?;
            let buffer = BufferId(payload.u64()?);
            nonzero("buffer ID", buffer.0)?;
            (
                AndroidMessage::UnregisterBuffer { object, buffer },
                Vec::new(),
            )
        }
        OP_PRESENT => {
            valid_object(object)?;
            let frame = FrameId(payload.u64()?);
            let buffer = BufferId(payload.u64()?);
            let configure_serial = payload.u32()?;
            let acquire_point = payload.u64()?;
            let release_point = payload.u64()?;
            nonzero("frame ID", frame.0)?;
            nonzero("buffer ID", buffer.0)?;
            nonzero("configure serial", u64::from(configure_serial))?;
            nonzero("acquire point", acquire_point)?;
            nonzero("release point", release_point)?;
            let count = usize::from(payload.u16()?);
            if count > MAX_DAMAGE_RECTS {
                return Err(WireError::Limit { field: "damage" });
            }
            let mut damage = Vec::with_capacity(count);
            for _ in 0..count {
                let rect = Damage {
                    x: payload.u32()?,
                    y: payload.u32()?,
                    width: payload.u32()?,
                    height: payload.u32()?,
                };
                validate_damage(rect)?;
                damage.push(rect);
            }
            (
                AndroidMessage::Present {
                    object,
                    frame,
                    buffer,
                    configure_serial,
                    acquire_point,
                    release_point,
                    damage,
                },
                Vec::new(),
            )
        }
        OP_BIND_INPUT_CHANNEL => {
            require_connection(header.object)?;
            let generation = payload.u64()?;
            nonzero("input generation", generation)?;
            let wire = payload.u16()?;
            nonzero("input wire ABI", u64::from(wire))?;
            (
                AndroidMessage::BindInputChannel { generation, wire },
                vec![DescriptorKind::InputChannel],
            )
        }
        OP_BIND_INPUT_ROUTE => {
            valid_object(object)?;
            let display = payload.u32()?;
            let incarnation = payload.u64()?;
            validate_input_route(display, incarnation)?;
            (
                AndroidMessage::BindInputRoute {
                    object,
                    display,
                    incarnation,
                },
                Vec::new(),
            )
        }
        OP_BIND_LAYER_STREAM => {
            valid_object(object)?;
            (
                AndroidMessage::BindLayerStream { object },
                vec![DescriptorKind::LayerStream; 2],
            )
        }
        OP_SET_CONTENT_STATE => {
            valid_object(object)?;
            let flags = payload.u32()?;
            known_bits(
                "content state",
                u64::from(flags),
                u64::from(content_state::KNOWN),
            )?;
            (
                AndroidMessage::SetContentState { object, flags },
                Vec::new(),
            )
        }
        OP_SET_FRAME_RATE => {
            valid_object(object)?;
            let millihz = payload.u32()?;
            if millihz > 1_000_000 {
                return Err(WireError::Limit {
                    field: "frame rate",
                });
            }
            (AndroidMessage::SetFrameRate { object, millihz }, Vec::new())
        }
        OP_REQUEST_ACTIVATION => {
            valid_object(object)?;
            (AndroidMessage::RequestActivation { object }, Vec::new())
        }
        OP_SET_KEEP_SCREEN_ON => {
            valid_object(object)?;
            let keep = match payload.u32()? {
                0 => false,
                1 => true,
                value => {
                    return Err(WireError::InvalidEnum {
                        field: "keep screen on",
                        value: u64::from(value),
                    });
                }
            };
            (AndroidMessage::SetKeepScreenOn { object, keep }, Vec::new())
        }
        OP_SET_PARENT => {
            valid_object(object)?;
            let parent = match payload.u64()? {
                0 => None,
                value if value == object.0 => {
                    return Err(WireError::InvalidEnum {
                        field: "parent task",
                        value,
                    });
                }
                value => {
                    let parent = TaskObjectId(value);
                    valid_object(parent)?;
                    Some(parent)
                }
            };
            (AndroidMessage::SetParent { object, parent }, Vec::new())
        }
        OP_PONG => {
            require_connection(header.object)?;
            (
                AndroidMessage::Pong {
                    cookie: payload.u64()?,
                },
                Vec::new(),
            )
        }
        opcode => return Err(WireError::UnknownOpcode(opcode)),
    };
    payload.finish()?;
    validate_descriptor_layout(header.fd_count, received_descriptors, &descriptors)?;
    Ok(DecodedPacket {
        message,
        descriptors,
    })
}

/// Encode one Denial-to-Android message.
///
/// # Errors
///
/// Rejects malformed limits, configuration, enum values, and text.
#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive message encoder stays visibly aligned with the wire opcode table"
)]
pub fn encode_denial(message: &DenialMessage) -> Result<EncodedPacket, WireError> {
    let mut payload = Writer::default();
    let (opcode, object) = match message {
        DenialMessage::RequestInputChannel { generation } => {
            nonzero("input generation", *generation)?;
            payload.u64(*generation);
            (OP_REQUEST_INPUT_CHANNEL, 0)
        }
        DenialMessage::ServerHello {
            major,
            minor,
            capabilities,
            max_task_objects,
            max_buffers_per_task,
            max_damage_rects,
        } => {
            validate_server_hello(
                *major,
                *minor,
                *capabilities,
                *max_task_objects,
                *max_buffers_per_task,
                *max_damage_rects,
            )?;
            payload.u16(*major);
            payload.u16(*minor);
            payload.u64(*capabilities);
            payload.u32(*max_task_objects);
            payload.u32(*max_buffers_per_task);
            payload.u32(*max_damage_rects);
            (OP_SERVER_HELLO, 0)
        }
        DenialMessage::DisplayPower { powered } => {
            payload.u8(u8::from(*powered));
            (OP_DISPLAY_POWER, 0)
        }
        DenialMessage::FormatFeedback {
            object,
            generation,
            formats,
        } => {
            nonzero("feedback generation", u64::from(*generation))?;
            if formats.is_empty() || formats.len() > MAX_FORMATS {
                return Err(WireError::Limit {
                    field: "format feedback",
                });
            }
            payload.u32(*generation);
            payload.u16(u16::try_from(formats.len()).map_err(|_| WireError::Limit {
                field: "format feedback",
            })?);
            for format in formats {
                if format.modifier == u64::MAX {
                    return Err(WireError::InvalidEnum {
                        field: "DRM modifier",
                        value: format.modifier,
                    });
                }
                payload.u32(format.fourcc);
                payload.u64(format.modifier);
            }
            (OP_FORMAT_FEEDBACK, valid_object(*object)?)
        }
        DenialMessage::RegisterRenderTarget {
            object,
            configure_serial,
            buffer,
        } => {
            nonzero("configure serial", u64::from(*configure_serial))?;
            buffer.validate()?;
            if buffer.id.0 == 0 || buffer.format.modifier == u64::MAX {
                return Err(WireError::InvalidEnum {
                    field: "render-target identity or modifier",
                    value: buffer.id.0,
                });
            }
            payload.u32(*configure_serial);
            encode_buffer(&mut payload, buffer)?;
            (OP_REGISTER_RENDER_TARGET, valid_object(*object)?)
        }
        DenialMessage::UnregisterRenderTarget { object, buffer } => {
            nonzero("render-target ID", buffer.0)?;
            payload.u64(buffer.0);
            (OP_UNREGISTER_RENDER_TARGET, valid_object(*object)?)
        }
        DenialMessage::Configure {
            object,
            serial,
            width,
            height,
            scale_numerator,
            scale_denominator,
            transform,
            refresh_millihz,
        } => {
            validate_configure(
                *serial,
                *width,
                *height,
                *scale_numerator,
                *scale_denominator,
                *refresh_millihz,
            )?;
            payload.u32(*serial);
            payload.u32(*width);
            payload.u32(*height);
            payload.u32(*scale_numerator);
            payload.u32(*scale_denominator);
            payload.u8(*transform as u8);
            payload.u32(*refresh_millihz);
            (OP_CONFIGURE, valid_object(*object)?)
        }
        DenialMessage::Density { object, dpi } => {
            payload.u32(*dpi);
            (OP_DENSITY, valid_object(*object)?)
        }
        DenialMessage::Insets {
            object,
            left,
            top,
            right,
            bottom,
        } => {
            payload.u32(*left);
            payload.u32(*top);
            payload.u32(*right);
            payload.u32(*bottom);
            (OP_INSETS, valid_object(*object)?)
        }
        DenialMessage::Visibility {
            object,
            visibility,
            focused,
        } => {
            payload.u8(*visibility as u8);
            payload.u8(u8::from(*focused));
            (OP_VISIBILITY, valid_object(*object)?)
        }
        DenialMessage::Close { object } => (OP_CLOSE, valid_object(*object)?),
        DenialMessage::BufferReleased {
            object,
            frame,
            buffer,
            release_point,
        } => {
            nonzero("frame ID", frame.0)?;
            nonzero("buffer ID", buffer.0)?;
            nonzero("release point", *release_point)?;
            payload.u64(frame.0);
            payload.u64(buffer.0);
            payload.u64(*release_point);
            (OP_BUFFER_RELEASED, valid_object(*object)?)
        }
        DenialMessage::Presented {
            object,
            frame,
            timestamp_nanos,
            refresh_period_nanos,
            sequence,
            flags,
        } => {
            nonzero("frame ID", frame.0)?;
            nonzero("refresh period", *refresh_period_nanos)?;
            known_bits(
                "presentation flags",
                u64::from(*flags),
                u64::from(presentation_flag::KNOWN),
            )?;
            payload.u64(frame.0);
            payload.u64(*timestamp_nanos);
            payload.u64(*refresh_period_nanos);
            payload.u64(*sequence);
            payload.u32(*flags);
            (OP_PRESENTED, valid_object(*object)?)
        }
        DenialMessage::ScenePresented {
            object,
            scene,
            timestamp_nanos,
            refresh_period_nanos,
            sequence,
        } => {
            nonzero("scene ID", *scene)?;
            nonzero("presentation timestamp", *timestamp_nanos)?;
            nonzero("refresh period", *refresh_period_nanos)?;
            payload.u64(*scene);
            payload.u64(*timestamp_nanos);
            payload.u64(*refresh_period_nanos);
            payload.u64(*sequence);
            (OP_SCENE_PRESENTED, valid_object(*object)?)
        }
        DenialMessage::Input {
            object,
            serial,
            timestamp_nanos,
            event,
        } => {
            nonzero("input serial", *serial)?;
            payload.u64(*serial);
            payload.u64(*timestamp_nanos);
            encode_input(&mut payload, *event);
            (OP_INPUT, valid_object(*object)?)
        }
        DenialMessage::Error {
            object,
            code,
            offending_opcode,
            message,
        } => {
            payload.u16(*code as u16);
            payload.u16(*offending_opcode);
            payload.string(message, MAX_ERROR_BYTES, "error")?;
            (
                OP_ERROR,
                match object {
                    Some(value) => valid_object(*value)?,
                    None => 0,
                },
            )
        }
        DenialMessage::Ping { cookie } => {
            payload.u64(*cookie);
            (OP_PING, 0)
        }
        DenialMessage::FrameGrant {
            latch_nanos,
            target_nanos,
            period_nanos,
        } => {
            frame_grant(*latch_nanos, *target_nanos, *period_nanos)?;
            payload.u64(*latch_nanos);
            payload.u64(*target_nanos);
            payload.u64(*period_nanos);
            (OP_FRAME_GRANT, 0)
        }
    };
    let descriptors = match message {
        DenialMessage::RegisterRenderTarget { buffer, .. } => {
            vec![DescriptorKind::DmabufPlane; buffer.planes.len()]
        }
        _ => Vec::new(),
    };
    finish_packet(opcode, object, &payload.bytes, descriptors)
}

/// Decode one Denial-to-Android sequenced packet.
///
/// # Errors
///
/// Rejects malformed framing, wrong-direction opcodes, unexpected descriptors,
/// and invalid typed fields.
#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive message decoder stays visibly aligned with the wire opcode table"
)]
pub fn decode_denial(
    packet: &[u8],
    received_descriptors: usize,
) -> Result<DecodedPacket<DenialMessage>, WireError> {
    let header = parse_header(packet, received_descriptors)?;
    let object = TaskObjectId(header.object);
    let mut payload = Reader::new(header.payload);
    let message = match header.opcode {
        OP_SERVER_HELLO => {
            require_connection(header.object)?;
            let major = payload.u16()?;
            let minor = payload.u16()?;
            let capabilities = payload.u64()?;
            let max_task_objects = payload.u32()?;
            let max_buffers_per_task = payload.u32()?;
            let max_damage_rects = payload.u32()?;
            validate_server_hello(
                major,
                minor,
                capabilities,
                max_task_objects,
                max_buffers_per_task,
                max_damage_rects,
            )?;
            DenialMessage::ServerHello {
                major,
                minor,
                capabilities,
                max_task_objects,
                max_buffers_per_task,
                max_damage_rects,
            }
        }
        OP_REQUEST_INPUT_CHANNEL => {
            require_connection(header.object)?;
            let generation = payload.u64()?;
            nonzero("input generation", generation)?;
            DenialMessage::RequestInputChannel { generation }
        }
        OP_DISPLAY_POWER => {
            require_connection(header.object)?;
            DenialMessage::DisplayPower {
                powered: decode_bool(payload.u8()?, "display power")?,
            }
        }
        OP_FORMAT_FEEDBACK => {
            valid_object(object)?;
            let generation = payload.u32()?;
            nonzero("feedback generation", u64::from(generation))?;
            let count = usize::from(payload.u16()?);
            if count == 0 || count > MAX_FORMATS {
                return Err(WireError::Limit {
                    field: "format feedback",
                });
            }
            let mut formats = Vec::with_capacity(count);
            for _ in 0..count {
                let format = FormatModifier {
                    fourcc: payload.u32()?,
                    modifier: payload.u64()?,
                };
                if format.modifier == u64::MAX {
                    return Err(WireError::InvalidEnum {
                        field: "DRM modifier",
                        value: format.modifier,
                    });
                }
                formats.push(format);
            }
            DenialMessage::FormatFeedback {
                object,
                generation,
                formats,
            }
        }
        OP_REGISTER_RENDER_TARGET => {
            valid_object(object)?;
            let configure_serial = payload.u32()?;
            nonzero("configure serial", u64::from(configure_serial))?;
            let buffer = decode_buffer(&mut payload)?;
            DenialMessage::RegisterRenderTarget {
                object,
                configure_serial,
                buffer,
            }
        }
        OP_UNREGISTER_RENDER_TARGET => {
            valid_object(object)?;
            let buffer = BufferId(payload.u64()?);
            nonzero("render-target ID", buffer.0)?;
            DenialMessage::UnregisterRenderTarget { object, buffer }
        }
        OP_CONFIGURE => {
            valid_object(object)?;
            let serial = payload.u32()?;
            let width = payload.u32()?;
            let height = payload.u32()?;
            let scale_numerator = payload.u32()?;
            let scale_denominator = payload.u32()?;
            let transform = decode_transform(payload.u8()?)?;
            let refresh_millihz = payload.u32()?;
            validate_configure(
                serial,
                width,
                height,
                scale_numerator,
                scale_denominator,
                refresh_millihz,
            )?;
            DenialMessage::Configure {
                object,
                serial,
                width,
                height,
                scale_numerator,
                scale_denominator,
                transform,
                refresh_millihz,
            }
        }
        OP_DENSITY => {
            valid_object(object)?;
            DenialMessage::Density {
                object,
                dpi: payload.u32()?,
            }
        }
        OP_INSETS => {
            valid_object(object)?;
            DenialMessage::Insets {
                object,
                left: payload.u32()?,
                top: payload.u32()?,
                right: payload.u32()?,
                bottom: payload.u32()?,
            }
        }
        OP_VISIBILITY => {
            valid_object(object)?;
            let visibility = decode_visibility(payload.u8()?)?;
            let focused = decode_bool(payload.u8()?, "focus")?;
            DenialMessage::Visibility {
                object,
                visibility,
                focused,
            }
        }
        OP_CLOSE => {
            valid_object(object)?;
            DenialMessage::Close { object }
        }
        OP_BUFFER_RELEASED => {
            valid_object(object)?;
            let frame = FrameId(payload.u64()?);
            let buffer = BufferId(payload.u64()?);
            let release_point = payload.u64()?;
            nonzero("frame ID", frame.0)?;
            nonzero("buffer ID", buffer.0)?;
            nonzero("release point", release_point)?;
            DenialMessage::BufferReleased {
                object,
                frame,
                buffer,
                release_point,
            }
        }
        OP_PRESENTED => {
            valid_object(object)?;
            let frame = FrameId(payload.u64()?);
            let timestamp_nanos = payload.u64()?;
            let refresh_period_nanos = payload.u64()?;
            let sequence = payload.u64()?;
            let flags = payload.u32()?;
            nonzero("frame ID", frame.0)?;
            nonzero("refresh period", refresh_period_nanos)?;
            known_bits(
                "presentation flags",
                u64::from(flags),
                u64::from(presentation_flag::KNOWN),
            )?;
            DenialMessage::Presented {
                object,
                frame,
                timestamp_nanos,
                refresh_period_nanos,
                sequence,
                flags,
            }
        }
        OP_SCENE_PRESENTED => {
            valid_object(object)?;
            let scene = payload.u64()?;
            let timestamp_nanos = payload.u64()?;
            let refresh_period_nanos = payload.u64()?;
            let sequence = payload.u64()?;
            nonzero("scene ID", scene)?;
            nonzero("presentation timestamp", timestamp_nanos)?;
            nonzero("refresh period", refresh_period_nanos)?;
            DenialMessage::ScenePresented {
                object,
                scene,
                timestamp_nanos,
                refresh_period_nanos,
                sequence,
            }
        }
        OP_INPUT => {
            valid_object(object)?;
            let serial = payload.u64()?;
            nonzero("input serial", serial)?;
            DenialMessage::Input {
                object,
                serial,
                timestamp_nanos: payload.u64()?,
                event: decode_input(&mut payload)?,
            }
        }
        OP_ERROR => {
            let object = (header.object != 0).then_some(object);
            DenialMessage::Error {
                object,
                code: decode_error_code(payload.u16()?)?,
                offending_opcode: payload.u16()?,
                message: payload.string(MAX_ERROR_BYTES, "error")?,
            }
        }
        OP_PING => {
            require_connection(header.object)?;
            DenialMessage::Ping {
                cookie: payload.u64()?,
            }
        }
        OP_FRAME_GRANT => {
            require_connection(header.object)?;
            let latch_nanos = payload.u64()?;
            let target_nanos = payload.u64()?;
            let period_nanos = payload.u64()?;
            frame_grant(latch_nanos, target_nanos, period_nanos)?;
            DenialMessage::FrameGrant {
                latch_nanos,
                target_nanos,
                period_nanos,
            }
        }
        opcode => return Err(WireError::UnknownOpcode(opcode)),
    };
    payload.finish()?;
    let descriptors = match &message {
        DenialMessage::RegisterRenderTarget { buffer, .. } => {
            vec![DescriptorKind::DmabufPlane; buffer.planes.len()]
        }
        _ => Vec::new(),
    };
    validate_descriptor_layout(header.fd_count, received_descriptors, &descriptors)?;
    Ok(DecodedPacket {
        message,
        descriptors,
    })
}

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn i32(&mut self, value: i32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn string(&mut self, value: &str, limit: usize, field: &'static str) -> Result<(), WireError> {
        if value.len() > limit {
            return Err(WireError::InvalidString { field });
        }
        self.u16(u16::try_from(value.len()).map_err(|_| WireError::InvalidString { field })?);
        self.bytes.extend_from_slice(value.as_bytes());
        Ok(())
    }
}

struct Reader<'a> {
    remaining: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], WireError> {
        if self.remaining.len() < count {
            return Err(WireError::Truncated);
        }
        let (value, remaining) = self.remaining.split_at(count);
        self.remaining = remaining;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32, WireError> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn string(&mut self, limit: usize, field: &'static str) -> Result<String, WireError> {
        let length = usize::from(self.u16()?);
        if length > limit {
            return Err(WireError::InvalidString { field });
        }
        let bytes = self.take(length)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| WireError::InvalidString { field })
    }

    fn finish(self) -> Result<(), WireError> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(WireError::TrailingBytes)
        }
    }
}

struct Header<'a> {
    opcode: u16,
    fd_count: usize,
    object: u64,
    payload: &'a [u8],
}

fn finish_packet(
    opcode: u16,
    object: u64,
    payload: &[u8],
    descriptors: Vec<DescriptorKind>,
) -> Result<EncodedPacket, WireError> {
    let complete_length = HEADER_BYTES
        .checked_add(payload.len())
        .ok_or(WireError::PacketTooLarge)?;
    if complete_length > MAX_PACKET_BYTES {
        return Err(WireError::PacketTooLarge);
    }
    let payload_length = u32::try_from(payload.len()).map_err(|_| WireError::PacketTooLarge)?;
    let fd_count = u16::try_from(descriptors.len()).map_err(|_| WireError::Limit {
        field: "descriptors",
    })?;
    let mut bytes = Vec::with_capacity(complete_length);
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
    // Existing peers reject a header newer than their implementation before
    // reading capabilities. Only each negotiated new opcode raises the header.
    let minor = match opcode {
        OP_REQUEST_ACTIVATION => ACTIVATION_PROTOCOL_MINOR,
        OP_DISPLAY_POWER => 5,
        OP_BIND_INPUT_CHANNEL | OP_BIND_INPUT_ROUTE | OP_REQUEST_INPUT_CHANNEL => 6,
        OP_REQUEST_RENDER_TARGETS => 7,
        OP_SCENE_PRESENTED => 8,
        OP_FRAME_GRANT => 9,
        OP_SET_KEEP_SCREEN_ON => 10,
        OP_DENSITY => 11,
        OP_SET_PARENT => 12,
        _ => BASE_PROTOCOL_MINOR,
    };
    bytes.extend_from_slice(&minor.to_le_bytes());
    bytes.extend_from_slice(&opcode.to_le_bytes());
    bytes.extend_from_slice(&fd_count.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&object.to_le_bytes());
    bytes.extend_from_slice(&payload_length.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(payload);
    Ok(EncodedPacket { bytes, descriptors })
}

fn validate_input_route(display: u32, incarnation: u64) -> Result<(), WireError> {
    if display > i32::MAX as u32 || incarnation == 0 || incarnation > i64::MAX as u64 {
        return Err(WireError::InvalidEnum {
            field: "native input route",
            value: incarnation,
        });
    }
    Ok(())
}

fn parse_header(packet: &[u8], received_descriptors: usize) -> Result<Header<'_>, WireError> {
    if packet.len() < HEADER_BYTES {
        return Err(WireError::ShortHeader);
    }
    if packet.len() > MAX_PACKET_BYTES {
        return Err(WireError::PacketTooLarge);
    }
    if packet[0..4] != MAGIC {
        return Err(WireError::InvalidMagic);
    }
    let mut reader = Reader::new(&packet[4..HEADER_BYTES]);
    let major = reader.u16()?;
    let minor = reader.u16()?;
    if major != PROTOCOL_MAJOR || minor > PROTOCOL_MINOR {
        return Err(WireError::UnsupportedVersion { major, minor });
    }
    let opcode = reader.u16()?;
    let fd_count = usize::from(reader.u16()?);
    let flags = reader.u32()?;
    let object = reader.u64()?;
    let payload_length = usize::try_from(reader.u32()?).map_err(|_| WireError::LengthMismatch)?;
    let reserved = reader.u32()?;
    reader.finish()?;
    if flags != 0 || reserved != 0 {
        return Err(WireError::UnsupportedHeaderBits);
    }
    if packet.len() != HEADER_BYTES.saturating_add(payload_length) {
        return Err(WireError::LengthMismatch);
    }
    if fd_count != received_descriptors {
        return Err(WireError::DescriptorCount {
            expected: fd_count,
            actual: received_descriptors,
        });
    }
    Ok(Header {
        opcode,
        fd_count,
        object,
        payload: &packet[HEADER_BYTES..],
    })
}

fn encode_buffer(payload: &mut Writer, buffer: &BufferMetadata) -> Result<(), WireError> {
    payload.u64(buffer.id.0);
    payload.u32(buffer.width);
    payload.u32(buffer.height);
    payload.u32(buffer.format.fourcc);
    payload.u64(buffer.format.modifier);
    payload.u8(
        u8::try_from(buffer.planes.len()).map_err(|_| WireError::Limit {
            field: "DMA-BUF planes",
        })?,
    );
    for plane in &buffer.planes {
        payload.u32(plane.index);
        payload.u32(plane.offset);
        payload.u32(plane.stride);
    }
    Ok(())
}

fn decode_buffer(payload: &mut Reader<'_>) -> Result<BufferMetadata, WireError> {
    let id = BufferId(payload.u64()?);
    let width = payload.u32()?;
    let height = payload.u32()?;
    let format = FormatModifier {
        fourcc: payload.u32()?,
        modifier: payload.u64()?,
    };
    let count = usize::from(payload.u8()?);
    if count == 0 || count > 4 {
        return Err(WireError::Limit {
            field: "DMA-BUF planes",
        });
    }
    let mut planes = Vec::with_capacity(count);
    for _ in 0..count {
        planes.push(PlaneMetadata {
            index: payload.u32()?,
            offset: payload.u32()?,
            stride: payload.u32()?,
        });
    }
    let buffer = BufferMetadata {
        id,
        width,
        height,
        format,
        planes,
    };
    buffer.validate()?;
    if buffer.id.0 == 0 || buffer.format.modifier == u64::MAX {
        return Err(WireError::InvalidEnum {
            field: "buffer identity or modifier",
            value: buffer.id.0,
        });
    }
    Ok(buffer)
}

fn encode_input(payload: &mut Writer, event: InputEvent) {
    match event {
        InputEvent::Touch {
            action,
            pointer_id,
            x_fixed,
            y_fixed,
            pressure,
        } => {
            payload.u8(0);
            payload.u8(action as u8);
            payload.u32(pointer_id);
            payload.i32(x_fixed);
            payload.i32(y_fixed);
            payload.u16(pressure);
        }
        InputEvent::Key {
            action,
            keycode,
            repeat,
        } => {
            payload.u8(1);
            payload.u8(action as u8);
            payload.u32(keycode);
            payload.u16(repeat);
        }
        InputEvent::Navigation { action } => {
            payload.u8(2);
            payload.u8(action as u8);
        }
    }
}

fn decode_input(payload: &mut Reader<'_>) -> Result<InputEvent, WireError> {
    match payload.u8()? {
        0 => Ok(InputEvent::Touch {
            action: decode_touch_action(payload.u8()?)?,
            pointer_id: payload.u32()?,
            x_fixed: payload.i32()?,
            y_fixed: payload.i32()?,
            pressure: payload.u16()?,
        }),
        1 => Ok(InputEvent::Key {
            action: decode_key_action(payload.u8()?)?,
            keycode: payload.u32()?,
            repeat: payload.u16()?,
        }),
        2 => Ok(InputEvent::Navigation {
            action: decode_navigation_action(payload.u8()?)?,
        }),
        value => Err(WireError::InvalidEnum {
            field: "input event",
            value: u64::from(value),
        }),
    }
}

fn valid_object(object: TaskObjectId) -> Result<u64, WireError> {
    if object.0 == 0 {
        Err(WireError::InvalidObject)
    } else {
        Ok(object.0)
    }
}

fn require_connection(object: u64) -> Result<(), WireError> {
    if object == 0 {
        Ok(())
    } else {
        Err(WireError::InvalidObject)
    }
}

fn frame_grant(latch: u64, target: u64, period: u64) -> Result<(), WireError> {
    nonzero("grant latch", latch)?;
    nonzero("grant period", period)?;
    if target <= latch {
        return Err(WireError::InvalidEnum {
            field: "grant target",
            value: target,
        });
    }
    Ok(())
}

fn nonzero(field: &'static str, value: u64) -> Result<(), WireError> {
    if value == 0 {
        Err(WireError::InvalidEnum { field, value })
    } else {
        Ok(())
    }
}

fn known_bits(field: &'static str, value: u64, known: u64) -> Result<(), WireError> {
    if value & !known == 0 {
        Ok(())
    } else {
        Err(WireError::InvalidEnum { field, value })
    }
}

fn valid_package(package: &str) -> bool {
    !package.is_empty()
        && package.len() <= MAX_PACKAGE_BYTES
        && package.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
}

fn validate_damage(damage: Damage) -> Result<(), WireError> {
    if damage.width == 0
        || damage.height == 0
        || damage.x.checked_add(damage.width).is_none()
        || damage.y.checked_add(damage.height).is_none()
    {
        Err(WireError::Limit {
            field: "damage rectangle",
        })
    } else {
        Ok(())
    }
}

fn validate_server_hello(
    major: u16,
    minor: u16,
    capabilities: u64,
    max_task_objects: u32,
    max_buffers_per_task: u32,
    max_damage_rects: u32,
) -> Result<(), WireError> {
    if major != PROTOCOL_MAJOR || minor > PROTOCOL_MINOR {
        return Err(WireError::UnsupportedVersion { major, minor });
    }
    if capabilities & capability::REQUIRED_V1 != capability::REQUIRED_V1 {
        return Err(WireError::InvalidEnum {
            field: "required capabilities",
            value: capabilities,
        });
    }
    if max_task_objects == 0 || max_task_objects > MAX_TASK_OBJECTS {
        return Err(WireError::Limit {
            field: "task objects",
        });
    }
    if max_buffers_per_task == 0 || max_buffers_per_task > MAX_BUFFERS_PER_TASK {
        return Err(WireError::Limit {
            field: "buffers per task",
        });
    }
    if max_damage_rects == 0
        || usize::try_from(max_damage_rects).unwrap_or(usize::MAX) > MAX_DAMAGE_RECTS
    {
        return Err(WireError::Limit {
            field: "damage rectangles",
        });
    }
    Ok(())
}

fn validate_configure(
    serial: u32,
    width: u32,
    height: u32,
    scale_numerator: u32,
    scale_denominator: u32,
    refresh_millihz: u32,
) -> Result<(), WireError> {
    if serial == 0
        || width == 0
        || height == 0
        || width > 16_384
        || height > 16_384
        || scale_numerator == 0
        || scale_denominator == 0
        || refresh_millihz == 0
        || refresh_millihz > 1_000_000
    {
        Err(WireError::Limit { field: "configure" })
    } else {
        Ok(())
    }
}

fn validate_descriptor_layout(
    header_count: usize,
    received_count: usize,
    expected: &[DescriptorKind],
) -> Result<(), WireError> {
    if header_count != expected.len() || received_count != expected.len() {
        Err(WireError::DescriptorCount {
            expected: expected.len(),
            actual: received_count,
        })
    } else {
        Ok(())
    }
}

fn decode_bool(value: u8, field: &'static str) -> Result<bool, WireError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(WireError::InvalidEnum {
            field,
            value: u64::from(value),
        }),
    }
}

macro_rules! decode_enum {
    ($name:ident, $type:ty => $output:ty, $field:literal, {$($value:literal => $variant:path),+ $(,)?}) => {
        fn $name(value: $type) -> Result<$output, WireError> {
            match value {
                $($value => Ok($variant),)+
                value => Err(WireError::InvalidEnum {
                    field: $field,
                    value: u64::from(value),
                }),
            }
        }
    };
}

decode_enum!(decode_transform, u8 => Transform, "transform", {
    0 => Transform::Normal,
    1 => Transform::Rotate90,
    2 => Transform::Rotate180,
    3 => Transform::Rotate270,
});
decode_enum!(decode_visibility, u8 => Visibility, "visibility", {
    0 => Visibility::Hidden,
    1 => Visibility::Visible,
    2 => Visibility::Obscured,
});
decode_enum!(decode_touch_action, u8 => TouchAction, "touch action", {
    0 => TouchAction::Down,
    1 => TouchAction::Motion,
    2 => TouchAction::Up,
    3 => TouchAction::Cancel,
});
decode_enum!(decode_key_action, u8 => KeyAction, "key action", {
    0 => KeyAction::Down,
    1 => KeyAction::Up,
});
decode_enum!(decode_navigation_action, u8 => NavigationAction, "navigation action", {
    0 => NavigationAction::Back,
    1 => NavigationAction::Home,
    2 => NavigationAction::Overview,
});
decode_enum!(decode_error_code, u16 => ProtocolErrorCode, "protocol error", {
    1 => ProtocolErrorCode::Incompatible,
    2 => ProtocolErrorCode::InvalidMessage,
    3 => ProtocolErrorCode::DuplicateObject,
    4 => ProtocolErrorCode::UnknownObject,
    5 => ProtocolErrorCode::ResourceLimit,
    6 => ProtocolErrorCode::InvalidState,
    7 => ProtocolErrorCode::ProtectedContent,
    8 => ProtocolErrorCode::UnsupportedFormat,
});

#[cfg(test)]
mod tests {
    use super::*;

    const OBJECT: TaskObjectId = TaskObjectId(7);

    #[test]
    fn demand_targets_are_descriptor_free_and_require_a_real_generation() {
        let request = AndroidMessage::RequestRenderTargets {
            object: OBJECT,
            configure_serial: 9,
        };
        let encoded = encode_android(&request).unwrap();
        assert_eq!(
            u16::from_le_bytes(encoded.bytes[6..8].try_into().unwrap()),
            7
        );
        assert!(encoded.descriptors.is_empty());
        assert_eq!(decode_android(&encoded.bytes, 0).unwrap().message, request);
        assert!(decode_android(&encoded.bytes, 1).is_err());
        for (object, configure_serial) in [(OBJECT, 0), (TaskObjectId(0), 9)] {
            assert!(
                encode_android(&AndroidMessage::RequestRenderTargets {
                    object,
                    configure_serial
                })
                .is_err()
            );
        }
        let legacy = encode_android(&AndroidMessage::AckConfigure {
            object: OBJECT,
            serial: 9,
        })
        .unwrap();
        assert_eq!(
            u16::from_le_bytes(legacy.bytes[6..8].try_into().unwrap()),
            BASE_PROTOCOL_MINOR
        );
    }

    fn buffer() -> BufferMetadata {
        BufferMetadata {
            id: BufferId(11),
            width: 1080,
            height: 2400,
            format: FormatModifier {
                fourcc: u32::from_le_bytes(*b"XR24"),
                modifier: 0,
            },
            planes: vec![PlaneMetadata {
                index: 0,
                offset: 0,
                stride: 4_320,
            }],
        }
    }

    fn round_trip_android(message: &AndroidMessage) {
        let encoded = encode_android(message).unwrap();
        let decoded = decode_android(&encoded.bytes, encoded.descriptors.len()).unwrap();
        assert_eq!(&decoded.message, message);
        assert_eq!(decoded.descriptors, encoded.descriptors);
    }

    fn round_trip_denial(message: &DenialMessage) {
        let encoded = encode_denial(message).unwrap();
        let decoded = decode_denial(&encoded.bytes, encoded.descriptors.len()).unwrap();
        assert_eq!(&decoded.message, message);
        assert_eq!(decoded.descriptors, encoded.descriptors);
    }

    #[test]
    fn direct_input_records_require_correct_descriptors_routes_and_wire_version() {
        let channel = AndroidMessage::BindInputChannel {
            generation: 1,
            wire: 7,
        };
        let encoded = encode_android(&channel).unwrap();
        assert_eq!(&encoded.bytes[6..8], &6_u16.to_le_bytes());
        assert!(decode_android(&encoded.bytes, 0).is_err());
        round_trip_android(&channel);
        round_trip_android(&AndroidMessage::BindInputRoute {
            object: OBJECT,
            display: 0,
            incarnation: 7,
        });
        round_trip_denial(&DenialMessage::RequestInputChannel { generation: 2 });
        assert!(
            encode_android(&AndroidMessage::BindInputRoute {
                object: OBJECT,
                display: 0,
                incarnation: 0
            })
            .is_err()
        );
        assert!(
            encode_android(&AndroidMessage::BindInputRoute {
                object: OBJECT,
                display: u32::MAX,
                incarnation: 7
            })
            .is_err()
        );
        assert!(encode_denial(&DenialMessage::RequestInputChannel { generation: 0 }).is_err());
    }

    #[test]
    fn activation_does_not_raise_the_wire_version_of_legacy_handshakes() {
        let hello = encode_android(&AndroidMessage::ClientHello {
            min_major: 1,
            max_major: 1,
            capabilities: capability::REQUIRED_V1 | capability::TASK_ACTIVATION,
        })
        .unwrap();
        let create = encode_android(&AndroidMessage::CreateTask {
            object: OBJECT,
            display: AndroidDisplayId(9),
            package: "org.example.app".into(),
        })
        .unwrap();
        for legacy in [hello, create] {
            assert_eq!(&legacy.bytes[6..8], &3_u16.to_le_bytes());
        }
        let activation =
            encode_android(&AndroidMessage::RequestActivation { object: OBJECT }).unwrap();
        assert_eq!(&activation.bytes[6..8], &4_u16.to_le_bytes());
        assert!(activation.descriptors.is_empty());
        assert_eq!(activation.bytes.len(), HEADER_BYTES);
        let display_power = encode_denial(&DenialMessage::DisplayPower { powered: true }).unwrap();
        assert_eq!(&display_power.bytes[6..8], &5_u16.to_le_bytes());
    }

    #[test]
    fn android_control_messages_round_trip() {
        for message in [
            AndroidMessage::ClientHello {
                min_major: 1,
                max_major: 1,
                capabilities: capability::REQUIRED_V1,
            },
            AndroidMessage::CreateTask {
                object: OBJECT,
                display: AndroidDisplayId(9),
                package: "org.example.app".to_owned(),
            },
            AndroidMessage::BindTask {
                object: OBJECT,
                task: AndroidTaskId(42),
            },
            AndroidMessage::AckConfigure {
                object: OBJECT,
                serial: 5,
            },
            AndroidMessage::SetContentState {
                object: OBJECT,
                flags: content_state::OPAQUE,
            },
            AndroidMessage::SetFrameRate {
                object: OBJECT,
                millihz: 120_000,
            },
            AndroidMessage::UnregisterBuffer {
                object: OBJECT,
                buffer: BufferId(11),
            },
            AndroidMessage::DestroyTask { object: OBJECT },
            AndroidMessage::RequestActivation { object: OBJECT },
            AndroidMessage::SetKeepScreenOn {
                object: OBJECT,
                keep: true,
            },
            AndroidMessage::SetParent {
                object: OBJECT,
                parent: Some(TaskObjectId(OBJECT.0 + 1)),
            },
            AndroidMessage::SetParent {
                object: OBJECT,
                parent: None,
            },
            AndroidMessage::Pong { cookie: 99 },
        ] {
            round_trip_android(&message);
        }
    }

    #[test]
    fn buffer_registration_names_exact_plane_descriptor_roles() {
        let message = AndroidMessage::RegisterBuffer {
            object: OBJECT,
            buffer: buffer(),
        };
        let encoded = encode_android(&message).unwrap();
        assert_eq!(encoded.descriptors, vec![DescriptorKind::DmabufPlane]);
        assert_eq!(decode_android(&encoded.bytes, 1).unwrap().message, message);
        assert_eq!(
            decode_android(&encoded.bytes, 0),
            Err(WireError::DescriptorCount {
                expected: 1,
                actual: 0,
            })
        );
    }

    #[test]
    fn host_render_target_registration_names_exact_plane_descriptor_roles() {
        let message = DenialMessage::RegisterRenderTarget {
            object: OBJECT,
            configure_serial: 5,
            buffer: buffer(),
        };
        let encoded = encode_denial(&message).unwrap();
        assert_eq!(encoded.descriptors, vec![DescriptorKind::DmabufPlane]);
        assert_eq!(decode_denial(&encoded.bytes, 1).unwrap().message, message);
        assert_eq!(
            decode_denial(&encoded.bytes, 0),
            Err(WireError::DescriptorCount {
                expected: 1,
                actual: 0,
            })
        );
        round_trip_denial(&DenialMessage::UnregisterRenderTarget {
            object: OBJECT,
            buffer: BufferId(11),
        });
    }

    #[test]
    fn timeline_descriptors_are_shared_once_in_fixed_order() {
        let message = AndroidMessage::BindTimelines { object: OBJECT };
        let encoded = encode_android(&message).unwrap();
        assert_eq!(
            encoded.descriptors,
            vec![
                DescriptorKind::AcquireTimeline,
                DescriptorKind::ReleaseTimeline
            ]
        );
        assert_eq!(decode_android(&encoded.bytes, 2).unwrap().message, message);
    }

    #[test]
    fn steady_state_present_is_descriptor_free() {
        let message = AndroidMessage::Present {
            object: OBJECT,
            frame: FrameId(21),
            buffer: BufferId(11),
            configure_serial: 5,
            acquire_point: 17,
            release_point: 18,
            damage: vec![Damage {
                x: 0,
                y: 0,
                width: 1080,
                height: 2400,
            }],
        };
        let encoded = encode_android(&message).unwrap();
        assert!(encoded.descriptors.is_empty());
        round_trip_android(&message);
    }

    #[test]
    fn denial_task_state_round_trips() {
        for message in [
            DenialMessage::ServerHello {
                major: 1,
                minor: 0,
                capabilities: capability::REQUIRED_V1,
                max_task_objects: MAX_TASK_OBJECTS,
                max_buffers_per_task: MAX_BUFFERS_PER_TASK,
                max_damage_rects: u32::try_from(MAX_DAMAGE_RECTS).unwrap(),
            },
            DenialMessage::DisplayPower { powered: false },
            DenialMessage::Configure {
                object: OBJECT,
                serial: 5,
                width: 1080,
                height: 2400,
                scale_numerator: 3,
                scale_denominator: 2,
                transform: Transform::Rotate90,
                refresh_millihz: 120_000,
            },
            DenialMessage::Density {
                object: OBJECT,
                dpi: 230,
            },
            DenialMessage::Insets {
                object: OBJECT,
                left: 0,
                top: 96,
                right: 0,
                bottom: 120,
            },
            DenialMessage::Visibility {
                object: OBJECT,
                visibility: Visibility::Visible,
                focused: true,
            },
            DenialMessage::Close { object: OBJECT },
            DenialMessage::BufferReleased {
                object: OBJECT,
                frame: FrameId(21),
                buffer: BufferId(11),
                release_point: 18,
            },
            DenialMessage::Presented {
                object: OBJECT,
                frame: FrameId(21),
                timestamp_nanos: 999,
                refresh_period_nanos: 8_333_333,
                sequence: 300,
                flags: presentation_flag::DISPLAYED | presentation_flag::DIRECT_SCANOUT,
            },
            DenialMessage::Ping { cookie: 99 },
        ] {
            round_trip_denial(&message);
        }
    }

    #[test]
    fn format_feedback_and_all_input_variants_round_trip() {
        round_trip_denial(&DenialMessage::FormatFeedback {
            object: OBJECT,
            generation: 1,
            formats: vec![buffer().format],
        });
        for event in [
            InputEvent::Touch {
                action: TouchAction::Motion,
                pointer_id: 3,
                x_fixed: 10 << 16,
                y_fixed: 20 << 16,
                pressure: 32_768,
            },
            InputEvent::Key {
                action: KeyAction::Down,
                keycode: 30,
                repeat: 0,
            },
            InputEvent::Navigation {
                action: NavigationAction::Back,
            },
        ] {
            round_trip_denial(&DenialMessage::Input {
                object: OBJECT,
                serial: 8,
                timestamp_nanos: 999,
                event,
            });
        }
    }

    #[test]
    fn error_message_may_be_connection_or_task_scoped() {
        for object in [None, Some(OBJECT)] {
            round_trip_denial(&DenialMessage::Error {
                object,
                code: ProtocolErrorCode::InvalidState,
                offending_opcode: OP_PRESENT,
                message: "stale configure".to_owned(),
            });
        }
    }

    #[test]
    fn framing_rejects_mutation_and_direction_confusion() {
        let mut encoded = encode_android(&AndroidMessage::Pong { cookie: 7 }).unwrap();
        encoded.bytes[0] = b'X';
        assert_eq!(
            decode_android(&encoded.bytes, 0),
            Err(WireError::InvalidMagic)
        );

        let denial = encode_denial(&DenialMessage::Ping { cookie: 7 }).unwrap();
        assert_eq!(
            decode_android(&denial.bytes, 0),
            Err(WireError::UnknownOpcode(OP_PING))
        );
    }

    #[test]
    fn malformed_identity_and_unbounded_damage_fail_closed() {
        assert_eq!(
            encode_android(&AndroidMessage::CreateTask {
                object: OBJECT,
                display: AndroidDisplayId(2),
                package: "not/a/package".to_owned(),
            }),
            Err(WireError::InvalidString {
                field: "task identity"
            })
        );
        assert!(matches!(
            encode_android(&AndroidMessage::Present {
                object: OBJECT,
                frame: FrameId(1),
                buffer: BufferId(1),
                configure_serial: 1,
                acquire_point: 1,
                release_point: 1,
                damage: vec![Damage {
                    x: u32::MAX,
                    y: 0,
                    width: 2,
                    height: 1,
                }],
            }),
            Err(WireError::Limit {
                field: "damage rectangle"
            })
        ));
    }

    #[test]
    fn trailing_payload_and_unknown_header_bits_are_fatal() {
        let encoded = encode_android(&AndroidMessage::Pong { cookie: 7 }).unwrap();
        let mut trailing = encoded.bytes.clone();
        trailing.push(0);
        assert_eq!(decode_android(&trailing, 0), Err(WireError::LengthMismatch));

        let mut flags = encoded.bytes;
        flags[12] = 1;
        assert_eq!(
            decode_android(&flags, 0),
            Err(WireError::UnsupportedHeaderBits)
        );
    }

    fn receiver() -> TaskPresentationState {
        let mut state = TaskPresentationState::new(OBJECT).unwrap();
        state.bind_timelines().unwrap();
        state.replace_feedback(1, [buffer().format]).unwrap();
        state.configure(5, 1080, 2400, 120_000).unwrap();
        state.acknowledge_configure(5).unwrap();
        state.register_buffer(buffer()).unwrap();
        state
    }

    #[test]
    fn task_receiver_caches_import_and_releases_only_after_signal() {
        let mut state = receiver();
        let damage = vec![Damage {
            x: 0,
            y: 0,
            width: 1080,
            height: 2400,
        }];
        let accepted = state
            .present(OBJECT, FrameId(1), BufferId(11), 5, 7, 8, damage)
            .unwrap();
        assert_eq!(accepted.release_point, 8);
        assert_eq!(state.registered_buffer_count(), 1);
        assert_eq!(state.in_flight_count(), 1);
        assert_eq!(
            state.unregister_buffer(BufferId(11)),
            Err(TaskStateError::BufferInFlight(BufferId(11)))
        );
        assert_eq!(
            state.release(FrameId(1), BufferId(11), 8).unwrap(),
            DenialMessage::BufferReleased {
                object: OBJECT,
                frame: FrameId(1),
                buffer: BufferId(11),
                release_point: 8,
            }
        );
        assert_eq!(state.in_flight_count(), 0);
        assert_eq!(state.unregister_buffer(BufferId(11)).unwrap(), buffer());
    }

    #[test]
    fn task_receiver_rejects_unbound_and_stale_configure_presents() {
        let mut state = TaskPresentationState::new(OBJECT).unwrap();
        state.replace_feedback(1, [buffer().format]).unwrap();
        state.configure(5, 1080, 2400, 120_000).unwrap();
        state.acknowledge_configure(5).unwrap();
        state.register_buffer(buffer()).unwrap();
        assert_eq!(
            state.present(OBJECT, FrameId(1), BufferId(11), 5, 1, 1, Vec::new()),
            Err(TaskStateError::TimelinesUnbound)
        );
        state.bind_timelines().unwrap();
        state.configure(6, 1080, 2400, 120_000).unwrap();
        assert_eq!(
            state.present(OBJECT, FrameId(1), BufferId(11), 5, 1, 1, Vec::new()),
            Err(TaskStateError::StaleConfigure {
                expected: 6,
                actual: 5,
            })
        );
        assert_eq!(
            state.present(OBJECT, FrameId(1), BufferId(11), 6, 1, 1, Vec::new()),
            Err(TaskStateError::UnacknowledgedConfigure)
        );
    }

    #[test]
    fn superseded_configure_ack_is_coalesced_without_acknowledging_latest() {
        let mut state = receiver();
        state.configure(6, 1080, 2400, 120_000).unwrap();

        state.acknowledge_configure(5).unwrap();
        assert_eq!(
            state.present(OBJECT, FrameId(1), BufferId(11), 6, 1, 1, Vec::new()),
            Err(TaskStateError::UnacknowledgedConfigure)
        );
        assert_eq!(
            state.acknowledge_configure(7),
            Err(TaskStateError::StaleConfigure {
                expected: 6,
                actual: 7,
            })
        );

        state.acknowledge_configure(6).unwrap();
        state
            .present(OBJECT, FrameId(1), BufferId(11), 6, 1, 1, Vec::new())
            .unwrap();
    }

    #[test]
    fn superseded_present_is_released_without_reviving_a_retired_buffer() {
        let mut state = receiver();
        state.unregister_buffer(BufferId(11)).unwrap();
        state.configure(6, 720, 1280, 120_000).unwrap();

        assert_eq!(
            state
                .discard_superseded_present(OBJECT, FrameId(41), BufferId(11), 5, 7, 8,)
                .unwrap(),
            DenialMessage::BufferReleased {
                object: OBJECT,
                frame: FrameId(41),
                buffer: BufferId(11),
                release_point: 8,
            }
        );
        assert_eq!(state.in_flight_count(), 0);
        assert_eq!(state.registered_buffer_count(), 0);

        assert_eq!(
            state.discard_superseded_present(OBJECT, FrameId(42), BufferId(12), 7, 9, 10,),
            Err(TaskStateError::StaleConfigure {
                expected: 6,
                actual: 7,
            })
        );
    }

    #[test]
    fn task_receiver_rejects_point_regression_and_release_mismatch() {
        let mut state = receiver();
        state
            .present(OBJECT, FrameId(1), BufferId(11), 5, 7, 8, Vec::new())
            .unwrap();
        assert_eq!(
            state.release(FrameId(1), BufferId(99), 8),
            Err(TaskStateError::ReleaseMismatch)
        );
        state.release(FrameId(1), BufferId(11), 8).unwrap();
        assert_eq!(
            state.present(OBJECT, FrameId(2), BufferId(11), 5, 7, 9, Vec::new()),
            Err(TaskStateError::TimelineRegression {
                timeline: "acquire",
                previous: 7,
                actual: 7,
            })
        );
    }

    #[test]
    fn direct_scene_timing_has_its_own_identity_and_no_descriptors() {
        let message = DenialMessage::ScenePresented {
            object: OBJECT,
            scene: 17,
            timestamp_nanos: 123_456_789,
            refresh_period_nanos: 8_333_333,
            sequence: 42,
        };
        let packet = encode_denial(&message).unwrap();
        assert!(packet.descriptors.is_empty());
        assert_eq!(
            u16::from_le_bytes(packet.bytes[6..8].try_into().unwrap()),
            8
        );
        assert_eq!(decode_denial(&packet.bytes, 0).unwrap().message, message);
        assert!(decode_denial(&packet.bytes, 1).is_err());
        for (scene, timestamp, period) in [(0, 1, 1), (1, 0, 1), (1, 1, 0)] {
            assert!(
                encode_denial(&DenialMessage::ScenePresented {
                    object: OBJECT,
                    scene,
                    timestamp_nanos: timestamp,
                    refresh_period_nanos: period,
                    sequence: 0
                })
                .is_err()
            );
        }
    }

    #[test]
    fn frame_grant_is_connection_scoped_and_ordered() {
        let message = DenialMessage::FrameGrant {
            latch_nanos: 1_000_000,
            target_nanos: 9_333_333,
            period_nanos: 8_333_333,
        };
        let packet = encode_denial(&message).unwrap();
        assert!(packet.descriptors.is_empty());
        assert_eq!(
            u16::from_le_bytes(packet.bytes[6..8].try_into().unwrap()),
            9
        );
        assert_eq!(decode_denial(&packet.bytes, 0).unwrap().message, message);
        assert!(decode_denial(&packet.bytes, 1).is_err());
        for (latch, target, period) in [(0, 1, 1), (2, 2, 1), (1, 2, 0)] {
            assert!(
                encode_denial(&DenialMessage::FrameGrant {
                    latch_nanos: latch,
                    target_nanos: target,
                    period_nanos: period,
                })
                .is_err()
            );
        }
    }
}
