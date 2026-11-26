//! Per-user PipeWire streams behind bounded, authenticated audio endpoints.
#![deny(unsafe_op_in_unsafe_fn)]

use droidloom_audio_protocol::*;
use std::ffi::{CString, c_char, c_void};
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::ptr::NonNull;
use std::sync::Once;

pub mod volume;

/// Initializes PipeWire for this process, once.
fn init_pipewire() {
    static INITIALIZE: Once = Once::new();
    // SAFETY: PipeWire initialization is process-global and performed once.
    INITIALIZE.call_once(|| unsafe { dl_pw_init() });
}

/// Host-side authority and routing. Capture defaults to denied in the CLI.
#[derive(Clone, Debug)]
pub struct Options {
    /// Kernel-mapped UID allowed to connect to the delegated endpoint.
    pub peer_uid: u32,
    /// Explicit host grant for microphone capture.
    pub allow_capture: bool,
    /// Optional stable PipeWire target name; session policy otherwise chooses it.
    pub target: Option<String>,
    /// Optional PipeWire remote for an isolated server or selected user session.
    pub remote: Option<String>,
    /// Capture a sink monitor for an explicit loopback test, rather than a microphone.
    pub capture_sink: bool,
}

#[repr(C)]
#[derive(Default)]
struct Region {
    slot: u32,
    frames: u32,
    offset: u32,
}
#[repr(C)]
#[derive(Default)]
struct Position {
    frames: u64,
    monotonic_ns: u64,
    latency_ns: u64,
    discontinuities: u64,
}

unsafe extern "C" {
    fn dl_pw_init();
    fn dl_pw_open(
        direction: u32,
        rate: u32,
        channels: u32,
        bytes: u32,
        fds: *const i32,
        samples: *const *mut u8,
        target: *const c_char,
        remote: *const c_char,
        name: *const c_char,
        capture_sink: u32,
        output: *mut *mut c_void,
    ) -> i32;
    fn dl_pw_destroy(stream: *mut c_void);
    fn dl_pw_acquire(stream: *mut c_void, requested: u32, region: *mut Region) -> i32;
    fn dl_pw_submit(stream: *mut c_void, slot: u32, frames: u32) -> i32;
    fn dl_pw_control(stream: *mut c_void, command: u64) -> i32;
    fn dl_pw_position(stream: *mut c_void, position: *mut Position) -> i32;
}

struct Stream {
    native: NonNull<c_void>,
    // Native stream must be destroyed before any sample mapping or FD is released.
    buffers: Vec<SampleBuffer>,
}

impl Stream {
    fn open(format: Format, options: &Options, generation: u64) -> io::Result<Self> {
        init_pipewire();
        let buffers = (0..BUFFERS)
            .map(|_| SampleBuffer::allocate(format))
            .collect::<io::Result<Vec<_>>>()?;
        let fds: Vec<_> = buffers
            .iter()
            .map(|buffer| buffer.as_fd().as_raw_fd())
            .collect();
        let pointers: Vec<_> = buffers.iter().map(SampleBuffer::pointer).collect();
        let target = CString::new(options.target.as_deref().unwrap_or(""))
            .map_err(|_| invalid("target contains nul"))?;
        let remote = CString::new(options.remote.as_deref().unwrap_or(""))
            .map_err(|_| invalid("remote contains nul"))?;
        let name = CString::new(format!(
            "droidloom.{}.{generation:x}",
            if format.direction == Direction::Playback {
                "playback"
            } else {
                "capture"
            }
        ))
        .unwrap();
        let mut native = std::ptr::null_mut();
        // SAFETY: four mappings/FDs and strings remain live for the call; mappings
        // are retained in Stream until dl_pw_destroy has stopped every callback.
        check(unsafe {
            dl_pw_open(
                format.direction as u32,
                format.rate,
                format.channels,
                format.bytes() as u32,
                fds.as_ptr(),
                pointers.as_ptr(),
                target.as_ptr(),
                remote.as_ptr(),
                name.as_ptr(),
                u32::from(options.capture_sink),
                &mut native,
            )
        })?;
        let native = NonNull::new(native).ok_or_else(|| invalid("PipeWire returned no stream"))?;
        Ok(Self { native, buffers })
    }
    fn acquire(&mut self, requested: u32) -> io::Result<Region> {
        let mut region = Region::default();
        // SAFETY: native owns a live stream and the output matches its C ABI.
        check(unsafe { dl_pw_acquire(self.native.as_ptr(), requested, &mut region) })?;
        Ok(region)
    }
    fn submit(&mut self, slot: u32, frames: u32) -> io::Result<()> {
        // SAFETY: endpoint validated the acquired slot and its immutable bounds.
        check(unsafe { dl_pw_submit(self.native.as_ptr(), slot, frames) })
    }
    fn control(&mut self, command: u64) -> io::Result<()> {
        // SAFETY: native stream is owned and command was validated by the endpoint.
        check(unsafe { dl_pw_control(self.native.as_ptr(), command) })
    }
    fn position(&mut self) -> io::Result<Position> {
        let mut position = Position::default();
        // SAFETY: position matches the C layout and the stream remains owned.
        check(unsafe { dl_pw_position(self.native.as_ptr(), &mut position) })?;
        Ok(position)
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: unique stream ownership; joins its loop before buffers drop.
        unsafe {
            dl_pw_destroy(self.native.as_ptr());
        }
    }
}

