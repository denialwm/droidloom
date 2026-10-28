//! Cell-private broker between SurfaceFlinger and the Denial presentation sink.

use droidloom_composer::{DisplayId, TaskId};
use droidloom_composer_aidl::denial::{DenialPresentationSink, DenialSinkError};
use droidloom_composer_aidl::minigbm::ImportedRenderTarget;
use droidloom_composer_aidl::service::SharedSession;
use droidloom_denial_ipc::{SeqPacket, SeqPacketListener};
use droidloom_surface_bridge::{Plane, Request, Response, decode_request, encode_response};
use std::collections::BTreeMap;
use std::os::fd::AsFd;
use droidloom_syncobj::frame_trace;

use crate::control::TaskCoordinator;

const AID_ROOT: u32 = 0;
const AID_SYSTEM: u32 = 1_000;
const MAX_RESERVATIONS: usize = 3;
const ERROR_NO_ENTRY: i32 = -2;
const ERROR_IO: i32 = -5;
const ERROR_TRY_AGAIN: i32 = -11;

/// Long-lived owner of SurfaceFlinger's direct-presentation socket.
#[derive(Clone)]
pub struct SurfaceBridge {
    sink: DenialPresentationSink,
    session: SharedSession,
    coordinator: TaskCoordinator,
    bootstrap_display: DisplayId,
}

impl SurfaceBridge {
    /// Bind a broker to the already configured built-in display.
    pub fn new(
        sink: DenialPresentationSink,
        session: SharedSession,
        coordinator: TaskCoordinator,
        display: DisplayId,
    ) -> Self {
        Self {
            sink,
            session,
            coordinator,
            bootstrap_display: display,
        }
    }

    /// Accept SurfaceFlinger reconnects for the lifetime of Composer.
    pub fn serve(&self, listener: SeqPacketListener) -> Result<(), String> {
        loop {
            let socket = listener
                .accept()
                .map_err(|error| format!("accept SurfaceFlinger bridge: {error}"))?;
            let credentials = socket
                .peer_credentials()
                .map_err(|error| format!("authenticate SurfaceFlinger bridge: {error}"))?;
            if credentials.uid != AID_ROOT && credentials.uid != AID_SYSTEM {
                eprintln!(
                    "Droidloom rejected SurfaceFlinger bridge peer pid={} uid={} gid={}",
                    credentials.pid, credentials.uid, credentials.gid
                );
                continue;
            }
            let bridge = self.clone();
            std::thread::Builder::new()
                .name("droidloom-sf-task".to_owned())
                .spawn(move || {
                    if let Err(error) = bridge.serve_connection(&socket) {
                        if !error.contains("is not authorized for export") {
                            eprintln!("Droidloom SurfaceFlinger bridge disconnected: {error}");
                        }
                    }
                })
                .map_err(|error| format!("start SurfaceFlinger task connection: {error}"))?;
        }
    }

