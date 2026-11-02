//! Ordinary Wayland multi-window presentation for Droidloom.

#![deny(unsafe_op_in_unsafe_fn)]

mod activation;
mod clipboard;
mod clock;
mod density;
mod fence_wakeup;
mod frame_timeline;
mod imports;
mod input;
mod input_time;
mod insets;
mod layers;
mod notifications;
mod presentation_audit;
mod readiness;
mod resize;
mod text_input;

use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use drm_fourcc::DrmFourcc;
use droidloom_denial_endpoint::{
    DenialEndpoint, DenialEndpointListener, EndpointAction, EndpointError, EndpointListenerError,
    ExpectedPeer,
};
use droidloom_denial_ipc::IpcError;
use droidloom_denial_protocol::{
    AcceptedWireFrame, BufferId, BufferMetadata, FormatModifier, FrameId, InputEvent, KeyAction,
    PlaneMetadata, TaskObjectId, TouchAction, Transform, Visibility, presentation_flag,
};
use droidloom_syncobj::{SyncobjDevice, WaylandTimeline};
use droidloom_window_policy::{LogicalSize, SessionMode, WindowPolicyPaths, WindowPolicyStore};
use gbm::{BufferObject, Device as GbmDevice, Modifier};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::presentation_time::{
    PresentTime, PresentationTimeHandler, PresentationTimeState,
};
use smithay_client_toolkit::reexports::csd_frame::WindowState;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::seat::keyboard::{
    KeyEvent, KeyboardHandler, Modifiers, RawModifiers, RepeatInfo,
};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::touch::TouchHandler;
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use thiserror::Error;
use wayland_client::globals::{BindError, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface, wl_touch,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, backend::WaylandError,
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1, zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1,
};
use wayland_protocols::wp::linux_drm_syncobj::v1::client::{
    wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1,
    wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1,
    wp_linux_drm_syncobj_timeline_v1::WpLinuxDrmSyncobjTimelineV1,
};
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback::{
    self, WpPresentationFeedback,
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols_wlr::output_power_management::v1::client::{
    zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1,
    zwlr_output_power_v1::{self, ZwlrOutputPowerV1},
};

const BOOTSTRAP_PACKAGE: &str = "android.droidloom.bootstrap";
const TARGET_POOL_LENGTH: usize = 3;
const RELEASE_POLL_FALLBACK: Duration = Duration::from_millis(4);
const LEFT_BUTTON: u32 = 0x110;
const FRACTIONAL_SCALE_DENOMINATOR: u32 = 120;
const MAX_BUFFER_DIMENSION: u32 = 16_384;

#[derive(Debug, Error)]
enum PresenterError {
    #[error("I/O failure: {0}")]
    Io(#[from] io::Error),
    #[error("Wayland connection failure: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("Wayland global enumeration failure: {0}")]
    Global(#[from] wayland_client::globals::GlobalError),
    #[error("Wayland toolkit global failure: {0}")]
    ToolkitGlobal(#[from] smithay_client_toolkit::error::GlobalError),
    #[error("Wayland global bind failure: {0}")]
    Bind(#[from] BindError),
    #[error("Wayland dispatch failure: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    #[error("Wayland transport failure: {0}")]
    Wayland(String),
    #[error("Droidloom endpoint listener failure: {0}")]
    Listener(#[from] EndpointListenerError),
    #[error("Droidloom endpoint failure: {0}")]
    Endpoint(#[from] EndpointError),
    #[error("Droidloom IPC failure: {0}")]
    Ipc(#[from] IpcError),
    #[error("DRM syncobj failure: {0}")]
    Syncobj(#[from] droidloom_syncobj::SyncobjError),
    #[error("GBM failure: {0}")]
    Gbm(String),
    #[error("the compositor does not advertise linux-dmabuf v4 feedback")]
    DmabufV4Required,
    #[error("the compositor exposes no linear ARGB8888 or XRGB8888 DMA-BUF format")]
    NoRenderFormat,
    #[error("task {0:?} does not exist")]
    UnknownTask(TaskObjectId),
    #[error("task {object:?} presented unknown buffer {buffer:?}")]
    UnknownBuffer {
        object: TaskObjectId,
        buffer: BufferId,
    },
    #[error("Droidloom configuration is invalid: {0}")]
    Configuration(&'static str),
}

struct RenderTarget {
    metadata: BufferMetadata,
    planes: Vec<OwnedFd>,
    wayland: Option<wl_buffer::WlBuffer>,
    sync: Option<BufferSync>,
    _allocation: Option<BufferObject<()>>,
    host_owned: bool,
    retired: bool,
    busy: bool,
}

// Timeline progress implies completion of every earlier point. Each buffer
// therefore owns its timeline: neither a newer buffer's acquire fence nor an
// out-of-order compositor release may satisfy this buffer's release point.
struct BufferSync {
    timeline: WaylandTimeline,
    proxy: WpLinuxDrmSyncobjTimelineV1,
    next_point: u64,
    release_wakeup: Option<OwnedFd>,
}

impl BufferSync {
    fn arm_release_wakeup(&mut self, point: u64) -> Result<(), PresenterError> {
        let Some(event) = self.release_wakeup.as_ref() else {
            return Ok(());
        };
        match self.timeline.notify_when_signalled(point, event.as_fd()) {
            Ok(()) => Ok(()),
            Err(droidloom_syncobj::SyncobjError::Drm { source, .. })
                if matches!(
                    source.raw_os_error(),
                    Some(libc::ENOTTY | libc::EOPNOTSUPP | libc::EINVAL)
                ) =>
            {
                // Older timeline-capable kernels still use the bounded poll
                // fallback. Never replace an unsupported wakeup with a release.
                self.release_wakeup = None;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for BufferSync {
    fn drop(&mut self) {
        self.proxy.destroy();
    }
}

struct PendingFrame {
    frame: FrameId,
    buffer: BufferId,
    release_point: u64,
    submitted_nanos: u64,
    trace: Option<presentation_audit::FrameTrace>,
}

// Timing metadata outlives buffer ownership; no buffer or fence is retained here.
struct FrameFeedback {
    proxy: WpPresentationFeedback,
    frame: FrameId,
    buffer: BufferId,
    submitted_nanos: u64,
    grant: Option<frame_timeline::Identity>,
}

// Bounded optional timing metadata: no buffer, FD, or release fence ownership.
struct SceneFeedback {
    proxy: WpPresentationFeedback,
    scene: u64,
    configure_serial: u32,
    grant: Option<frame_timeline::Identity>,
}
const MAX_SCENE_FEEDBACK: usize = 8;

struct TaskWindow {
    timeline: frame_timeline::Timeline,
    layers: Option<layers::Stream>,
    package: String,
    android_task: Option<u64>,
    input_route: Option<input::Route>,
    window: Option<Window>,
    viewport: Option<WpViewport>,
    fractional_scale: Option<WpFractionalScaleV1>,
    sync_surface: Option<WpLinuxDrmSyncobjSurfaceV1>,
    logical_size: Option<(u32, u32)>,
    buffer_size: (u32, u32),
    applied_scale_120: u32,
    resize: resize::Pending,
    configure_serial: u32,
    refresh_millihz: u32,
    entered_outputs: Vec<wl_output::WlOutput>,
    presentation_output: Option<wl_output::WlOutput>,
    /// The density last sent for the task; None before the first.
    density_dpi: Option<u32>,
    targets: BTreeMap<BufferId, RenderTarget>,
    pending: VecDeque<PendingFrame>,
    presentation_feedback: Vec<FrameFeedback>,
    scene_feedback: Vec<SceneFeedback>,
    presentation_audit: Option<presentation_audit::Audit>,
    content_opaque: bool,
    applied_opaque: Option<bool>,
    composed_root: bool,
    focused: bool,
    /// Denial minimized the window (xdg `suspended`): Android stops the task.
    suspended: bool,
    /// Held while a shown Android window has `FLAG_KEEP_SCREEN_ON`. Denial
    /// keeps its displays on only while this window is visible.
    idle_inhibitor: Option<ZwpIdleInhibitorV1>,
    unmap_requested: bool,
    closing: bool,
}

impl TaskWindow {
    fn headless(&self) -> bool {
        self.package == BOOTSTRAP_PACKAGE
    }

    fn accepts_present(&self) -> bool {
        !self.headless() && self.window.is_some() && !self.unmap_requested && !self.closing
    }

    fn visibility(&self) -> Visibility {
        if self.suspended {
            Visibility::Hidden
        } else {
            Visibility::Visible
        }
    }

    fn surface(&self) -> Option<&wl_surface::WlSurface> {
        self.window.as_ref().map(Window::wl_surface)
    }
}

#[derive(Clone, Copy)]
struct Contact {
    object: TaskObjectId,
    pointer_id: u32,
    position_fixed: (i32, i32),
}

impl Contact {
    fn event(self, action: TouchAction) -> InputEvent {
        InputEvent::Touch {
            action,
            pointer_id: self.pointer_id,
            x_fixed: self.position_fixed.0,
            y_fixed: self.position_fixed.1,
            pressure: match action {
                TouchAction::Down | TouchAction::Motion => u16::MAX,
                TouchAction::Up | TouchAction::Cancel => 0,
            },
        }
    }
}

// A cancelled/replaced shared stream also invalidates contacts waiting behind
// clipboard synchronization. Preserve keyboard events, which are independent.
fn clear_contact_state(
    pointer: &mut Option<Contact>,
    touches: &mut BTreeMap<i32, Contact>,
    deferred: &mut VecDeque<(TaskObjectId, InputEvent, u64)>,
) {
    *pointer = None;
    touches.clear();
    deferred.retain(|(_, event, _)| !matches!(event, InputEvent::Touch { .. }));
}

struct App {
    activation: activation::Activation,
    layer_globals: layers::Globals,
    clipboard: clipboard::Clipboard,
    clipboard_inputs: VecDeque<(TaskObjectId, InputEvent, u64)>,
    text_input: text_input::TextInput,
    registry_state: RegistryState,
    output_state: OutputState,
    output_power_manager: ZwlrOutputPowerManagerV1,
    output_power: Vec<OutputPowerState>,
    idle_inhibit_manager: Option<ZwpIdleInhibitManagerV1>,
    host_display_powered: Option<bool>,
    published_display_power: Option<bool>,
    presentation_time: PresentationTimeState,
    presentation_clock: Option<u32>,
    seat_state: SeatState,
    dmabuf_state: DmabufState,
    feedback: Option<DmabufFeedback>,
    compositor: CompositorState,
    xdg_shell: XdgShell,
    insets_manager: Option<insets::DenialInsetsManagerV1>,
    frame_timeline_manager: Option<frame_timeline::Manager>,
    clock: clock::Clock,
    viewporter: WpViewporter,
    fractional_scale_manager: WpFractionalScaleManagerV1,
    sync_manager: WpLinuxDrmSyncobjManagerV1,
    syncobj: SyncobjDevice,
    gbm: GbmDevice<File>,
    listener: DenialEndpointListener,
    window_policy: WindowPolicyStore,
    /// The density each task gets on the output it is on.
    densities: density::Densities,
    endpoint: Option<DenialEndpoint>,
    tasks: BTreeMap<TaskObjectId, TaskWindow>,
    task_work: readiness::Tasks,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    touch: Option<wl_touch::WlTouch>,
    focused: Option<TaskObjectId>,
    pointer_contact: Option<Contact>,
    touch_contacts: BTreeMap<i32, Contact>,
    // Touch and mouse share Android's injected pointer device; keys are separate.
    motion_times: input_time::Stream,
    key_times: input_time::Stream,
    input_timestamps: input_time::Timestamps,
    next_buffer_id: u64,
    next_input_serial: u64,
    direct_input: input::DirectInput,
    socket_path: PathBuf,
    fatal: Option<String>,
}

struct OutputPowerState {
    output: wl_output::WlOutput,
    control: Option<ZwlrOutputPowerV1>,
    powered: Option<bool>,
}

// A failed power control belongs to one output, not the Wayland transport.
// Keep the output unknown until wl_output removal: failure can also mean that
// power management is unsupported on an output which is still connected.
fn update_output_power(
    states: &mut [OutputPowerState],
    control: &ZwlrOutputPowerV1,
    event: zwlr_output_power_v1::Event,
) -> bool {
    let Some(output) = states
        .iter_mut()
        .find(|output| output.control.as_ref() == Some(control))
    else {
        return false; // Late events after control/global retirement.
    };
    match event {
        zwlr_output_power_v1::Event::Mode { mode } => {
            output.powered = match mode {
                WEnum::Value(zwlr_output_power_v1::Mode::On) => Some(true),
                WEnum::Value(zwlr_output_power_v1::Mode::Off) => Some(false),
                _ => None,
            };
        }
        zwlr_output_power_v1::Event::Failed => {
            // The protocol requires destruction even when the output has gone.
            // Taking ownership prevents a second destroy on global removal.
            if let Some(control) = output.control.take() {
                control.destroy();
            }
            output.powered = None;
            eprintln!("Droidloom output-power control retired; waiting for output topology");
        }
        _ => return false,
    }
    true
}

fn remove_output_power(states: &mut Vec<OutputPowerState>, output: &wl_output::WlOutput) {
    states.retain_mut(|state| {
        if state.output == *output {
            if let Some(control) = state.control.take() {
                control.destroy();
            }
            false
        } else {
            true
        }
    });
}

impl Drop for App {
    fn drop(&mut self) {
        remove_owned_socket(&self.socket_path);
    }
}

impl App {
    fn recompute_display_power(&mut self) -> Result<(), PresenterError> {
        let powered =
            aggregate_display_power(self.output_power.iter().map(|output| output.powered));
        self.host_display_powered = powered;
        self.publish_display_power()
    }

    fn publish_display_power(&mut self) -> Result<(), PresenterError> {
        let Some(powered) = self.host_display_powered else {
            return Ok(());
        };
        if self.published_display_power == Some(powered) {
            return Ok(());
        }
        let Some(endpoint) = self.endpoint.as_ref().filter(|endpoint| endpoint.ready()) else {
            return Ok(());
        };
        if endpoint.direct_input() {
            if let Err(error) = self.direct_input.set_power(powered) {
                eprintln!("Droidloom input power: {error}");
            }
        } else {
            endpoint.send_display_power(powered)?;
        }
        if !powered || (endpoint.direct_input() && self.direct_input.fd().is_none()) {
            self.clear_contacts();
        }
        self.published_display_power = Some(powered);
        Ok(())
    }

    fn task_for_surface(&self, surface: &wl_surface::WlSurface) -> Option<TaskObjectId> {
        self.tasks.iter().find_map(|(object, task)| {
            task.surface()
                .is_some_and(|candidate| candidate == surface)
                .then_some(*object)
        })
    }

    fn task_refresh_millihz(&self, object: TaskObjectId) -> u32 {
        let Some(task) = self.tasks.get(&object) else {
            return 60_000;
        };
        if task.package == BOOTSTRAP_PACKAGE {
            // Android's shared scheduler must accommodate active task outputs,
            // but an unrelated faster monitor must not drive its cadence.
            return self
                .tasks
                .iter()
                .filter(|(_, task)| {
                    task.package != BOOTSTRAP_PACKAGE
                        && !task.closing
                        && !task.entered_outputs.is_empty()
                })
                .map(|(object, _)| self.task_refresh_millihz(*object))
                .max()
                .unwrap_or(60_000);
        }
        select_surface_refresh(
            &task.entered_outputs,
            task.presentation_output.as_ref(),
            |output| self.output_refresh_millihz(output),
        )
    }

    fn output_refresh_millihz(&self, output: &wl_output::WlOutput) -> Option<u32> {
        self.output_state.info(output).and_then(|info| {
            info.modes
                .iter()
                .find(|mode| mode.current && mode.refresh_rate > 0)
                .and_then(|mode| u32::try_from(mode.refresh_rate).ok())
        })
    }

    /// Gives the task the density of the output it is on, when that
    /// changed: before its configure, so Android lays it out once.
    fn update_task_density(&mut self, object: TaskObjectId) -> Result<(), PresenterError> {
        let Some(task) = self.tasks.get(&object) else {
            return Ok(());
        };
        if task.package == BOOTSTRAP_PACKAGE || task.configure_serial == 0 {
            return Ok(());
        }
        let name = select_surface_output(
            &task.entered_outputs,
            task.presentation_output.as_ref(),
            |output| self.output_state.info(output).map(|_| 0),
        )
        .and_then(|output| self.output_state.info(output))
        .and_then(|info| info.name);
        let dpi = self.densities.dpi(name.as_deref(), task.applied_scale_120);
        if task.density_dpi == Some(dpi) {
            return Ok(());
        }
        self.endpoint
            .as_ref()
            .ok_or(PresenterError::Configuration("endpoint disappeared"))?
            .send_density(object, dpi)?;
        if let Some(task) = self.tasks.get_mut(&object) {
            task.density_dpi = Some(dpi);
        }
        Ok(())
    }

    fn update_task_refresh(&mut self, object: TaskObjectId) -> Result<(), PresenterError> {
        self.update_task_density(object)?;
        let refresh = self.task_refresh_millihz(object);
        let task = self
            .tasks
            .get_mut(&object)
            .ok_or(PresenterError::UnknownTask(object))?;
        if task.configure_serial == 0 || task.refresh_millihz == refresh {
            return Ok(());
        }
        // A cadence change preserves the configure's allocation generation.
        // Existing targets and in-flight frames still satisfy its dimensions.
        self.endpoint
            .as_mut()
            .ok_or(PresenterError::Configuration("endpoint disappeared"))?
            .send_configure(
                object,
                task.configure_serial,
                task.buffer_size.0,
                task.buffer_size.1,
                task.applied_scale_120,
                FRACTIONAL_SCALE_DENOMINATOR,
                Transform::Normal,
                refresh,
            )?;
        task.refresh_millihz = refresh;
        if task.package != BOOTSTRAP_PACKAGE {
            let bootstrap = self
                .tasks
                .iter()
                .find_map(|(object, task)| (task.package == BOOTSTRAP_PACKAGE).then_some(*object));
            if let Some(bootstrap) = bootstrap {
                self.update_task_refresh(bootstrap)?;
            }
        }
        Ok(())
    }

    fn update_output_refreshes(&mut self) -> Result<(), PresenterError> {
        let objects = self.tasks.keys().copied().collect::<Vec<_>>();
        for object in objects {
            self.update_task_refresh(object)?;
        }
        Ok(())
    }

    fn host_canvas_size(&self) -> Option<(u32, u32)> {
        self.output_state
            .outputs()
            .filter_map(|output| self.output_state.info(&output))
            .filter_map(|info| {
                let mode = info
                    .modes
                    .iter()
                    .find(|mode| mode.current)
                    .or_else(|| info.modes.iter().find(|mode| mode.preferred))?;
                oriented_output_size(mode.dimensions, info.transform)
            })
            .max_by_key(|(width, height)| u64::from(*width) * u64::from(*height))
    }

    fn host_logical_canvas_size(&self) -> Option<LogicalSize> {
        self.output_state
            .outputs()
            .filter_map(|output| self.output_state.info(&output))
            .filter_map(|info| {
                let logical = info.logical_size.and_then(|(width, height)| {
                    Some(LogicalSize::new(
                        u32::try_from(width).ok()?,
                        u32::try_from(height).ok()?,
                    ))
                });
                logical.transpose().ok().flatten().or_else(|| {
                    let mode = info
                        .modes
                        .iter()
                        .find(|mode| mode.current)
                        .or_else(|| info.modes.iter().find(|mode| mode.preferred))?;
                    let (width, height) = oriented_output_size(mode.dimensions, info.transform)?;
                    let scale = u32::try_from(info.scale_factor.max(1)).ok()?;
                    LogicalSize::new(width.div_ceil(scale), height.div_ceil(scale)).ok()
                })
            })
            .max_by_key(|size| u64::from(size.width) * u64::from(size.height))
    }

    fn reconfigure_bootstrap(&mut self, qh: &QueueHandle<Self>) -> Result<(), PresenterError> {
        let Some(object) = self
            .tasks
            .iter()
            .find_map(|(object, task)| task.headless().then_some(*object))
        else {
            return Ok(());
        };
        let Some((width, height)) = self.host_canvas_size() else {
            return Ok(());
        };
        self.configure_task(
            qh,
            object,
            resize::Configure {
                size: LogicalSize { width, height },
                scale_120: FRACTIONAL_SCALE_DENOMINATOR,
                remember: false,
            },
        )
    }

    fn selected_format(&self) -> Result<(DrmFourcc, Vec<Modifier>), PresenterError> {
        let feedback = self
            .feedback
            .as_ref()
            .ok_or(PresenterError::DmabufV4Required)?;
        for fourcc in [DrmFourcc::Argb8888, DrmFourcc::Xrgb8888] {
            let mut modifiers = feedback
                .tranches()
                .iter()
                .flat_map(|tranche| tranche.formats.iter())
                .filter_map(|index| feedback.format_table().get(*index as usize))
                // Host Mesa and Android Mesa can disagree about compressed or
                // tiled layouts even when Wayland advertises them. Linear
                // DMA-BUFs keep the shared render targets importable across
                // that boundary, including laptops with multiple GPUs.
                .filter(|entry| {
                    entry.format == fourcc as u32 && entry.modifier == u64::from(Modifier::Linear)
                })
                .map(|entry| Modifier::from(entry.modifier))
                .collect::<Vec<_>>();
            modifiers.sort_unstable_by_key(|modifier| u64::from(*modifier));
            modifiers.dedup();
            if !modifiers.is_empty() {
                return Ok((fourcc, modifiers));
            }
        }
        Err(PresenterError::NoRenderFormat)
    }

    fn published_formats(&self) -> Result<Vec<FormatModifier>, PresenterError> {
        let (fourcc, modifiers) = self.selected_format()?;
        Ok(modifiers
            .into_iter()
            .map(|modifier| FormatModifier {
                fourcc: fourcc as u32,
                modifier: u64::from(modifier),
            })
            .collect())
    }

    fn create_task(
        &mut self,
        qh: &QueueHandle<Self>,
        object: TaskObjectId,
        package: &str,
    ) -> Result<(), PresenterError> {
        let headless = package == BOOTSTRAP_PACKAGE;
        if !headless && let Err(error) = self.window_policy.reload_policy() {
            eprintln!("Droidloom kept the last valid window policy: {error}");
        }
        let (window, viewport, fractional_scale, sync_surface) = if headless {
            (None, None, None, None)
        } else {
            let surface = self.compositor.create_surface(qh);
            let window = self
                .xdg_shell
                .create_window(surface, WindowDecorations::None, qh);
            window.set_title(package.to_owned());
            window.set_app_id(package.to_owned());
            window.set_min_size(Some((1, 1)));
            if let Some(manager) = &self.insets_manager {
                manager.set_self_managed(window.wl_surface());
            }
            // fractional-scale-v1 keeps the wl_surface at scale 1 and uses a
            // viewport to map the higher-resolution buffer into XDG logical
            // coordinates.
            window.wl_surface().set_buffer_scale(1);
            let viewport = self.viewporter.get_viewport(window.wl_surface(), qh, ());
            let fractional_scale =
                self.fractional_scale_manager
                    .get_fractional_scale(window.wl_surface(), qh, object);
            let sync_surface = self.sync_manager.get_surface(window.wl_surface(), qh, ());
            window.commit();
            (
                Some(window),
                Some(viewport),
                Some(fractional_scale),
                Some(sync_surface),
            )
        };
        self.tasks.insert(
            object,
            TaskWindow {
                timeline: frame_timeline::Timeline::new(
                    self.frame_timeline_manager.as_ref(),
                    window.as_ref().map(Window::wl_surface),
                    qh,
                ),
                layers: None,
                package: package.to_owned(),
                android_task: None,
                input_route: None,
                window,
                viewport,
                fractional_scale,
                sync_surface,
                logical_size: None,
                buffer_size: (0, 0),
                applied_scale_120: FRACTIONAL_SCALE_DENOMINATOR,
                resize: resize::Pending::new(FRACTIONAL_SCALE_DENOMINATOR),
                configure_serial: 0,
                refresh_millihz: 60_000,
                entered_outputs: Vec::new(),
                presentation_output: None,
                density_dpi: None,
                targets: BTreeMap::new(),
                pending: VecDeque::new(),
                presentation_feedback: Vec::new(),
                scene_feedback: Vec::new(),
                content_opaque: false,
                applied_opaque: None,
                composed_root: false,
                presentation_audit: droidloom_timing::traced(droidloom_timing::Diagnostic::Audit)
                    .then(presentation_audit::Audit::default),
                focused: false,
                suspended: false,
                idle_inhibitor: None,
                unmap_requested: false,
                closing: false,
            },
        );
        let formats = self.published_formats()?;
        self.endpoint
            .as_mut()
            .ok_or(PresenterError::Configuration("endpoint disappeared"))?
            .send_format_feedback(object, 1, formats)?;
        if headless {
            // The hidden bootstrap object defines Android's single built-in
            // desktop canvas. wl_output mode dimensions are already physical
            // pixels, equivalent to logical output size multiplied by its
            // scale, so do not apply the scale a second time here.
            let (width, height) = self
                .host_canvas_size()
                .ok_or(PresenterError::Configuration(
                    "Wayland output has no current pixel mode",
                ))?;
            self.configure_task(
                qh,
                object,
                resize::Configure {
                    size: LogicalSize { width, height },
                    scale_120: FRACTIONAL_SCALE_DENOMINATOR,
                    remember: false,
                },
            )?;
        }
        Ok(())
    }

    fn apply_pending_resizes(&mut self, qh: &QueueHandle<Self>) -> Result<(), PresenterError> {
        // Wayland callbacks only replace pending geometry. Apply the final
        // size/scale before draining Android frames or publishing layer config.
        while let Some(object) = self.task_work.pop(readiness::RESIZE) {
            let Some(task) = self.tasks.get_mut(&object) else {
                continue;
            };
            let Some(configure) = task.resize.take() else {
                continue;
            };
            if !task.accepts_present() {
                continue;
            }
            self.configure_task(qh, object, configure)?;
            if configure.remember {
                let package = &self
                    .tasks
                    .get(&object)
                    .expect("configured task exists")
                    .package;
                if let Err(error) = self.window_policy.remember(package, configure.size) {
                    // Persistence failure is non-fatal to an otherwise valid frame.
                    eprintln!("Droidloom could not remember {package} window size: {error}");
                }
            }
        }
        Ok(())
    }

    fn configure_task(
        &mut self,
        qh: &QueueHandle<Self>,
        object: TaskObjectId,
        configure: resize::Configure,
    ) -> Result<(), PresenterError> {
        let width = configure.size.width;
        let height = configure.size.height;
        let scale_120 = configure.scale_120;
        let buffer_width = scale_dimension(width, scale_120)?;
        let buffer_height = scale_dimension(height, scale_120)?;
        let needs_configure = {
            let task = self
                .tasks
                .get(&object)
                .ok_or(PresenterError::UnknownTask(object))?;
            task.configure_serial == 0
                || configure.changes_geometry(task.logical_size, task.applied_scale_120)
                || task.buffer_size != (buffer_width, buffer_height)
        };
        if !needs_configure {
            return Ok(());
        }

        self.abort_timeline_work(object, frame_timeline::Terminal::Configure)?;
        self.retire_host_targets(object)?;
        let serial = self
            .tasks
            .get(&object)
            .and_then(|task| task.configure_serial.checked_add(1))
            .ok_or(PresenterError::Configuration("configure serial exhausted"))?;
        let refresh = self.task_refresh_millihz(object);
        {
            let task = self
                .tasks
                .get_mut(&object)
                .ok_or(PresenterError::UnknownTask(object))?;
            task.logical_size = Some((width, height));
            task.applied_scale_120 = scale_120;
            task.applied_opaque = None;
            task.buffer_size = (buffer_width, buffer_height);
            task.configure_serial = serial;
            task.refresh_millihz = refresh;
            if let Some(viewport) = task.viewport.as_ref() {
                let destination_width = i32::try_from(width)
                    .map_err(|_| PresenterError::Configuration("logical width exceeds Wayland"))?;
                let destination_height = i32::try_from(height)
                    .map_err(|_| PresenterError::Configuration("logical height exceeds Wayland"))?;
                viewport.set_destination(destination_width, destination_height);
            }
        }
        self.update_task_density(object)?;
        self.endpoint
            .as_mut()
            .ok_or(PresenterError::Configuration("endpoint disappeared"))?
            .send_configure(
                object,
                serial,
                buffer_width,
                buffer_height,
                scale_120,
                FRACTIONAL_SCALE_DENOMINATOR,
                Transform::Normal,
                refresh,
            )?;
        self.task_work.mark(object, readiness::CONFIG);
        // Only a peer that negotiated demand allocation may configure an
        // empty pool. Bootstrap can request its targets through the same
        // dequeue path if its normally elided composition is ever needed.
        if !self
            .endpoint
            .as_ref()
            .is_some_and(DenialEndpoint::lazy_render_targets)
        {
            self.ensure_render_targets(qh, object, serial)?;
        }
        self.update_output_refreshes()
    }

    fn ensure_render_targets(
        &mut self,
        qh: &QueueHandle<Self>,
        object: TaskObjectId,
        serial: u32,
    ) -> Result<(), PresenterError> {
        let task = self
            .tasks
            .get(&object)
            .ok_or(PresenterError::UnknownTask(object))?;
        // Requests race resizes and close on the other socket. Never allocate
        // obsolete dimensions or resurrect a closing task's target pool.
        if (!task.headless() && !task.accepts_present())
            || task.closing
            || task.unmap_requested
            || task.configure_serial != serial
            || task
                .targets
                .values()
                .any(|target| target.host_owned && !target.retired)
        {
            return Ok(());
        }
        let (width, height) = task.buffer_size;
        let mut targets: Vec<RenderTarget> = Vec::with_capacity(TARGET_POOL_LENGTH);
        for _ in 0..TARGET_POOL_LENGTH {
            let id = self.allocate_buffer_id()?;
            let target = self.allocate_target(qh, id, width, height)?;
            targets.push(target);
        }
        // Finish allocation before publishing any descriptors. A failed GBM
        // allocation must not leave Android holding an incomplete pool.
        for (index, target) in targets.iter().enumerate() {
            let planes = target.planes.iter().map(AsFd::as_fd).collect::<Vec<_>>();
            let result = self
                .endpoint
                .as_mut()
                .ok_or(PresenterError::Configuration("endpoint disappeared"))?
                .register_render_target(object, serial, &target.metadata, &planes);
            if let Err(error) = result {
                for registered in &targets[..index] {
                    let _ = self
                        .endpoint
                        .as_mut()
                        .unwrap()
                        .unregister_render_target(object, registered.metadata.id);
                }
                return Err(error.into());
            }
        }
        self.tasks.get_mut(&object).unwrap().targets.extend(
            targets
                .into_iter()
                .map(|target| (target.metadata.id, target)),
        );
        eprintln!(
            "Droidloom composition targets: object={} configure={serial} size={width}x{height} buffers={TARGET_POOL_LENGTH}",
            object.0
        );
        Ok(())
    }

    fn allocate_buffer_id(&mut self) -> Result<BufferId, PresenterError> {
        self.next_buffer_id = self
            .next_buffer_id
            .checked_add(1)
            .ok_or(PresenterError::Configuration("buffer identity exhausted"))?;
        Ok(BufferId(self.next_buffer_id))
    }

    fn allocate_target(
        &self,
        _qh: &QueueHandle<Self>,
        id: BufferId,
        width: u32,
        height: u32,
    ) -> Result<RenderTarget, PresenterError> {
        let (fourcc, modifiers) = self.selected_format()?;
        let allocation = self
            .gbm
            .create_buffer_object_with_modifiers::<()>(
                width,
                height,
                fourcc,
                modifiers.iter().copied(),
            )
            .map_err(|error| PresenterError::Gbm(error.to_string()))?;
        let plane_count = allocation.plane_count();
        if !(1..=4).contains(&plane_count) {
            return Err(PresenterError::Gbm(format!(
                "GBM returned invalid plane count {plane_count}"
            )));
        }
        let actual_modifier = u64::from(allocation.modifier());
        let mut planes = Vec::with_capacity(plane_count as usize);
        let mut plane_metadata = Vec::with_capacity(plane_count as usize);
        for index in 0..plane_count {
            let plane = i32::try_from(index)
                .map_err(|_| PresenterError::Configuration("plane index overflow"))?;
            planes.push(
                allocation
                    .fd_for_plane(plane)
                    .map_err(|error| PresenterError::Gbm(error.to_string()))?,
            );
            plane_metadata.push(PlaneMetadata {
                index,
                offset: allocation.offset(plane),
                stride: allocation.stride_for_plane(plane),
            });
        }
        let metadata = BufferMetadata {
            id,
            width,
            height,
            format: FormatModifier {
                fourcc: fourcc as u32,
                modifier: actual_modifier,
            },
            planes: plane_metadata,
        };
        let wayland = None;
        Ok(RenderTarget {
            metadata,
            planes,
            wayland,
            sync: None,
            _allocation: Some(allocation),
            host_owned: true,
            retired: false,
            busy: false,
        })
    }

    fn create_wayland_buffer(
        &self,
        qh: &QueueHandle<Self>,
        metadata: &BufferMetadata,
        planes: &[OwnedFd],
    ) -> Result<wl_buffer::WlBuffer, PresenterError> {
        if planes.len() != metadata.planes.len() {
            return Err(PresenterError::Configuration(
                "DMA-BUF plane count mismatch",
            ));
        }
        let params = self.dmabuf_state.create_params(qh)?;
        for (plane, fd) in metadata.planes.iter().zip(planes) {
            params.add(
                fd.as_fd(),
                plane.index,
                plane.offset,
                plane.stride,
                metadata.format.modifier,
            );
        }
        let width = i32::try_from(metadata.width)
            .map_err(|_| PresenterError::Configuration("buffer width exceeds Wayland"))?;
        let height = i32::try_from(metadata.height)
            .map_err(|_| PresenterError::Configuration("buffer height exceeds Wayland"))?;
        let (buffer, proxy) = params.create_immed(
            width,
            height,
            metadata.format.fourcc,
            zwp_linux_buffer_params_v1::Flags::empty(),
            qh,
        );
        proxy.destroy();
        Ok(buffer)
    }

    fn retire_host_targets(&mut self, object: TaskObjectId) -> Result<(), PresenterError> {
        let idle = {
            let task = self
                .tasks
                .get_mut(&object)
                .ok_or(PresenterError::UnknownTask(object))?;
            task.targets
                .iter_mut()
                .filter_map(|(id, target)| {
                    if target.host_owned {
                        target.retired = true;
                        (!target.busy).then_some(*id)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        for id in idle {
            self.endpoint
                .as_mut()
                .ok_or(PresenterError::Configuration("endpoint disappeared"))?
                .unregister_render_target(object, id)?;
            if let Some(target) = self
                .tasks
                .get_mut(&object)
                .and_then(|task| task.targets.remove(&id))
            {
                if let Some(buffer) = target.wayland {
                    buffer.destroy();
                }
            }
        }
        Ok(())
    }

    fn register_android_buffer(
        &mut self,
        _qh: &QueueHandle<Self>,
        object: TaskObjectId,
        metadata: BufferMetadata,
        planes: Vec<OwnedFd>,
    ) -> Result<(), PresenterError> {
        let wayland = None;
        let task = self
            .tasks
            .get_mut(&object)
            .ok_or(PresenterError::UnknownTask(object))?;
        if task.targets.contains_key(&metadata.id) {
            return Err(PresenterError::Configuration(
                "duplicate local buffer identity",
            ));
        }
        task.targets.insert(
            metadata.id,
            RenderTarget {
                metadata,
                planes,
                wayland,
                sync: None,
                _allocation: None,
                host_owned: false,
                retired: false,
                busy: false,
            },
        );
        Ok(())
    }

    fn present(
        &mut self,
        qh: &QueueHandle<Self>,
        frame: &AcceptedWireFrame,
        acquire_fence: &OwnedFd,
        target_grant: Option<frame_timeline::Target>,
    ) -> Result<(), PresenterError> {
        let object = frame.object;
        let mut frame_trace = presentation_audit::FrameTrace::new(
            object.0,
            frame.frame.0,
            frame.buffer.0,
            acquire_fence.as_fd(),
        );
        if !self
            .tasks
            .get(&object)
            .ok_or(PresenterError::UnknownTask(object))?
            .accepts_present()
        {
            // Host close and Android task destruction are asynchronous. A
            // frame already queued by SurfaceFlinger can legally arrive after
            // the native toplevel has been unmapped. Complete that frame as a
            // deliberate discard; the task record remains until its earlier
            // Wayland releases drain, but the missing surface is not fatal to
            // the presenter or to unrelated Android applications.
            self.endpoint
                .as_mut()
                .ok_or(PresenterError::Configuration("endpoint disappeared"))?
                .discard_frame(object, frame.frame)?;
            return Ok(());
        }

        // Import only buffers that will actually be attached to a visible
        // surface. Android's hidden bootstrap frames and superseded resize
        // targets must not fill the compositor's pending DMA-BUF import queue.
        let target = self
            .tasks
            .get(&object)
            .and_then(|task| task.targets.get(&frame.buffer))
            .ok_or(PresenterError::UnknownBuffer {
                object,
                buffer: frame.buffer,
            })?;
        if target.wayland.is_none() {
            let buffer = self.create_wayland_buffer(qh, &target.metadata, &target.planes)?;
            let timeline = WaylandTimeline::create(&self.syncobj)?;
            let descriptor = timeline.export_descriptor()?;
            let proxy = self
                .sync_manager
                .import_timeline(descriptor.as_fd(), qh, ());
            let target = self
                .tasks
                .get_mut(&object)
                .and_then(|task| task.targets.get_mut(&frame.buffer))
                .expect("target was just checked");
            target.wayland = Some(buffer);
            // SAFETY: eventfd has no pointer arguments; a nonnegative return
            // value is a new descriptor uniquely owned below.
            let event = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            if event < 0 {
                return Err(io::Error::last_os_error().into());
            }
            target.sync = Some(BufferSync {
                timeline,
                proxy,
                next_point: 0,
                // SAFETY: ownership of the successful eventfd transfers once.
                release_wakeup: Some(unsafe { OwnedFd::from_raw_fd(event) }),
            });
        }
        let task = self
            .tasks
            .get_mut(&object)
            .ok_or(PresenterError::UnknownTask(object))?;
        let surface = task
            .surface()
            .cloned()
            .expect("accepts_present guaranteed a mapped window");
        if task.applied_opaque != Some(task.content_opaque) {
            if task.content_opaque {
                let (width, height) = task.logical_size.ok_or(PresenterError::Configuration(
                    "opaque target has no logical size",
                ))?;
                let region = Region::new(&self.compositor)?;
                region.add(
                    0,
                    0,
                    i32::try_from(width).map_err(|_| {
                        PresenterError::Configuration("opaque width exceeds Wayland")
                    })?,
                    i32::try_from(height).map_err(|_| {
                        PresenterError::Configuration("opaque height exceeds Wayland")
                    })?,
                );
                surface.set_opaque_region(Some(region.wl_region()));
            } else {
                surface.set_opaque_region(None);
            }
            task.applied_opaque = Some(task.content_opaque);
            if droidloom_syncobj::frame_trace::enabled() {
                eprintln!(
                    "Droidloom content state: object={} frame={} opaque={} source=android monotonic_ns={}",
                    object.0,
                    frame.frame.0,
                    task.content_opaque,
                    droidloom_syncobj::frame_trace::now_ns()
                );
            }
        }
        let layers_hidden = task.layers.as_mut().is_some_and(|layers| layers.hide());
        if layers_hidden {
            if let Some(viewport) = task.viewport.as_ref() {
                viewport.set_source(-1.0, -1.0, -1.0, -1.0);
            }
        }
        let target = task
            .targets
            .get_mut(&frame.buffer)
            .ok_or(PresenterError::UnknownBuffer {
                object,
                buffer: frame.buffer,
            })?;
        if target.busy || target.retired {
            return Err(PresenterError::Configuration(
                "presented buffer is unavailable",
            ));
        }
        let sync = target
            .sync
            .as_mut()
            .ok_or(PresenterError::Configuration("buffer timeline is absent"))?;
        let acquire_point = sync
            .next_point
            .checked_add(1)
            .ok_or(PresenterError::Configuration(
                "Wayland sync point exhausted",
            ))?;
        let release_point = acquire_point
            .checked_add(1)
            .ok_or(PresenterError::Configuration(
                "Wayland sync point exhausted",
            ))?;
        sync.next_point = release_point;
        sync.timeline
            .import_acquire_fence(acquire_point, acquire_fence.as_fd())?;
        sync.arm_release_wakeup(release_point)?;
        if task.sync_surface.is_none() {
            task.sync_surface = Some(self.sync_manager.get_surface(&surface, qh, ()));
        }
        let sync_surface = task
            .sync_surface
            .as_ref()
            .ok_or(PresenterError::Configuration(
                "window sync surface is absent",
            ))?;
        let (acquire_hi, acquire_lo) = split_point(acquire_point);
        let (release_hi, release_lo) = split_point(release_point);
        sync_surface.set_acquire_point(&sync.proxy, acquire_hi, acquire_lo);
        sync_surface.set_release_point(&sync.proxy, release_hi, release_lo);
        // Timing feedback is optional and can arrive long after the compositor
        // releases a buffer (especially for obscured windows). Bound outstanding
        // callbacks independently of render-target ownership.
        let feedback = if self
            .endpoint
            .as_mut()
            .ok_or(PresenterError::Configuration("endpoint disappeared"))?
            .track_presentation(object, frame.frame)?
        {
            Some(self.presentation_time.feedback(&surface, qh)?)
        } else {
            None
        };
        surface.attach(target.wayland.as_ref(), 0, 0);
        task.composed_root = true;
        if layers_hidden {
            // Android damage compares app frames, not our constant root backing.
            // Replacing that root with a composed target changes the whole image.
            surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        }
        for damage in &frame.damage {
            surface.damage_buffer(
                i32::try_from(damage.x).unwrap_or(i32::MAX),
                i32::try_from(damage.y).unwrap_or(i32::MAX),
                i32::try_from(damage.width).unwrap_or(i32::MAX),
                i32::try_from(damage.height).unwrap_or(i32::MAX),
            );
        }
        let submitted_nanos = if task.presentation_audit.is_some() {
            monotonic_timestamp_nanos()?
        } else {
            0
        };
        if let Some(trace) = frame_trace.as_mut() {
            trace.commit();
        }
        if let (Some(timeline), Some(grant)) = (&task.timeline.surface, &target_grant) {
            timeline.target(grant);
        }
        surface.commit();
        self.activation.mapped(object, &surface);
        target.busy = true;
        task.pending.push_back(PendingFrame {
            frame: frame.frame,
            buffer: frame.buffer,
            release_point,
            submitted_nanos,
            trace: frame_trace,
        });
        if let Some(feedback) = feedback {
            task.presentation_feedback.push(FrameFeedback {
                grant: target_grant.map(|g| g.identity),
                proxy: feedback,
                frame: frame.frame,
                buffer: frame.buffer,
                submitted_nanos,
            });
        }
        self.task_work.frames.insert(object);
        self.task_work.mark(object, readiness::FRAMES);
        Ok(())
    }

    fn release_ready_frames(&mut self) -> Result<(), PresenterError> {
        let mut ready = Vec::new();
        while let Some(object) = self.task_work.pop(readiness::FRAMES) {
            let Some(task) = self.tasks.get_mut(&object) else {
                continue;
            };
            // Denial returns buffers in any order: a minimized window keeps its
            // last shown frame while it releases the newer commits it replaced.
            let mut index = 0;
            while let Some(pending) = task.pending.get(index) {
                let sync = task
                    .targets
                    .get(&pending.buffer)
                    .and_then(|target| target.sync.as_ref())
                    .ok_or(PresenterError::Configuration(
                        "pending buffer timeline is absent",
                    ))?;
                // Only the compositor's real release fence controls reuse.
                // Presentation feedback retains its own bounded frame identity.
                if pending.release_point > sync.timeline.signalled_point()? {
                    index += 1;
                    continue;
                }
                let mut pending = task.pending.remove(index).expect("index was present");
                if let Some(trace) = pending.trace.as_mut() {
                    trace.release_ready(
                        task.presentation_feedback
                            .iter()
                            .any(|feedback| feedback.frame == pending.frame),
                    );
                }
                if let Some(audit) = task.presentation_audit.as_mut() {
                    audit.released(pending.submitted_nanos, monotonic_timestamp_nanos()?);
                }
                ready.push((object, pending));
            }
            if task.pending.is_empty() {
                self.task_work.frames.remove(&object);
            }
            let fallback = task.pending.iter().any(|pending| {
                task.targets
                    .get(&pending.buffer)
                    .and_then(|target| target.sync.as_ref())
                    .is_some_and(|sync| sync.release_wakeup.is_none())
            });
            self.task_work.frame_fallback(object, fallback);
            if task.closing {
                self.task_work.mark(object, readiness::CLEANUP);
            }
        }
        for (object, pending) in ready {
            self.endpoint
                .as_mut()
                .ok_or(PresenterError::Configuration("endpoint disappeared"))?
                .finish_release(object, pending.frame)?;
            if let Some(mut trace) = pending.trace {
                trace.returned();
            }
            let (retired, closing) = {
                let task = self
                    .tasks
                    .get_mut(&object)
                    .ok_or(PresenterError::UnknownTask(object))?;
                let target =
                    task.targets
                        .get_mut(&pending.buffer)
                        .ok_or(PresenterError::UnknownBuffer {
                            object,
                            buffer: pending.buffer,
                        })?;
                target.busy = false;
                (target.retired && target.host_owned, task.closing)
            };
            if retired && !closing {
                self.endpoint
                    .as_mut()
                    .ok_or(PresenterError::Configuration("endpoint disappeared"))?
                    .unregister_render_target(object, pending.buffer)?;
                if let Some(target) = self
                    .tasks
                    .get_mut(&object)
                    .and_then(|task| task.targets.remove(&pending.buffer))
                {
                    if let Some(buffer) = target.wayland {
                        buffer.destroy();
                    }
                }
            }
        }
        while let Some(object) = self.task_work.pop(readiness::CLEANUP) {
            if self.tasks.get(&object).is_some_and(|task| {
                task.closing
                    && task.pending.is_empty()
                    && task.layers.as_ref().is_none_or(|s| !s.busy())
            }) {
                self.remove_task(object);
            }
        }
        Ok(())
    }

    fn remove_task(&mut self, object: TaskObjectId) {
        self.retire_feedback(object, frame_timeline::Terminal::Epoch);
        self.retire_contacts(Some(object));
        self.task_work.remove(object);
        self.activation.remove(object);
        if let Some(mut task) = self.tasks.remove(&object) {
            task.timeline.stop();
            if let Some(fractional_scale) = task.fractional_scale.take() {
                fractional_scale.destroy();
            }
            if let Some(viewport) = task.viewport.take() {
                viewport.destroy();
            }
            if let Some(surface) = task.sync_surface.take() {
                surface.destroy();
            }
            if let Some(inhibitor) = task.idle_inhibitor.take() {
                inhibitor.destroy();
            }
            for (_, target) in task.targets {
                if let Some(buffer) = target.wayland {
                    buffer.destroy();
                }
            }
        }
        if self.focused == Some(object) {
            self.focused = None;
        }
        if self.endpoint.is_some() {
            if let Err(error) = self.update_output_refreshes() {
                self.fail(&error);
            }
        }
    }

    fn unmap_task_window(&mut self, object: TaskObjectId) -> Result<(), PresenterError> {
        self.abort_timeline_work(object, frame_timeline::Terminal::Configure)?;
        self.retire_contacts(Some(object));
        self.activation.remove(object);
        let task = self
            .tasks
            .get_mut(&object)
            .ok_or(PresenterError::UnknownTask(object))?;
        task.resize.cancel();
        let Some(surface) = task.surface().cloned() else {
            return Ok(());
        };

        // An xdg_toplevel close request is terminal for the native window.
        // Unmap it immediately, like any ordinary Wayland client, while the
        // retained timeline/target bookkeeping drains buffers Android had
        // already submitted before its task removal completed.
        surface.attach(None, 0, 0);
        surface.commit();
        task.timeline.stop();
        if let Some(stream) = task.layers.as_mut() {
            stream.hide();
        }
        if let Some(fractional_scale) = task.fractional_scale.take() {
            fractional_scale.destroy();
        }
        if let Some(viewport) = task.viewport.take() {
            viewport.destroy();
        }
        if let Some(sync_surface) = task.sync_surface.take() {
            sync_surface.destroy();
        }
        if let Some(inhibitor) = task.idle_inhibitor.take() {
            inhibitor.destroy();
        }
        // Window is reference counted by SCTK. Taking our handle below does
        // not guarantee that WindowInner is dropped here, so relying on Drop
        // can leave the compositor-side toplevel mapped until some unrelated
        // callback releases the final clone. The close request is terminal:
        // destroy the role explicitly and let the remaining surface/buffer
        // objects drain independently.
        if let Some(window) = task.window.as_ref() {
            window.xdg_toplevel().destroy();
        }
        task.window.take();
        task.unmap_requested = false;
        self.task_work
            .mark(object, readiness::CONFIG | readiness::CLEANUP);

        if self.focused == Some(object) {
            self.focused = None;
        }
        eprintln!(
            "Droidloom destroyed terminal Android xdg_toplevel object={}",
            object.0
        );
        Ok(())
    }

    fn unmap_requested_tasks(&mut self) -> Result<(), PresenterError> {
        while let Some(object) = self.task_work.pop(readiness::UNMAP) {
            if !self.tasks.contains_key(&object) {
                continue;
            }
            self.unmap_task_window(object)?;
            let finished = self.tasks.get(&object).is_some_and(|task| {
                task.closing
                    && task.pending.is_empty()
                    && task.layers.as_ref().is_none_or(|s| !s.busy())
            });
            if finished {
                self.remove_task(object);
            }
        }
        Ok(())
    }

    fn destroy_task(&mut self, object: TaskObjectId) -> Result<(), PresenterError> {
        let task = self
            .tasks
            .get_mut(&object)
            .ok_or(PresenterError::UnknownTask(object))?;
        task.closing = true;
        task.unmap_requested = true;
        self.task_work.mark(
            object,
            readiness::UNMAP | readiness::CONFIG | readiness::CLEANUP,
        );
        Ok(())
    }

    fn process_action(
        &mut self,
        qh: &QueueHandle<Self>,
        action: EndpointAction,
    ) -> Result<(), PresenterError> {
        match action {
            EndpointAction::RequestRenderTargets {
                object,
                configure_serial,
            } => {
                self.ensure_render_targets(qh, object, configure_serial)?;
            }
            EndpointAction::BindInputChannel {
                generation,
                wire,
                socket,
            } => {
                if self
                    .direct_input
                    .attach(generation, wire, socket)
                    .map_err(PresenterError::Wayland)?
                {
                    self.clear_contacts();
                    eprintln!("Droidloom direct input: connected to InputFlinger, wire ABI {wire}");
                }
            }
            EndpointAction::BindInputRoute {
                object,
                task,
                display,
                incarnation,
            } => {
                let route = input::Route {
                    task: task.0,
                    display,
                    incarnation,
                };
                if self
                    .tasks
                    .get(&object)
                    .and_then(|window| window.input_route)
                    .is_some_and(|previous| previous != route)
                {
                    self.retire_contacts(Some(object));
                }
                if let Some(window) = self.tasks.get_mut(&object) {
                    window.input_route = Some(input::Route {
                        task: task.0,
                        display,
                        incarnation,
                    });
                }
            }
            EndpointAction::SetKeepScreenOn { object, keep } => {
                let task = self
                    .tasks
                    .get_mut(&object)
                    .ok_or(PresenterError::UnknownTask(object))?;
                if !keep {
                    if let Some(inhibitor) = task.idle_inhibitor.take() {
                        inhibitor.destroy();
                    }
                } else if task.idle_inhibitor.is_none()
                    && task.accepts_present()
                    && let (Some(manager), Some(window)) =
                        (self.idle_inhibit_manager.as_ref(), task.window.as_ref())
                {
                    task.idle_inhibitor =
                        Some(manager.create_inhibitor(window.wl_surface(), qh, ()));
                }
            }
            EndpointAction::RequestActivation { object } => {
                if self.focused != Some(object)
                    && let Some(task) = self
                        .tasks
                        .get(&object)
                        .filter(|task| task.accepts_present())
                {
                    let surface = task.window.as_ref().map(|window| window.wl_surface());
                    self.activation.request(qh, object, &task.package, surface);
                }
            }
            EndpointAction::BindLayerStream { object, sockets } => {
                self.bind_layers(object, sockets)?
            }
            EndpointAction::ClientReady => {
                if self
                    .endpoint
                    .as_ref()
                    .is_some_and(DenialEndpoint::lazy_render_targets)
                {
                    eprintln!("Droidloom graphics: fallback targets allocated on demand");
                }
                self.pump_direct_input()?;
                self.publish_display_power()?;
            }
            EndpointAction::TimelinesBound { .. }
            | EndpointAction::ConfigureAcknowledged { .. }
            | EndpointAction::SupersededPresentDiscarded { .. }
            | EndpointAction::Pong { .. }
            | EndpointAction::SetFrameRate { .. } => {}
            EndpointAction::SetContentState { object, flags } => {
                self.tasks
                    .get_mut(&object)
                    .ok_or(PresenterError::UnknownTask(object))?
                    .content_opaque = flags & droidloom_denial_protocol::content_state::OPAQUE != 0;
            }
            EndpointAction::BindTask { object, task } => {
                if let Some(window) = self.tasks.get_mut(&object) {
                    window.android_task = Some(task.0);
                }
            }
            EndpointAction::CreateTask {
                object,
                display: _,
                package,
            } => self.create_task(qh, object, &package)?,
            EndpointAction::DestroyTask { object } => self.destroy_task(object)?,
            EndpointAction::RegisterBuffer {
                object,
                buffer,
                planes,
            } => self.register_android_buffer(qh, object, buffer, planes)?,
            EndpointAction::UnregisterBuffer { object, buffer } => {
                let target = self
                    .tasks
                    .get_mut(&object)
                    .ok_or(PresenterError::UnknownTask(object))?
                    .targets
                    .remove(&buffer)
                    .ok_or(PresenterError::UnknownBuffer { object, buffer })?;
                if let Some(buffer) = target.wayland {
                    buffer.destroy();
                }
            }
            EndpointAction::Present {
                frame,
                acquire_fence,
            } => self.queue_present(qh, frame, acquire_fence)?,
        }
        Ok(())
    }

    fn accept_endpoint(&mut self) -> Result<(), PresenterError> {
        if self.endpoint.is_some() {
            return Ok(());
        }
        match self.listener.accept() {
            Ok(endpoint) => {
                let endpoint = if self.activation.supported() {
                    endpoint.with_activation_requests()
                } else {
                    endpoint
                };
                let endpoint = endpoint
                    .with_direct_input()
                    .with_lazy_render_targets()
                    .with_scene_presentation()
                    .with_task_density();
                let endpoint = if self.idle_inhibit_manager.is_some() {
                    endpoint.with_keep_screen_on()
                } else {
                    endpoint
                };
                // Android takes Denial's grants as its vsync only when they
                // can be compared with the private protocol's clock.
                let endpoint = if self.frame_timeline_manager.is_some()
                    && self.presentation_clock == Some(libc::CLOCK_MONOTONIC as u32)
                {
                    endpoint.with_host_clock()
                } else {
                    endpoint
                };
                endpoint.socket().socket().set_nonblocking(true)?;
                self.endpoint = Some(endpoint);
            }
            Err(error) if listener_would_block(&error) => {}
            Err(EndpointListenerError::UnexpectedPeer { .. }) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn drain_endpoint(&mut self, qh: &QueueHandle<Self>) -> Result<(), PresenterError> {
        loop {
            let action = match self.endpoint.as_mut() {
                Some(endpoint) => match endpoint.receive_action() {
                    Ok(action) => action,
                    Err(error) if endpoint_would_block(&error) => return Ok(()),
                    Err(error) => {
                        eprintln!("droidloom-wayland: Android endpoint disconnected: {error}");
                        self.disconnect_endpoint();
                        return Ok(());
                    }
                },
                None => return Ok(()),
            };
            self.process_action(qh, action)?;
        }
    }

    fn disconnect_endpoint(&mut self) {
        self.activation.clear();
        self.text_input.disconnect();
        self.endpoint = None;
        self.direct_input.disconnect();
        self.published_display_power = None;
        for object in self.tasks.keys().copied().collect::<Vec<_>>() {
            self.remove_task(object);
        }
        self.focused = None;
        self.pointer_contact = None;
        self.touch_contacts.clear();
    }

    fn clear_contacts(&mut self) {
        clear_contact_state(
            &mut self.pointer_contact,
            &mut self.touch_contacts,
            &mut self.clipboard_inputs,
        );
    }

    fn retire_contacts(&mut self, object: Option<TaskObjectId>) {
        if let Some(object) = object {
            let route = self.tasks.get(&object).and_then(|task| task.input_route);
            if let Some(route) = route {
                match monotonic_timestamp_nanos() {
                    Ok(timestamp) => {
                        if let Err(error) = self.direct_input.retire_route(route, timestamp) {
                            eprintln!("Droidloom input retirement: {error}");
                        }
                    }
                    Err(error) => {
                        eprintln!("Droidloom input retirement: {error}");
                        self.direct_input.disconnect();
                    }
                }
            }
            if !self
                .pointer_contact
                .is_some_and(|contact| contact.object == object)
                && !self
                    .touch_contacts
                    .values()
                    .any(|contact| contact.object == object)
                && !self.clipboard_inputs.iter().any(|(owner, event, _)| {
                    *owner == object && matches!(event, InputEvent::Touch { .. })
                })
            {
                return;
            }
        }
        if self
            .endpoint
            .as_ref()
            .is_some_and(DenialEndpoint::direct_input)
        {
            match monotonic_timestamp_nanos() {
                Ok(timestamp) => {
                    if let Err(error) = self.direct_input.cancel_all(timestamp) {
                        eprintln!("Droidloom input retirement: {error}");
                    }
                }
                Err(error) => {
                    eprintln!("Droidloom input retirement: {error}");
                    self.direct_input.disconnect();
                }
            }
        } else if self.endpoint.is_some() {
            let mut contacts: Vec<_> = self.touch_contacts.values().copied().collect();
            contacts.extend(self.pointer_contact);
            // Legacy policy groups by task. Every route needs retirement.
            contacts.sort_by_key(|contact| contact.object);
            contacts.dedup_by_key(|contact| contact.object);
            for contact in contacts {
                let result = self.synthesized_time().and_then(|time| {
                    self.send_input(contact.object, contact.event(TouchAction::Cancel), time)
                });
                if let Err(error) = result {
                    eprintln!("Droidloom input retirement: {error}");
                }
            }
        }
        self.clear_contacts();
    }

    /// Nanoseconds for an event of `device` with the compositor's `millis`,
    /// at full precision when the compositor sent the full time.
    fn input_time(
        &mut self,
        device: input_time::Device,
        millis: u32,
    ) -> Result<u64, PresenterError> {
        let now = monotonic_timestamp_nanos()?;
        let precise = self.input_timestamps.take(device, millis);
        let stream = match device {
            input_time::Device::Keyboard => &mut self.key_times,
            input_time::Device::Pointer | input_time::Device::Touch => &mut self.motion_times,
        };
        Ok(stream.event(millis, precise, now))
    }

    /// Nanoseconds for a touch or mouse event the presenter makes itself.
    fn synthesized_time(&mut self) -> Result<u64, PresenterError> {
        Ok(self.motion_times.received(monotonic_timestamp_nanos()?))
    }

    /// Send one event stamped with its hardware time on the shared monotonic clock.
    fn send_input(
        &mut self,
        object: TaskObjectId,
        event: InputEvent,
        timestamp: u64,
    ) -> Result<bool, PresenterError> {
        if !matches!(
            event,
            InputEvent::Touch {
                action: TouchAction::Cancel,
                ..
            }
        ) && self.clipboard.blocked()
            && self.clipboard_inputs.len() < 512
        {
            self.clipboard_inputs.push_back((object, event, timestamp));
            return Ok(true);
        }
        self.next_input_serial = self
            .next_input_serial
            .checked_add(1)
            .ok_or(PresenterError::Configuration("input serial exhausted"))?;
        if droidloom_timing::traced(droidloom_timing::Diagnostic::Input) {
            let receipt = monotonic_timestamp_nanos()?;
            eprintln!(
                "Droidloom input trace: stage=wayland serial={} object={} event={event:?} time_ns={timestamp} receipt_ns={receipt} age_us={}",
                self.next_input_serial,
                object.0,
                receipt.saturating_sub(timestamp) / 1_000
            );
        }
        if self
            .endpoint
            .as_ref()
            .is_some_and(DenialEndpoint::direct_input)
        {
            let window = self
                .tasks
                .get(&object)
                .ok_or(PresenterError::UnknownTask(object))?;
            if window.closing
                && !matches!(
                    event,
                    InputEvent::Touch {
                        action: TouchAction::Cancel,
                        ..
                    }
                )
            {
                return Ok(false);
            }
            let Some(route) = window.input_route else {
                // The task binding and native route arrive in ordered records,
                // but Wayland may dispatch input between their receipt.
                return Ok(false);
            };
            if let Err(error) = self.direct_input.send(
                route,
                (window.applied_scale_120, FRACTIONAL_SCALE_DENOMINATOR),
                self.next_input_serial,
                timestamp,
                event,
            ) {
                // The native receiver cancels held input on EOF. Recover this
                // connection without taking down unrelated app windows.
                eprintln!("Droidloom direct input: {error}");
                self.clear_contacts();
                return Ok(false);
            }
            return Ok(true);
        }
        self.endpoint
            .as_ref()
            .ok_or(PresenterError::Configuration("endpoint is absent"))?
            .send_input(object, self.next_input_serial, timestamp, event)?;
        Ok(true)
    }

    fn pump_direct_input(&mut self) -> Result<(), PresenterError> {
        if let Err(error) = self.direct_input.flush() {
            eprintln!("Droidloom input: {error}");
        }
        if self
            .endpoint
            .as_ref()
            .is_some_and(DenialEndpoint::direct_input)
            && self.direct_input.fd().is_none()
        {
            self.clear_contacts();
        }
        if let Some(endpoint) = self
            .endpoint
            .as_ref()
            .filter(|endpoint| endpoint.direct_input())
        {
            if let Some(generation) = self.direct_input.next_request() {
                endpoint.request_input_channel(generation)?;
            }
        }
        Ok(())
    }

    fn update_text_input(&mut self) {
        let task = self
            .focused
            .filter(|object| Some(*object) == self.text_input.entered)
            .and_then(|object| self.tasks.get(&object))
            .filter(|task| !task.closing && task.window.is_some())
            .and_then(|task| task.android_task);
        self.text_input.update(task);
    }

    fn pump_text_input(&mut self) {
        if let Err(error) = self.text_input.pump() {
            eprintln!("Droidloom text-input disconnected: {error}");
            self.text_input.disconnect();
        }
        self.update_text_input();
    }

    fn set_focus(&mut self, object: Option<TaskObjectId>) -> Result<(), PresenterError> {
        if self.focused == object {
            return Ok(());
        }
        if let Some(previous) = self.focused
            && let Some(task) = self.tasks.get_mut(&previous)
        {
            task.focused = false;
            if let Some(endpoint) = self.endpoint.as_ref() {
                endpoint.send_visibility(previous, task.visibility(), false)?;
            }
        }
        self.focused = object;
        self.update_text_input();
        if let Some(object) = object
            && let Some(task) = self.tasks.get_mut(&object)
        {
            task.focused = true;
            if let Some(endpoint) = self.endpoint.as_ref() {
                endpoint.send_visibility(object, task.visibility(), true)?;
            }
        }
        Ok(())
    }

    fn fixed_position(&self, object: TaskObjectId, position: (f64, f64)) -> (i32, i32) {
        let Some(task) = self.tasks.get(&object) else {
            return (0, 0);
        };
        let Some((logical_width, logical_height)) = task.logical_size else {
            return (0, 0);
        };
        let x = position
            .0
            .clamp(0.0, f64::from(logical_width).max(1.0) - 0.001);
        let y = position
            .1
            .clamp(0.0, f64::from(logical_height).max(1.0) - 0.001);
        (fixed_16_16(x), fixed_16_16(y))
    }

    fn fail(&mut self, error: &dyn ToString) {
        self.fatal = Some(error.to_string());
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
        surface.set_buffer_scale(1);
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        let Some(object) = self.task_for_surface(surface) else {
            return;
        };
        let task = self.tasks.get_mut(&object).expect("surface has a task");
        if !task.entered_outputs.contains(output) {
            task.entered_outputs.push(output.clone());
        }
        if let Err(error) = self.update_task_refresh(object) {
            self.fail(&error);
        }
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        let Some(object) = self.task_for_surface(surface) else {
            return;
        };
        let task = self.tasks.get_mut(&object).expect("surface has a task");
        task.entered_outputs.retain(|entered| entered != output);
        if task.presentation_output.as_ref() == Some(output) {
            task.presentation_output = None;
        }
        if let Err(error) = self.update_task_refresh(object) {
            self.fail(&error);
        }
    }
}

impl Dispatch<WpFractionalScaleV1, TaskObjectId> for App {
    fn event(
        state: &mut Self,
        _proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        object: &TaskObjectId,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let wp_fractional_scale_v1::Event::PreferredScale { scale } = event else {
            return;
        };
        if scale == 0 {
            state.fail(&"compositor advertised a zero fractional scale");
            return;
        }
        let Some(task) = state.tasks.get_mut(object) else {
            return;
        };
        if task.accepts_present() && task.resize.preferred_scale(scale, task.logical_size) {
            state.task_work.mark(*object, readiness::RESIZE);
        }
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, window: &Window) {
        let object = self.tasks.iter().find_map(|(object, task)| {
            task.window
                .as_ref()
                .is_some_and(|candidate| candidate.wl_surface() == window.wl_surface())
                .then_some(*object)
        });
        if let Some(object) = object
            && let Some(endpoint) = self.endpoint.as_ref()
        {
            match endpoint.send_close(object) {
                Ok(()) => {
                    if let Some(task) = self.tasks.get_mut(&object) {
                        task.unmap_requested = true;
                        self.task_work
                            .mark(object, readiness::UNMAP | readiness::CONFIG);
                    }
                }
                Err(error) => self.fail(&error),
            }
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let object = self.tasks.iter().find_map(|(object, task)| {
            task.window
                .as_ref()
                .is_some_and(|candidate| candidate.wl_surface() == window.wl_surface())
                .then_some(*object)
        });
        let Some(object) = object else {
            return;
        };
        let task = self.tasks.get_mut(&object).expect("surface has a task");
        if !task.accepts_present() {
            return;
        }
        let suspended = configure.state.contains(WindowState::SUSPENDED);
        if task.suspended != suspended {
            task.suspended = suspended;
            let (visibility, focused) = (task.visibility(), task.focused);
            if let Some(endpoint) = self.endpoint.as_ref()
                && let Err(error) = endpoint.send_visibility(object, visibility, focused)
            {
                self.fail(&error);
                return;
            }
        }
        let task = &self.tasks[&object];
        let package = task.package.clone();
        let previous = task.resize.logical_size(task.logical_size);
        let requested_width = configure.new_size.0.map(NonZeroU32::get);
        let requested_height = configure.new_size.1.map(NonZeroU32::get);
        let size = previous.map_or_else(
            || {
                let bounds = configure
                    .suggested_bounds
                    .and_then(|(width, height)| LogicalSize::new(width, height).ok());
                self.window_policy.resolve_initial(
                    &package,
                    requested_width,
                    requested_height,
                    bounds,
                    self.host_logical_canvas_size(),
                )
            },
            |previous| LogicalSize {
                width: requested_width.unwrap_or(previous.width),
                height: requested_height.unwrap_or(previous.height),
            },
        );
        let remember = previous.is_some()
            && !configure.is_resizing()
            && !configure.is_maximized()
            && !configure.is_fullscreen();
        self.tasks
            .get_mut(&object)
            .expect("surface has a task")
            .resize
            .configure(size, remember);
        self.task_work.mark(object, readiness::RESIZE);
    }
}

impl DmabufHandler for App {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_feedback(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _proxy: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        feedback: DmabufFeedback,
    ) {
        self.feedback = Some(feedback);
    }

    fn created(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        buffer: wl_buffer::WlBuffer,
    ) {
        self.layer_imported(params, Some(buffer));
    }

    fn failed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    ) {
        self.layer_imported(params, None);
    }

    fn released(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _buffer: &wl_buffer::WlBuffer,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let control = self
            .output_power_manager
            .get_output_power(&output, qh, output.clone());
        self.output_power.push(OutputPowerState {
            output,
            control: Some(control),
            powered: None,
        });
        if let Err(error) = self
            .reconfigure_bootstrap(qh)
            .and_then(|()| self.update_output_refreshes())
        {
            self.fail(&error);
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        if let Err(error) = self
            .reconfigure_bootstrap(qh)
            .and_then(|()| self.update_output_refreshes())
        {
            self.fail(&error);
        }
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        remove_output_power(&mut self.output_power, &output);
        if let Err(error) = self.recompute_display_power() {
            self.fail(&error);
        }
        for task in self.tasks.values_mut() {
            task.entered_outputs.retain(|entered| entered != &output);
            if task.presentation_output.as_ref() == Some(&output) {
                task.presentation_output = None;
            }
        }
        if let Err(error) = self
            .reconfigure_bootstrap(qh)
            .and_then(|()| self.update_output_refreshes())
        {
            self.fail(&error);
        }
    }
}

impl Dispatch<ZwlrOutputPowerV1, wl_output::WlOutput> for App {
    fn event(
        state: &mut Self,
        control: &ZwlrOutputPowerV1,
        event: zwlr_output_power_v1::Event,
        _output: &wl_output::WlOutput,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if update_output_power(&mut state.output_power, control, event) {
            if let Err(error) = state.recompute_display_power() {
                state.fail(&error);
            }
        }
    }
}

impl PresentationTimeHandler for App {
    fn presentation_time_state(&mut self) -> &mut PresentationTimeState {
        &mut self.presentation_time
    }

    fn presented(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        feedback: &WpPresentationFeedback,
        surface: &wl_surface::WlSurface,
        outputs: Vec<wl_output::WlOutput>,
        time: PresentTime,
        refresh: u32,
        sequence: u64,
        flags: WEnum<wp_presentation_feedback::Kind>,
    ) {
        if let Some((object, index)) = self.tasks.iter().find_map(|(object, task)| {
            task.scene_feedback
                .iter()
                .position(|f| f.proxy == *feedback)
                .map(|index| (*object, index))
        }) {
            let task = self.tasks.get_mut(&object).unwrap();
            let observed = task.scene_feedback.swap_remove(index);
            // Late feedback may belong to a previous configure or removed output.
            // The actual buffer-release path remains completely independent.
            let output = outputs
                .iter()
                .find(|o| task.entered_outputs.contains(o))
                .cloned()
                .or_else(|| {
                    (outputs.is_empty() && task.entered_outputs.len() == 1)
                        .then(|| task.entered_outputs[0].clone())
                });
            if !task.accepts_present()
                || observed.configure_serial != task.configure_serial
                || output.is_none()
                || time.clk_id != libc::CLOCK_MONOTONIC as u32
            {
                task.timeline
                    .finish(observed.grant, frame_timeline::Terminal::Configure);
                self.task_work
                    .mark(object, readiness::TIMELINE | readiness::RELEASES);
                return;
            }
            task.timeline
                .finish(observed.grant, frame_timeline::Terminal::Presented);
            self.task_work
                .mark(object, readiness::TIMELINE | readiness::RELEASES);
            let changed = task.presentation_output != output;
            task.presentation_output = output;
            let result = (|| {
                if changed {
                    self.update_task_refresh(object)?;
                }
                let refresh =
                    presentation_refresh_period(refresh, self.tasks[&object].refresh_millihz)?;
                let timestamp = presentation_timestamp_nanos(time.tv_sec, time.tv_nsec)?;
                if observed.scene != 0 {
                    self.endpoint
                        .as_mut()
                        .ok_or(PresenterError::Configuration("endpoint is absent"))?
                        .send_scene_presented(
                            object,
                            observed.scene,
                            timestamp,
                            refresh,
                            sequence,
                        )?;
                }
                Ok::<(), PresenterError>(())
            })();
            if let Err(error) = result {
                self.fail(&error);
            }
            return;
        }
        let Some(object) = self.task_for_surface(surface) else {
            return;
        };
        let task = self.tasks.get_mut(&object).expect("surface has a task");
        // Retired callbacks can still arrive after resize/epoch/unmap cleanup.
        if !task
            .presentation_feedback
            .iter()
            .any(|f| f.proxy == *feedback)
        {
            return;
        }
        // Presentation feedback identifies the timing output for overlapping
        // surfaces. Ignore late feedback from an output already left.
        if let Some(output) = outputs
            .into_iter()
            .find(|output| task.entered_outputs.contains(output))
        {
            if task.presentation_output.as_ref() != Some(&output) {
                task.presentation_output = Some(output);
                if let Err(error) = self.update_task_refresh(object) {
                    self.fail(&error);
                    return;
                }
            }
        }
        let result = (|| {
            if time.clk_id != u32::try_from(libc::CLOCK_MONOTONIC).unwrap_or_default() {
                return Err(PresenterError::Configuration(
                    "presentation clock is not CLOCK_MONOTONIC",
                ));
            }
            let (completed, advertised_refresh) = {
                let task = self
                    .tasks
                    .get_mut(&object)
                    .ok_or(PresenterError::UnknownTask(object))?;
                let index = task
                    .presentation_feedback
                    .iter()
                    .position(|candidate| candidate.proxy == *feedback)
                    .ok_or(PresenterError::Configuration(
                        "presentation feedback has no submitted frame",
                    ))?;
                let completed = task.presentation_feedback.swap_remove(index);
                task.timeline
                    .finish(completed.grant, frame_timeline::Terminal::Presented);
                self.task_work
                    .mark(object, readiness::TIMELINE | readiness::RELEASES);
                (completed, task.refresh_millihz)
            };
            let frame = completed.frame;
            let timestamp_nanos = presentation_timestamp_nanos(time.tv_sec, time.tv_nsec)?;
            let task = self.tasks.get_mut(&object).expect("task was just checked");
            if droidloom_syncobj::frame_trace::sampled(frame.0) {
                droidloom_syncobj::frame_trace::event(
                    "presented",
                    object.0,
                    frame.0,
                    completed.buffer.0,
                    &[
                        ("display_ns", timestamp_nanos),
                        ("refresh_ns", u64::from(refresh)),
                        ("display_sequence", sequence),
                    ],
                );
            }
            if let Some(audit) = task.presentation_audit.as_mut() {
                audit.presented(
                    object.0,
                    completed.submitted_nanos,
                    timestamp_nanos,
                    monotonic_timestamp_nanos()?,
                    task.pending.len(),
                );
            }
            let refresh_period_nanos = presentation_refresh_period(refresh, advertised_refresh)?;
            let protocol_flags = if matches!(
                flags,
                WEnum::Value(kind) if kind.contains(wp_presentation_feedback::Kind::ZeroCopy)
            ) {
                presentation_flag::DISPLAYED | presentation_flag::DIRECT_SCANOUT
            } else {
                presentation_flag::DISPLAYED | presentation_flag::COMPOSITED
            };
            self.endpoint
                .as_mut()
                .ok_or(PresenterError::Configuration("endpoint is absent"))?
                .send_presented(
                    object,
                    frame,
                    timestamp_nanos,
                    refresh_period_nanos,
                    sequence,
                    protocol_flags,
                )?;
            Ok(())
        })();
        if let Err(error) = result {
            self.fail(&error);
        }
    }

    fn discarded(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        feedback: &WpPresentationFeedback,
        surface: &wl_surface::WlSurface,
    ) {
        if let Some((object, index)) = self.tasks.iter().find_map(|(object, task)| {
            task.scene_feedback
                .iter()
                .position(|f| f.proxy == *feedback)
                .map(|index| (*object, index))
        }) {
            let task = self.tasks.get_mut(&object).unwrap();
            let completed = task.scene_feedback.swap_remove(index);
            task.timeline
                .finish(completed.grant, frame_timeline::Terminal::Missed);
            self.task_work
                .mark(object, readiness::TIMELINE | readiness::RELEASES);
            return;
        }
        let Some(object) = self.task_for_surface(surface) else {
            return;
        };
        let Some(task) = self.tasks.get_mut(&object) else {
            return;
        };
        if let Some(index) = task
            .presentation_feedback
            .iter()
            .position(|candidate| candidate.proxy == *feedback)
        {
            let completed = task.presentation_feedback.swap_remove(index);
            task.timeline
                .finish(completed.grant, frame_timeline::Terminal::Missed);
            self.task_work
                .mark(object, readiness::TIMELINE | readiness::RELEASES);
            let frame = completed.frame;
            if droidloom_syncobj::frame_trace::sampled(frame.0) {
                droidloom_syncobj::frame_trace::event(
                    "discarded",
                    object.0,
                    frame.0,
                    completed.buffer.0,
                    &[],
                );
            }
            if let Some(endpoint) = self.endpoint.as_mut() {
                endpoint.forget_presentation(object, frame);
            }
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.clipboard.seat(&seat, qh);
            if let Some(manager) = &self.text_input.manager {
                self.text_input.resource = Some(manager.get_text_input(&seat, qh, ()));
                self.text_input.attach_panel_feedback(qh);
            }
        }
        let result = match capability {
            Capability::Keyboard if self.keyboard.is_none() => self
                .seat_state
                .get_keyboard(qh, &seat, None)
                .map(|keyboard| {
                    self.input_timestamps.keyboard(&keyboard, qh);
                    self.keyboard = Some(keyboard);
                })
                .map_err(|error| error.to_string()),
            Capability::Pointer if self.pointer.is_none() => self
                .seat_state
                .get_pointer(qh, &seat)
                .map(|pointer| {
                    self.input_timestamps.pointer(&pointer, qh);
                    self.pointer = Some(pointer);
                })
                .map_err(|error| error.to_string()),
            Capability::Touch if self.touch.is_none() => self
                .seat_state
                .get_touch(qh, &seat)
                .map(|touch| {
                    self.input_timestamps.touch(&touch, qh);
                    self.touch = Some(touch);
                })
                .map_err(|error| error.to_string()),
            _ => Ok(()),
        };
        if let Err(error) = result {
            self.fail(&error);
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard
                if self
                    .keyboard
                    .as_ref()
                    .and_then(|keyboard| {
                        keyboard.data::<smithay_client_toolkit::seat::keyboard::KeyboardData<App>>()
                    })
                    .is_some_and(|data| data.seat() == &seat) =>
            {
                self.input_timestamps.remove(input_time::Device::Keyboard);
                self.keyboard = None;
                self.text_input.remove_resource();
            }
            Capability::Pointer
                if self
                    .pointer
                    .as_ref()
                    .and_then(|pointer| {
                        pointer.data::<smithay_client_toolkit::seat::pointer::PointerData>()
                    })
                    .is_some_and(|data| data.seat() == &seat) =>
            {
                self.retire_contacts(None);
                self.input_timestamps.remove(input_time::Device::Pointer);
                self.pointer = None;
            }
            Capability::Touch
                if self
                    .touch
                    .as_ref()
                    .and_then(|touch| {
                        touch.data::<smithay_client_toolkit::seat::touch::TouchData>()
                    })
                    .is_some_and(|data| data.seat() == &seat) =>
            {
                self.retire_contacts(None);
                self.input_timestamps.remove(input_time::Device::Touch);
                self.touch = None;
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, conn: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        for capability in [Capability::Keyboard, Capability::Pointer, Capability::Touch] {
            self.remove_capability(conn, qh, seat.clone(), capability);
        }
    }
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        serial: u32,
        _raw: &[u32],
        _keysyms: &[smithay_client_toolkit::seat::keyboard::Keysym],
    ) {
        self.clipboard.focus(true);
        self.clipboard.serial(serial);
        let object = self.task_for_surface(surface);
        if let Err(error) = self.set_focus(object) {
            self.fail(&error);
        }
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
        self.clipboard.focus(false);
        if self.focused == self.task_for_surface(surface)
            && let Err(error) = self.set_focus(None)
        {
            self.fail(&error);
        }
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.clipboard.serial(serial);
        if let Some(data) =
            _keyboard.data::<smithay_client_toolkit::seat::keyboard::KeyboardData<App>>()
            && let Some(surface) = self
                .focused
                .and_then(|id| self.tasks.get(&id))
                .and_then(TaskWindow::surface)
        {
            self.activation.input(serial, data.seat(), surface);
        }
        if let Some(object) = self.focused
            && let Err(error) = self
                .input_time(input_time::Device::Keyboard, event.time)
                .and_then(|time| {
                    self.send_input(
                        object,
                        InputEvent::Key {
                            action: KeyAction::Down,
                            keycode: event.raw_code,
                            repeat: 0,
                        },
                        time,
                    )
                })
        {
            self.fail(&error);
        }
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.clipboard.serial(serial);
        if let Some(object) = self.focused
            && let Err(error) = self
                .input_time(input_time::Device::Keyboard, event.time)
                .and_then(|time| {
                    self.send_input(
                        object,
                        InputEvent::Key {
                            action: KeyAction::Down,
                            keycode: event.raw_code,
                            repeat: 1,
                        },
                        time,
                    )
                })
        {
            self.fail(&error);
        }
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.clipboard.serial(serial);
        if let Some(object) = self.focused
            && let Err(error) = self
                .input_time(input_time::Device::Keyboard, event.time)
                .and_then(|time| {
                    self.send_input(
                        object,
                        InputEvent::Key {
                            action: KeyAction::Up,
                            keycode: event.raw_code,
                            repeat: 0,
                        },
                        time,
                    )
                })
        {
            self.fail(&error);
        }
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
    }

    fn update_repeat_info(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _info: RepeatInfo,
    ) {
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        if self.pointer.as_ref() != Some(_pointer) {
            return;
        }
        for event in events {
            if let PointerEventKind::Press { serial, .. } = event.kind
                && let Some(data) =
                    _pointer.data::<smithay_client_toolkit::seat::pointer::PointerData>()
            {
                self.activation.input(serial, data.seat(), &event.surface);
            }
            if let PointerEventKind::Press { serial, .. }
            | PointerEventKind::Release { serial, .. } = event.kind
            {
                self.clipboard.serial(serial);
            }
            let object = self.task_for_surface(&event.surface);
            match event.kind {
                PointerEventKind::Press { button, time, .. } if button == LEFT_BUTTON => {
                    if self.pointer_contact.is_some() {
                        self.retire_contacts(None);
                    }
                    let Some(object) = object else { continue };
                    let (x_fixed, y_fixed) = self.fixed_position(object, event.position);
                    let contact = Contact {
                        object,
                        pointer_id: u32::MAX,
                        position_fixed: (x_fixed, y_fixed),
                    };
                    let sent =
                        self.input_time(input_time::Device::Pointer, time)
                            .and_then(|time| {
                                self.send_input(object, contact.event(TouchAction::Down), time)
                            });
                    match sent {
                        Err(error) => self.fail(&error),
                        Ok(true) => self.pointer_contact = Some(contact),
                        Ok(false) => {}
                    }
                }
                PointerEventKind::Motion { time } => {
                    let Some(mut contact) = self.pointer_contact else {
                        continue;
                    };
                    // Coordinates belong to event.surface, not a newly focused task.
                    if object == Some(contact.object) {
                        contact.position_fixed =
                            self.fixed_position(contact.object, event.position);
                    }
                    let sent =
                        self.input_time(input_time::Device::Pointer, time)
                            .and_then(|time| {
                                self.send_input(
                                    contact.object,
                                    contact.event(TouchAction::Motion),
                                    time,
                                )
                            });
                    match sent {
                        Err(error) => self.fail(&error),
                        Ok(true) => self.pointer_contact = Some(contact),
                        Ok(false) => {}
                    }
                }
                PointerEventKind::Release { button, time, .. } if button == LEFT_BUTTON => {
                    let Some(mut contact) = self.pointer_contact.take() else {
                        continue;
                    };
                    if object == Some(contact.object) {
                        contact.position_fixed =
                            self.fixed_position(contact.object, event.position);
                    }
                    if let Err(error) =
                        self.input_time(input_time::Device::Pointer, time)
                            .and_then(|time| {
                                self.send_input(
                                    contact.object,
                                    contact.event(TouchAction::Up),
                                    time,
                                )
                            })
                    {
                        self.fail(&error);
                    }
                }
                _ => {}
            }
        }
    }
}

impl TouchHandler for App {
    fn down(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &wl_touch::WlTouch,
        _serial: u32,
        time: u32,
        surface: wl_surface::WlSurface,
        id: i32,
        position: (f64, f64),
    ) {
        if self.touch.as_ref() != Some(_touch) {
            return;
        }
        if id < 0 || self.touch_contacts.contains_key(&id) {
            self.retire_contacts(None);
        }
        let Some(object) = self.task_for_surface(&surface) else {
            return;
        };
        let Ok(pointer_id) = u32::try_from(id) else {
            return;
        };
        self.text_input.note_touch();
        if let Some(data) = _touch.data::<smithay_client_toolkit::seat::touch::TouchData>() {
            self.activation.input(_serial, data.seat(), &surface);
        }
        let (x_fixed, y_fixed) = self.fixed_position(object, position);
        let contact = Contact {
            object,
            pointer_id,
            position_fixed: (x_fixed, y_fixed),
        };
        let sent = self
            .input_time(input_time::Device::Touch, time)
            .and_then(|time| self.send_input(object, contact.event(TouchAction::Down), time));
        match sent {
            Err(error) => self.fail(&error),
            Ok(true) => {
                self.touch_contacts.insert(id, contact);
            }
            Ok(false) => {}
        }
    }

    fn up(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &wl_touch::WlTouch,
        _serial: u32,
        time: u32,
        id: i32,
    ) {
        if self.touch.as_ref() != Some(_touch) {
            return;
        }
        let Some(contact) = self.touch_contacts.remove(&id) else {
            return;
        };
        // wl_touch.up carries no coordinates. Android still needs the last
        // position; substituting (0, 0) turns a button tap into a release outside it.
        if let Err(error) = self
            .input_time(input_time::Device::Touch, time)
            .and_then(|time| self.send_input(contact.object, contact.event(TouchAction::Up), time))
        {
            self.fail(&error);
        }
    }

    fn motion(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &wl_touch::WlTouch,
        time: u32,
        id: i32,
        position: (f64, f64),
    ) {
        if self.touch.as_ref() != Some(_touch) {
            return;
        }
        let Some(mut contact) = self.touch_contacts.get(&id).copied() else {
            return;
        };
        contact.position_fixed = self.fixed_position(contact.object, position);
        let sent = self
            .input_time(input_time::Device::Touch, time)
            .and_then(|time| {
                self.send_input(contact.object, contact.event(TouchAction::Motion), time)
            });
        match sent {
            Err(error) => self.fail(&error),
            Ok(true) => {
                self.touch_contacts.insert(id, contact);
            }
            Ok(false) => {}
        }
    }

    fn shape(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &wl_touch::WlTouch,
        _id: i32,
        _major: f64,
        _minor: f64,
    ) {
    }

    fn orientation(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &wl_touch::WlTouch,
        _id: i32,
        _orientation: f64,
    ) {
    }

    fn cancel(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _touch: &wl_touch::WlTouch) {
        if self.touch.as_ref() != Some(_touch) {
            return;
        }
        self.retire_contacts(None);
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState, frame_timeline::Registry];
}

smithay_client_toolkit::delegate_compositor!(App);
smithay_client_toolkit::delegate_output!(App);
smithay_client_toolkit::delegate_xdg_shell!(App);
smithay_client_toolkit::delegate_xdg_window!(App);
smithay_client_toolkit::delegate_dmabuf!(App);
wayland_client::delegate_dispatch!(App: [WpPresentationFeedback: smithay_client_toolkit::presentation_time::PresentationTimeData] => PresentationTimeState);
smithay_client_toolkit::delegate_seat!(App);
smithay_client_toolkit::delegate_keyboard!(App);
smithay_client_toolkit::delegate_pointer!(App);
smithay_client_toolkit::delegate_touch!(App);
smithay_client_toolkit::delegate_registry!(App);

wayland_client::delegate_noop!(App: ignore WpLinuxDrmSyncobjManagerV1);
wayland_client::delegate_noop!(App: ignore WpLinuxDrmSyncobjSurfaceV1);
wayland_client::delegate_noop!(App: ignore WpLinuxDrmSyncobjTimelineV1);
wayland_client::delegate_noop!(App: ignore WpFractionalScaleManagerV1);
wayland_client::delegate_noop!(App: ignore WpViewporter);
wayland_client::delegate_noop!(App: ignore WpViewport);
wayland_client::delegate_noop!(App: ignore ZwlrOutputPowerManagerV1);
wayland_client::delegate_noop!(App: ignore ZwpIdleInhibitManagerV1);
wayland_client::delegate_noop!(App: ignore ZwpIdleInhibitorV1);

fn main() {
    eprintln!("DROIDLOOM_DIRECT_INPUT_ABI=1;");
    eprintln!("DROIDLOOM_HOST_CHANNELS_ABI=1;");
    // SAFETY: the process is still single-threaded. Mesa consumes this optional
    // worker policy after its own full-affinity reset, before executing jobs.
    if let Some(groups) =
        droidloom_cpu_placement::initialize(droidloom_cpu_placement::Role::Graphics)
    {
        unsafe {
            env::set_var("MESA_BACKGROUND_CPUS", groups.background.list());
        }
    }
    if let Err(error) = run() {
        eprintln!("droidloom-wayland: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), PresenterError> {
    let runtime_root = PathBuf::from(
        env::var("XDG_RUNTIME_DIR")
            .map_err(|_| PresenterError::Configuration("XDG_RUNTIME_DIR is missing"))?,
    );
    let socket_path = env::var_os("DROIDLOOM_HOST_SOCKET").map_or_else(
        || runtime_root.join("droidloom/native-bridge.sock"),
        PathBuf::from,
    );
    let render_node = env::var_os("DROIDLOOM_RENDER_NODE")
        .map_or_else(|| PathBuf::from("/dev/dri/renderD128"), PathBuf::from);
    validate_render_node(&render_node)?;
    let identity = fs::metadata("/proc/self")?;
    prepare_socket_path(&socket_path, &runtime_root, identity.uid())?;
    // The async reactor may be created synchronously by Bridge::start.
    droidloom_cpu_placement::current(droidloom_cpu_placement::Role::Background);
    let host_grants = activation::HostGrants::default();
    let notifications = notifications::Bridge::start(
        socket_path.with_file_name("notifications.sock"),
        host_grants.clone(),
    );
    droidloom_cpu_placement::current(droidloom_cpu_placement::Role::Graphics);
    let _notifications = notifications?;

    let render_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&render_node)?;
    let gbm = GbmDevice::new(render_file.try_clone()?)?;
    let syncobj = SyncobjDevice::from_file(render_file);
    let expected_peer = ExpectedPeer {
        pid: parse_env_u32("DROIDLOOM_HOST_PEER_PID", 0)?,
        uid: parse_env_u32("DROIDLOOM_HOST_PEER_UID", identity.uid())?,
        gid: parse_env_u32("DROIDLOOM_HOST_PEER_GID", identity.gid())?,
    };
    let listener = DenialEndpointListener::bind(&socket_path, syncobj.clone(), expected_peer)?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o666))?;
    listener.set_nonblocking(true)?;
    let session_mode = env::var("DROIDLOOM_MODE")
        .unwrap_or_else(|_| "desktop".into())
        .parse::<SessionMode>()
        .map_err(|_| PresenterError::Configuration("DROIDLOOM_MODE must be desktop or mobile"))?;
    eprintln!("Droidloom session mode: {session_mode}");
    let window_policy = WindowPolicyPaths::from_environment(
        env::var_os("XDG_CONFIG_HOME"),
        env::var_os("XDG_STATE_HOME"),
        env::var_os("HOME"),
    )
    .and_then(WindowPolicyStore::load)
    .unwrap_or_else(|error| {
        eprintln!("Droidloom is using an in-memory window policy: {error}");
        WindowPolicyStore::ephemeral()
    })
    .with_session_mode(session_mode);

    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();
    let compositor = CompositorState::bind(&globals, &qh)?;
    let xdg_shell = XdgShell::bind(&globals, &qh)?;
    let viewporter: WpViewporter = globals.bind(&qh, 1..=1, ())?;
    let fractional_scale_manager: WpFractionalScaleManagerV1 = globals.bind(&qh, 1..=1, ())?;
    let dmabuf_state = DmabufState::new(&globals, &qh);
    let presentation_time = PresentationTimeState::bind(&globals, &qh);
    if !matches!(dmabuf_state.version(), Some(4..)) {
        return Err(PresenterError::DmabufV4Required);
    }
    let sync_manager: WpLinuxDrmSyncobjManagerV1 = globals.bind(&qh, 1..=1, ())?;
    let output_power_manager: ZwlrOutputPowerManagerV1 = globals.bind(&qh, 1..=1, ())?;
    let mut app = App {
        activation: activation::Activation::new(&globals, &qh, host_grants),
        layer_globals: layers::Globals {
            subcompositor: globals.bind(&qh, 1..=1, ())?,
            shm: globals.bind(&qh, 1..=1, ())?,
            alpha: globals.bind(&qh, 1..=1, ()).ok(),
        },
        clipboard: clipboard::Clipboard::new(
            socket_path.with_file_name("clipboard.sock"),
            &globals,
            &qh,
        )?,
        clipboard_inputs: VecDeque::new(),
        text_input: text_input::TextInput::new(
            socket_path.with_file_name("text-input.sock"),
            globals.bind(&qh, 1..=1, ()).ok(),
        )?,
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        output_power_manager,
        output_power: Vec::new(),
        idle_inhibit_manager: globals.bind(&qh, 1..=1, ()).ok(),
        host_display_powered: None,
        published_display_power: None,
        presentation_time,
        presentation_clock: None,
        seat_state: SeatState::new(&globals, &qh),
        dmabuf_state,
        feedback: None,
        compositor,
        xdg_shell,
        insets_manager: globals.bind(&qh, 1..=1, ()).ok(),
        frame_timeline_manager: globals.bind(&qh, 1..=1, ()).ok(),
        clock: clock::Clock::new(),
        viewporter,
        fractional_scale_manager,
        sync_manager,
        syncobj,
        gbm,
        listener,
        window_policy,
        densities: density::Densities::load(env::var_os("XDG_CONFIG_HOME"), env::var_os("HOME")),
        endpoint: None,
        tasks: BTreeMap::new(),
        task_work: readiness::Tasks::default(),
        keyboard: None,
        pointer: None,
        touch: None,
        focused: None,
        pointer_contact: None,
        touch_contacts: BTreeMap::new(),
        motion_times: input_time::Stream::default(),
        key_times: input_time::Stream::default(),
        input_timestamps: input_time::Timestamps::new(globals.bind(&qh, 1..=1, ()).ok()),
        next_buffer_id: 0,
        next_input_serial: 0,
        direct_input: input::DirectInput::default(),
        socket_path,
        fatal: None,
    };
    app.text_input.bind_panel_feedback(&globals, &qh);
    app.dmabuf_state.get_default_feedback(&qh)?;
    while app.feedback.is_none() || app.host_canvas_size().is_none() {
        event_queue.blocking_dispatch(&mut app)?;
    }
    notify_ready()?;

    run_presenter_loop(&conn, event_queue, app)
}

fn run_presenter_loop(
    conn: &Connection,
    mut event_queue: EventQueue<App>,
    mut app: App,
) -> Result<(), PresenterError> {
    let qh = event_queue.handle();
    let mut poll_descriptors = Vec::new();
    let mut poll_owners = Vec::new();
    let mut layer_tasks = Vec::new();
    loop {
        event_queue.dispatch_pending(&mut app)?;
        app.task_work.notify_fallback(Instant::now());
        app.unmap_requested_tasks()?;
        if let Some(error) = app.fatal.take() {
            return Err(PresenterError::Wayland(error));
        }
        app.apply_pending_resizes(&qh)?;
        app.accept_endpoint()?;
        app.drain_endpoint(&qh)?;
        app.refresh_frame_timelines(&qh)?;
        app.flush_timeline_frames(&qh)?;
        app.pump_direct_input()?;
        app.drain_layers(&qh, &mut layer_tasks)?;
        app.pump_text_input();
        app.clipboard.pump(&qh);
        if !app.clipboard.blocked() {
            while let Some((object, input, timestamp)) = app.clipboard_inputs.pop_front() {
                if app.tasks.contains_key(&object) {
                    if let Err(error) = app.send_input(object, input, timestamp) {
                        eprintln!("Droidloom queued input: {error}");
                    }
                }
            }
        }
        app.unmap_requested_tasks()?;
        app.release_ready_frames()?;
        poll_sources(
            conn,
            &mut event_queue,
            &mut app,
            &mut poll_descriptors,
            &mut poll_owners,
        )?;
    }
}

fn notify_ready() -> Result<(), PresenterError> {
    let Some(address) = env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    let socket = UnixDatagram::unbound()?;
    let payload = b"READY=1\nSTATUS=Connected to Wayland; ready to start Android";
    if let Some(name) = address.as_bytes().strip_prefix(b"@") {
        let address = SocketAddr::from_abstract_name(name)?;
        socket.send_to_addr(payload, &address)?;
    } else {
        socket.send_to(payload, Path::new(&address))?;
    }
    Ok(())
}

fn poll_sources(
    conn: &Connection,
    event_queue: &mut EventQueue<App>,
    app: &mut App,
    descriptors: &mut Vec<libc::pollfd>,
    owners: &mut Vec<readiness::Source>,
) -> Result<(), PresenterError> {
    let write_blocked = match event_queue.flush() {
        Ok(()) => {
            for object in &app.task_work.frames {
                if let Some(task) = app.tasks.get_mut(object) {
                    for pending in &mut task.pending {
                        if let Some(trace) = pending.trace.as_mut() {
                            trace.flushed();
                        }
                    }
                }
            }
            false
        }
        Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => true,
        Err(error) => return Err(PresenterError::Wayland(error.to_string())),
    };
    let now = Instant::now();
    app.report_stalled_scenes(now);
    let fallback_deadline = app.task_work.deadline(now, RELEASE_POLL_FALLBACK);
    let stall_deadline = app.stalled_scene_deadline();
    // Pending Wayland events must be dispatched before sleeping. The previous
    // fixed timeout masked a missed wakeup here when the socket was drained
    // by another reader but our event queue still contained callbacks.
    let Some(read_guard) = event_queue.prepare_read() else {
        return Ok(());
    };
    descriptors.clear();
    owners.clear();
    descriptors.extend([
        libc::pollfd {
            fd: conn.as_fd().as_raw_fd(),
            events: libc::POLLIN | if write_blocked { libc::POLLOUT } else { 0 },
            revents: 0,
        },
        libc::pollfd {
            fd: app.listener.listener().as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ]);
    if let Some(endpoint) = app.endpoint.as_ref() {
        descriptors.push(libc::pollfd {
            fd: endpoint.socket().socket().as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
    }
    if let Some(fd) = app.densities.fd() {
        descriptors.push(libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
    }
    for (fd, events) in app.text_input.fds().chain(app.clipboard.fds()) {
        descriptors.push(libc::pollfd {
            fd,
            events,
            revents: 0,
        });
    }
    if let Some((fd, events)) = app.direct_input.fd() {
        descriptors.push(libc::pollfd {
            fd,
            events,
            revents: 0,
        });
    }
    owners.resize(descriptors.len(), readiness::Source::Other);
    for object in &app.task_work.streams {
        let Some(stream) = app.tasks.get(object).and_then(|t| t.layers.as_ref()) else {
            continue;
        };
        for (fd, work) in stream.fds() {
            descriptors.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
            owners.push(readiness::Source::Task {
                object: *object,
                work,
                eventfd: work == readiness::RELEASES,
            });
        }
    }
    for object in &app.task_work.frames {
        let Some(task) = app.tasks.get(object) else {
            continue;
        };
        for pending in &task.pending {
            let Some(target) = task.targets.get(&pending.buffer) else {
                continue;
            };
            if let Some(event) = target
                .sync
                .as_ref()
                .and_then(|sync| sync.release_wakeup.as_ref())
            {
                descriptors.push(libc::pollfd {
                    fd: event.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
                owners.push(readiness::Source::Task {
                    object: *object,
                    work: readiness::FRAMES,
                    eventfd: true,
                });
            }
        }
    }
    let clipboard_deadline = (!app.clipboard_inputs.is_empty())
        .then(|| app.clipboard.unblock_deadline())
        .flatten();
    let deadline = clipboard_deadline
        .into_iter()
        .chain(fallback_deadline)
        .chain(stall_deadline)
        .min();
    let timeout = if app.task_work.pending() {
        0 // Flush Wayland first, then service work queued during this turn.
    } else {
        poll_timeout_millis(deadline, now)
    };
    // SAFETY: `descriptors` is live writable storage for exactly its length;
    // poll retains no pointer after returning.
    let result = unsafe {
        libc::poll(
            descriptors.as_mut_ptr(),
            libc::nfds_t::try_from(descriptors.len()).unwrap_or(libc::nfds_t::MAX),
            timeout,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
    app.clipboard.notify_ready(descriptors);
    let densities = app.densities.fd();
    if descriptors
        .iter()
        .any(|event| Some(event.fd) == densities && event.revents & libc::POLLIN != 0)
        && app.densities.changed()
    {
        let objects: Vec<TaskObjectId> = app.tasks.keys().copied().collect();
        for object in objects {
            app.update_task_density(object)?;
        }
    }
    app.direct_input.notify_ready(descriptors);
    if app
        .endpoint
        .as_ref()
        .is_some_and(DenialEndpoint::direct_input)
        && app.direct_input.fd().is_none()
    {
        app.clear_contacts();
    }
    app.task_work.notify_fallback(Instant::now());
    for (event, owner) in descriptors.iter().zip(owners.iter().copied()) {
        if owner.notify(&mut app.task_work, event.revents) {
            let mut count = 0_u64;
            // SAFETY: all entries in this slice borrow live nonblocking
            // eventfds; count is writable storage for the required eight bytes.
            let result = unsafe {
                libc::read(
                    event.fd,
                    std::ptr::from_mut(&mut count).cast(),
                    size_of::<u64>(),
                )
            };
            if result < 0 {
                let error = io::Error::last_os_error();
                if !matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    return Err(error.into());
                }
            }
            // The next loop checks the real timeline and presentation feedback.
            // Draining here prevents spinning while feedback is still pending.
        }
    }
    if descriptors[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
        match read_guard.read() {
            Ok(_) => {}
            Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(PresenterError::Wayland(error.to_string())),
        }
    } else {
        drop(read_guard);
    }
    Ok(())
}

fn poll_timeout_millis(deadline: Option<Instant>, now: Instant) -> i32 {
    let timeout = deadline.map(|deadline| deadline.saturating_duration_since(now));
    // Round up so sub-millisecond deadlines cannot turn into a busy loop.
    timeout.map_or(-1, |duration| {
        i32::try_from(duration.as_nanos().div_ceil(1_000_000)).unwrap_or(i32::MAX)
    })
}

fn split_point(point: u64) -> (u32, u32) {
    (
        u32::try_from(point >> 32).unwrap_or_default(),
        u32::try_from(point & u64::from(u32::MAX)).unwrap_or_default(),
    )
}

fn monotonic_timestamp_nanos() -> Result<u64, PresenterError> {
    let mut timestamp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `timestamp` is valid writable storage and CLOCK_MONOTONIC needs
    // no additional lifetime or ownership guarantees.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut timestamp) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let seconds = u64::try_from(timestamp.tv_sec)
        .map_err(|_| PresenterError::Configuration("monotonic clock returned negative seconds"))?;
    let nanos = u64::try_from(timestamp.tv_nsec)
        .map_err(|_| PresenterError::Configuration("monotonic clock returned negative nanos"))?;
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanos))
        .ok_or(PresenterError::Configuration(
            "monotonic timestamp overflowed nanoseconds",
        ))
}

fn presentation_timestamp_nanos(seconds: u64, nanos: u32) -> Result<u64, PresenterError> {
    if nanos >= 1_000_000_000 {
        return Err(PresenterError::Configuration(
            "presentation timestamp nanoseconds are invalid",
        ));
    }
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(u64::from(nanos)))
        .ok_or(PresenterError::Configuration(
            "presentation timestamp overflowed nanoseconds",
        ))
}

fn presentation_refresh_period(
    presentation_refresh_nanos: u32,
    advertised_refresh_millihz: u32,
) -> Result<u64, PresenterError> {
    if presentation_refresh_nanos != 0 {
        return Ok(u64::from(presentation_refresh_nanos));
    }
    if advertised_refresh_millihz == 0 {
        return Err(PresenterError::Configuration(
            "presentation feedback omitted refresh period",
        ));
    }
    Ok(1_000_000_000_000_u64 / u64::from(advertised_refresh_millihz))
}

#[allow(clippy::cast_possible_truncation)]
fn fixed_16_16(value: f64) -> i32 {
    let scaled = (value * 65_536.0).round();
    if scaled <= f64::from(i32::MIN) {
        i32::MIN
    } else if scaled >= f64::from(i32::MAX) {
        i32::MAX
    } else {
        scaled as i32
    }
}

fn scale_dimension(logical: u32, scale_120: u32) -> Result<u32, PresenterError> {
    if scale_120 == 0 {
        return Err(PresenterError::Configuration(
            "fractional scale numerator is zero",
        ));
    }
    let scaled = (u64::from(logical) * u64::from(scale_120)
        + u64::from(FRACTIONAL_SCALE_DENOMINATOR / 2))
        / u64::from(FRACTIONAL_SCALE_DENOMINATOR);
    let scaled = u32::try_from(scaled)
        .map_err(|_| PresenterError::Configuration("scaled window dimension overflowed"))?
        .max(1);
    if scaled > MAX_BUFFER_DIMENSION {
        return Err(PresenterError::Configuration(
            "scaled window dimension exceeds protocol limit",
        ));
    }
    Ok(scaled)
}

fn oriented_output_size(
    dimensions: (i32, i32),
    transform: wl_output::Transform,
) -> Option<(u32, u32)> {
    let width = u32::try_from(dimensions.0)
        .ok()
        .filter(|value| *value != 0)?;
    let height = u32::try_from(dimensions.1)
        .ok()
        .filter(|value| *value != 0)?;
    let (width, height) = match transform {
        wl_output::Transform::_90
        | wl_output::Transform::_270
        | wl_output::Transform::Flipped90
        | wl_output::Transform::Flipped270 => (height, width),
        _ => (width, height),
    };
    (width <= MAX_BUFFER_DIMENSION && height <= MAX_BUFFER_DIMENSION).then_some((width, height))
}

fn parse_env_u32(name: &str, default: u32) -> Result<u32, PresenterError> {
    env::var(name).map_or(Ok(default), |value| {
        value
            .parse()
            .map_err(|_| PresenterError::Configuration("peer identity is not an integer"))
    })
}

fn validate_render_node(path: &Path) -> Result<(), PresenterError> {
    let valid_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("renderD"))
        .is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        });
    if path.parent() == Some(Path::new("/dev/dri")) && valid_name {
        Ok(())
    } else {
        Err(PresenterError::Configuration(
            "render node must be /dev/dri/renderD<number>",
        ))
    }
}

fn prepare_socket_path(
    socket: &Path,
    runtime_root: &Path,
    effective_uid: u32,
) -> Result<(), PresenterError> {
    if !socket.is_absolute() || !runtime_root.is_absolute() {
        return Err(PresenterError::Configuration(
            "runtime and endpoint paths must be absolute",
        ));
    }
    let Some(parent) = socket.parent() else {
        return Err(PresenterError::Configuration("endpoint has no parent"));
    };
    if parent.parent() != Some(runtime_root) {
        return Err(PresenterError::Configuration(
            "endpoint must be one directory below XDG_RUNTIME_DIR",
        ));
    }
    match fs::create_dir(parent) {
        Ok(()) => fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != effective_uid
        || metadata.mode() & 0o077 != 0
    {
        return Err(PresenterError::Configuration(
            "runtime directory is not private and session-owned",
        ));
    }
    match fs::symlink_metadata(socket) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(socket)?,
        Ok(_) => {
            return Err(PresenterError::Configuration(
                "refusing to replace a non-socket endpoint",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn remove_owned_socket(socket: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(socket)
        && metadata.file_type().is_socket()
    {
        let _ = fs::remove_file(socket);
    }
}

fn endpoint_would_block(error: &EndpointError) -> bool {
    matches!(
        error,
        EndpointError::Ipc(IpcError::Io(source)) if source.kind() == io::ErrorKind::WouldBlock
    )
}

fn listener_would_block(error: &EndpointListenerError) -> bool {
    matches!(
        error,
        EndpointListenerError::Ipc(IpcError::Io(source))
            if source.kind() == io::ErrorKind::WouldBlock
    )
}

// Use the presentation clock's output when known. Until feedback arrives,
// the most recently entered output is deterministic; unmapped tasks use 60 Hz.
fn select_surface_refresh<T: PartialEq>(
    entered: &[T],
    presented: Option<&T>,
    mut refresh: impl FnMut(&T) -> Option<u32>,
) -> u32 {
    select_surface_output(entered, presented, &mut refresh)
        .and_then(refresh)
        .unwrap_or(60_000)
}

/// The output whose refresh a surface takes: where it is presented, else the
/// most recently entered output with a known refresh.
fn select_surface_output<'a, T: PartialEq>(
    entered: &'a [T],
    presented: Option<&'a T>,
    mut refresh: impl FnMut(&T) -> Option<u32>,
) -> Option<&'a T> {
    presented
        .filter(|output| entered.contains(output) && refresh(output).is_some())
        .or_else(|| {
            entered
                .iter()
                .rev()
                .find(|output| refresh(output).is_some())
        })
}

fn aggregate_display_power(states: impl IntoIterator<Item = Option<bool>>) -> Option<bool> {
    let mut saw_output = false;
    let mut unknown = false;
    for state in states {
        saw_output = true;
        match state {
            Some(true) => return Some(true),
            Some(false) => (),
            None => unknown = true,
        }
    }
    // One known powered output is enough to keep Android awake. Sleep only
    // when every remaining output explicitly reports off; unknown/empty is
    // not evidence of screen-off.
    (saw_output && !unknown).then_some(false)
}

#[cfg(test)]
mod output_power_test;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contact_retirement_clears_shared_task_routes_pointer_and_deferred_touches() {
        let first = Contact {
            object: TaskObjectId(1),
            pointer_id: 0,
            position_fixed: (10, 20),
        };
        let second = Contact {
            object: TaskObjectId(2),
            pointer_id: 1,
            ..first
        };
        let mouse = Contact {
            pointer_id: u32::MAX,
            ..first
        };
        let mut pointer = Some(mouse);
        let mut touches = BTreeMap::from([(0, first), (1, second)]);
        let key = InputEvent::Key {
            action: KeyAction::Down,
            keycode: 30,
            repeat: 0,
        };
        let mut deferred = VecDeque::from([
            (first.object, first.event(TouchAction::Down), 1),
            (second.object, second.event(TouchAction::Motion), 2),
            (mouse.object, mouse.event(TouchAction::Up), 3),
            (first.object, key.clone(), 4),
        ]);
        clear_contact_state(&mut pointer, &mut touches, &mut deferred);
        assert!(pointer.is_none() && touches.is_empty());
        assert_eq!(deferred, VecDeque::from([(first.object, key, 4)]));
        // Late UP/MOTION callbacks cannot find an old owner after reconnect,
        // surface removal or seat/capability retirement.
        assert!(touches.remove(&0).is_none());
        assert!(touches.get(&1).is_none());
        touches.insert(0, first);
        pointer = Some(mouse);
        assert_eq!(touches[&0].object, first.object);
        assert_eq!(pointer.unwrap().pointer_id, u32::MAX);
    }

    #[test]
    fn display_power_sleeps_only_when_all_outputs_are_known_off() {
        assert_eq!(aggregate_display_power([]), None);
        assert_eq!(aggregate_display_power([None, Some(true)]), Some(true));
        assert_eq!(aggregate_display_power([Some(true), None]), Some(true));
        assert_eq!(aggregate_display_power([Some(false), None]), None);
        assert_eq!(
            aggregate_display_power([Some(false), Some(false)]),
            Some(false)
        );
        assert_eq!(
            aggregate_display_power([Some(false), Some(true)]),
            Some(true)
        );
    }

    #[test]
    fn refresh_tracks_surface_membership_presentation_moves_and_mode_changes() {
        let mode = |output: &u8| match output {
            1 => Some(240_000),
            2 => Some(143_973),
            _ => None,
        };
        assert_eq!(select_surface_refresh(&[2], None, mode), 143_973);
        assert_eq!(select_surface_refresh(&[1, 2], Some(&1), mode), 240_000);
        assert_eq!(select_surface_refresh(&[1, 2], Some(&2), mode), 143_973);
        assert_eq!(select_surface_refresh(&[2], Some(&1), mode), 143_973);
        assert_eq!(select_surface_refresh(&[1, 2], None, mode), 143_973);
        assert_eq!(
            select_surface_refresh(&[2], Some(&2), |_| Some(120_000)),
            120_000
        );
        assert_eq!(select_surface_refresh(&[2], Some(&2), |_| None), 60_000);
        assert_eq!(select_surface_refresh(&[], Some(&1), mode), 60_000);
    }

    #[test]
    fn idle_wait_has_no_periodic_timer() {
        let now = Instant::now();
        assert_eq!(poll_timeout_millis(None, now), -1);
        let deadline = Some(now + Duration::from_secs(10));
        assert_eq!(poll_timeout_millis(deadline, now), 10_000);
    }

    #[test]
    fn clipboard_deadline_wakes_even_without_socket_traffic() {
        let now = Instant::now();
        assert_eq!(poll_timeout_millis(Some(now), now), 0);
        assert_eq!(
            poll_timeout_millis(Some(now + Duration::from_micros(50)), now),
            1
        );
        assert_eq!(
            poll_timeout_millis(Some(now - Duration::from_millis(1)), now),
            0
        );
    }

    #[test]
    fn touch_tap_releases_at_down_position_without_motion() {
        let contact = Contact {
            object: TaskObjectId(1),
            pointer_id: 7,
            position_fixed: (123 * 256 + 64, 456 * 256 + 128),
        };
        for action in [TouchAction::Down, TouchAction::Up] {
            assert_eq!(
                contact.event(action),
                InputEvent::Touch {
                    action,
                    pointer_id: 7,
                    x_fixed: 123 * 256 + 64,
                    y_fixed: 456 * 256 + 128,
                    pressure: if action == TouchAction::Up {
                        0
                    } else {
                        u16::MAX
                    },
                }
            );
        }
    }

    #[test]
    fn touch_drag_releases_at_latest_position_independently_per_finger() {
        let mut first = Contact {
            object: TaskObjectId(1),
            pointer_id: 0,
            position_fixed: (100 * 256, 200 * 256),
        };
        let second = Contact {
            pointer_id: 1,
            position_fixed: (300 * 256, 400 * 256),
            ..first
        };
        first.position_fixed = (-10 * 256, 500 * 256);
        for (contact, pointer_id, x_fixed, y_fixed) in [
            (first, 0, -10 * 256, 500 * 256),
            (second, 1, 300 * 256, 400 * 256),
        ] {
            assert_eq!(
                contact.event(TouchAction::Up),
                InputEvent::Touch {
                    action: TouchAction::Up,
                    pointer_id,
                    x_fixed,
                    y_fixed,
                    pressure: 0,
                }
            );
        }
    }

    #[test]
    fn fractional_scale_rounds_logical_dimensions_to_physical_pixels() {
        assert_eq!(scale_dimension(480, 132).unwrap(), 528);
        assert_eq!(scale_dimension(800, 132).unwrap(), 880);
        assert_eq!(scale_dimension(481, 120).unwrap(), 481);
        assert_eq!(scale_dimension(1, 180).unwrap(), 2);
    }

    #[test]
    fn fractional_scale_rejects_invalid_or_oversized_dimensions() {
        assert!(scale_dimension(480, 0).is_err());
        assert!(scale_dimension(MAX_BUFFER_DIMENSION, 240).is_err());
    }

    #[test]
    fn android_canvas_uses_oriented_physical_output_pixels() {
        assert_eq!(
            oriented_output_size((1264, 2780), wl_output::Transform::Normal),
            Some((1264, 2780))
        );
        assert_eq!(
            oriented_output_size((2780, 1264), wl_output::Transform::_90),
            Some((1264, 2780))
        );
        assert_eq!(
            oriented_output_size((0, 2780), wl_output::Transform::Normal),
            None
        );
    }

    #[test]
    fn presentation_timing_uses_host_values_and_refresh_fallback() {
        assert_eq!(
            presentation_timestamp_nanos(42, 123_456_789).unwrap(),
            42_123_456_789
        );
        assert_eq!(
            presentation_refresh_period(8_333_333, 120_000).unwrap(),
            8_333_333
        );
        assert_eq!(presentation_refresh_period(0, 120_000).unwrap(), 8_333_333);
        assert!(presentation_timestamp_nanos(0, 1_000_000_000).is_err());
        assert!(presentation_refresh_period(0, 0).is_err());
    }
}