fn check(status: i32) -> io::Result<()> {
    if status < 0 {
        Err(io::Error::from_raw_os_error(-status))
    } else {
        Ok(())
    }
}
fn ipc(error: IpcError) -> io::Error {
    match error {
        IpcError::Io(error) => error,
        IpcError::PeerClosed => io::Error::from(io::ErrorKind::UnexpectedEof),
        error => io::Error::other(error.to_string()),
    }
}
fn generation() -> io::Result<u64> {
    let mut value = 0u64;
    // SAFETY: writes exactly one u64 to its live, writable storage.
    let count = unsafe { libc::getrandom((&raw mut value).cast(), 8, 0) };
    if count != 8 || value == 0 {
        return Err(io::Error::other("cannot create audio generation"));
    }
    Ok(value)
}
fn reply(socket: &SeqPacket, request: Record, op: u32, values: [u64; 5]) -> io::Result<()> {
    socket
        .send_record(
            &Record {
                op,
                values,
                ..request
            }
            .encode(),
            &[],
        )
        .map_err(ipc)
}
fn error_reply(socket: &SeqPacket, request: Record, error: &io::Error) {
    let _ = reply(
        socket,
        request,
        ERROR,
        [
            error.raw_os_error().unwrap_or(libc::EPROTO) as u64,
            0,
            0,
            0,
            0,
        ],
    );
}
fn receive(socket: &SeqPacket) -> io::Result<Record> {
    let mut bytes = [0; RECORD_BYTES];
    let received = socket.receive_record_into(&mut bytes).map_err(ipc)?;
    if !received.descriptors.is_empty() {
        return Err(invalid("client cannot register sample descriptors"));
    }
    Record::decode(received.bytes)
}