    fn serve_connection(&self, socket: &SeqPacket) -> Result<(), String> {
        let mut receive_buffer = vec![0; droidloom_denial_protocol::MAX_PACKET_BYTES];
        let mut ready = false;
        let mut layers_opened = false;
        let mut selected_display = None;
        let mut selected_task = None;
        let mut reservations: BTreeMap<u64, ImportedRenderTarget> = BTreeMap::new();
        let tracing = frame_trace::enabled();
        let mut acquire_begin = 0;
        let mut acquire_retries = 0_u64;
        let mut timings: BTreeMap<u64, (u64, u64, u64)> = BTreeMap::new();
        let result = (|| -> Result<(), String> {
            loop {
                let record = match socket.receive_record_into(&mut receive_buffer) {
                    Ok(record) => record,
                    Err(error) => break Err(format!("receive SurfaceFlinger request: {error}")),
                };
                let request = match decode_request(record.bytes, record.descriptors.len()) {
                    Ok(request) => request,
                    Err(error) => break Err(format!("decode SurfaceFlinger request: {error}")),
                };
                match request {
                    Request::Hello { task } if !ready => {
                        let (display, width, height) = match self.select_channel(task) {
                            Ok(channel) => channel,
                            Err(error) => {
                                let code = channel_error_code(&error);
                                if code == ERROR_IO {
                                    eprintln!(
                                        "Droidloom SurfaceFlinger task {task} selection failed: {error}"
                                    );
                                }
                                send_response(socket, &Response::Error { code }, &[])?;
                                break Ok(());
                            }
                        };
                        ready = true;
                        selected_display = Some(display);
                        selected_task = Some(task);
                        send_response(
                            socket,
                            &Response::Hello {
                                display: display.0,
                                width,
                                height,
                            },
                            &[],
                        )?;
                    }
                    Request::Hello { .. } => {
                        break Err("repeated SurfaceFlinger hello".to_owned());
                    }
                    _ if !ready => break Err("SurfaceFlinger hello is required".to_owned()),
                    Request::CompleteHeadless { display } => {
                        let display = require_display(selected_display, display)?;
                        if selected_task != Some(0) || display != self.bootstrap_display
                            || !reservations.is_empty()
                        {
                            send_response(socket, &Response::Error { code: -22 }, &[])?;
                            continue;
                        }
                        let fence = record.descriptors.into_iter().next();
                        match self.sink.complete_headless_frame(display, fence) {
                            Ok(()) => send_response(socket, &Response::Ack, &[])?,
                            Err(error) => {
                                eprintln!("Droidloom bootstrap fence handoff failed: {error}");
                                send_response(socket, &Response::Error { code: ERROR_IO }, &[])?;
                            }
                        }
                    }
                    Request::Acquire { display } => {
                        if tracing && acquire_begin == 0 { acquire_begin = frame_trace::now_ns(); }
                        let display = require_display(selected_display, display)?;
                        if reservations.len() >= MAX_RESERVATIONS {
                            send_response(socket, &Response::Error { code: -16 }, &[])?;
                            continue;
                        }
                        let target = match self.sink.acquire_surfaceflinger_target(display) {
                            Ok(target) => target,
                            Err(DenialSinkError::NoRenderTarget(_)) => {
                                if tracing { acquire_retries += 1; }
                                // Normal backpressure, not a logging event. The
                                // task retries on a later composition cycle.
                                send_response(socket, &Response::Error { code: ERROR_TRY_AGAIN }, &[])?;
                                continue;
                            }
                            Err(error) => {
                                eprintln!("Droidloom SurfaceFlinger acquire failed: {error}");
                                send_response(socket, &Response::Error { code: ERROR_IO }, &[])?;
                                continue;
                            }
                        };
                        let planes = target
                            .metadata
                            .planes
                            .iter()
                            .map(|plane| Plane {
                                offset: plane.offset,
                                stride: plane.stride,
                            })
                            .collect::<Vec<_>>();
                        let descriptors =
                            target.plane_fds.iter().map(AsFd::as_fd).collect::<Vec<_>>();
                        let response = Response::Target {
                            display: display.0,
                            buffer: target.metadata.id.0,
                            configure_serial: target.configure_serial,
                            width: target.metadata.width,
                            height: target.metadata.height,
                            fourcc: target.metadata.format.fourcc,
                            modifier: target.metadata.format.modifier,
                            planes,
                        };
                        if let Err(error) = send_response(socket, &response, &descriptors) {
                            self.sink.cancel_surfaceflinger_target(display, target);
                            break Err(error);
                        }
                        let buffer = target.metadata.id.0;
                        if tracing {
                            timings.insert(buffer, (acquire_begin, frame_trace::now_ns(), acquire_retries));
                            acquire_begin = 0;
                            acquire_retries = 0;
                        }
                        if reservations.insert(buffer, target).is_some() {
                            break Err(format!(
                                "SurfaceFlinger acquired duplicate target {buffer}"
                            ));
                        }
                    }
                    Request::Present { display, buffer, opaque, damage } => {
                        let received = if tracing { frame_trace::now_ns() } else { 0 };
                        let timing = timings.remove(&buffer);
                        let display = require_display(selected_display, display)?;
                        let Some(target) = reservations.remove(&buffer) else {
                            send_response(socket, &Response::Error { code: -22 }, &[])?;
                            continue;
                        };
                        let mut descriptors = record.descriptors.into_iter();
                        let acquire_fence = descriptors.next();
                        let submit = self
                            .session
                            .lock()
                            .map_err(|_| "Composer session lock is poisoned".to_owned())
                            .and_then(|mut session| {
                                self.sink
                                    .submit_surfaceflinger_target(
                                        &mut session,
                                        display,
                                        target,
                                        acquire_fence,
                                        opaque,
                                        damage.map(|rect| droidloom_transport::Damage {
                                            x: rect.x, y: rect.y,
                                            width: rect.width, height: rect.height,
                                        }),
                                    )
                                    .map_err(|error| error.to_string())
                            });
                        match submit {
                            Ok(frame) => {
                                send_response(socket, &Response::Ack, &[])?;
                                if let Some((begin, acquired, retries)) = timing {
                                    frame_trace::event("sf_bridge", 0, frame.0, buffer, &[
                                        ("display", display.0), ("android_task", selected_task.unwrap_or(0)),
                                        ("acquire_begin_ns", begin), ("acquired_ns", acquired),
                                        ("acquire_retries", retries), ("present_received_ns", received),
                                        ("ack_sent_ns", frame_trace::now_ns()),
                                    ]);
                                }
                            }
                            Err(error) => {
                                eprintln!("Droidloom SurfaceFlinger present failed: {error}");
                                send_response(socket, &Response::Error { code: -5 }, &[])?;
                            }
                        }
                    }
                    Request::OpenLayers { display } => {
                        let display = require_display(selected_display, display)?;
                        if layers_opened || selected_task == Some(0) {
                            send_response(socket, &Response::Error { code: -22 }, &[])?;
                            continue;
                        }
                        let (requests, host_requests) = SeqPacket::pair().map_err(|e| e.to_string())?;
                        let (events, host_events) = SeqPacket::pair().map_err(|e| e.to_string())?;
                        self.sink.bind_layer_stream(display, &[host_requests.as_fd(), host_events.as_fd()]).map_err(|e| e.to_string())?;
                        send_response(socket, &Response::LayerStream, &[requests.as_fd(), events.as_fd()])?;
                        layers_opened = true;
                    }
                    Request::Cancel { display, buffer } => {
                        timings.remove(&buffer);
                        let display = require_display(selected_display, display)?;
                        let Some(target) = reservations.remove(&buffer) else {
                            send_response(socket, &Response::Error { code: -22 }, &[])?;
                            continue;
                        };
                        self.sink.cancel_surfaceflinger_target(display, target);
                        send_response(socket, &Response::Ack, &[])?;
                    }
                    Request::Retire { display } => {
                        let display = require_display(selected_display, display)?;
                        let task = selected_task
                            .filter(|task| *task != 0)
                            .ok_or_else(|| "bootstrap display cannot be retired".to_owned())?;
                        for (_, target) in std::mem::take(&mut reservations) {
                            self.sink.cancel_surfaceflinger_target(display, target);
                        }
                        self.coordinator.retire_direct_task(TaskId(task), display)?;
                        send_response(socket, &Response::Ack, &[])?;
                        eprintln!("Droidloom retired destroyed Android task {task}");
                        break Ok(());
                    }
                }
            }
        })();
        let selected_display = selected_display.unwrap_or(self.bootstrap_display);
        for (_, target) in reservations {
            self.sink
                .cancel_surfaceflinger_target(selected_display, target);
        }
        result
    }

