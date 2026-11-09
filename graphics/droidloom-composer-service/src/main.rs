//! Android vendor Composer V5 service entry point for Droidloom.

#![forbid(unsafe_code)]

mod control;
mod input_handoff;
mod surface_bridge;

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use binder::ProcessState;
use droidloom_composer::denial::AppliedDenialEvent;
use droidloom_composer::hwc3::Session;
use droidloom_composer_aidl::denial::DenialPresentationSink;
use droidloom_composer_aidl::minigbm::MinigbmBufferAdapter;
use droidloom_composer_aidl::service::{ComposerLifecycle, ComposerService, SharedSession};
use droidloom_denial_ipc::{ProtocolSocket, SeqPacket, SeqPacketListener};
use droidloom_denial_protocol::{
    AndroidMessage, DenialMessage, PROTOCOL_MAJOR, TaskObjectId, Visibility, capability,
};
use droidloom_syncobj::SyncobjDevice;

use crate::control::TaskCoordinator;
use crate::input_handoff::InputHandoff;
use crate::surface_bridge::SurfaceBridge;

const DEFAULT_SOCKET: &str = "/dev/socket/droidloom/denial";
const DEFAULT_RENDER_NODE: &str = "/dev/dri/renderD128";
const CONTROL_SOCKET_NAME: &str = "droidloom-task-control";
const SURFACE_SOCKET_NAME: &str = "droidloom-surfaceflinger";
const INPUT_SOCKET: &str = "/dev/socket/droidloom-framework/input";
const SERVICE_NAME: &str = "android.hardware.graphics.composer3.IComposer/default";
const BOOTSTRAP_PACKAGE: &str = "android.droidloom.bootstrap";