/// Run one authenticated stream until disconnect. Every mapping is reclaimed by
/// the host even if the client disappears while retaining a buffer credit.
pub fn serve_connection(socket: SeqPacket, options: &Options) -> io::Result<()> {
    socket_timeout(&socket, 3000)?;
    let peer = socket.peer_credentials().map_err(ipc)?;
    if peer.uid != options.peer_uid {
        eprintln!(
            "droidloom-audio: rejected peer UID {}, expected {}",
            peer.uid, options.peer_uid
        );
        return Err(io::Error::from_raw_os_error(libc::EACCES));
    }
    let open = receive(&socket)?;
    let format = match open.format() {
        Ok(format) if format.direction != Direction::Capture || options.allow_capture => format,
        Ok(_) => {
            eprintln!("droidloom-audio: microphone capture denied by host policy");
            let error = io::Error::from_raw_os_error(libc::EACCES);
            error_reply(&socket, open, &error);
            return Err(error);
        }
        Err(error) => {
            error_reply(&socket, open, &error);
            return Err(error);
        }
    };
    let generation = generation()?;
    let mut stream = match Stream::open(format, options, generation) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!(
                "droidloom-audio: could not open {:?} {}/{} PCM16: {error}",
                format.direction, format.rate, format.channels
            );
            error_reply(&socket, open, &error);
            return Err(error);
        }
    };
    let readonly = if format.direction == Direction::Capture {
        stream
            .buffers
            .iter()
            .map(SampleBuffer::readonly_fd)
            .collect::<io::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let descriptors = if readonly.is_empty() {
        stream.buffers.iter().map(AsFd::as_fd).collect::<Vec<_>>()
    } else {
        readonly.iter().map(AsFd::as_fd).collect::<Vec<_>>()
    };
    socket
        .send_record(
            &Record {
                op: CONFIG,
                generation,
                values: [
                    u64::from(format.rate),
                    u64::from(format.channels),
                    u64::from(format.frames),
                    BUFFERS as u64,
                    SHARED_POOL | (open.values[4] & BURST_POSITION),
                ],
                ..open
            }
            .encode(),
            &descriptors,
        )
        .map_err(ipc)?;
    eprintln!(
        "droidloom-audio: {:?} {}/{} PCM16, four shared buffers, generation {generation:x}, burst_position={}",
        format.direction,
        format.rate,
        format.channels,
        open.values[4] & BURST_POSITION != 0
    );
    let mut previous_request = open.request;
    let mut held: Option<Region> = None;
    let mut active = false;
    loop {
        // A paused connection may remain idle indefinitely, but every received
        // record and each backend operation has a bounded execution deadline.
        let request = match receive(&socket) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            result => result?,
        };
        if request.generation != generation || request.request <= previous_request {
            let error = invalid("stale audio generation or request");
            error_reply(&socket, request, &error);
            return Err(error);
        }
        previous_request = request.request;
        let result = (|| {
            match request.op {
                ACQUIRE | ACQUIRE_POSITION
                    if (request.op == ACQUIRE || open.values[4] & BURST_POSITION != 0)
                        && active
                        && held.is_none()
                        && request.values[1..] == [0; 4]
                        && request.values[0] > 0
                        && request.values[0] <= u64::from(format.frames) =>
                {
                    let region = stream.acquire(request.values[0] as u32)?;
                    if region.slot as usize >= BUFFERS
                        || region.frames == 0
                        || region.frames > request.values[0] as u32
                        || region.offset as usize % format.frame_bytes() != 0
                        || region.offset as usize + region.frames as usize * format.frame_bytes()
                            > format.bytes()
                    {
                        return Err(invalid("PipeWire returned an invalid sample region"));
                    }
                    let values = if request.op == ACQUIRE_POSITION {
                        let position = stream.position()?;
                        [
                            pack_credit(region.slot, region.frames, region.offset)?,
                            position.frames,
                            position.monotonic_ns,
                            position.latency_ns,
                            position.discontinuities,
                        ]
                    } else {
                        [
                            u64::from(region.slot),
                            u64::from(region.frames),
                            u64::from(region.offset),
                            u64::from(format.frames),
                            0,
                        ]
                    };
                    reply(&socket, request, request.op, values)?;
                    held = Some(region);
                }
                SUBMIT | SUBMIT_POSITION
                    if (request.op == SUBMIT || open.values[4] & BURST_POSITION != 0)
                        && request.values[2..] == [0; 3] =>
                {
                    let region = held.as_ref().ok_or_else(|| invalid("no audio credit"))?;
                    if request.values[0] != u64::from(region.slot)
                        || request.values[1] > u64::from(region.frames)
                    {
                        return Err(invalid("invalid slot or submit exceeds credit"));
                    }
                    stream.submit(region.slot, request.values[1] as u32)?;
                    held = None;
                    if request.op == SUBMIT_POSITION {
                        let position = stream.position()?;
                        reply(
                            &socket,
                            request,
                            STATUS,
                            [
                                position.frames,
                                position.monotonic_ns,
                                position.latency_ns,
                                position.discontinuities,
                                0,
                            ],
                        )?;
                    } else {
                        reply(&socket, request, ACK, [0; 5])?;
                    }
                }
                CONTROL
                    if held.is_none()
                        && request.values[1..] == [0; 4]
                        && (START..=STANDBY).contains(&request.values[0]) =>
                {
                    if request.values[0] == DRAIN && format.direction == Direction::Capture {
                        return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
                    }
                    stream.control(request.values[0])?;
                    if matches!(request.values[0], START | PAUSE | STANDBY) {
                        active = request.values[0] == START;
                    }
                    reply(&socket, request, ACK, [0; 5])?;
                }
                STATUS if request.values == [0; 5] => {
                    let position = stream.position()?;
                    reply(
                        &socket,
                        request,
                        STATUS,
                        [
                            position.frames,
                            position.monotonic_ns,
                            position.latency_ns,
                            position.discontinuities,
                            0,
                        ],
                    )?;
                }
                _ => return Err(invalid("invalid audio operation or stream state")),
            }
            Ok(())
        })();
        if let Err(error) = result {
            error_reply(&socket, request, &error);
            // Native removal/error and malformed operations invalidate the generation.
            return Err(error);
        }
    }
}