    fn select_channel(&self, task: u64) -> Result<(DisplayId, u32, u32), String> {
        // Every channel reports Denial's latest configure, the size Composer
        // gives SurfaceFlinger: a rotated output resizes the built-in display.
        let display = if task == 0 {
            self.bootstrap_display
        } else {
            self.coordinator.direct_task_display(TaskId(task))?
        };
        let session = self
            .session
            .lock()
            .map_err(|_| "Composer session lock is poisoned".to_owned())?;
        let configure = session
            .windows()
            .composer_for_display(display)
            .map_err(|error| error.to_string())?
            .latest_configure()
            .ok_or_else(|| format!("Android channel {task} has no Denial configure"))?;
        Ok((display, configure.width, configure.height))
    }
}

fn require_display(selected: Option<DisplayId>, requested: u64) -> Result<DisplayId, String> {
    let selected = selected.ok_or_else(|| "SurfaceFlinger hello is required".to_owned())?;
    if requested == selected.0 {
        Ok(selected)
    } else {
        Err(format!(
            "SurfaceFlinger addressed display {requested}; selected channel is {}",
            selected.0
        ))
    }
}

fn channel_error_code(error: &str) -> i32 {
    if error.contains("is not authorized for export") {
        ERROR_NO_ENTRY
    } else if error.contains("export is not ready") {
        ERROR_TRY_AGAIN
    } else {
        ERROR_IO
    }
}

fn send_response(
    socket: &SeqPacket,
    response: &Response,
    descriptors: &[std::os::fd::BorrowedFd<'_>],
) -> Result<(), String> {
    let bytes = encode_response(response, descriptors.len())
        .map_err(|error| format!("encode SurfaceFlinger response: {error}"))?;
    socket
        .send_record(&bytes, descriptors)
        .map_err(|error| format!("send SurfaceFlinger response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_selection_failures_have_stable_retry_semantics() {
        assert_eq!(
            channel_error_code("Android task 2 is not authorized for export"),
            ERROR_NO_ENTRY
        );
        assert_eq!(
            channel_error_code("Android task 7 export is not ready"),
            ERROR_TRY_AGAIN
        );
        assert_eq!(
            channel_error_code("task-control state lock is poisoned"),
            ERROR_IO
        );
    }
}
