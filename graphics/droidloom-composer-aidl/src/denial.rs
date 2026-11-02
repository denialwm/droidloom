//! Concrete Composer presentation sink for the Denial-native protocol.
//!
//! One [`TaskChannel`] exists per Android task/display. DMA-BUF descriptors
//! cross the socket only on first allocation registration; acquire and release
//! timeline descriptors cross only when the task is bound. A steady-state
//! submission sends one descriptor-free `Present` record.

use std::collections::BTreeMap;
use std::os::fd::AsFd;
use std::sync::{Arc, Mutex, MutexGuard};

use binder::ParcelFileDescriptor;
use droidloom_composer::denial::{
    AppliedDenialEvent, DenialBridgeError, DenialTaskBinding, PreparedPresent, RenderTargetDemand,
};
use droidloom_composer::{
    ClientTarget, ComposerError, DisplayId, ReservedTaskWindowSpec, TaskId, hwc3::ErrorCode,
    hwc3::Session,
};
use droidloom_denial_ipc::{AttachedDescriptor, IpcError, ProtocolSocket};
use droidloom_denial_protocol::{AndroidMessage, DenialMessage, DescriptorKind, TaskObjectId};
use droidloom_syncobj::frame_trace;
use droidloom_syncobj::{AndroidTaskTimelines, SyncobjDevice, SyncobjError};
use droidloom_transport::{AcceptedFrame, BufferId, BufferMetadata};
use thiserror::Error;

use crate::minigbm::{ImportedRenderTarget, PreparedLayer, PresentationSink};
use crate::{PresentationFailure, gles::LayerCompositor};

