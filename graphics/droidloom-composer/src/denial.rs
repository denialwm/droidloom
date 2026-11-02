//! Typed bridge between Composer task displays and the Denial-native protocol.

use std::collections::BTreeMap;

use droidloom_denial_protocol::{
    AndroidDisplayId, AndroidMessage, AndroidTaskId, DenialMessage, InputEvent, TaskObjectId,
    Transform, Visibility,
};
use droidloom_transport::{AcceptedFrame, Configure, ReleasedFrame};
use thiserror::Error;

use crate::{
    ComposerError, DisplayId, ReservedTaskWindowSpec, TaskId, TaskWindowError, VsyncSample,
    hwc3::Session,
};

/// Coalesce fallback allocation requests across retries and geometry changes.
#[derive(Debug, Default)]
pub struct RenderTargetDemand {
    serial: Option<u32>,
    requested: Option<u32>,
}

impl RenderTargetDemand {
    /// Observe geometry; refresh-only updates keep the outstanding request.
    pub fn configure(&mut self, serial: u32) {
        if self.serial != Some(serial) {
            self.serial = Some(serial);
            self.requested = None;
        }
    }

    /// Current target generation, independently of older in-flight buffers.
    pub fn serial(&self) -> Option<u32> {
        self.serial
    }

    /// Request a missing pool; a busy existing pool must wait for GPU release.
    pub fn request(&self, enabled: bool, has_pool: bool) -> Option<u32> {
        self.serial
            .filter(|serial| enabled && !has_pool && self.requested != Some(*serial))
    }

    /// Record only a successfully sent request; transport failure is retryable.
    pub fn sent(&mut self, serial: u32) {
        if self.serial == Some(serial) {
            self.requested = Some(serial);
        }
    }
}

/// Android-side binding of one Composer logical display to one Denial task.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DenialTaskBinding {
    object: TaskObjectId,
    task: Option<TaskId>,
    display: DisplayId,
    package: String,
    acknowledged_configure: Option<u32>,
    next_timeline_point: u64,
    pending_release_points: BTreeMap<droidloom_transport::FrameId, u64>,
    destroy_requested: bool,
}

/// One client-target present paired with its preallocated timeline points.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedPresent {
    /// Descriptor-free wire message to send after importing the acquire fence.
    pub message: AndroidMessage,
    /// Point into which the adapter imports `SurfaceFlinger`'s acquire `sync_file`.
    pub acquire_point: u64,
    /// Point the adapter exports as `SurfaceFlinger`'s present fence after
    /// Denial materializes it with renderer/KMS completion.
    pub release_point: u64,
}

/// Host event applied to Composer state or forwarded to Android integration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppliedDenialEvent {
    /// Authoritative format/modifier feedback updated Composer validation.
    FormatFeedback {
        /// Monotonic feedback generation applied to Composer.
        generation: u32,
    },
    /// A new configure awaits Android display reconfiguration and ack.
    Configure {
        /// Configure serial.
        serial: u32,
        /// Required pixel width.
        width: u32,
        /// Required pixel height.
        height: u32,
        /// Logical-scale numerator.
        scale_numerator: u32,
        /// Logical-scale denominator.
        scale_denominator: u32,
        /// Denial output transform.
        transform: Transform,
        /// Host refresh policy in millihertz.
        refresh_millihz: u32,
    },
    /// The task's density changed with the output it is on.
    Density {
        /// Dots per inch; 0 follows Android's display density.
        dpi: u32,
    },
    /// Denial-owned system-bar and cutout insets changed.
    Insets {
        /// Left logical inset.
        left: u32,
        /// Top logical inset.
        top: u32,
        /// Right logical inset.
        right: u32,
        /// Bottom logical inset.
        bottom: u32,
    },
    /// Denial changed task visibility or focus.
    Visibility {
        /// Current visibility.
        visibility: Visibility,
        /// Keyboard focus state.
        focused: bool,
    },
    /// Denial requested Android task finish/close.
    Close,
    /// A release point was signalled and Composer retired the frame.
    BufferReleased(ReleasedFrame),
    /// Presentation timing optionally produced an enabled HWC vsync sample.
    Presented(Option<VsyncSample>),
    /// Typed input must be delivered through the Android input bridge.
    Input {
        /// Host input sequence.
        serial: u64,
        /// Monotonic event timestamp.
        timestamp_nanos: u64,
        /// Typed input event.
        event: InputEvent,
    },
}

