//! Denial-side endpoint for one authenticated Droidloom connection.
//!
//! This crate stops at explicit compositor actions. It does not create a
//! second window system: Denial maps each [`EndpointAction::CreateTask`] to one
//! native task/window, imports each buffer once, and feeds presentation
//! completion back through the methods on [`DenialEndpoint`].

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use droidloom_denial_ipc::{
    AttachedDescriptor, IpcError, PeerCredentials, ProtocolSocket, SeqPacketListener,
};
use droidloom_denial_protocol::{
    AcceptedWireFrame, AndroidDisplayId, AndroidMessage, AndroidTaskId, BufferId, BufferMetadata,
    DenialMessage, DescriptorKind, FormatModifier, FrameId, InputEvent, MAX_BUFFERS_PER_TASK,
    MAX_DAMAGE_RECTS, MAX_TASK_OBJECTS, PROTOCOL_MAJOR, PROTOCOL_MINOR, ProtocolErrorCode,
    TaskObjectId, TaskPresentationState, TaskStateError, Visibility, capability,
};
use droidloom_syncobj::{DenialTaskTimelines, SyncobjDevice, SyncobjError};
use thiserror::Error;

/// One fully validated operation for Denial's native task/window machinery.
#[derive(Debug)]
pub enum EndpointAction {
    /// Allocate fallback composition targets for the current task generation.
    RequestRenderTargets {
        /// Existing task object.
        object: TaskObjectId,
        /// Requested configure generation; stale requests can be discarded.
        configure_serial: u32,
    },
    /// Adopt one explicitly requested connected native input writer.
    BindInputChannel {
        /// Host request generation.
        generation: u64,
        /// Native input wire ABI.
        wire: u16,
        /// Owned connected sequenced-packet socket.
        socket: OwnedFd,
    },
    /// Cache the native route for an authenticated, bound task.
    BindInputRoute {
        /// Host task object.
        object: TaskObjectId,
        /// Bound Android task identity.
        task: AndroidTaskId,
        /// Actual Android logical display.
        display: u32,
        /// Framework task incarnation.
        incarnation: u64,
    },
    /// Connection-level version and capability negotiation completed.
    ClientReady,
    /// Create exactly one native Denial task/window.
    CreateTask {
        /// Per-connection protocol object.
        object: TaskObjectId,
        /// Dedicated Android logical display.
        display: AndroidDisplayId,
        /// Package identity for shell policy and diagnostics.
        package: String,
    },
    /// Bind the real `ActivityTaskManager` identity after Android launches onto
    /// the already-configured dedicated display.
    BindTask {
        /// Existing protocol object.
        object: TaskObjectId,
        /// Real nonzero Android task identity.
        task: AndroidTaskId,
    },
    /// Tear down exactly one native task/window and all cached imports.
    DestroyTask {
        /// Object whose task was removed.
        object: TaskObjectId,
    },
    /// The task's shared acquire and release timelines are ready.
    TimelinesBound {
        /// Bound task object.
        object: TaskObjectId,
    },
    /// Android acknowledged the latest native-window configure.
    ConfigureAcknowledged {
        /// Configured task object.
        object: TaskObjectId,
        /// Acknowledged configure serial.
        serial: u32,
    },
    /// Import one allocation into Denial's renderer and cache it by ID.
    RegisterBuffer {
        /// Owning task object.
        object: TaskObjectId,
        /// Validated immutable allocation metadata.
        buffer: BufferMetadata,
        /// Owned DMA-BUF plane descriptors in dense plane order.
        planes: Vec<OwnedFd>,
    },
    /// Retire one idle renderer import.
    UnregisterBuffer {
        /// Owning task object.
        object: TaskObjectId,
        /// Allocation to retire.
        buffer: BufferId,
    },
    /// Submit one frame to Denial's renderer/scheduler.
    Present {
        /// Fully validated cached-buffer and timeline transaction.
        frame: AcceptedWireFrame,
        /// Acquire `sync_file` for renderer/KMS explicit synchronization.
        acquire_fence: OwnedFd,
    },
    /// A render completed after Denial had already superseded its configure.
    /// The endpoint consumed its timeline points and released it without
    /// exposing stale content to the compositor.
    SupersededPresentDiscarded {
        /// Owning task object.
        object: TaskObjectId,
        /// Discarded frame identity.
        frame: FrameId,
    },
    /// Private layer stream scoped by the authenticated Composer task.
    BindLayerStream {
        /// Owning task.
        object: TaskObjectId,
        /// Requests followed by release events.
        sockets: Vec<OwnedFd>,
    },
    /// Update content-state policy for following frames.
    SetContentState {
        /// Owning task object.
        object: TaskObjectId,
        /// Validated protocol content-state bits.
        flags: u32,
    },
    /// Update one task's host frame-rate vote.
    SetFrameRate {
        /// Owning task object.
        object: TaskObjectId,
        /// Requested millihertz, or zero to withdraw the vote.
        millihz: u32,
    },
    /// Ask the shell to activate a task, subject to native focus policy.
    RequestActivation {
        /// Existing task object.
        object: TaskObjectId,
    },
    /// Whether the host keeps its displays on while the task is visible.
    SetKeepScreenOn {
        /// Existing task object.
        object: TaskObjectId,
        /// A shown Android window of the task has `FLAG_KEEP_SCREEN_ON`.
        keep: bool,
    },
    /// Android answered a host liveness probe.
    Pong {
        /// Cookie from the corresponding `Ping`.
        cookie: u64,
    },
}

/// Connection, object, descriptor, or explicit-sync failure.
#[derive(Debug, Error)]
pub enum EndpointError {
    /// A non-handshake message arrived before a valid client hello.
    #[error("Droidloom message arrived before ClientHello")]
    HandshakeRequired,
    /// Client hello was repeated on a live connection.
    #[error("Droidloom ClientHello was repeated")]
    RepeatedHandshake,
    /// Client and server version/capability requirements do not intersect.
    #[error("Droidloom client version or required capabilities are incompatible")]
    IncompatibleClient,
    /// Task-object capacity was reached.
    #[error("Droidloom task-object capacity reached")]
    TaskLimit,
    /// Object, task, or display identity conflicts with a live task.
    #[error("Droidloom task object, Android task, or display identity is already live")]
    DuplicateIdentity,
    /// A task-scoped message named no live object.
    #[error("unknown Droidloom task object {0:?}")]
    UnknownObject(TaskObjectId),
    /// Destroy was requested while Denial still owns frames.
    #[error("Droidloom task {0:?} still has frames in flight")]
    TaskBusy(TaskObjectId),
    /// Expected descriptor role was absent after typed packet decoding.
    #[error("Droidloom packet omitted descriptor role {0:?}")]
    MissingDescriptor(DescriptorKind),
    /// Release materialization or completion named a frame not pending locally.
    #[error("Droidloom frame {frame:?} is not pending for task {object:?}")]
    UnknownFrame {
        /// Owning task object.
        object: TaskObjectId,
        /// Frame identity.
        frame: FrameId,
    },
    /// Sequenced-packet transport failed.
    #[error(transparent)]
    Ipc(#[from] IpcError),
    /// Safe per-task state rejected message ordering or metadata.
    #[error(transparent)]
    State(#[from] TaskStateError),
    /// Linux DRM syncobj operation failed.
    #[error(transparent)]
    Syncobj(#[from] SyncobjError),
}

/// Exact kernel-authenticated identity allowed to own one endpoint connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpectedPeer {
    /// Expected process ID in Denial's PID namespace. Zero is an explicit
    /// development-only wildcard used before the supervisor registration
    /// channel can publish the cell's dynamically assigned host PID.
    pub pid: u32,
    /// Expected effective user ID.
    pub uid: u32,
    /// Expected effective group ID.
    pub gid: u32,
}

impl ExpectedPeer {
    fn accepts(self, actual: PeerCredentials) -> bool {
        (self.pid == 0 || self.pid == actual.pid)
            && self.uid == actual.uid
            && self.gid == actual.gid
    }
}

/// Listener bind, accept, or peer-authentication failure.
#[derive(Debug, Error)]
pub enum EndpointListenerError {
    /// Filesystem socket or peer-credential operation failed.
    #[error(transparent)]
    Ipc(#[from] IpcError),
    /// A local process other than the supervisor-authorized Android Composer
    /// attempted to claim the cell endpoint.
    #[error(
        "rejected Droidloom peer pid={actual_pid} uid={actual_uid} gid={actual_gid}; expected pid={expected_pid} (0 is the development wildcard) uid={expected_uid} gid={expected_gid}"
    )]
    UnexpectedPeer {
        /// Authorized PID.
        expected_pid: u32,
        /// Authorized UID.
        expected_uid: u32,
        /// Authorized GID.
        expected_gid: u32,
        /// Kernel-reported PID.
        actual_pid: u32,
        /// Kernel-reported UID.
        actual_uid: u32,
        /// Kernel-reported GID.
        actual_gid: u32,
    },
}

/// Private filesystem listener that authenticates a cell before constructing
/// its protocol endpoint.
#[derive(Debug)]
pub struct DenialEndpointListener {
    listener: SeqPacketListener,
    device: SyncobjDevice,
    expected_peer: ExpectedPeer,
}

impl DenialEndpointListener {
    /// Bind the Denial-owned socket inside a caller-created mode-0700 runtime
    /// directory.
    ///
    /// # Errors
    ///
    /// Propagates filesystem socket creation failures. Existing paths are not
    /// removed or replaced.
    pub fn bind(
        path: impl AsRef<Path>,
        device: SyncobjDevice,
        expected_peer: ExpectedPeer,
    ) -> Result<Self, EndpointListenerError> {
        Ok(Self {
            listener: SeqPacketListener::bind(path)?,
            device,
            expected_peer,
        })
    }