/// Task lifecycle, socket, or explicit-synchronization failure.
#[derive(Debug, Error)]
pub enum DenialSinkError {
    /// Another live task already owns the display.
    #[error("Denial task display {0:?} is already registered")]
    DuplicateDisplay(DisplayId),
    /// Another live task already owns the protocol object.
    #[error("Denial task object {0:?} is already registered")]
    DuplicateObject(TaskObjectId),
    /// No live task owns the requested display.
    #[error("Denial task display {0:?} is not registered")]
    UnknownDisplay(DisplayId),
    /// A task-scoped host event named no live task object.
    #[error("Denial task object {0:?} is not registered")]
    UnknownObject(TaskObjectId),
    /// A connection-scoped message reached task dispatch.
    #[error("connection-scoped Denial message cannot be applied to a task")]
    ConnectionMessage,
    /// A cached buffer ID was reused with different immutable metadata.
    #[error("buffer {0:?} was reused with different immutable metadata")]
    BufferMetadataMismatch(BufferId),
    /// Native plane descriptors do not match the validated plane table.
    #[error("DMA-BUF plane descriptor count does not match metadata")]
    PlaneDescriptorMismatch,
    /// Denial repeated a live render-target ID.
    #[error("Denial repeated render target {0:?}")]
    DuplicateRenderTarget(BufferId),
    /// Denial revoked a render target that was not registered.
    #[error("Denial revoked unknown render target {0:?}")]
    UnknownRenderTarget(BufferId),
    /// No target in the newest configured pool is currently reusable.
    #[error("Denial display {0:?} has no idle render target")]
    NoRenderTarget(DisplayId),
    /// Task state was poisoned by a panic and cannot be trusted.
    #[error("Denial task presentation state lock is poisoned")]
    Poisoned,
    /// Typed task/Composer bridge rejected an ordering transition.
    #[error(transparent)]
    Bridge(#[from] DenialBridgeError),
    /// Typed sequenced-packet transport failed.
    #[error(transparent)]
    Ipc(#[from] IpcError),
    /// Linux DRM syncobj operation failed.
    #[error(transparent)]
    Syncobj(#[from] SyncobjError),
    /// EGL/GLES layer composition failed.
    #[error("Droidloom layer composition failed: {0}")]
    Graphics(String),
    /// The safe Composer state rejected a direct SurfaceFlinger frame.
    #[error(transparent)]
    Composer(#[from] ComposerError),
}

impl DenialSinkError {
    fn error_code(&self) -> ErrorCode {
        match self {
            Self::UnknownDisplay(_) => ErrorCode::BadDisplay,
            Self::BufferMetadataMismatch(_)
            | Self::PlaneDescriptorMismatch
            | Self::DuplicateRenderTarget(_)
            | Self::UnknownRenderTarget(_)
            | Self::Bridge(_) => ErrorCode::BadParameter,
            Self::DuplicateDisplay(_)
            | Self::DuplicateObject(_)
            | Self::UnknownObject(_)
            | Self::ConnectionMessage
            | Self::NoRenderTarget(_)
            | Self::Poisoned
            | Self::Ipc(_)
            | Self::Syncobj(_)
            | Self::Graphics(_)
            | Self::Composer(_) => ErrorCode::NoResources,
        }
    }
}

/// Cloneable concrete sink shared by the Binder adapter and lifecycle/event
/// pump. The socket is full-duplex; only one event-pump thread may receive.
#[derive(Clone, Debug)]
pub struct DenialPresentationSink {
    socket: Arc<ProtocolSocket>,
    device: SyncobjDevice,
    tasks: Arc<Mutex<BTreeMap<DisplayId, TaskChannel>>>,
    compositor: Arc<Mutex<Option<LayerCompositor>>>,
    activation_requests: bool,
    lazy_render_targets: bool,
    keep_screen_on: bool,
}

impl DenialPresentationSink {
    /// Create an empty sink after the connection-level hello has succeeded.
    pub fn new(socket: ProtocolSocket, device: SyncobjDevice) -> Self {
        Self {
            socket: Arc::new(socket),
            device,
            tasks: Arc::new(Mutex::new(BTreeMap::new())),
            compositor: Arc::new(Mutex::new(None)),
            activation_requests: false,
            lazy_render_targets: false,
            keep_screen_on: false,
        }
    }

    /// Record the activation capability negotiated with this host.
    #[must_use]
    pub fn with_activation_requests(mut self, enabled: bool) -> Self {
        self.activation_requests = enabled;
        self
    }

    /// Record whether this host allocates fallback targets on demand.
    #[must_use]
    pub fn with_lazy_render_targets(mut self, enabled: bool) -> Self {
        self.lazy_render_targets = enabled;
        self
    }

    /// Record whether this host keeps its displays on for a task.
    #[must_use]
    pub fn with_keep_screen_on(mut self, enabled: bool) -> Self {
        self.keep_screen_on = enabled;
        self
    }

    /// Tell the host whether a shown window of the task has Android's
    /// `FLAG_KEEP_SCREEN_ON`. Only changes are sent.
    ///
    /// # Errors
    /// Returns unknown-display or transport errors; older hosts remain usable.
    pub fn set_keep_screen_on(&self, display: DisplayId, keep: bool) -> Result<(), DenialSinkError> {
        if !self.keep_screen_on {
            return Ok(());
        }
        let mut tasks = self.lock_tasks()?;
        let channel = tasks
            .get_mut(&display)
            .ok_or(DenialSinkError::UnknownDisplay(display))?;
        if channel.keep_screen_on != keep {
            self.socket.send_android(
                &AndroidMessage::SetKeepScreenOn {
                    object: channel.binding.object(),
                    keep,
                },
                &[],
            )?;
            channel.keep_screen_on = keep;
        }
        Ok(())
    }

    /// Request host activation of an already bound task when supported.
    ///
    /// # Errors
    /// Returns unknown-display or transport errors; older hosts remain usable.
    pub fn request_activation(&self, display: DisplayId) -> Result<(), DenialSinkError> {
        if self.activation_requests {
            let tasks = self.lock_tasks()?;
            let channel = tasks
                .get(&display)
                .ok_or(DenialSinkError::UnknownDisplay(display))?;
            self.socket.send_android(
                &AndroidMessage::RequestActivation {
                    object: channel.binding.object(),
                },
                &[],
            )?;
        }
        Ok(())
    }

    /// Borrow the connected socket for a single-owner protocol event pump.
    pub fn socket(&self) -> &ProtocolSocket {
        &self.socket
    }

    /// Create and bind one native Denial task object and its two timelines.
    ///
    /// The caller must first create the matching [`Session`] task window. This
    /// method holds the task map lock through both sends so a concurrent event
    /// receiver cannot observe a configure before local insertion completes.
    ///
    /// # Errors
    ///
    /// Rejects duplicate display/object ownership and propagates timeline or
    /// transport failures. A failed timeline bind attempts remote task cleanup.
    pub fn register_task(
        &self,
        object: TaskObjectId,
        reservation: &ReservedTaskWindowSpec,
    ) -> Result<(), DenialSinkError> {
        let mut tasks = self.lock_tasks()?;
        if tasks.contains_key(&reservation.display) {
            return Err(DenialSinkError::DuplicateDisplay(reservation.display));
        }
        if tasks.values().any(|task| task.binding.object() == object) {
            return Err(DenialSinkError::DuplicateObject(object));
        }

        let binding = DenialTaskBinding::new(object, reservation)?;
        let timelines = AndroidTaskTimelines::create(&self.device)?;
        let descriptors = timelines.export_descriptors()?;
        self.socket
            .send_android(&binding.create_task_message(), &[])?;
        let timeline_fds = [descriptors.acquire.as_fd(), descriptors.release.as_fd()];
        if let Err(error) = self
            .socket
            .send_android(&binding.bind_timelines_message(), &timeline_fds)
        {
            let _ = self
                .socket
                .send_android(&AndroidMessage::DestroyTask { object }, &[]);
            return Err(error.into());
        }
        tasks.insert(
            reservation.display,
            TaskChannel {
                binding,
                timelines,
                render_targets: BTreeMap::new(),
                frame_targets: BTreeMap::new(),
                trace_fences: BTreeMap::new(),
                content_opaque: None,
                keep_screen_on: false,
                surfaceflinger_present_fence: None,
                headless_completion: None,
                android_display_extent: None,
                target_demand: RenderTargetDemand::default(),
            },
        );
        Ok(())
    }

    /// Publish the real Android task identity after launch on the connected
    /// display. Local state is committed only after the record is sent.
    ///
    /// # Errors
    ///
    /// Rejects unknown displays, repeated/zero bindings, and transport errors.
    pub fn bind_task(&self, display: DisplayId, task: TaskId) -> Result<(), DenialSinkError> {
        let mut tasks = self.lock_tasks()?;
        if tasks.iter().any(|(owned_display, channel)| {
            *owned_display != display && channel.binding.task() == Some(task)
        }) {
            return Err(DenialSinkError::Bridge(
                DenialBridgeError::UnexpectedMessage,
            ));
        }
        let channel = tasks
            .get_mut(&display)
            .ok_or(DenialSinkError::UnknownDisplay(display))?;
        if channel.binding.task() == Some(task) {
            return Ok(());
        }
        let message = channel.binding.bind_task_message(task)?;
        self.socket.send_android(&message, &[])?;
        channel.binding.mark_task_bound(task)?;
        Ok(())
    }

    /// Ask the host to release its displayed frame, keeping the binding alive
    /// so the event pump can route the resulting buffer releases. Retry safely.
    ///
    /// # Errors
    ///
    /// Rejects an unknown display and preserves state on transport failure.
    pub fn request_task_removal(&self, display: DisplayId) -> Result<(), DenialSinkError> {
        let mut tasks = self.lock_tasks()?;
        let task = tasks
            .get_mut(&display)
            .ok_or(DenialSinkError::UnknownDisplay(display))?;
        task.binding
            .request_destroy(|message| self.socket.send_android(&message, &[]))?;
        Ok(())
    }

    /// Forget a destroyed binding after the Composer core has drained releases.
    ///
    /// # Errors
    /// Rejects premature removal or a poisoned task map.
    pub fn unregister_task(&self, display: DisplayId) -> Result<(), DenialSinkError> {
        let mut tasks = self.lock_tasks()?;
        if tasks
            .get(&display)
            .is_some_and(|task| !task.binding.destroy_requested())
        {
            return Err(DenialBridgeError::UnexpectedMessage.into());
        }
        tasks.remove(&display);
        Ok(())
    }

    /// Acknowledge a host configure in both Composer and the protocol stream.
    ///
    /// # Errors
    ///
    /// Rejects an unknown display or stale serial and propagates send failure.
    pub fn acknowledge_configure(
        &self,
        session: &mut Session,
        display: DisplayId,
        serial: u32,
    ) -> Result<(), DenialSinkError> {
        let mut tasks = self.lock_tasks()?;
        let task = tasks
            .get_mut(&display)
            .ok_or(DenialSinkError::UnknownDisplay(display))?;
        let message = task.binding.acknowledge_configure(session, serial)?;
        self.socket.send_android(&message, &[])?;
        Ok(())
    }

    /// Apply one already-decoded task-scoped host event to its exact Composer
    /// display and protocol binding.
    ///
    /// # Errors
    ///
    /// Rejects connection messages, unknown objects, wrong ordering, or a
    /// poisoned task map.
    pub fn apply_task_event(
        &self,
        session: &mut Session,
        message: &DenialMessage,
    ) -> Result<AppliedDenialEvent, DenialSinkError> {
        let received = if frame_trace::enabled() {
            frame_trace::now_ns()
        } else {
            0
        };
        let object = task_object(message).ok_or(DenialSinkError::ConnectionMessage)?;
        let mut tasks = self.lock_tasks()?;
        let task = tasks
            .values_mut()
            .find(|task| task.binding.object() == object)
            .ok_or(DenialSinkError::UnknownObject(object))?;
        let event = task.binding.apply_event(session, message)?;
        if let AppliedDenialEvent::Configure {
            width,
            height,
            serial,
            ..
        } = &event
        {
            pin_android_display_extent(&mut task.android_display_extent, *width, *height)?;
            task.target_demand.configure(*serial);
        }
        if let DenialMessage::BufferReleased { frame, buffer, .. } = message {
            let tracked = task
                .frame_targets
                .remove(frame)
                .ok_or(DenialSinkError::UnknownRenderTarget(*buffer))?;
            if tracked != *buffer {
                return Err(DenialSinkError::BufferMetadataMismatch(*buffer));
            }
            let remove = {
                let target = task
                    .render_targets
                    .get_mut(buffer)
                    .ok_or(DenialSinkError::UnknownRenderTarget(*buffer))?;
                target.busy = false;
                target.revoked
            };
            if remove {
                task.render_targets.remove(buffer);
            }
            if frame_trace::sampled(frame.0) {
                frame_trace::event(
                    "android_reusable",
                    object.0,
                    frame.0,
                    buffer.0,
                    &[
                        ("receive_ns", received),
                        ("reusable_ns", frame_trace::now_ns()),
                    ],
                );
                if let Some(fence) = task.trace_fences.remove(frame) {
                    match frame_trace::fence_signal_ns(fence.as_fd()) {
                        Ok(Some(signal)) => frame_trace::event(
                            "android_gpu_complete",
                            object.0,
                            frame.0,
                            buffer.0,
                            &[("signal_ns", signal)],
                        ),
                        _ => frame_trace::event(
                            "android_gpu_timestamp_unavailable",
                            object.0,
                            frame.0,
                            buffer.0,
                            &[],
                        ),
                    }
                }
            }
        }
        Ok(event)
    }

    /// Cache one Denial-owned render target and its received DMA-BUF planes.
    pub fn register_render_target(
        &self,
        object: TaskObjectId,
        configure_serial: u32,
        buffer: BufferMetadata,
        descriptors: Vec<AttachedDescriptor>,
    ) -> Result<(), DenialSinkError> {
        let planes = descriptors
            .into_iter()
            .map(|descriptor| {
                if descriptor.kind == DescriptorKind::DmabufPlane {
                    Ok(descriptor.fd)
                } else {
                    Err(DenialSinkError::PlaneDescriptorMismatch)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        if planes.len() != buffer.planes.len() {
            return Err(DenialSinkError::PlaneDescriptorMismatch);
        }
        let mut tasks = self.lock_tasks()?;
        let task = tasks
            .values_mut()
            .find(|task| task.binding.object() == object)
            .ok_or(DenialSinkError::UnknownObject(object))?;
        if task.render_targets.contains_key(&buffer.id) {
            return Err(DenialSinkError::DuplicateRenderTarget(buffer.id));
        }
        task.render_targets.insert(
            buffer.id,
            HostRenderTarget {
                configure_serial,
                metadata: buffer,
                plane_fds: planes,
                reserved: false,
                busy: false,
                revoked: false,
            },
        );
        Ok(())
    }

    /// Drop one Denial-owned render target after the host revokes it.
    pub fn unregister_render_target(
        &self,
        object: TaskObjectId,
        buffer: BufferId,
    ) -> Result<(), DenialSinkError> {
        let mut tasks = self.lock_tasks()?;
        let task = tasks
            .values_mut()
            .find(|task| task.binding.object() == object)
            .ok_or(DenialSinkError::UnknownObject(object))?;
        let target = task
            .render_targets
            .get_mut(&buffer)
            .ok_or(DenialSinkError::UnknownRenderTarget(buffer))?;
        if target.reserved || target.busy {
            // A resize retires the previous target generation immediately,
            // but Denial can race a revoke against a target SurfaceFlinger is
            // rendering or whose frame is still displayed. Stop selecting it
            // now and drop the cached import after cancellation/release.
            target.revoked = true;
            return Ok(());
        }
        task.render_targets.remove(&buffer);
        Ok(())
    }

    /// Reserve an idle Denial-owned target for SurfaceFlinger's RenderEngine.
    ///
    /// # Errors
    ///
    /// Rejects an unknown display or a pool with no currently idle target.
    pub fn acquire_surfaceflinger_target(
        &self,
        display: DisplayId,
    ) -> Result<ImportedRenderTarget, DenialSinkError> {
        let mut tasks = self.lock_tasks()?;
        let task = tasks
            .get_mut(&display)
            .ok_or(DenialSinkError::UnknownDisplay(display))?;
        if task.binding.destroy_requested() {
            return Err(DenialSinkError::NoRenderTarget(display));
        }
        self.ensure_render_targets(task)?;
        let newest_serial = task
            .target_demand
            .serial()
            .ok_or(DenialSinkError::NoRenderTarget(display))?;
        let target = task
            .render_targets
            .values_mut()
            .find(|target| {
                target.configure_serial == newest_serial
                    && !target.reserved
                    && !target.busy
                    && !target.revoked
            })
            .ok_or(DenialSinkError::NoRenderTarget(display))?;
        let plane_fds = target
            .plane_fds
            .iter()
            .map(|fd| {
                fd.try_clone().map_err(|error| {
                    DenialSinkError::Graphics(format!(
                        "duplicate SurfaceFlinger target plane: {error}"
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        target.reserved = true;
        Ok(ImportedRenderTarget {
            metadata: target.metadata.clone(),
            configure_serial: target.configure_serial,
            plane_fds,
        })
    }

    /// Return a target reservation that SurfaceFlinger did not submit.
    pub fn cancel_surfaceflinger_target(&self, display: DisplayId, target: ImportedRenderTarget) {
        self.cancel_reserved_target(display, target.metadata.id);
    }

    /// Preserve the real GPU completion fence for a bufferless bootstrap frame.
    /// No render target, host present, or task timeline point is consumed.
    ///
    /// # Errors
    /// Rejects unknown or retiring displays and propagates sync-file creation
    /// or duplication errors. The broker restricts this operation to task zero.
    pub fn complete_headless_frame(
        &self,
        display: DisplayId,
        ready: Option<std::os::fd::OwnedFd>,
    ) -> Result<(), DenialSinkError> {
        let mut tasks = self.lock_tasks()?;
        let task = tasks
            .get_mut(&display)
            .ok_or(DenialSinkError::NoRenderTarget(display))?;
        if task.binding.destroy_requested() {
            return Err(DenialSinkError::NoRenderTarget(display));
        }
        let fence = crate::headless_fence::ready_fence(ready, || {
            // NO_FENCE means task work is already complete. Reuse the DRM
            // objects, but signal/export a fresh point for each frame: reusing
            // one signalled sync-file would reuse its old completion timestamp.
            if task.headless_completion.is_none() {
                task.headless_completion = Some((
                    AndroidTaskTimelines::create(&self.device).map_err(std::io::Error::other)?,
                    0_u64,
                ));
            }
            let (timelines, point) = task.headless_completion.as_mut().unwrap();
            let next = point
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("headless completion timeline exhausted"))?;
            timelines
                .signal_acquire(next)
                .map_err(std::io::Error::other)?;
            *point = next; // A failed export must not reuse the signalled point.
            timelines
                .export_acquire_fence(next)
                .map_err(std::io::Error::other)
        })
        .map_err(|error| DenialSinkError::Graphics(format!("headless present fence: {error}")))?;
        task.surfaceflinger_present_fence = Some(fence);
        Ok(())
    }

    /// Import SurfaceFlinger's ready fence and present its already-composed
    /// display target to Denial without another graphics pass.
    ///
    /// # Errors
    ///
    /// Preserves the configure, target ownership, timeline, and release
    /// invariants used by the original HWC composition path.
    pub fn submit_surfaceflinger_target(
        &self,
        session: &mut Session,
        display: DisplayId,
        target: ImportedRenderTarget,
        acquire_fence: Option<std::os::fd::OwnedFd>,
        opaque: bool,
        damage: Option<droidloom_transport::Damage>,
    ) -> Result<droidloom_transport::FrameId, DenialSinkError> {
        // Legacy or unknown damage remains full. The transport validates
        // bounds before accepting the target; empty damage remains empty.
        let damage = damage.unwrap_or(droidloom_transport::Damage {
            x: 0,
            y: 0,
            width: target.metadata.width,
            height: target.metadata.height,
        });
        let damage = if damage.width == 0 && damage.height == 0 {
            Vec::new()
        } else {
            vec![damage]
        };
        let frame = match session
            .windows_mut()
            .composer_for_display_mut(display)
            .map_err(|error| DenialSinkError::Graphics(error.to_string()))?
            .submit_rendered_target(
                display,
                ClientTarget {
                    buffer: target.metadata.clone(),
                    damage,
                    has_acquire_fence: true,
                },
            ) {
            Ok(frame) => frame,
            Err(error) => {
                self.cancel_reserved_target(display, target.metadata.id);
                return Err(error.into());
            }
        };

        if let Err(error) =
            self.send_surfaceflinger_frame(display, &frame, &target, acquire_fence, opaque)
        {
            if let Ok(composer) = session.windows_mut().composer_for_display_mut(display) {
                let _ = composer.cancel_unsubmitted_target(frame.frame_id);
            }
            self.cancel_reserved_target(display, target.metadata.id);
            return Err(error);
        }
        Ok(frame.frame_id)
    }

    /// Delegate an authenticated pair of sockets to the task's presenter.
    pub fn bind_layer_stream(
        &self,
        display: DisplayId,
        sockets: &[std::os::fd::BorrowedFd<'_>],
    ) -> Result<(), DenialSinkError> {
        let tasks = self.lock_tasks()?;
        let task = tasks
            .get(&display)
            .ok_or(DenialSinkError::UnknownDisplay(display))?;
        if task.binding.destroy_requested() {
            return Err(DenialBridgeError::UnexpectedMessage.into());
        }
        self.socket.send_android(
            &AndroidMessage::BindLayerStream {
                object: task.binding.object(),
            },
            sockets,
        )?;
        Ok(())
    }

    /// Resolve a protocol task object to its dedicated Composer display.
    ///
    /// # Errors
    ///
    /// Rejects an unknown object or poisoned task map.
    pub fn display_for_object(&self, object: TaskObjectId) -> Result<DisplayId, DenialSinkError> {
        let tasks = self.lock_tasks()?;
        tasks
            .iter()
            .find_map(|(display, task)| (task.binding.object() == object).then_some(*display))
            .ok_or(DenialSinkError::UnknownObject(object))
    }

    fn submit_frame(
        &self,
        display: DisplayId,
        frame: AcceptedFrame,
        target: ImportedRenderTarget,
        mut layers: Vec<PreparedLayer>,
    ) -> Result<ParcelFileDescriptor, PresentationFailure> {
        if target.metadata != frame.buffer {
            self.cancel_reserved_target(display, target.metadata.id);
            return Err(PresentationFailure::unsubmitted(ErrorCode::BadParameter));
        }
        let display_extent = match self.android_display_extent(display) {
            Ok(extent) => extent,
            Err(error) => {
                self.cancel_reserved_target(display, target.metadata.id);
                return Err(PresentationFailure::unsubmitted(error.error_code()));
            }
        };
        let acquire_fence = match self.compose(&target, &mut layers, display_extent) {
            Ok(fence) => fence,
            Err(error) => {
                eprintln!("droidloom-composer: composition failed: {error}");
                self.cancel_reserved_target(display, target.metadata.id);
                return Err(PresentationFailure::unsubmitted(error.error_code()));
            }
        };
        // SurfaceFlinger's present fence protects the Android layer buffers
        // read by Droidloom's composition pass. Denial's independent release
        // event protects the composed, Denial-owned target from reuse; waiting
        // for that event here would deadlock Android presentation on the host
        // UI's next raster frame.
        let present_fence = acquire_fence.as_fd().try_clone_to_owned().map_err(|_| {
            self.cancel_reserved_target(display, target.metadata.id);
            PresentationFailure::unsubmitted(ErrorCode::NoResources)
        })?;
        let mut tasks = self
            .lock_tasks()
            .map_err(|error| PresentationFailure::unsubmitted(error.error_code()))?;
        let task = tasks
            .get_mut(&display)
            .ok_or_else(|| PresentationFailure::unsubmitted(ErrorCode::BadDisplay))?;
        let Some(host_target) = task.render_targets.get(&frame.buffer.id) else {
            return Err(PresentationFailure::unsubmitted(ErrorCode::BadParameter));
        };
        if !host_target.reserved
            || host_target.busy
            || host_target.revoked
            || host_target.metadata != frame.buffer
        {
            return Err(PresentationFailure::unsubmitted(ErrorCode::BadParameter));
        }

        let prepared = task
            .binding
            .prepare_present(display, &frame)
            .map_err(|error| {
                PresentationFailure::unsubmitted(DenialSinkError::from(error).error_code())
            })?;
        if let Err(error) = task
            .timelines
            .import_acquire_fence(prepared.acquire_point, acquire_fence.as_fd())
        {
            cancel_prepared(&mut task.binding, &frame, &prepared);
            if let Some(target) = task.render_targets.get_mut(&frame.buffer.id) {
                target.reserved = false;
            }
            return Err(PresentationFailure::unsubmitted(
                DenialSinkError::from(error).error_code(),
            ));
        }
        if let Err(error) = self.socket.send_android(&prepared.message, &[]) {
            cancel_prepared(&mut task.binding, &frame, &prepared);
            if let Some(target) = task.render_targets.get_mut(&frame.buffer.id) {
                target.reserved = false;
            }
            return Err(PresentationFailure::unsubmitted(
                DenialSinkError::from(error).error_code(),
            ));
        }

        let host_target = task
            .render_targets
            .get_mut(&frame.buffer.id)
            .expect("target was validated before present send");
        host_target.reserved = false;
        host_target.busy = true;
        task.frame_targets.insert(frame.frame_id, frame.buffer.id);

        Ok(ParcelFileDescriptor::new(present_fence))
    }

    fn send_surfaceflinger_frame(
        &self,
        display: DisplayId,
        frame: &AcceptedFrame,
        target: &ImportedRenderTarget,
        acquire_fence: Option<std::os::fd::OwnedFd>,
        opaque: bool,
    ) -> Result<(), DenialSinkError> {
        let tracing = frame_trace::sampled(frame.frame_id.0);
        let begin = if tracing { frame_trace::now_ns() } else { 0 };
        if target.metadata != frame.buffer || !frame.requires_acquire_fence {
            return Err(DenialSinkError::BufferMetadataMismatch(target.metadata.id));
        }
        let mut tasks = self.lock_tasks()?;
        let task = tasks
            .get_mut(&display)
            .ok_or(DenialSinkError::UnknownDisplay(display))?;
        // A target reserved before destruction may finish rendering afterward.
        // Reject it before sending any metadata to the already closed window.
        if task.binding.destroy_requested() {
            return Err(DenialBridgeError::UnexpectedMessage.into());
        }
        let host_target = task
            .render_targets
            .get(&frame.buffer.id)
            .ok_or(DenialSinkError::UnknownRenderTarget(frame.buffer.id))?;
        if !host_target.reserved
            || host_target.busy
            || host_target.revoked
            || host_target.metadata != frame.buffer
        {
            return Err(DenialSinkError::BufferMetadataMismatch(frame.buffer.id));
        }

        // Send metadata before this exact frame on the ordered transport. A
        // transparent successor must clear a preceding frame's opaque proof.
        if task.content_opaque != Some(opaque) {
            self.socket.send_android(
                &AndroidMessage::SetContentState {
                    object: task.binding.object(),
                    flags: if opaque {
                        droidloom_denial_protocol::content_state::OPAQUE
                    } else {
                        0
                    },
                },
                &[],
            )?;
            task.content_opaque = Some(opaque);
        }
        let prepared = task.binding.prepare_present(display, frame)?;
        let import_begin = if tracing { frame_trace::now_ns() } else { 0 };
        let import_result = if let Some(acquire_fence) = acquire_fence.as_ref() {
            task.timelines
                .import_acquire_fence(prepared.acquire_point, acquire_fence.as_fd())
        } else {
            task.timelines.signal_acquire(prepared.acquire_point)
        };
        if let Err(error) = import_result {
            cancel_prepared(&mut task.binding, frame, &prepared);
            return Err(error.into());
        }
        let present_fence = match task.timelines.export_acquire_fence(prepared.acquire_point) {
            Ok(fence) => fence,
            Err(error) => {
                cancel_prepared(&mut task.binding, frame, &prepared);
                return Err(error.into());
            }
        };
        let send_begin = if tracing { frame_trace::now_ns() } else { 0 };
        if let Err(error) = self.socket.send_android(&prepared.message, &[]) {
            cancel_prepared(&mut task.binding, frame, &prepared);
            return Err(error.into());
        }

        let host_target = task
            .render_targets
            .get_mut(&frame.buffer.id)
            .expect("target was validated before direct present");
        host_target.reserved = false;
        host_target.busy = true;
        task.frame_targets.insert(frame.frame_id, frame.buffer.id);
        task.surfaceflinger_present_fence = Some(present_fence);
        if tracing {
            // Keep the original render fence: exporting an already completed DRM
            // point may replace it with a timestamp-less/global placeholder.
            if let Some(fence) = acquire_fence.as_ref().and_then(|fd| fd.try_clone().ok()) {
                task.trace_fences.insert(frame.frame_id, fence);
            }
            frame_trace::event(
                "android_submit",
                task.binding.object().0,
                frame.frame_id.0,
                frame.buffer.id.0,
                &[
                    ("display", display.0),
                    ("begin_ns", begin),
                    ("import_begin_ns", import_begin),
                    ("send_begin_ns", send_begin),
                    ("send_done_ns", frame_trace::now_ns()),
                    ("acquire_point", prepared.acquire_point),
                ],
            );
        }
        Ok(())
    }

    fn compose(
        &self,
        target: &ImportedRenderTarget,
        layers: &mut [PreparedLayer],
        display_extent: (u32, u32),
    ) -> Result<std::os::fd::OwnedFd, DenialSinkError> {
        let mut compositor = self
            .compositor
            .lock()
            .map_err(|_| DenialSinkError::Poisoned)?;
        if compositor.is_none() {
            *compositor = Some(
                LayerCompositor::new(self.device.as_fd())
                    .map_err(|error| DenialSinkError::Graphics(error.to_string()))?,
            );
        }
        compositor
            .as_mut()
            .expect("initialized above")
            .compose(target, layers, display_extent)
            .map_err(|error| DenialSinkError::Graphics(error.to_string()))
    }

    fn android_display_extent(&self, display: DisplayId) -> Result<(u32, u32), DenialSinkError> {
        let tasks = self.lock_tasks()?;
        let task = tasks
            .get(&display)
            .ok_or(DenialSinkError::UnknownDisplay(display))?;
        task.android_display_extent.ok_or_else(|| {
            DenialSinkError::Graphics("Android display extent is not configured".to_owned())
        })
    }

    fn cancel_reserved_target(&self, display: DisplayId, buffer: BufferId) {
        if let Ok(mut tasks) = self.tasks.lock() {
            if let Some(task) = tasks.get_mut(&display) {
                let remove = if let Some(target) = task.render_targets.get_mut(&buffer) {
                    if !target.busy {
                        target.reserved = false;
                    }
                    !target.busy && !target.reserved && target.revoked
                } else {
                    false
                };
                if remove {
                    task.render_targets.remove(&buffer);
                }
            }
        }
    }

    fn lock_tasks(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<DisplayId, TaskChannel>>, DenialSinkError> {
        self.tasks.lock().map_err(|_| DenialSinkError::Poisoned)
    }

    // Called only from an actual composition dequeue, never from direct-layer
    // export. Busy targets already exist and must wait for real GPU release.
    fn ensure_render_targets(&self, task: &mut TaskChannel) -> Result<(), DenialSinkError> {
        let Some(serial) = task.target_demand.serial() else {
            return Ok(());
        };
        let has_pool = task
            .render_targets
            .values()
            .any(|target| !target.revoked && target.configure_serial == serial);
        if let Some(serial) = task
            .target_demand
            .request(self.lazy_render_targets, has_pool)
        {
            self.socket.send_android(
                &AndroidMessage::RequestRenderTargets {
                    object: task.binding.object(),
                    configure_serial: serial,
                },
                &[],
            )?;
            task.target_demand.sent(serial);
        }
        Ok(())
    }
}

impl PresentationSink for DenialPresentationSink {
    fn acquire_target(&mut self, display: DisplayId) -> Result<ImportedRenderTarget, ErrorCode> {
        let mut tasks = self.lock_tasks().map_err(|error| error.error_code())?;
        let task = tasks.get_mut(&display).ok_or(ErrorCode::BadDisplay)?;
        if task.binding.destroy_requested() {
            return Err(ErrorCode::NoResources);
        }
        self.ensure_render_targets(task)
            .map_err(|error| error.error_code())?;
        let newest_serial = task.target_demand.serial().ok_or(ErrorCode::NoResources)?;
        let target = task
            .render_targets
            .values_mut()
            .find(|target| {
                target.configure_serial == newest_serial
                    && !target.reserved
                    && !target.busy
                    && !target.revoked
            })
            .ok_or(ErrorCode::NoResources)?;
        let plane_fds = target
            .plane_fds
            .iter()
            .map(|fd| fd.try_clone().map_err(|_| ErrorCode::NoResources))
            .collect::<Result<Vec<_>, _>>()?;
        target.reserved = true;
        Ok(ImportedRenderTarget {
            metadata: target.metadata.clone(),
            configure_serial: target.configure_serial,
            plane_fds,
        })
    }

    fn cancel_target(&mut self, display: DisplayId, target: ImportedRenderTarget) {
        self.cancel_reserved_target(display, target.metadata.id);
    }

    fn take_direct_present_fence(
        &mut self,
        display: DisplayId,
    ) -> Result<ParcelFileDescriptor, ErrorCode> {
        let mut tasks = self.lock_tasks().map_err(|error| error.error_code())?;
        let task = tasks.get_mut(&display).ok_or(ErrorCode::BadDisplay)?;
        task.surfaceflinger_present_fence
            .take()
            .map(ParcelFileDescriptor::new)
            .ok_or(ErrorCode::NoResources)
    }

    fn submit(
        &mut self,
        display: DisplayId,
        frame: AcceptedFrame,
        target: ImportedRenderTarget,
        layers: Vec<PreparedLayer>,
    ) -> Result<ParcelFileDescriptor, PresentationFailure> {
        self.submit_frame(display, frame, target, layers)
    }
}

#[derive(Debug)]
struct TaskChannel {
    binding: DenialTaskBinding,
    timelines: AndroidTaskTimelines,
    render_targets: BTreeMap<BufferId, HostRenderTarget>,
    frame_targets: BTreeMap<droidloom_transport::FrameId, BufferId>,
    /// Sampled original render fences, bounded by the existing in-flight pool.
    trace_fences: BTreeMap<droidloom_transport::FrameId, std::os::fd::OwnedFd>,
    /// Last opaque-coverage proof sent to the host for this task.
    content_opaque: Option<bool>,
    /// Last keep-screen-on state sent to the host for this task.
    keep_screen_on: bool,
    /// SurfaceFlinger's GPU completion fence awaiting Composer3 present reply.
    surfaceflinger_present_fence: Option<std::os::fd::OwnedFd>,
    /// Dedicated completion timeline and last point for frames without GPU work.
    headless_completion: Option<(AndroidTaskTimelines, u64)>,
    /// SurfaceFlinger display space pinned by the first hotplug configure.
    /// Denial may resize or scale the target independently, while Android
    /// layer frames remain in this stable coordinate space.
    android_display_extent: Option<(u32, u32)>,
    /// Latest geometry generation and its one outstanding allocation request.
    target_demand: RenderTargetDemand,
}

#[derive(Debug)]
struct HostRenderTarget {
    configure_serial: u32,
    metadata: BufferMetadata,
    plane_fds: Vec<std::os::fd::OwnedFd>,
    reserved: bool,
    busy: bool,
    /// Denial retired this generation while a render or displayed frame still
    /// owned it. The cache entry is removed at the corresponding safe point.
    revoked: bool,
}

fn cancel_prepared(
    binding: &mut DenialTaskBinding,
    frame: &AcceptedFrame,
    prepared: &PreparedPresent,
) {
    let _ = binding.cancel_unsubmitted_present(frame.frame_id, prepared.release_point);
}

fn task_object(message: &DenialMessage) -> Option<TaskObjectId> {
    match message {
        DenialMessage::FormatFeedback { object, .. }
        | DenialMessage::RegisterRenderTarget { object, .. }
        | DenialMessage::UnregisterRenderTarget { object, .. }
        | DenialMessage::Configure { object, .. }
        | DenialMessage::Insets { object, .. }
        | DenialMessage::Density { object, .. }
        | DenialMessage::Visibility { object, .. }
        | DenialMessage::Close { object }
        | DenialMessage::BufferReleased { object, .. }
        | DenialMessage::Presented { object, .. }
        | DenialMessage::ScenePresented { object, .. }
        | DenialMessage::Input { object, .. } => Some(*object),
        DenialMessage::Error { object, .. } => *object,
        DenialMessage::ServerHello { .. }
        | DenialMessage::RequestInputChannel { .. }
        | DenialMessage::DisplayPower { .. }
        | DenialMessage::Ping { .. }
        | DenialMessage::FrameGrant { .. } => None,
    }
}

fn pin_android_display_extent(
    extent: &mut Option<(u32, u32)>,
    width: u32,
    height: u32,
) -> Result<(), DenialSinkError> {
    if width == 0 || height == 0 {
        return Err(DenialSinkError::Graphics(
            "invalid Android display extent".to_owned(),
        ));
    }
    extent.get_or_insert((width, height));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::pin_android_display_extent;

    #[test]
    fn android_display_extent_remains_the_hotplug_size() {
        let mut extent = None;
        pin_android_display_extent(&mut extent, 480, 800).expect("initial configure");
        pin_android_display_extent(&mut extent, 1264, 2684).expect("host-scale configure");
        assert_eq!(extent, Some((480, 800)));
    }

    #[test]
    fn android_display_extent_rejects_empty_configures() {
        let mut extent = None;
        assert!(pin_android_display_extent(&mut extent, 0, 800).is_err());
        assert_eq!(extent, None);
    }
}