/// Invalid mapping between a Denial task object and Composer state.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DenialBridgeError {
    /// A message addressed a different Denial task object.
    #[error("message addressed {actual:?}; binding owns {expected:?}")]
    WrongObject {
        /// Bound object.
        expected: TaskObjectId,
        /// Received object.
        actual: TaskObjectId,
    },
    /// A connection-level or unsupported event reached a task binding.
    #[error("message is not a task event handled by the Composer bridge")]
    UnexpectedMessage,
    /// A released buffer did not match Composer's in-flight frame.
    #[error("Denial release buffer does not match Composer frame ownership")]
    ReleaseMismatch,
    /// Timeline point space was exhausted.
    #[error("DRM syncobj timeline point space exhausted")]
    TimelineExhausted,
    /// Task/display lifecycle state is invalid.
    #[error(transparent)]
    Task(#[from] TaskWindowError),
    /// Per-display Composer state rejected the event.
    #[error(transparent)]
    Composer(#[from] ComposerError),
}

impl DenialTaskBinding {
    /// Bind an existing task/display mapping to a non-zero protocol object.
    ///
    /// # Errors
    ///
    /// Rejects protocol object zero.
    pub fn new(
        object: TaskObjectId,
        reservation: &ReservedTaskWindowSpec,
    ) -> Result<Self, DenialBridgeError> {
        if object.0 == 0 {
            return Err(DenialBridgeError::UnexpectedMessage);
        }
        Ok(Self {
            object,
            task: None,
            display: reservation.display,
            package: reservation.package.clone(),
            acknowledged_configure: None,
            next_timeline_point: 0,
            pending_release_points: BTreeMap::new(),
            destroy_requested: false,
        })
    }

    /// Return the independent Denial task object.
    pub fn object(&self) -> TaskObjectId {
        self.object
    }

    /// Send destruction once, retaining this binding until all releases drain.
    /// A failed send leaves the request retryable.
    ///
    /// # Errors
    /// Propagates the sender's transport error.
    pub fn request_destroy<E>(
        &mut self,
        send: impl FnOnce(AndroidMessage) -> Result<(), E>,
    ) -> Result<(), E> {
        if !self.destroy_requested {
            send(AndroidMessage::DestroyTask {
                object: self.object,
            })?;
            self.destroy_requested = true;
        }
        Ok(())
    }

    /// Destruction forbids new frames while existing releases remain routable.
    pub fn destroy_requested(&self) -> bool {
        self.destroy_requested
    }

    /// Return the dedicated Android logical display.
    pub fn display(&self) -> DisplayId {
        self.display
    }

    /// Return the real Android task identity once the launch controller bound it.
    pub fn task(&self) -> Option<TaskId> {
        self.task
    }

    /// Build the task-creation request from the trusted local mapping.
    pub fn create_task_message(&self) -> AndroidMessage {
        AndroidMessage::CreateTask {
            object: self.object,
            display: AndroidDisplayId(self.display.0),
            package: self.package.clone(),
        }
    }

    /// Bind the real `ActivityTaskManager` identity after the display is live.
    ///
    /// # Errors
    ///
    /// Rejects zero or repeated task bindings.
    pub fn bind_task_message(&self, task: TaskId) -> Result<AndroidMessage, DenialBridgeError> {
        if task.0 == 0 || self.task.is_some() {
            return Err(DenialBridgeError::UnexpectedMessage);
        }
        Ok(AndroidMessage::BindTask {
            object: self.object,
            task: AndroidTaskId(task.0),
        })
    }

    /// Commit a task binding only after its wire record was sent.
    ///
    /// # Errors
    ///
    /// Rejects zero or repeated task bindings.
    pub fn mark_task_bound(&mut self, task: TaskId) -> Result<(), DenialBridgeError> {
        if task.0 == 0 || self.task.is_some() {
            return Err(DenialBridgeError::UnexpectedMessage);
        }
        self.task = Some(task);
        Ok(())
    }

    /// Build the one-time task timeline binding request.
    pub fn bind_timelines_message(&self) -> AndroidMessage {
        AndroidMessage::BindTimelines {
            object: self.object,
        }
    }

    /// Build a one-time cached DMA-BUF registration request.
    pub fn register_buffer_message(&self, frame: &AcceptedFrame) -> AndroidMessage {
        AndroidMessage::RegisterBuffer {
            object: self.object,
            buffer: frame.buffer.clone(),
        }
    }

    /// Acknowledge Composer's latest configure and build its wire request.
    ///
    /// # Errors
    ///
    /// Rejects an unknown display or stale serial without emitting a message.
    pub fn acknowledge_configure(
        &mut self,
        session: &mut Session,
        serial: u32,
    ) -> Result<AndroidMessage, DenialBridgeError> {
        session
            .windows_mut()
            .composer_for_display_mut(self.display)?
            .acknowledge_configure(serial)?;
        self.acknowledged_configure = Some(serial);
        Ok(AndroidMessage::AckConfigure {
            object: self.object,
            serial,
        })
    }

    /// Allocate monotonic points and build one descriptor-free present.
    ///
    /// The platform adapter imports the real acquire fence before sending
    /// `message`. After sending, it waits only for release-point availability
    /// and exports the still-unsignalled point. Skipping point values after a
    /// failed syscall is valid; point reuse is not.
    ///
    /// # Errors
    ///
    /// Rejects a missing configure, wrong task display, or exhausted point ID.
    pub fn prepare_present(
        &mut self,
        display: DisplayId,
        frame: &AcceptedFrame,
    ) -> Result<PreparedPresent, DenialBridgeError> {
        if self.destroy_requested || display != self.display {
            return Err(DenialBridgeError::UnexpectedMessage);
        }
        let configure_serial = self
            .acknowledged_configure
            .ok_or(DenialBridgeError::UnexpectedMessage)?;
        let point = self
            .next_timeline_point
            .checked_add(1)
            .ok_or(DenialBridgeError::TimelineExhausted)?;
        if self.pending_release_points.contains_key(&frame.frame_id) {
            return Err(DenialBridgeError::UnexpectedMessage);
        }
        self.next_timeline_point = point;
        self.pending_release_points.insert(frame.frame_id, point);
        Ok(PreparedPresent {
            message: AndroidMessage::Present {
                object: self.object,
                frame: frame.frame_id,
                buffer: frame.buffer.id,
                configure_serial,
                acquire_point: point,
                release_point: point,
                damage: frame.damage.clone(),
            },
            acquire_point: point,
            release_point: point,
        })
    }

    /// Forget a prepared frame after a failure known to precede a successful
    /// `Present` send. The timeline point remains consumed and is never reused.
    ///
    /// # Errors
    ///
    /// Rejects an unknown frame or mismatched release point.
    pub fn cancel_unsubmitted_present(
        &mut self,
        frame: droidloom_transport::FrameId,
        release_point: u64,
    ) -> Result<(), DenialBridgeError> {
        if self.pending_release_points.get(&frame) != Some(&release_point) {
            return Err(DenialBridgeError::ReleaseMismatch);
        }
        self.pending_release_points.remove(&frame);
        Ok(())
    }

    /// Apply one task-scoped Denial event to Composer state.
    ///
    /// Lifecycle, inset, and input events are returned for the Android session
    /// service. Feedback, configure, release, and timing also update Composer.
    ///
    /// # Errors
    ///
    /// Rejects wrong-object, connection-level, stale release, and invalid
    /// Composer state without silently crossing task boundaries.
    #[allow(
        clippy::too_many_lines,
        reason = "the match is the complete v1 task-event dispatch table"
    )]
    pub fn apply_event(
        &mut self,
        session: &mut Session,
        message: &DenialMessage,
    ) -> Result<AppliedDenialEvent, DenialBridgeError> {
        let object = message_object(message).ok_or(DenialBridgeError::UnexpectedMessage)?;
        if object != self.object {
            return Err(DenialBridgeError::WrongObject {
                expected: self.object,
                actual: object,
            });
        }
        match message {
            DenialMessage::FormatFeedback {
                generation,
                formats,
                ..
            } => {
                session
                    .windows_mut()
                    .composer_for_display_mut(self.display)?
                    .replace_feedback(formats.iter().copied());
                Ok(AppliedDenialEvent::FormatFeedback {
                    generation: *generation,
                })
            }
            DenialMessage::Configure {
                serial,
                width,
                height,
                scale_numerator,
                scale_denominator,
                transform,
                refresh_millihz,
                ..
            } => {
                session
                    .windows_mut()
                    .composer_for_display_mut(self.display)?
                    .configure(Configure {
                        serial: *serial,
                        width: *width,
                        height: *height,
                        refresh_millihz: *refresh_millihz,
                    });
                self.acknowledged_configure = None;
                Ok(AppliedDenialEvent::Configure {
                    serial: *serial,
                    width: *width,
                    height: *height,
                    scale_numerator: *scale_numerator,
                    scale_denominator: *scale_denominator,
                    transform: *transform,
                    refresh_millihz: *refresh_millihz,
                })
            }
            DenialMessage::Insets {
                left,
                top,
                right,
                bottom,
                ..
            } => Ok(AppliedDenialEvent::Insets {
                left: *left,
                top: *top,
                right: *right,
                bottom: *bottom,
            }),
            DenialMessage::Density { dpi, .. } => Ok(AppliedDenialEvent::Density { dpi: *dpi }),
            DenialMessage::Visibility {
                visibility,
                focused,
                ..
            } => Ok(AppliedDenialEvent::Visibility {
                visibility: *visibility,
                focused: *focused,
            }),
            DenialMessage::Close { .. } => Ok(AppliedDenialEvent::Close),
            DenialMessage::BufferReleased {
                frame,
                buffer,
                release_point,
                ..
            } => {
                if self.pending_release_points.get(frame) != Some(release_point) {
                    return Err(DenialBridgeError::ReleaseMismatch);
                }
                let composer = session
                    .windows_mut()
                    .composer_for_display_mut(self.display)?;
                if composer.in_flight_buffer(*frame) != Some(*buffer) {
                    return Err(DenialBridgeError::ReleaseMismatch);
                }
                let released = composer.release_presented_target(*frame, true)?;
                self.pending_release_points.remove(frame);
                Ok(AppliedDenialEvent::BufferReleased(released))
            }
            DenialMessage::Presented {
                timestamp_nanos, ..
            }
            | DenialMessage::ScenePresented {
                timestamp_nanos, ..
            } => {
                let sample = session
                    .windows_mut()
                    .composer_for_display_mut(self.display)?
                    .presentation_sample(*timestamp_nanos)?;
                Ok(AppliedDenialEvent::Presented(sample))
            }
            DenialMessage::Input {
                serial,
                timestamp_nanos,
                event,
                ..
            } => Ok(AppliedDenialEvent::Input {
                serial: *serial,
                timestamp_nanos: *timestamp_nanos,
                event: *event,
            }),
            DenialMessage::ServerHello { .. }
            | DenialMessage::RequestInputChannel { .. }
            | DenialMessage::DisplayPower { .. }
            | DenialMessage::Error { .. }
            | DenialMessage::Ping { .. }
            | DenialMessage::FrameGrant { .. }
            | DenialMessage::RegisterRenderTarget { .. }
            | DenialMessage::UnregisterRenderTarget { .. } => {
                Err(DenialBridgeError::UnexpectedMessage)
            }
        }
    }
}