    /// Borrow the listener for event-loop FD registration.
    pub const fn listener(&self) -> &SeqPacketListener {
        &self.listener
    }

    /// Enable or disable non-blocking accepts.
    ///
    /// # Errors
    ///
    /// Propagates `fcntl(2)` failures.
    pub fn set_nonblocking(&self, enabled: bool) -> Result<(), EndpointListenerError> {
        self.listener.set_nonblocking(enabled)?;
        Ok(())
    }

    /// Accept exactly one supervisor-authorized Composer peer.
    ///
    /// # Errors
    ///
    /// Rejects every PID/UID/GID mismatch before parsing protocol bytes.
    pub fn accept(&self) -> Result<DenialEndpoint, EndpointListenerError> {
        let socket = self.listener.accept()?;
        let actual = socket.peer_credentials()?;
        if !self.expected_peer.accepts(actual) {
            return Err(EndpointListenerError::UnexpectedPeer {
                expected_pid: self.expected_peer.pid,
                expected_uid: self.expected_peer.uid,
                expected_gid: self.expected_peer.gid,
                actual_pid: actual.pid,
                actual_uid: actual.uid,
                actual_gid: actual.gid,
            });
        }
        Ok(DenialEndpoint::new(
            ProtocolSocket::new(socket),
            self.device.clone(),
        ))
    }
}

/// One Denial-owned protocol connection and all of its native task state.
#[derive(Debug)]
pub struct DenialEndpoint {
    socket: ProtocolSocket,
    receive_buffer: Box<[u8]>,
    device: SyncobjDevice,
    ready: bool,
    activation_requests: bool,
    direct_input: bool,
    lazy_render_targets: bool,
    scene_presentation: bool,
    host_clock: bool,
    keep_screen_on: bool,
    task_density: bool,
    tasks: BTreeMap<TaskObjectId, TaskEndpoint>,
}

impl DenialEndpoint {
    /// Whether version and capability negotiation has completed.
    #[must_use]
    pub const fn ready(&self) -> bool {
        self.ready
    }

    /// Construct an endpoint after the listener authenticates the peer with
    /// `SO_PEERCRED`. The first accepted record must still be `ClientHello`.
    pub fn new(socket: ProtocolSocket, device: SyncobjDevice) -> Self {
        Self {
            socket,
            receive_buffer: vec![0; droidloom_denial_protocol::MAX_PACKET_BYTES].into_boxed_slice(),
            device,
            ready: false,
            activation_requests: false,
            direct_input: false,
            lazy_render_targets: false,
            scene_presentation: false,
            host_clock: false,
            keep_screen_on: false,
            task_density: false,
            tasks: BTreeMap::new(),
        }
    }

    /// Opt in only when the host implements activation under its focus policy.
    ///
    /// # Panics
    /// Panics if capability negotiation has already completed.
    #[must_use]
    pub fn with_activation_requests(mut self) -> Self {
        assert!(
            !self.ready,
            "activation must be configured before handshake"
        );
        self.activation_requests = true;
        self
    }

    /// Enable direct native input handoff before capability negotiation.
    ///
    /// # Panics
    /// Panics if capability negotiation has already completed.
    #[must_use]
    pub fn with_direct_input(mut self) -> Self {
        assert!(
            !self.ready,
            "direct input must be configured before handshake"
        );
        self.direct_input = true;
        self
    }

    /// Enable demand allocation before capability negotiation.
    ///
    /// # Panics
    /// Panics if the handshake has completed.
    #[must_use]
    pub fn with_lazy_render_targets(mut self) -> Self {
        assert!(!self.ready, "target policy must precede handshake");
        self.lazy_render_targets = true;
        self
    }

    /// Whether both peers support demand allocation of composition buffers.
    #[must_use]
    pub fn lazy_render_targets(&self) -> bool {
        self.ready && self.lazy_render_targets
    }

    /// Whether direct native input was negotiated.
    #[must_use]
    pub fn direct_input(&self) -> bool {
        self.ready && self.direct_input
    }

    /// Request a connected native input socket without forwarding input records.
    ///
    /// # Errors
    /// Rejects unsupported peers and propagates send failures.
    pub fn request_input_channel(&self, generation: u64) -> Result<(), EndpointError> {
        if !self.direct_input() {
            return Err(EndpointError::IncompatibleClient);
        }
        self.socket
            .send_denial(&DenialMessage::RequestInputChannel { generation })?;
        Ok(())
    }

    /// Borrow the socket for calloop registration and peer diagnostics.
    pub fn socket(&self) -> &ProtocolSocket {
        &self.socket
    }