fn main() {
    // Keep metadata in the executable itself, tied to the linked wire encoder.
    let compatibility = std::str::from_utf8(&droidloom_input_protocol::BUILD_COMPATIBILITY)
        .expect("input compatibility marker is ASCII");
    if std::env::args().any(|arg| arg == "--compatibility") {
        println!(
            "{compatibility} DROIDLOOM_TASK_BACKEND_ABI=3; DROIDLOOM_DIRECT_INPUT_ABI=1; DROIDLOOM_NATIVE_INPUT_ABI=2;"
        );
        return;
    }
    eprintln!(
        "Droidloom Composer {compatibility} DROIDLOOM_TASK_BACKEND_ABI=3; DROIDLOOM_DIRECT_INPUT_ABI=1; DROIDLOOM_NATIVE_INPUT_ABI=2;"
    );
    if let Err(error) = run() {
        eprintln!("Droidloom Composer fatal: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let (socket_path, render_node) = arguments()?;
    let control_listener = SeqPacketListener::from_android_init_socket(CONTROL_SOCKET_NAME)
        .map_err(|error| format!("adopt Android init task-control socket: {error}"))?;
    let surface_listener = SeqPacketListener::from_android_init_socket(SURFACE_SOCKET_NAME)
        .map_err(|error| format!("adopt Android init SurfaceFlinger socket: {error}"))?;
    let render_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&render_node)
        .map_err(|error| format!("open render node {}: {error}", render_node.display()))?;
    let socket = ProtocolSocket::new(
        SeqPacket::connect(&socket_path)
            .map_err(|error| format!("connect {}: {error}", socket_path.display()))?,
    );
    let capabilities = handshake(&socket)?;

    let sink = DenialPresentationSink::new(socket, SyncobjDevice::from_file(render_file))
        .with_activation_requests(capabilities & capability::TASK_ACTIVATION != 0)
        .with_lazy_render_targets(capabilities & capability::LAZY_RENDER_TARGETS != 0)
        .with_keep_screen_on(capabilities & capability::KEEP_SCREEN_ON != 0);
    let session: SharedSession = Arc::new(Mutex::new(Session::default()));
    let factory_sink = sink.clone();
    let service = ComposerService::new(
        Arc::clone(&session),
        move || MinigbmBufferAdapter::new(factory_sink.clone()),
        capabilities & capability::HOST_CLOCK != 0,
    );
    let lifecycle = service.lifecycle();
    let coordinator = TaskCoordinator::new(sink.clone(), lifecycle.clone());
    let input = InputHandoff::new(sink.clone(), INPUT_SOCKET);

    ProcessState::set_thread_pool_max_thread_count(4);
    ProcessState::start_thread_pool();
    let service = service.into_binder();
    binder::add_service(SERVICE_NAME, service.as_binder())
        .map_err(|error| format!("register {SERVICE_NAME}: {error:?}"))?;

    let event_coordinator = coordinator.clone();
    let event_session = Arc::clone(&session);
    let surface_sink = sink.clone();
    let _event_thread = std::thread::Builder::new()
        .name("droidloom-denial-events".to_owned())
        .spawn(move || {
            if let Err(error) = event_loop(
                &sink,
                &event_session,
                &lifecycle,
                &event_coordinator,
                &input,
            ) {
                eprintln!("Droidloom Denial event loop fatal: {error}");
                std::process::exit(1);
            }
        })
        .map_err(|error| format!("start Denial event loop: {error}"))?;

    let (bootstrap_object, bootstrap_display) = coordinator
        .stage_bootstrap_display(BOOTSTRAP_PACKAGE.to_owned())
        .map_err(|error| format!("stage SurfaceFlinger bootstrap display: {error}"))?;
    eprintln!(
        "Droidloom bootstrap display configured: object={} display={}",
        bootstrap_object.0, bootstrap_display.0
    );

    let surface_bridge = SurfaceBridge::new(
        surface_sink,
        Arc::clone(&session),
        coordinator.clone(),
        bootstrap_display,
    );
    let _surface_thread = std::thread::Builder::new()
        .name("droidloom-surfaceflinger".to_owned())
        .spawn(move || {
            if let Err(error) = surface_bridge.serve(surface_listener) {
                eprintln!("Droidloom SurfaceFlinger listener fatal: {error}");
                std::process::exit(1);
            }
        })
        .map_err(|error| format!("start SurfaceFlinger bridge thread: {error}"))?;

    let _control_thread = std::thread::Builder::new()
        .name("droidloom-task-control".to_owned())
        .spawn(move || {
            if let Err(error) = coordinator.serve(control_listener) {
                eprintln!("Droidloom task-control listener fatal: {error}");
                std::process::exit(1);
            }
        })
        .map_err(|error| format!("start task-control listener: {error}"))?;

    ProcessState::join_thread_pool();
    Err("Binder thread pool exited unexpectedly".to_owned())
}

fn arguments() -> Result<(PathBuf, PathBuf), String> {
    let mut arguments = std::env::args_os().skip(1);
    let socket = arguments
        .next()
        .map_or_else(|| PathBuf::from(DEFAULT_SOCKET), PathBuf::from);
    let render_node = arguments
        .next()
        .map_or_else(|| PathBuf::from(DEFAULT_RENDER_NODE), PathBuf::from);
    if arguments.next().is_some() {
        return Err("usage: composer-service [DENIAL_SOCKET [DRM_RENDER_NODE]]".to_owned());
    }
    Ok((socket, render_node))
}

fn handshake(socket: &ProtocolSocket) -> Result<u64, String> {
    socket
        .send_android(
            &AndroidMessage::ClientHello {
                min_major: PROTOCOL_MAJOR,
                max_major: PROTOCOL_MAJOR,
                capabilities: capability::REQUIRED_V1
                    | capability::TASK_ACTIVATION
                    | capability::DIRECT_INPUT
                    | capability::LAZY_RENDER_TARGETS
                    | capability::SCENE_PRESENTATION
                    | capability::HOST_CLOCK
                    | capability::KEEP_SCREEN_ON
                    | capability::TASK_DENSITY,
            },
            &[],
        )
        .map_err(|error| format!("send ClientHello: {error}"))?;
    match socket
        .receive_denial()
        .map_err(|error| format!("receive ServerHello: {error}"))?
        .message
    {
        DenialMessage::ServerHello {
            major,
            capabilities,
            ..
        } if major == PROTOCOL_MAJOR
            && capabilities & (capability::REQUIRED_V1 | capability::DIRECT_INPUT)
                == capability::REQUIRED_V1 | capability::DIRECT_INPUT =>
        {
            Ok(capabilities)
        }
        DenialMessage::Error { message, .. } => {
            Err(format!("Denial rejected handshake: {message}"))
        }
        _ => Err("Denial returned an incompatible handshake response".to_owned()),
    }
}

fn event_loop(
    sink: &DenialPresentationSink,
    session: &SharedSession,
    lifecycle: &ComposerLifecycle,
    coordinator: &TaskCoordinator,
    input: &InputHandoff,
) -> Result<(), String> {
    let mut receive_buffer = vec![0; droidloom_denial_protocol::MAX_PACKET_BYTES];
    loop {
        let received = sink
            .socket()
            .receive_denial_into(&mut receive_buffer)
            .map_err(|error| error.to_string())?;
        let message = received.message;
        match message {
            DenialMessage::RequestInputChannel { generation } => input.request(generation)?,
            DenialMessage::Ping { cookie } => sink
                .socket()
                .send_android(&AndroidMessage::Pong { cookie }, &[])
                .map_err(|error| error.to_string())?,
            DenialMessage::FrameGrant {
                latch_nanos,
                period_nanos,
                ..
            } => lifecycle.frame_grant(latch_nanos, period_nanos),
            DenialMessage::Error { message, .. } => {
                return Err(format!("Denial protocol error: {message}"));
            }
            DenialMessage::ServerHello { .. } => {
                return Err("repeated Denial ServerHello".to_owned());
            }
            DenialMessage::DisplayPower { .. } | DenialMessage::Input { .. } => {
                return Err(
                    "legacy input forwarding is retired; native input capability is required"
                        .into(),
                );
            }
            DenialMessage::RegisterRenderTarget {
                object,
                configure_serial,
                buffer,
            } => {
                if coordinator.is_retired(object) || coordinator.is_removing(object) {
                    continue;
                }
                if let Err(error) = sink.register_render_target(
                    object,
                    configure_serial,
                    buffer,
                    received.descriptors,
                ) {
                    // Retirement may complete between the phase check above
                    // and acquiring the sink's task map.
                    if coordinator.is_removing(object) || coordinator.is_retired(object) {
                        continue;
                    }
                    return Err(error.to_string());
                }
            }
            DenialMessage::UnregisterRenderTarget { object, buffer } => {
                if coordinator.is_retired(object) || coordinator.is_removing(object) {
                    continue;
                }
                if let Err(error) = sink.unregister_render_target(object, buffer) {
                    if coordinator.is_removing(object) || coordinator.is_retired(object) {
                        continue;
                    }
                    return Err(error.to_string());
                }
            }
            DenialMessage::ScenePresented {
                object,
                timestamp_nanos,
                refresh_period_nanos,
                ..
            } => {
                if !coordinator.is_retired(object)
                    && !coordinator.is_removing(object)
                    && coordinator.is_direct_task(object)
                {
                    lifecycle.observe_presentation(timestamp_nanos, refresh_period_nanos);
                }
            }
            task_message => {
                if let DenialMessage::Presented {
                    timestamp_nanos,
                    refresh_period_nanos,
                    ..
                } = &task_message
                {
                    lifecycle.observe_presentation(*timestamp_nanos, *refresh_period_nanos);
                }
                let object = task_object(&task_message)
                    .ok_or_else(|| "task event omitted object identity".to_owned())?;
                if coordinator.is_retired(object)
                    || (coordinator.is_removing(object)
                        && !matches!(task_message, DenialMessage::BufferReleased { .. }))
                {
                    continue;
                }
                let display = match sink.display_for_object(object) {
                    Ok(display) => display,
                    Err(_) if coordinator.is_retired(object) => continue,
                    Err(error) => return Err(error.to_string()),
                };
                let event = {
                    let mut session = session
                        .lock()
                        .map_err(|_| "Composer session lock is poisoned".to_owned())?;
                    let event = sink
                        .apply_task_event(&mut session, &task_message)
                        .map_err(|error| error.to_string())?;
                    if let AppliedDenialEvent::Configure { serial, .. } = event {
                        sink.acknowledge_configure(&mut session, display, serial)
                            .map_err(|error| error.to_string())?;
                    }
                    event
                };
                // A configure has completed once its state has been applied and
                // acknowledged to Denial. Wake the reservation before invoking
                // Android's hotplug callback: during SurfaceFlinger bootstrap
                // that callback may wait for the reservation owner to proceed.
                if let AppliedDenialEvent::Configure { .. } = &event {
                    coordinator.display_connected(object)?;
                }
                dispatch_event(lifecycle, coordinator, object, display, event)?;
            }
        }
    }
}

fn dispatch_event(
    lifecycle: &ComposerLifecycle,
    coordinator: &TaskCoordinator,
    object: TaskObjectId,
    display: droidloom_composer::DisplayId,
    event: AppliedDenialEvent,
) -> Result<(), String> {
    match event {
        AppliedDenialEvent::Configure { width, height, .. } => {
            if coordinator.is_direct_task(object) {
                if let Err(error) = coordinator.queue_window_command(
                    object,
                    droidloom_task_control::TaskAction::Resize { width, height },
                ) {
                    eprintln!(
                        "Droidloom deferred Android object {} bounds: {error}",
                        object.0
                    );
                }
                return Ok(());
            }
            if lifecycle
                .is_connected(display)
                .map_err(|error| format!("query hotplug state: {error:?}"))?
            {
                lifecycle
                    .refresh(display)
                    .map_err(|error| format!("refresh task display: {error:?}"))?;
            } else {
                lifecycle
                    .connect_display(display)
                    .map_err(|error| format!("connect task display: {error:?}"))?;
            }
        }
        AppliedDenialEvent::Presented(_) => {}
        AppliedDenialEvent::Density { dpi } => {
            if coordinator.is_direct_task(object) {
                // Queued before the configure's resize, so Android lays the
                // task out once at its new size and density.
                if let Err(error) = coordinator.queue_window_command(
                    object,
                    droidloom_task_control::TaskAction::Density { dpi },
                ) {
                    eprintln!(
                        "Droidloom deferred Android object {} density: {error}",
                        object.0
                    );
                }
            } else if let Err(error) = coordinator.queue_new_task_density(object, dpi) {
                // On the bootstrap display: the density tasks are created at.
                eprintln!("Droidloom deferred the density of new tasks: {error}");
            }
        }
        AppliedDenialEvent::Visibility {
            visibility,
            focused,
        } => {
            if coordinator.is_direct_task(object) {
                use droidloom_task_control::TaskAction;
                // A minimized window stops its Android task; Android ignores
                // repeats, so every report is mirrored without local state.
                let hidden = visibility == Visibility::Hidden;
                let focus = (focused && !hidden).then_some(TaskAction::Focus);
                let show = if hidden { TaskAction::Hide } else { TaskAction::Show };
                for action in std::iter::once(show).chain(focus) {
                    if let Err(error) = coordinator.queue_window_command(object, action) {
                        eprintln!(
                            "Droidloom deferred Android object {} {action:?}: {error}",
                            object.0
                        );
                    }
                }
            }
        }
        AppliedDenialEvent::FormatFeedback { .. }
        | AppliedDenialEvent::Insets { .. }
        | AppliedDenialEvent::BufferReleased(_) => {}
        AppliedDenialEvent::Close => {
            if !coordinator.is_direct_task(object) {
                eprintln!(
                    "Droidloom ignored close for non-task display {display:?} object={}",
                    object.0
                );
                return Ok(());
            }
            if let Err(error) =
                coordinator.queue_window_command(object, droidloom_task_control::TaskAction::Close)
            {
                eprintln!(
                    "Droidloom could not request Android object {} removal for host close: {error}",
                    object.0
                );
            }
        }
        AppliedDenialEvent::Input { .. } => {
            return Err("legacy task input forwarding is retired".into());
        }
    }
    Ok(())
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