fn message_object(message: &DenialMessage) -> Option<TaskObjectId> {
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

#[cfg(test)]
mod tests {
    use droidloom_denial_protocol::{
        AndroidMessage, DenialMessage, TaskObjectId, Transform, presentation_flag,
    };
    use droidloom_transport::{BufferId, BufferMetadata, Damage, FormatModifier, PlaneMetadata};

    use super::*;

    #[test]
    fn direct_scene_feedback_samples_timing_without_releasing_targets() {
        let (mut session, mut binding) = setup();
        configure(&mut session, &mut binding);
        let composer = session
            .windows_mut()
            .composer_for_display_mut(DisplayId(9))
            .unwrap();
        composer
            .set_power_mode(DisplayId(9), PowerMode::On)
            .unwrap();
        composer.set_vsync_enabled(DisplayId(9), true).unwrap();
        let message = DenialMessage::ScenePresented {
            object: TaskObjectId(7),
            scene: 99,
            timestamp_nanos: 1_000_000_000,
            refresh_period_nanos: 16_666_666,
            sequence: 5,
        };
        assert!(matches!(
            binding.apply_event(&mut session, &message).unwrap(),
            AppliedDenialEvent::Presented(Some(_))
        ));
        assert_eq!(
            binding.apply_event(&mut session, &message).unwrap(),
            AppliedDenialEvent::Presented(None)
        );
        assert_eq!(
            session
                .windows()
                .composer_for_display(DisplayId(9))
                .unwrap()
                .in_flight_count(),
            0
        );
        let mut wrong_object = message;
        if let DenialMessage::ScenePresented { object, .. } = &mut wrong_object {
            *object = TaskObjectId(8);
        }
        assert!(matches!(
            binding.apply_event(&mut session, &wrong_object),
            Err(DenialBridgeError::WrongObject { .. })
        ));
    }

    #[test]
    fn target_demand_is_bounded_by_configuration_and_not_gpu_busyness() {
        let mut demand = RenderTargetDemand::default();
        assert_eq!(demand.request(true, false), None);
        demand.configure(3);
        assert_eq!(demand.request(false, false), None); // Legacy host stays eager.
        assert_eq!(demand.request(true, false), Some(3));
        assert_eq!(demand.request(true, false), Some(3)); // Failed send can retry.
        demand.sent(3);
        for _ in 0..1000 {
            demand.configure(3); // A cadence update is not a new allocation.
            assert_eq!(demand.request(true, false), None);
        }
        demand.configure(4);
        demand.sent(3); // Late completion cannot suppress the new generation.
        assert_eq!(demand.request(true, false), Some(4));
        assert_eq!(demand.request(true, true), None); // Even if every target is busy.
        demand.sent(4);
        assert_eq!(demand.request(true, false), None);
    }
    use crate::{
        ClientTarget, Composition, LayerId, LayerState, PowerMode, ReservedTaskWindowSpec,
        TaskWindowSpec,
    };

    const FORMAT: FormatModifier = FormatModifier {
        fourcc: u32::from_le_bytes(*b"XR24"),
        modifier: 0,
    };

    fn setup() -> (Session, DenialTaskBinding) {
        let spec = TaskWindowSpec {
            task: TaskId(42),
            display: DisplayId(9),
            package: "org.example.app".to_owned(),
        };
        let mut session = Session::default();
        session.create_task_window(spec.clone()).unwrap();
        let mut binding = DenialTaskBinding::new(
            TaskObjectId(7),
            &ReservedTaskWindowSpec {
                display: spec.display,
                package: spec.package.clone(),
            },
        )
        .unwrap();
        assert_eq!(
            binding.create_task_message(),
            AndroidMessage::CreateTask {
                object: TaskObjectId(7),
                display: AndroidDisplayId(9),
                package: "org.example.app".to_owned(),
            }
        );
        assert_eq!(
            binding.bind_task_message(TaskId(42)).unwrap(),
            AndroidMessage::BindTask {
                object: TaskObjectId(7),
                task: AndroidTaskId(42),
            }
        );
        binding.mark_task_bound(TaskId(42)).unwrap();
        (session, binding)
    }

    fn configure(session: &mut Session, binding: &mut DenialTaskBinding) {
        binding
            .apply_event(
                session,
                &DenialMessage::FormatFeedback {
                    object: TaskObjectId(7),
                    generation: 1,
                    formats: vec![FORMAT],
                },
            )
            .unwrap();
        binding
            .apply_event(
                session,
                &DenialMessage::Configure {
                    object: TaskObjectId(7),
                    serial: 5,
                    width: 800,
                    height: 600,
                    scale_numerator: 1,
                    scale_denominator: 1,
                    transform: Transform::Normal,
                    refresh_millihz: 60_000,
                },
            )
            .unwrap();
        assert_eq!(
            binding.acknowledge_configure(session, 5).unwrap(),
            AndroidMessage::AckConfigure {
                object: TaskObjectId(7),
                serial: 5,
            }
        );
    }

    #[test]
    fn host_events_update_only_the_bound_task_display() {
        let (mut session, mut binding) = setup();
        configure(&mut session, &mut binding);
        assert_eq!(
            binding.apply_event(
                &mut session,
                &DenialMessage::Visibility {
                    object: TaskObjectId(8),
                    visibility: Visibility::Visible,
                    focused: true,
                },
            ),
            Err(DenialBridgeError::WrongObject {
                expected: TaskObjectId(7),
                actual: TaskObjectId(8),
            })
        );
        assert_eq!(
            session
                .windows()
                .composer_for_display(DisplayId(9))
                .unwrap()
                .latest_configure(),
            Some(Configure {
                serial: 5,
                width: 800,
                height: 600,
                refresh_millihz: 60_000,
            })
        );
    }

    #[test]
    fn accepted_composer_frame_becomes_one_descriptor_free_present() {
        let (mut session, mut binding) = setup();
        configure(&mut session, &mut binding);
        let composer = session
            .windows_mut()
            .composer_for_display_mut(DisplayId(9))
            .unwrap();
        composer
            .set_power_mode(DisplayId(9), PowerMode::On)
            .unwrap();
        composer
            .create_layer(
                DisplayId(9),
                LayerState {
                    id: LayerId(1),
                    requested_composition: Composition::Client,
                    z: 0,
                    protected_content: false,
                },
            )
            .unwrap();
        composer.validate_display(DisplayId(9)).unwrap();
        composer.accept_display_changes(DisplayId(9)).unwrap();
        composer
            .set_client_target(
                DisplayId(9),
                ClientTarget {
                    buffer: BufferMetadata {
                        id: BufferId(11),
                        width: 800,
                        height: 600,
                        format: FORMAT,
                        planes: vec![PlaneMetadata {
                            index: 0,
                            offset: 0,
                            stride: 3_200,
                        }],
                    },
                    damage: vec![Damage {
                        x: 0,
                        y: 0,
                        width: 800,
                        height: 600,
                    }],
                    has_acquire_fence: true,
                },
            )
            .unwrap();
        let frame = composer.present_display(DisplayId(9)).unwrap().frame;
        let prepared = binding.prepare_present(DisplayId(9), &frame).unwrap();
        let AndroidMessage::Present {
            acquire_point,
            release_point,
            ..
        } = prepared.message
        else {
            panic!("expected present")
        };
        assert_eq!((acquire_point, release_point), (1, 1));
        let encoded = droidloom_denial_protocol::encode_android(&AndroidMessage::Present {
            object: TaskObjectId(7),
            frame: frame.frame_id,
            buffer: frame.buffer.id,
            configure_serial: 5,
            acquire_point,
            release_point,
            damage: frame.damage,
        })
        .unwrap();
        assert!(encoded.descriptors.is_empty());
    }

    #[test]
    fn destroy_request_retries_failed_send_and_sends_success_only_once() {
        let (_, mut binding) = setup();
        assert_eq!(
            binding.request_destroy(|_| Err("disconnected")),
            Err("disconnected")
        );
        assert!(!binding.destroy_requested());
        let mut messages = Vec::new();
        for _ in 0..3 {
            binding
                .request_destroy(|message| {
                    messages.push(message);
                    Ok::<_, ()>(())
                })
                .unwrap();
        }
        assert_eq!(
            messages,
            vec![AndroidMessage::DestroyTask {
                object: TaskObjectId(7)
            }]
        );
    }

    #[test]
    fn release_and_presentation_events_return_to_composer() {
        let (mut session, mut binding) = setup();
        configure(&mut session, &mut binding);
        let composer = session
            .windows_mut()
            .composer_for_display_mut(DisplayId(9))
            .unwrap();
        composer
            .set_power_mode(DisplayId(9), PowerMode::On)
            .unwrap();
        composer.set_vsync_enabled(DisplayId(9), true).unwrap();
        composer
            .create_layer(
                DisplayId(9),
                LayerState {
                    id: LayerId(1),
                    requested_composition: Composition::Client,
                    z: 0,
                    protected_content: false,
                },
            )
            .unwrap();
        composer.validate_display(DisplayId(9)).unwrap();
        composer.accept_display_changes(DisplayId(9)).unwrap();
        composer
            .set_client_target(
                DisplayId(9),
                ClientTarget {
                    buffer: BufferMetadata {
                        id: BufferId(11),
                        width: 800,
                        height: 600,
                        format: FORMAT,
                        planes: vec![PlaneMetadata {
                            index: 0,
                            offset: 0,
                            stride: 3_200,
                        }],
                    },
                    damage: Vec::new(),
                    has_acquire_fence: true,
                },
            )
            .unwrap();
        let accepted = composer.present_display(DisplayId(9)).unwrap().frame;
        let frame = accepted.frame_id;
        binding.prepare_present(DisplayId(9), &accepted).unwrap();
        binding.request_destroy(|_| Ok::<_, ()>(())).unwrap();
        assert!(matches!(
            session.windows_mut().remove_display(DisplayId(9)),
            Err(TaskWindowError::DisplayBuffersInFlight { .. })
        ));
        assert!(binding.prepare_present(DisplayId(9), &accepted).is_err());
        assert!(matches!(
            binding
                .apply_event(
                    &mut session,
                    &DenialMessage::Presented {
                        object: TaskObjectId(7),
                        frame,
                        timestamp_nanos: 1_000_000_000,
                        refresh_period_nanos: 16_666_666,
                        sequence: 1,
                        flags: presentation_flag::DISPLAYED,
                    },
                )
                .unwrap(),
            AppliedDenialEvent::Presented(Some(_))
        ));
        assert!(matches!(
            binding
                .apply_event(
                    &mut session,
                    &DenialMessage::BufferReleased {
                        object: TaskObjectId(7),
                        frame,
                        buffer: BufferId(11),
                        release_point: 1,
                    },
                )
                .unwrap(),
            AppliedDenialEvent::BufferReleased(_)
        ));
        assert_eq!(
            session
                .windows()
                .composer_for_display(DisplayId(9))
                .unwrap()
                .in_flight_count(),
            0
        );
        assert!(session.windows_mut().remove_display(DisplayId(9)).is_ok());
    }
}