    /// Receive, validate, and reduce exactly one Android record to a native
    /// Denial action.
    ///
    /// # Errors
    ///
    /// Propagates transport, ordering, descriptor import, and object failures.
    /// The caller should send a bounded fatal error when possible and close the
    /// connection rather than attempting to recover from ambiguous state.
    pub fn receive_action(&mut self) -> Result<EndpointAction, EndpointError> {
        let received = self.socket.receive_android_into(&mut self.receive_buffer)?;
        if !self.ready && !matches!(received.message, AndroidMessage::ClientHello { .. }) {
            return Err(EndpointError::HandshakeRequired);
        }
        match received.message {
            AndroidMessage::RequestRenderTargets {
                object,
                configure_serial,
            } => {
                if !self.lazy_render_targets {
                    return Err(EndpointError::IncompatibleClient);
                }
                self.task(object)?;
                Ok(EndpointAction::RequestRenderTargets {
                    object,
                    configure_serial,
                })
            }
            AndroidMessage::BindInputChannel { generation, wire } => {
                self.bind_input_channel(generation, wire, received.descriptors)
            }
            AndroidMessage::BindInputRoute {
                object,
                display,
                incarnation,
            } => {
                if !self.direct_input {
                    return Err(EndpointError::IncompatibleClient);
                }
                let task = self
                    .task(object)?
                    .task
                    .ok_or(EndpointError::UnknownObject(object))?;
                Ok(EndpointAction::BindInputRoute {
                    object,
                    task,
                    display,
                    incarnation,
                })
            }
            AndroidMessage::ClientHello {
                min_major,
                max_major,
                capabilities,
            } => self.client_hello(min_major, max_major, capabilities),
            AndroidMessage::CreateTask {
                object,
                display,
                package,
            } => self.create_task(object, display, package),
            AndroidMessage::BindTask { object, task } => self.bind_task(object, task),
            AndroidMessage::DestroyTask { object } => self.destroy_task(object),
            AndroidMessage::BindTimelines { object } => {
                self.bind_timelines(object, received.descriptors)
            }
            AndroidMessage::AckConfigure { object, serial } => {
                self.task_mut(object)?.state.acknowledge_configure(serial)?;
                Ok(EndpointAction::ConfigureAcknowledged { object, serial })
            }
            AndroidMessage::RegisterBuffer { object, buffer } => {
                self.register_buffer(object, buffer, received.descriptors)
            }
            AndroidMessage::UnregisterBuffer { object, buffer } => {
                self.task_mut(object)?.state.unregister_buffer(buffer)?;
                Ok(EndpointAction::UnregisterBuffer { object, buffer })
            }
            AndroidMessage::Present {
                object,
                frame,
                buffer,
                configure_serial,
                acquire_point,
                release_point,
                damage,
            } => self.present(
                object,
                frame,
                buffer,
                configure_serial,
                acquire_point,
                release_point,
                damage,
            ),
            AndroidMessage::BindLayerStream { object } => {
                self.task(object)?;
                Ok(EndpointAction::BindLayerStream {
                    object,
                    sockets: received.descriptors.into_iter().map(|d| d.fd).collect(),
                })
            }
            AndroidMessage::SetContentState { object, flags } => {
                self.task(object)?;
                Ok(EndpointAction::SetContentState { object, flags })
            }
            AndroidMessage::SetFrameRate { object, millihz } => {
                self.task(object)?;
                Ok(EndpointAction::SetFrameRate { object, millihz })
            }
            AndroidMessage::RequestActivation { object } => {
                if !self.activation_requests {
                    return Err(EndpointError::IncompatibleClient);
                }
                if self.task(object)?.task.is_none() {
                    return Err(EndpointError::UnknownObject(object));
                }
                Ok(EndpointAction::RequestActivation { object })
            }
            AndroidMessage::SetKeepScreenOn { object, keep } => {
                if !self.keep_screen_on {
                    return Err(EndpointError::IncompatibleClient);
                }
                self.task(object)?;
                Ok(EndpointAction::SetKeepScreenOn { object, keep })
            }
            AndroidMessage::Pong { cookie } => Ok(EndpointAction::Pong { cookie }),
        }
    }

    fn bind_input_channel(
        &self,
        generation: u64,
        wire: u16,
        descriptors: Vec<AttachedDescriptor>,
    ) -> Result<EndpointAction, EndpointError> {
        if !self.direct_input {
            return Err(EndpointError::IncompatibleClient);
        }
        let socket = descriptors
            .into_iter()
            .next()
            .ok_or(EndpointError::IncompatibleClient)?
            .fd;
        Ok(EndpointAction::BindInputChannel {
            generation,
            wire,
            socket,
        })
    }

    /// Publish authoritative DMA-BUF feedback for one native task.
    ///
    /// # Errors
    ///
    /// Rejects unknown tasks or invalid/regressing feedback and propagates send
    /// failure.
    pub fn send_format_feedback(
        &mut self,
        object: TaskObjectId,
        generation: u32,
        formats: Vec<FormatModifier>,
    ) -> Result<(), EndpointError> {
        self.task_mut(object)?
            .state
            .replace_feedback(generation, formats.iter().copied())?;
        self.socket.send_denial(&DenialMessage::FormatFeedback {
            object,
            generation,
            formats,
        })?;
        Ok(())
    }

    /// Lend one Denial-owned render target and its plane descriptors to a task.
    ///
    /// The target is reserved in the same release-before-reuse state machine
    /// used by `Present`. A failed socket send rolls the reservation back.
    ///
    /// # Errors
    ///
    /// Rejects unknown tasks, duplicate or invalid targets, mismatched
    /// descriptors, and socket-send failures.
    pub fn register_render_target(
        &mut self,
        object: TaskObjectId,
        configure_serial: u32,
        buffer: &BufferMetadata,
        planes: &[BorrowedFd<'_>],
    ) -> Result<(), EndpointError> {
        self.task_mut(object)?
            .state
            .register_buffer(buffer.clone())?;
        let message = DenialMessage::RegisterRenderTarget {
            object,
            configure_serial,
            buffer: buffer.clone(),
        };
        if let Err(error) = self.socket.send_denial_with_descriptors(&message, planes) {
            let _ = self.task_mut(object)?.state.unregister_buffer(buffer.id);
            return Err(error.into());
        }
        Ok(())
    }

    /// Revoke one idle Denial-owned render target from a task.
    ///
    /// A failed send restores the local registration so ownership does not
    /// become ambiguous across the transport boundary.
    ///
    /// # Errors
    ///
    /// Rejects unknown tasks, unknown or busy targets, and socket-send
    /// failures.
    pub fn unregister_render_target(
        &mut self,
        object: TaskObjectId,
        buffer: BufferId,
    ) -> Result<(), EndpointError> {
        let metadata = self.task_mut(object)?.state.unregister_buffer(buffer)?;
        let message = DenialMessage::UnregisterRenderTarget { object, buffer };
        if let Err(error) = self.socket.send_denial(&message) {
            self.task_mut(object)?.state.register_buffer(metadata)?;
            return Err(error.into());
        }
        Ok(())
    }

    /// Configure one native task window and invalidate an older acknowledgement.
    ///
    /// # Errors
    ///
    /// Rejects unknown tasks or invalid dimensions and propagates send failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "arguments exactly match the version-1 Configure transaction"
    )]
    pub fn send_configure(
        &mut self,
        object: TaskObjectId,
        serial: u32,
        width: u32,
        height: u32,
        scale_numerator: u32,
        scale_denominator: u32,
        transform: droidloom_denial_protocol::Transform,
        refresh_millihz: u32,
    ) -> Result<(), EndpointError> {
        self.task_mut(object)?
            .state
            .configure(serial, width, height, refresh_millihz)?;
        self.socket.send_denial(&DenialMessage::Configure {
            object,
            serial,
            width,
            height,
            scale_numerator,
            scale_denominator,
            transform,
            refresh_millihz,
        })?;
        Ok(())
    }

    /// Publish Denial-owned visibility and focus state for an Android task.
    ///
    /// # Errors
    ///
    /// Rejects unknown objects and propagates codec or socket failure.
    pub fn send_visibility(
        &self,
        object: TaskObjectId,
        visibility: Visibility,
        focused: bool,
    ) -> Result<(), EndpointError> {
        self.task(object)?;
        self.socket.send_denial(&DenialMessage::Visibility {
            object,
            visibility,
            focused,
        })?;
        Ok(())
    }

    /// Publish the aggregate power state of the host's physical outputs.
    ///
    /// # Errors
    ///
    /// Propagates codec or socket failure.
    pub fn send_display_power(&self, powered: bool) -> Result<(), EndpointError> {
        self.socket
            .send_denial(&DenialMessage::DisplayPower { powered })?;
        Ok(())
    }

    /// Request that Android finish the task represented by a native window.
    ///
    /// # Errors
    ///
    /// Rejects unknown objects and propagates codec or socket failure.
    pub fn send_close(&self, object: TaskObjectId) -> Result<(), EndpointError> {
        self.task(object)?;
        self.socket.send_denial(&DenialMessage::Close { object })?;
        Ok(())
    }

    /// Route one typed host input event to the exact Android task object.
    ///
    /// # Errors
    ///
    /// Rejects unknown objects and propagates codec or socket failure.
    pub fn send_input(
        &self,
        object: TaskObjectId,
        serial: u64,
        timestamp_nanos: u64,
        event: InputEvent,
    ) -> Result<(), EndpointError> {
        self.task(object)?;
        self.socket.send_denial(&DenialMessage::Input {
            object,
            serial,
            timestamp_nanos,
            event,
        })?;
        Ok(())
    }

    /// Validate Denial's renderer/KMS completion for a pending frame.
    ///
    /// Composer returns its own composition-completion fence to `SurfaceFlinger`.
    /// The Denial-owned output target is made reusable only by the later
    /// `BufferReleased` event, so v1's legacy release-timeline point no longer
    /// needs materialization and cannot impose ordering on independent target
    /// completions.
    ///
    /// # Errors
    ///
    /// Rejects unknown task/frame identities or an unbound task timeline.
    pub fn materialize_release(
        &mut self,
        object: TaskObjectId,
        frame: FrameId,
        _completion_fence: BorrowedFd<'_>,
    ) -> Result<(), EndpointError> {
        let task = self.task(object)?;
        task.pending
            .get(&frame)
            .ok_or(EndpointError::UnknownFrame { object, frame })?;
        if task.timelines.is_none() {
            return Err(TaskStateError::TimelinesUnbound.into());
        }
        Ok(())
    }

    /// Release a frame that Denial discarded without sampling.
    ///
    /// # Errors
    ///
    /// Rejects unknown task/frame identities and propagates send failure.
    pub fn discard_frame(
        &mut self,
        object: TaskObjectId,
        frame: FrameId,
    ) -> Result<(), EndpointError> {
        let task = self.task(object)?;
        if !task.pending.contains_key(&frame) {
            return Err(EndpointError::UnknownFrame { object, frame });
        }
        if task.timelines.is_none() {
            return Err(TaskStateError::TimelinesUnbound.into());
        }
        self.finish_release(object, frame)
    }

    /// Send `BufferReleased` after Denial's completion notification confirms
    /// that renderer/KMS use has ended.
    ///
    /// # Errors
    ///
    /// Rejects unknown task/frame identities or a mismatched state tuple and
    /// propagates send failure.
    pub fn finish_release(
        &mut self,
        object: TaskObjectId,
        frame: FrameId,
    ) -> Result<(), EndpointError> {
        let (message, retiring, finished_retirement) = {
            let task = self.task_mut(object)?;
            let pending = task
                .pending
                .get(&frame)
                .cloned()
                .ok_or(EndpointError::UnknownFrame { object, frame })?;
            let message = task
                .state
                .release(frame, pending.buffer, pending.release_point)?;
            task.pending.remove(&frame);
            (
                message,
                task.retiring,
                task.retiring && task.state.in_flight_count() == 0,
            )
        };
        if finished_retirement {
            self.tasks.remove(&object);
        }
        // Composer intentionally forgets a retired task as soon as it asks
        // Denial to destroy the window. Terminal releases generated while the
        // host drops that window are consumed locally instead of addressing an
        // Android-side object which no longer exists.
        if !retiring {
            self.socket.send_denial(&message)?;
        }
        Ok(())
    }

    /// Opt into real display observations for direct layer scenes.
    pub fn with_scene_presentation(mut self) -> Self {
        self.scene_presentation = true;
        self
    }

    /// Whether direct-scene feedback is mutually supported.
    pub fn scene_presentation(&self) -> bool {
        self.ready && self.scene_presentation
    }

    /// Opt into forwarding the host's frame grants as Android's vsync.
    pub fn with_host_clock(mut self) -> Self {
        self.host_clock = true;
        self
    }

    /// Whether Android takes its vsync from forwarded host frame grants.
    pub fn host_clock(&self) -> bool {
        self.ready && self.host_clock
    }

    /// Opt in when the host can keep its displays on for a task.
    pub fn with_keep_screen_on(mut self) -> Self {
        self.keep_screen_on = true;
        self
    }

    /// Opt in when the host gives each task its output's density.
    pub fn with_task_density(mut self) -> Self {
        self.task_density = true;
        self
    }

    /// Lays `object` out at `dpi`, 0 for Android's display density. Without
    /// negotiated task density this is a no-op.
    ///
    /// # Errors
    ///
    /// Rejects unknown tasks and propagates codec or socket failure.
    pub fn send_density(&self, object: TaskObjectId, dpi: u32) -> Result<(), EndpointError> {
        if self.ready && self.task_density {
            self.task(object)?;
            self.socket
                .send_denial(&DenialMessage::Density { object, dpi })?;
        }
        Ok(())
    }

    /// Forward one host frame grant. Without a negotiated host clock this is
    /// a no-op; Android then runs its own timer.
    ///
    /// # Errors
    ///
    /// Propagates codec or socket failure.
    pub fn send_frame_grant(
        &self,
        latch_nanos: u64,
        target_nanos: u64,
        period_nanos: u64,
    ) -> Result<(), EndpointError> {
        if self.host_clock() {
            self.socket.send_denial(&DenialMessage::FrameGrant {
                latch_nanos,
                target_nanos,
                period_nanos,
            })?;
        }
        Ok(())
    }

    /// Forward display timing for a committed direct scene. This never accesses
    /// composed-frame ownership or signals any release timeline.
    pub fn send_scene_presented(
        &mut self,
        object: TaskObjectId,
        scene: u64,
        timestamp_nanos: u64,
        refresh_period_nanos: u64,
        sequence: u64,
    ) -> Result<(), EndpointError> {
        if !self.scene_presentation() || self.task_mut(object)?.retiring {
            return Ok(());
        }
        self.socket.send_denial(&DenialMessage::ScenePresented {
            object,
            scene,
            timestamp_nanos,
            refresh_period_nanos,
            sequence,
        })?;
        Ok(())
    }

    /// Retain a frame identity for optional presentation feedback after release.
    /// Returns false when the bounded feedback queue is full; callers should
    /// still submit the frame, without requesting another timing callback.
    ///
    /// # Errors
    /// Rejects an unknown task or a frame which has not been accepted.
    pub fn track_presentation(
        &mut self,
        object: TaskObjectId,
        frame: FrameId,
    ) -> Result<bool, EndpointError> {
        let task = self.task_mut(object)?;
        if !task.pending.contains_key(&frame) {
            return Err(EndpointError::UnknownFrame { object, frame });
        }
        if task.presentation_frames.len() >= 64 {
            return Ok(false);
        }
        Ok(task.presentation_frames.insert(frame))
    }

    /// Forget timing metadata for a compositor-discarded frame.
    pub fn forget_presentation(&mut self, object: TaskObjectId, frame: FrameId) {
        if let Some(task) = self.tasks.get_mut(&object) {
            task.presentation_frames.remove(&frame);
        }
    }

    /// Report one host presentation timestamp independently of buffer release.
    ///
    /// # Errors
    ///
    /// Rejects unknown task/frame identities and propagates codec/send failure.
    pub fn send_presented(
        &mut self,
        object: TaskObjectId,
        frame: FrameId,
        timestamp_nanos: u64,
        refresh_period_nanos: u64,
        sequence: u64,
        flags: u32,
    ) -> Result<(), EndpointError> {
        let task = self.task_mut(object)?;
        let tracked = task.presentation_frames.remove(&frame);
        if !tracked && !task.pending.contains_key(&frame) {
            return Err(EndpointError::UnknownFrame { object, frame });
        }
        if task.retiring {
            return Ok(());
        }
        self.socket.send_denial(&DenialMessage::Presented {
            object,
            frame,
            timestamp_nanos,
            refresh_period_nanos,
            sequence,
            flags,
        })?;
        Ok(())
    }

    /// Send one bounded fatal error before the connection is closed.
    ///
    /// # Errors
    ///
    /// Propagates codec or socket failure.
    pub fn send_fatal(
        &self,
        object: Option<TaskObjectId>,
        code: ProtocolErrorCode,
        offending_opcode: u16,
        message: String,
    ) -> Result<(), EndpointError> {
        self.socket.send_denial(&DenialMessage::Error {
            object,
            code,
            offending_opcode,
            message,
        })?;
        Ok(())
    }

    fn client_hello(
        &mut self,
        min_major: u16,
        max_major: u16,
        capabilities: u64,
    ) -> Result<EndpointAction, EndpointError> {
        if self.ready {
            return Err(EndpointError::RepeatedHandshake);
        }
        if min_major > PROTOCOL_MAJOR
            || max_major < PROTOCOL_MAJOR
            || capabilities & capability::REQUIRED_V1 != capability::REQUIRED_V1
        {
            return Err(EndpointError::IncompatibleClient);
        }
        self.activation_requests &= capabilities & capability::TASK_ACTIVATION != 0;
        self.direct_input &= capabilities & capability::DIRECT_INPUT != 0;
        self.lazy_render_targets &= capabilities & capability::LAZY_RENDER_TARGETS != 0;
        self.scene_presentation &= capabilities & capability::SCENE_PRESENTATION != 0;
        self.host_clock &= capabilities & capability::HOST_CLOCK != 0;
        self.keep_screen_on &= capabilities & capability::KEEP_SCREEN_ON != 0;
        self.task_density &= capabilities & capability::TASK_DENSITY != 0;
        self.socket.send_denial(&DenialMessage::ServerHello {
            major: PROTOCOL_MAJOR,
            minor: if self.task_density {
                PROTOCOL_MINOR
            } else if self.keep_screen_on {
                10
            } else if self.host_clock {
                9
            } else if self.scene_presentation {
                8
            } else if self.lazy_render_targets {
                7
            } else {
                6
            },
            capabilities: capability::REQUIRED_V1
                | if self.activation_requests {
                    capability::TASK_ACTIVATION
                } else {
                    0
                }
                | if self.direct_input {
                    capability::DIRECT_INPUT
                } else {
                    0
                }
                | if self.scene_presentation {
                    capability::SCENE_PRESENTATION
                } else {
                    0
                }
                | if self.host_clock {
                    capability::HOST_CLOCK
                } else {
                    0
                }
                | if self.keep_screen_on {
                    capability::KEEP_SCREEN_ON
                } else {
                    0
                }
                | if self.task_density {
                    capability::TASK_DENSITY
                } else {
                    0
                }
                | if self.lazy_render_targets {
                    capability::LAZY_RENDER_TARGETS
                } else {
                    0
                },
            max_task_objects: MAX_TASK_OBJECTS,
            max_buffers_per_task: MAX_BUFFERS_PER_TASK,
            max_damage_rects: u32::try_from(MAX_DAMAGE_RECTS).unwrap_or(u32::MAX),
        })?;
        self.ready = true;
        Ok(EndpointAction::ClientReady)
    }

    fn create_task(
        &mut self,
        object: TaskObjectId,
        display: AndroidDisplayId,
        package: String,
    ) -> Result<EndpointAction, EndpointError> {
        if self.tasks.len() >= usize::try_from(MAX_TASK_OBJECTS).unwrap_or(usize::MAX) {
            return Err(EndpointError::TaskLimit);
        }
        if self.tasks.contains_key(&object)
            || self.tasks.values().any(|live| live.display == display)
        {
            return Err(EndpointError::DuplicateIdentity);
        }
        let state = TaskPresentationState::new(object)?;
        self.tasks.insert(
            object,
            TaskEndpoint {
                task: None,
                display,
                state,
                timelines: None,
                pending: BTreeMap::new(),
                presentation_frames: BTreeSet::new(),
                retiring: false,
            },
        );
        Ok(EndpointAction::CreateTask {
            object,
            display,
            package,
        })
    }

    fn bind_task(
        &mut self,
        object: TaskObjectId,
        task: AndroidTaskId,
    ) -> Result<EndpointAction, EndpointError> {
        if self.tasks.values().any(|live| live.task == Some(task)) {
            return Err(EndpointError::DuplicateIdentity);
        }
        let endpoint = self.task_mut(object)?;
        if endpoint.task.is_some() {
            return Err(EndpointError::DuplicateIdentity);
        }
        endpoint.task = Some(task);
        Ok(EndpointAction::BindTask { object, task })
    }

    fn destroy_task(&mut self, object: TaskObjectId) -> Result<EndpointAction, EndpointError> {
        let task = self.task_mut(object)?;
        task.retiring = true;
        if task.state.in_flight_count() == 0 {
            self.tasks.remove(&object);
        }
        Ok(EndpointAction::DestroyTask { object })
    }

    fn bind_timelines(
        &mut self,
        object: TaskObjectId,
        descriptors: Vec<AttachedDescriptor>,
    ) -> Result<EndpointAction, EndpointError> {
        let mut descriptors = descriptors.into_iter();
        let acquire = take_descriptor(&mut descriptors, DescriptorKind::AcquireTimeline)?;
        let release = take_descriptor(&mut descriptors, DescriptorKind::ReleaseTimeline)?;
        let timelines =
            DenialTaskTimelines::import(&self.device, acquire.as_fd(), release.as_fd())?;
        let task = self.task_mut(object)?;
        task.state.bind_timelines()?;
        task.timelines = Some(timelines);
        Ok(EndpointAction::TimelinesBound { object })
    }

    fn register_buffer(
        &mut self,
        object: TaskObjectId,
        buffer: BufferMetadata,
        descriptors: Vec<AttachedDescriptor>,
    ) -> Result<EndpointAction, EndpointError> {
        let planes = descriptors
            .into_iter()
            .map(|descriptor| {
                if descriptor.kind == DescriptorKind::DmabufPlane {
                    Ok(descriptor.fd)
                } else {
                    Err(EndpointError::MissingDescriptor(
                        DescriptorKind::DmabufPlane,
                    ))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.task_mut(object)?
            .state
            .register_buffer(buffer.clone())?;
        Ok(EndpointAction::RegisterBuffer {
            object,
            buffer,
            planes,
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "arguments exactly match the version-1 Present transaction"
    )]
    fn present(
        &mut self,
        object: TaskObjectId,
        frame: FrameId,
        buffer: BufferId,
        configure_serial: u32,
        acquire_point: u64,
        release_point: u64,
        damage: Vec<droidloom_denial_protocol::Damage>,
    ) -> Result<EndpointAction, EndpointError> {
        let trace_begin = droidloom_syncobj::frame_trace::sampled(frame.0)
            .then(droidloom_syncobj::frame_trace::now_ns);
        let task = self.task_mut(object)?;
        let accepted = match task.state.present(
            object,
            frame,
            buffer,
            configure_serial,
            acquire_point,
            release_point,
            damage,
        ) {
            Ok(accepted) => accepted,
            Err(TaskStateError::StaleConfigure { .. }) => {
                let released = task.state.discard_superseded_present(
                    object,
                    frame,
                    buffer,
                    configure_serial,
                    acquire_point,
                    release_point,
                )?;
                let timelines = task
                    .timelines
                    .as_mut()
                    .ok_or(TaskStateError::TimelinesUnbound)?;
                // Validate that Android materialized the promised GPU fence.
                // The stale target is never sampled by Denial, so the fence
                // itself can be dropped and the separate release point can be
                // signalled immediately.
                drop(timelines.export_acquire_fence(acquire_point)?);
                timelines.signal_release(release_point)?;
                self.socket.send_denial(&released)?;
                return Ok(EndpointAction::SupersededPresentDiscarded { object, frame });
            }
            Err(error) => return Err(error.into()),
        };
        let acquire_fence = match task
            .timelines
            .as_mut()
            .ok_or(TaskStateError::TimelinesUnbound)?
            .export_acquire_fence(acquire_point)
        {
            Ok(fence) => fence,
            Err(error) => {
                task.state
                    .cancel_unsubmitted(frame, buffer, release_point)?;
                return Err(error.into());
            }
        };
        task.pending.insert(frame, accepted.clone());
        if let Some(begin) = trace_begin {
            droidloom_syncobj::frame_trace::event(
                "host_receive",
                object.0,
                frame.0,
                buffer.0,
                &[
                    ("receive_ns", begin),
                    (
                        "fence_exported_ns",
                        droidloom_syncobj::frame_trace::now_ns(),
                    ),
                ],
            );
        }
        Ok(EndpointAction::Present {
            frame: accepted,
            acquire_fence,
        })
    }

    fn task(&self, object: TaskObjectId) -> Result<&TaskEndpoint, EndpointError> {
        self.tasks
            .get(&object)
            .ok_or(EndpointError::UnknownObject(object))
    }

    fn task_mut(&mut self, object: TaskObjectId) -> Result<&mut TaskEndpoint, EndpointError> {
        self.tasks
            .get_mut(&object)
            .ok_or(EndpointError::UnknownObject(object))
    }
}

#[derive(Debug)]
struct TaskEndpoint {
    task: Option<AndroidTaskId>,
    display: AndroidDisplayId,
    state: TaskPresentationState,
    timelines: Option<DenialTaskTimelines>,
    pending: BTreeMap<FrameId, AcceptedWireFrame>,
    // Frame identities only: these never retain a DMA-BUF or a release fence.
    presentation_frames: BTreeSet<FrameId>,
    retiring: bool,
}

fn take_descriptor(
    descriptors: &mut impl Iterator<Item = AttachedDescriptor>,
    kind: DescriptorKind,
) -> Result<OwnedFd, EndpointError> {
    let descriptor = descriptors
        .next()
        .ok_or(EndpointError::MissingDescriptor(kind))?;
    if descriptor.kind != kind {
        return Err(EndpointError::MissingDescriptor(kind));
    }
    Ok(descriptor.fd)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io;
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt as _;

    use droidloom_denial_ipc::{ProtocolSocket, SeqPacket};
    use droidloom_denial_protocol::{
        AndroidDisplayId, AndroidMessage, AndroidTaskId, DenialMessage, PROTOCOL_MINOR,
        TaskObjectId, capability,
    };
    use droidloom_syncobj::SyncobjDevice;

    use super::{
        DenialEndpoint, DenialEndpointListener, EndpointAction, EndpointError,
        EndpointListenerError, ExpectedPeer,
    };

    fn pair() -> (ProtocolSocket, DenialEndpoint) {
        let (android, denial) = SeqPacket::pair().unwrap();
        let placeholder = File::open("/dev/null").unwrap();
        (
            ProtocolSocket::new(android),
            DenialEndpoint::new(
                ProtocolSocket::new(denial),
                SyncobjDevice::from_file(placeholder),
            ),
        )
    }

    #[test]
    fn direct_input_requires_negotiation_and_an_already_bound_task() {
        let (android, endpoint) = pair();
        let mut denial = endpoint.with_direct_input();
        android
            .send_android(
                &AndroidMessage::ClientHello {
                    min_major: 1,
                    max_major: 1,
                    capabilities: capability::REQUIRED_V1 | capability::DIRECT_INPUT,
                },
                &[],
            )
            .unwrap();
        denial.receive_action().unwrap();
        assert!(denial.direct_input());
        android.receive_denial().unwrap();
        let object = TaskObjectId(7);
        let route = AndroidMessage::BindInputRoute {
            object,
            display: 0,
            incarnation: 99,
        };
        android.send_android(&route, &[]).unwrap();
        assert!(matches!(
            denial.receive_action(),
            Err(EndpointError::UnknownObject(_))
        ));
        android
            .send_android(
                &AndroidMessage::CreateTask {
                    object,
                    display: AndroidDisplayId(9),
                    package: "org.example.app".into(),
                },
                &[],
            )
            .unwrap();
        denial.receive_action().unwrap();
        android.send_android(&route, &[]).unwrap();
        assert!(matches!(
            denial.receive_action(),
            Err(EndpointError::UnknownObject(_))
        ));
        android
            .send_android(
                &AndroidMessage::BindTask {
                    object,
                    task: AndroidTaskId(42),
                },
                &[],
            )
            .unwrap();
        denial.receive_action().unwrap();
        android.send_android(&route, &[]).unwrap();
        assert!(matches!(
            denial.receive_action().unwrap(),
            EndpointAction::BindInputRoute {
                task: AndroidTaskId(42),
                display: 0,
                incarnation: 99,
                ..
            }
        ));
        denial.request_input_channel(1).unwrap();
        assert_eq!(
            android.receive_denial().unwrap().message,
            DenialMessage::RequestInputChannel { generation: 1 }
        );
        let (writer, receiver) = SeqPacket::pair().unwrap();
        android
            .send_android(
                &AndroidMessage::BindInputChannel {
                    generation: 1,
                    wire: 7,
                },
                &[writer.as_fd()],
            )
            .unwrap();
        drop(writer);
        let EndpointAction::BindInputChannel { socket, .. } = denial.receive_action().unwrap()
        else {
            panic!("expected handoff");
        };
        drop(socket);
        assert!(receiver.receive_record().is_err());
    }

    #[test]
    fn target_demand_requires_mutual_support_and_an_existing_task() {
        for (host, client) in [(false, true), (true, false), (true, true)] {
            let (android, mut endpoint) = pair();
            if host {
                endpoint = endpoint.with_lazy_render_targets();
            }
            android
                .send_android(
                    &AndroidMessage::ClientHello {
                        min_major: 1,
                        max_major: 1,
                        capabilities: capability::REQUIRED_V1
                            | if client {
                                capability::LAZY_RENDER_TARGETS
                            } else {
                                0
                            },
                    },
                    &[],
                )
                .unwrap();
            endpoint.receive_action().unwrap();
            let DenialMessage::ServerHello {
                minor,
                capabilities,
                ..
            } = android.receive_denial().unwrap().message
            else {
                panic!("hello")
            };
            let supported = host && client;
            assert_eq!(endpoint.lazy_render_targets(), supported);
            assert_eq!(
                capabilities & capability::LAZY_RENDER_TARGETS != 0,
                supported
            );
            assert_eq!(minor, if supported { 7 } else { 6 });
            let object = TaskObjectId(7);
            let request = AndroidMessage::RequestRenderTargets {
                object,
                configure_serial: 3,
            };
            android.send_android(&request, &[]).unwrap();
            assert!(endpoint.receive_action().is_err()); // No allocation for an unknown task.
            android
                .send_android(
                    &AndroidMessage::CreateTask {
                        object,
                        display: AndroidDisplayId(9),
                        package: "org.example.app".into(),
                    },
                    &[],
                )
                .unwrap();
            endpoint.receive_action().unwrap();
            android.send_android(&request, &[]).unwrap();
            assert_eq!(
                matches!(
                    endpoint.receive_action(),
                    Ok(EndpointAction::RequestRenderTargets {
                        object: TaskObjectId(7),
                        configure_serial: 3
                    })
                ),
                supported
            );
        }
    }

    #[test]
    fn listener_authenticates_exact_kernel_peer_before_protocol() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("denial-droidloom.sock");
        let process = std::fs::metadata("/proc/self").unwrap();
        let expected = ExpectedPeer {
            pid: std::process::id(),
            uid: process.uid(),
            gid: process.gid(),
        };
        let listener = match DenialEndpointListener::bind(
            &path,
            SyncobjDevice::from_file(File::open("/dev/null").unwrap()),
            expected,
        ) {
            Ok(listener) => listener,
            Err(EndpointListenerError::Ipc(droidloom_denial_ipc::IpcError::Io(error)))
                if error.kind() == io::ErrorKind::PermissionDenied =>
            {
                // Some test sandboxes deny filesystem Unix-socket binds.
                return;
            }
            Err(error) => panic!("unexpected listener bind failure: {error}"),
        };
        let android = ProtocolSocket::new(SeqPacket::connect(&path).unwrap());
        let mut denial = listener.accept().unwrap();
        android
            .send_android(
                &AndroidMessage::ClientHello {
                    min_major: 1,
                    max_major: 1,
                    capabilities: capability::REQUIRED_V1,
                },
                &[],
            )
            .unwrap();
        assert!(matches!(
            denial.receive_action().unwrap(),
            EndpointAction::ClientReady
        ));
    }

    #[test]
    fn exact_peer_policy_rejects_every_identity_mismatch() {
        let expected = ExpectedPeer {
            pid: 1,
            uid: 2,
            gid: 3,
        };
        assert!(expected.accepts(droidloom_denial_ipc::PeerCredentials {
            pid: 1,
            uid: 2,
            gid: 3,
        }));
        for actual in [
            droidloom_denial_ipc::PeerCredentials {
                pid: 4,
                uid: 2,
                gid: 3,
            },
            droidloom_denial_ipc::PeerCredentials {
                pid: 1,
                uid: 4,
                gid: 3,
            },
            droidloom_denial_ipc::PeerCredentials {
                pid: 1,
                uid: 2,
                gid: 4,
            },
        ] {
            assert!(!expected.accepts(actual));
        }
    }

    #[test]
    fn development_pid_wildcard_keeps_uid_and_gid_exact() {
        let expected = ExpectedPeer {
            pid: 0,
            uid: 1000,
            gid: 1000,
        };
        assert!(expected.accepts(droidloom_denial_ipc::PeerCredentials {
            pid: 42,
            uid: 1000,
            gid: 1000,
        }));
        assert!(!expected.accepts(droidloom_denial_ipc::PeerCredentials {
            pid: 42,
            uid: 1001,
            gid: 1000,
        }));
        assert!(!expected.accepts(droidloom_denial_ipc::PeerCredentials {
            pid: 42,
            uid: 1000,
            gid: 1001,
        }));
    }

    #[test]
    fn hello_selects_required_v1_capabilities() {
        let (android, mut denial) = pair();
        android
            .send_android(
                &AndroidMessage::ClientHello {
                    min_major: 1,
                    max_major: 1,
                    capabilities: capability::REQUIRED_V1,
                },
                &[],
            )
            .unwrap();
        assert!(matches!(
            denial.receive_action().unwrap(),
            EndpointAction::ClientReady
        ));
        assert!(matches!(
            android.receive_denial().unwrap().message,
            DenialMessage::ServerHello {
                capabilities: capability::REQUIRED_V1,
                ..
            }
        ));
    }

    #[test]
    fn activation_requires_mutual_support_and_a_bound_live_task() {
        for (host_support, client_support) in [(false, true), (true, false), (true, true)] {
            let (android, endpoint) = pair();
            let mut denial = if host_support {
                endpoint.with_activation_requests()
            } else {
                endpoint
            };
            android
                .send_android(
                    &AndroidMessage::ClientHello {
                        min_major: 1,
                        max_major: 1,
                        capabilities: capability::REQUIRED_V1
                            | if client_support {
                                capability::TASK_ACTIVATION
                            } else {
                                0
                            },
                    },
                    &[],
                )
                .unwrap();
            denial.receive_action().unwrap();
            let DenialMessage::ServerHello {
                capabilities,
                minor,
                ..
            } = android.receive_denial().unwrap().message
            else {
                panic!("missing hello")
            };
            let supported = host_support && client_support;
            assert_eq!(capabilities & capability::TASK_ACTIVATION != 0, supported);
            assert_eq!(minor, 6); // No demand-allocation capability in this handshake.
            let object = TaskObjectId(1);
            android
                .send_android(&AndroidMessage::RequestActivation { object }, &[])
                .unwrap();
            assert!(denial.receive_action().is_err()); // Unknown task, even with the capability.
            android
                .send_android(
                    &AndroidMessage::CreateTask {
                        object,
                        display: AndroidDisplayId(1),
                        package: "org.example.app".into(),
                    },
                    &[],
                )
                .unwrap();
            denial.receive_action().unwrap();
            android
                .send_android(&AndroidMessage::RequestActivation { object }, &[])
                .unwrap();
            assert!(denial.receive_action().is_err()); // A reserved display is not a bound task.
            android
                .send_android(
                    &AndroidMessage::BindTask {
                        object,
                        task: AndroidTaskId(26),
                    },
                    &[],
                )
                .unwrap();
            denial.receive_action().unwrap();
            for _ in 0..2 {
                // Reopening requests activation of the same object, never a second window.
                android
                    .send_android(&AndroidMessage::RequestActivation { object }, &[])
                    .unwrap();
                if supported {
                    assert!(matches!(
                        denial.receive_action().unwrap(),
                        EndpointAction::RequestActivation {
                            object: TaskObjectId(1)
                        }
                    ));
                } else {
                    assert!(matches!(
                        denial.receive_action(),
                        Err(EndpointError::IncompatibleClient)
                    ));
                }
            }
        }
    }

    #[test]
    fn every_android_task_becomes_an_independent_native_action() {
        let (android, mut denial) = pair();
        android
            .send_android(
                &AndroidMessage::ClientHello {
                    min_major: 1,
                    max_major: 1,
                    capabilities: capability::REQUIRED_V1,
                },
                &[],
            )
            .unwrap();
        denial.receive_action().unwrap();
        android.receive_denial().unwrap();

        for (object, task, display) in [(1, 11, 101), (2, 12, 102)] {
            android
                .send_android(
                    &AndroidMessage::CreateTask {
                        object: TaskObjectId(object),
                        display: AndroidDisplayId(display),
                        package: format!("org.example.app{task}"),
                    },
                    &[],
                )
                .unwrap();
            assert!(matches!(
                denial.receive_action().unwrap(),
                EndpointAction::CreateTask {
                    object: TaskObjectId(value),
                    ..
                } if value == object
            ));
            android
                .send_android(
                    &AndroidMessage::BindTask {
                        object: TaskObjectId(object),
                        task: AndroidTaskId(task),
                    },
                    &[],
                )
                .unwrap();
            assert!(matches!(
                denial.receive_action().unwrap(),
                EndpointAction::BindTask {
                    object: TaskObjectId(value),
                    task: AndroidTaskId(bound),
                } if value == object && bound == task
            ));
        }
    }

    #[test]
    fn task_messages_before_hello_fail_closed() {
        let (android, mut denial) = pair();
        android
            .send_android(
                &AndroidMessage::CreateTask {
                    object: TaskObjectId(1),
                    display: AndroidDisplayId(1),
                    package: "org.example.app".to_owned(),
                },
                &[],
            )
            .unwrap();
        assert!(matches!(
            denial.receive_action(),
            Err(EndpointError::HandshakeRequired)
        ));
    }

    #[test]
    fn frame_grants_flow_only_with_a_negotiated_host_clock() {
        for host in [false, true] {
            for client in [false, true] {
                let (android, mut endpoint) = pair();
                if host {
                    endpoint = endpoint.with_host_clock();
                }
                android
                    .send_android(
                        &AndroidMessage::ClientHello {
                            min_major: 1,
                            max_major: 1,
                            capabilities: capability::REQUIRED_V1
                                | if client { capability::HOST_CLOCK } else { 0 },
                        },
                        &[],
                    )
                    .unwrap();
                endpoint.receive_action().unwrap();
                let DenialMessage::ServerHello {
                    minor,
                    capabilities,
                    ..
                } = android.receive_denial().unwrap().message
                else {
                    panic!("hello");
                };
                assert_eq!(endpoint.host_clock(), host && client);
                assert_eq!(capabilities & capability::HOST_CLOCK != 0, host && client);
                assert_eq!(minor, if host && client { 9 } else { 6 });
                endpoint.send_frame_grant(10, 20, 10).unwrap();
                endpoint.send_display_power(true).unwrap();
                let first = android.receive_denial().unwrap().message;
                if host && client {
                    assert_eq!(
                        first,
                        DenialMessage::FrameGrant {
                            latch_nanos: 10,
                            target_nanos: 20,
                            period_nanos: 10,
                        }
                    );
                } else {
                    assert_eq!(first, DenialMessage::DisplayPower { powered: true });
                }
            }
        }
    }

    #[test]
    fn task_density_flows_only_when_negotiated() {
        for host in [false, true] {
            for client in [false, true] {
                let (android, mut endpoint) = pair();
                if host {
                    endpoint = endpoint.with_task_density();
                }
                android
                    .send_android(
                        &AndroidMessage::ClientHello {
                            min_major: 1,
                            max_major: 1,
                            capabilities: capability::REQUIRED_V1
                                | if client { capability::TASK_DENSITY } else { 0 },
                        },
                        &[],
                    )
                    .unwrap();
                endpoint.receive_action().unwrap();
                let DenialMessage::ServerHello {
                    minor,
                    capabilities,
                    ..
                } = android.receive_denial().unwrap().message
                else {
                    panic!("hello");
                };
                let supported = host && client;
                assert_eq!(capabilities & capability::TASK_DENSITY != 0, supported);
                assert_eq!(minor, if supported { PROTOCOL_MINOR } else { 6 });
                let object = TaskObjectId(1);
                android
                    .send_android(
                        &AndroidMessage::CreateTask {
                            object,
                            display: AndroidDisplayId(1),
                            package: "org.example.app".into(),
                        },
                        &[],
                    )
                    .unwrap();
                endpoint.receive_action().unwrap();
                endpoint.send_density(object, 230).unwrap();
                endpoint.send_display_power(true).unwrap();
                let first = android.receive_denial().unwrap().message;
                if supported {
                    assert_eq!(first, DenialMessage::Density { object, dpi: 230 });
                } else {
                    assert_eq!(first, DenialMessage::DisplayPower { powered: true });
                }
            }
        }
    }

    #[test]
    fn keep_screen_on_flows_only_when_negotiated() {
        for host in [false, true] {
            for client in [false, true] {
                let (android, mut endpoint) = pair();
                if host {
                    endpoint = endpoint.with_keep_screen_on();
                }
                android
                    .send_android(
                        &AndroidMessage::ClientHello {
                            min_major: 1,
                            max_major: 1,
                            capabilities: capability::REQUIRED_V1
                                | if client {
                                    capability::KEEP_SCREEN_ON
                                } else {
                                    0
                                },
                        },
                        &[],
                    )
                    .unwrap();
                endpoint.receive_action().unwrap();
                let DenialMessage::ServerHello {
                    minor,
                    capabilities,
                    ..
                } = android.receive_denial().unwrap().message
                else {
                    panic!("hello");
                };
                let supported = host && client;
                assert_eq!(capabilities & capability::KEEP_SCREEN_ON != 0, supported);
                assert_eq!(minor, if supported { 10 } else { 6 });
                let object = TaskObjectId(1);
                android
                    .send_android(
                        &AndroidMessage::CreateTask {
                            object,
                            display: AndroidDisplayId(1),
                            package: "org.example.app".into(),
                        },
                        &[],
                    )
                    .unwrap();
                endpoint.receive_action().unwrap();
                android
                    .send_android(&AndroidMessage::SetKeepScreenOn { object, keep: true }, &[])
                    .unwrap();
                if supported {
                    assert!(matches!(
                        endpoint.receive_action().unwrap(),
                        EndpointAction::SetKeepScreenOn {
                            object: TaskObjectId(1),
                            keep: true,
                        }
                    ));
                } else {
                    assert!(matches!(
                        endpoint.receive_action(),
                        Err(EndpointError::IncompatibleClient)
                    ));
                }
            }
        }
    }

    #[test]
    fn scene_feedback_is_negotiated_and_does_not_release_composed_frames() {
        for host in [false, true] {
            for client in [false, true] {
                let (android, mut endpoint) = pair();
                if host {
                    endpoint = endpoint.with_scene_presentation();
                }
                android
                    .send_android(
                        &AndroidMessage::ClientHello {
                            min_major: 1,
                            max_major: 1,
                            capabilities: capability::REQUIRED_V1
                                | if client {
                                    capability::SCENE_PRESENTATION
                                } else {
                                    0
                                },
                        },
                        &[],
                    )
                    .unwrap();
                endpoint.receive_action().unwrap();
                let DenialMessage::ServerHello {
                    minor,
                    capabilities,
                    ..
                } = android.receive_denial().unwrap().message
                else {
                    panic!("hello");
                };
                assert_eq!(endpoint.scene_presentation(), host && client);
                assert_eq!(
                    capabilities & capability::SCENE_PRESENTATION != 0,
                    host && client
                );
                assert_eq!(minor, if host && client { 8 } else { 6 });
                let object = TaskObjectId(7);
                android
                    .send_android(
                        &AndroidMessage::CreateTask {
                            object,
                            display: AndroidDisplayId(9),
                            package: "org.test.app".into(),
                        },
                        &[],
                    )
                    .unwrap();
                endpoint.receive_action().unwrap();
                endpoint
                    .send_scene_presented(object, 22, 1234, 16_666_667, 5)
                    .unwrap();
                assert!(endpoint.tasks[&object].pending.is_empty());
                assert!(endpoint.tasks[&object].presentation_frames.is_empty());
                assert!(endpoint.tasks[&object].timelines.is_none());
                if host && client {
                    assert_eq!(
                        android.receive_denial().unwrap().message,
                        DenialMessage::ScenePresented {
                            object,
                            scene: 22,
                            timestamp_nanos: 1234,
                            refresh_period_nanos: 16_666_667,
                            sequence: 5
                        }
                    );
                    assert!(
                        endpoint
                            .send_scene_presented(TaskObjectId(99), 22, 1234, 16_666_667, 5)
                            .is_err()
                    );
                }
                endpoint.tasks.get_mut(&object).unwrap().retiring = true;
                endpoint
                    .send_scene_presented(object, 23, 4321, 16_666_667, 6)
                    .unwrap();
            }
        }
    }
}
