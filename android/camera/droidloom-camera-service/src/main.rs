//! Android camera provider AIDL V4 service for Droidloom.
//!
//! Every camera braun offers on the host is one Android camera. A session
//! asks the host user's device broker for a braun session, opens its camera
//! when the first request comes, and fills each request's buffers from the
//! next preview braun renders. A JPEG output takes a braun photo; its
//! buffer comes back once braun has developed it, seconds later.
//!
//! One braun camera streams at a time, so every camera conflicts with every
//! other. A host app that takes the camera ends the Android session with a
//! device error, as an unplugged camera would.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use android_hardware_camera_common::aidl::android::hardware::camera::common::{
    CameraDeviceStatus::CameraDeviceStatus, CameraResourceCost::CameraResourceCost,
    Status::Status as CameraStatus, VendorTagSection::VendorTagSection,
};
use android_hardware_camera_device::aidl::android::hardware::camera::device::{
    BufferCache::BufferCache,
    BufferStatus::BufferStatus,
    CameraMetadata::CameraMetadata,
    CameraOfflineSessionInfo::CameraOfflineSessionInfo,
    CaptureRequest::CaptureRequest,
    CaptureResult::CaptureResult,
    ConfigureStreamsRet::ConfigureStreamsRet,
    ErrorCode::ErrorCode,
    ErrorMsg::ErrorMsg,
    HalStream::HalStream,
    ICameraDevice::{BnCameraDevice, ICameraDevice},
    ICameraDeviceCallback::ICameraDeviceCallback,
    ICameraDeviceSession::{BnCameraDeviceSession, ICameraDeviceSession},
    ICameraInjectionSession::ICameraInjectionSession,
    ICameraOfflineSession::ICameraOfflineSession,
    NotifyMsg::NotifyMsg,
    RequestTemplate::RequestTemplate,
    ShutterMsg::ShutterMsg,
    Stream::Stream,
    StreamBuffer::StreamBuffer,
    StreamConfiguration::StreamConfiguration,
    StreamConfigurationMode::StreamConfigurationMode,
    StreamRotation::StreamRotation,
    StreamType::StreamType,
};
use android_hardware_camera_provider::aidl::android::hardware::camera::provider::{
    CameraIdAndStreamCombination::CameraIdAndStreamCombination,
    ConcurrentCameraIdCombination::ConcurrentCameraIdCombination,
    ICameraProvider::{BnCameraProvider, ICameraProvider},
    ICameraProviderCallback::ICameraProviderCallback,
};
use android_hardware_common::aidl::android::hardware::common::NativeHandle::NativeHandle;
use android_hardware_common_fmq::aidl::android::hardware::common::fmq::{
    MQDescriptor::MQDescriptor, SynchronizedReadWrite::SynchronizedReadWrite,
};
use android_hardware_graphics_common::aidl::android::hardware::graphics::common::{
    BufferUsage::BufferUsage, PixelFormat::PixelFormat,
};
use binder::{BinderFeatures, Interface, ProcessState, Status, Strong};
use droidloom_camera::braun::{self, Reply};
use droidloom_camera::characteristics::{
    MAX_ZOOM, characteristics, default_request, stream_sizes,
};
use droidloom_camera::control::Control;
use droidloom_camera::exif;
use droidloom_camera::frame::{self, Plane, Source, Target};
use droidloom_camera::metadata::Metadata;
use droidloom_camera::tags;
use droidloom_denial_ipc::{IpcError, SeqPacket};
use droidloom_device_protocol::{Camera, Facing, MAX_RECORD, Message};
use droidloom_minigbm::{MinigbmMetadata, parse_minigbm_handle};

const SERVICE_NAME: &str = "android.hardware.camera.provider.ICameraProvider/internal/0";
/// The supervisor binds the broker's camera socket here.
const SOCKET: &str = "/dev/socket/droidloom/camera";
/// While no camera is listed, the broker is asked again this often, as
/// braun finds cameras whose drivers load late.
const RETRY_EVERY: Duration = Duration::from_secs(2);
const RETRIES: u32 = 30;
/// How long a request waits for braun's next preview. A photo's burst
/// holds the preview for about a second.
const FRAME_WAIT: Duration = Duration::from_secs(5);
const FENCE_WAIT_MS: i32 = 1000;
const MAX_PROCESSED_STREAMS: usize = 3;
const MAX_BUFFERS: i32 = 4;
const MAX_PHOTO_BUFFERS: i32 = 2;
/// `_IOW('b', 0, struct dma_buf_sync)`.
const DMA_BUF_IOCTL_SYNC: u32 = 0x4008_6200;
const DMA_BUF_SYNC_WRITE: u64 = 2;
const DMA_BUF_SYNC_END: u64 = 4;
/// `CameraBlobId::JPEG`.
const BLOB_JPEG: i32 = 0x00ff;

fn camera_error<T>(code: CameraStatus) -> binder::Result<T> {
    Err(Status::new_service_specific_error(code.0, None))
}

fn unsupported<T>() -> binder::Result<T> {
    camera_error(CameraStatus::OPERATION_NOT_SUPPORTED)
}

fn illegal<T>() -> binder::Result<T> {
    camera_error(CameraStatus::ILLEGAL_ARGUMENT)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn device_name(index: usize) -> String {
    format!("device@1.1/internal/{index}")
}

/// One exchange with the broker; the descriptors beside its reply.
fn ask_broker(message: &Message) -> Option<(Message, Vec<OwnedFd>)> {
    let socket = SeqPacket::connect(SOCKET).ok()?;
    socket.set_io_timeout(Duration::from_secs(5)).ok()?;
    socket.send_record(&message.encode(), &[]).ok()?;
    let mut bytes = vec![0; MAX_RECORD];
    let record = socket.receive_record_into(&mut bytes).ok()?;
    let reply = Message::decode(record.bytes).ok()?;
    Some((reply, record.descriptors))
}

/// braun's cameras: the first back camera, then the first front one, then
/// the rest, so that cameras 0 and 1 are where apps expect them.
fn list_cameras() -> Vec<Camera> {
    let Some((Message::Cameras(mut cameras), _)) = ask_broker(&Message::CameraList) else {
        return Vec::new();
    };
    let mut ordered = Vec::with_capacity(cameras.len());
    for facing in [Facing::Back, Facing::Front] {
        if let Some(index) = cameras.iter().position(|camera| camera.facing == facing) {
            ordered.push(cameras.remove(index));
        }
    }
    ordered.append(&mut cameras);
    ordered
}

fn boottime_ns() -> i64 {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes the timespec it is given.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &raw mut now) };
    i64::from(now.tv_sec) * 1_000_000_000 + i64::from(now.tv_nsec)
}

/// Waits until `fd` is readable, `timeout_ms` at most (-1: forever).
fn wait_readable(fd: BorrowedFd<'_>, timeout_ms: i32) -> io::Result<bool> {
    let mut poll = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    loop {
        // SAFETY: one pollfd, owned by this frame.
        let ready = unsafe { libc::poll(&raw mut poll, 1, timeout_ms) };
        if ready >= 0 {
            return Ok(ready > 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// A shared mapping of a whole descriptor.
struct Mapping {
    data: *mut u8,
    length: usize,
}

impl Mapping {
    fn new(fd: BorrowedFd<'_>, length: usize, writable: bool) -> io::Result<Self> {
        let protection = if writable { libc::PROT_READ | libc::PROT_WRITE } else { libc::PROT_READ };
        // SAFETY: maps `length` bytes of a descriptor this process holds; the
        // mapping is unmapped once, in Drop.
        let data = unsafe {
            libc::mmap(std::ptr::null_mut(), length, protection, libc::MAP_SHARED, fd.as_raw_fd(), 0)
        };
        if data == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { data: data.cast(), length })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping holds `length` bytes until Drop.
        unsafe { std::slice::from_raw_parts(self.data, self.length) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: the mapping holds `length` bytes until Drop, and only this
        // value hands them out.
        unsafe { std::slice::from_raw_parts_mut(self.data, self.length) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: mapped by `new` with this length.
        unsafe { libc::munmap(self.data.cast(), self.length) };
    }
}

// The mapping is plain memory; who touches it is the session's business.
// SAFETY: no thread-bound state is held.
unsafe impl Send for Mapping {}

/// The size of a dma-buf or file.
fn descriptor_size(fd: BorrowedFd<'_>) -> io::Result<usize> {
    // SAFETY: lseek on a descriptor this process holds.
    let end = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
    usize::try_from(end).map_err(|_| io::Error::last_os_error())
}

fn dma_buf_sync(fd: BorrowedFd<'_>, flags: u64) {
    let sync: u64 = flags;
    // SAFETY: DMA_BUF_IOCTL_SYNC reads one u64 of flags.
    unsafe { libc::ioctl(fd.as_raw_fd(), DMA_BUF_IOCTL_SYNC as _, &raw const sync) };
}

/// A gralloc buffer, sent once and named by its id after.
struct Buffer {
    fds: Vec<OwnedFd>,
    layout: Option<MinigbmMetadata>,
}

impl Buffer {
    fn import(handle: &NativeHandle) -> io::Result<Self> {
        let fds = handle
            .fds
            .iter()
            .map(|fd| fd.as_ref().try_clone())
            .collect::<io::Result<Vec<_>>>()?;
        let layout = parse_minigbm_handle(fds.len(), &handle.ints).ok();
        Ok(Self { fds, layout })
    }

    /// Writes into the whole buffer through `write`, between dma-buf syncs.
    fn write(&self, write: impl FnOnce(&mut [u8], &MinigbmMetadata) -> bool) -> bool {
        let (Some(fd), Some(layout)) = (self.fds.first(), &self.layout) else {
            return false;
        };
        let Ok(length) = descriptor_size(fd.as_fd()) else {
            return false;
        };
        let Ok(mut mapping) = Mapping::new(fd.as_fd(), length, true) else {
            return false;
        };
        dma_buf_sync(fd.as_fd(), DMA_BUF_SYNC_WRITE);
        let written = write(mapping.bytes_mut(), layout);
        dma_buf_sync(fd.as_fd(), DMA_BUF_SYNC_WRITE | DMA_BUF_SYNC_END);
        written
    }
}

/// A buffer to fill for a request.
struct Output {
    stream: i32,
    buffer_id: i64,
    buffer: Option<Arc<Buffer>>,
    acquire: Option<OwnedFd>,
}

impl Output {
    fn ready(&self) -> bool {
        self.acquire
            .as_ref()
            .is_none_or(|fence| wait_readable(fence.as_fd(), FENCE_WAIT_MS).unwrap_or(false))
    }

    fn returned(&self, ok: bool) -> StreamBuffer {
        StreamBuffer {
            streamId: self.stream,
            bufferId: self.buffer_id,
            status: if ok { BufferStatus::OK } else { BufferStatus::ERROR },
            ..StreamBuffer::default()
        }
    }
}

/// A configured stream.
#[derive(Clone, Copy)]
struct Configured {
    blob: bool,
    /// The size a JPEG stream asks braun for; `None` for the full photo.
    photo_size: Option<(u32, u32)>,
    buffer_size: usize,
}

struct Request {
    frame: i32,
    settings: Metadata,
    outputs: Vec<Output>,
}

/// A photo braun is taking for `frame`.
struct Photo {
    shot: u32,
    frame: i32,
    output: Output,
    file: OwnedFd,
    buffer_size: usize,
    orientation: u16,
}

#[derive(Default)]
struct State {
    streams: HashMap<i32, Configured>,
    buffers: HashMap<(i32, i64), Arc<Buffer>>,
    queue: VecDeque<Request>,
    photos: VecDeque<Photo>,
    has_settings: bool,
    busy: bool,
    closed: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

fn no_input() -> StreamBuffer {
    StreamBuffer { streamId: -1, bufferId: 0, ..StreamBuffer::default() }
}

fn send_result(callback: &dyn ICameraDeviceCallback, result: CaptureResult) {
    if let Err(error) = callback.processCaptureResult(&[result]) {
        eprintln!("droidloom camera: result not delivered: {error}");
    }
}

fn buffers_result(frame: i32, buffers: Vec<StreamBuffer>) -> CaptureResult {
    CaptureResult {
        frameNumber: frame,
        outputBuffers: buffers,
        inputBuffer: no_input(),
        partialResult: 0,
        ..CaptureResult::default()
    }
}

fn notify_error(callback: &dyn ICameraDeviceCallback, frame: i32, stream: i32, code: ErrorCode) {
    let message = NotifyMsg::Error(ErrorMsg { frameNumber: frame, errorStreamId: stream, errorCode: code });
    if let Err(error) = callback.notify(&[message]) {
        eprintln!("droidloom camera: error not delivered: {error}");
    }
}

/// Tells Android a request failed and hands its buffers back.
fn fail_request(callback: &dyn ICameraDeviceCallback, request: &Request) {
    notify_error(callback, request.frame, -1, ErrorCode::ERROR_REQUEST);
    let buffers = request.outputs.iter().map(|output| output.returned(false)).collect();
    send_result(callback, buffers_result(request.frame, buffers));
}

fn fail_photo(callback: &dyn ICameraDeviceCallback, photo: &Photo) {
    notify_error(callback, photo.frame, photo.output.stream, ErrorCode::ERROR_BUFFER);
    send_result(callback, buffers_result(photo.frame, vec![photo.output.returned(false)]));
}

/// A braun session with its camera open.
struct Braun {
    socket: SeqPacket,
    previews: Mapping,
    slot_bytes: usize,
    bytes: Vec<u8>,
}

/// A preview braun handed over.
#[derive(Clone, Copy)]
struct Preview {
    slot: u32,
    width: u32,
    height: u32,
}

enum Lost {
    /// No preview came in time.
    Late,
    /// The camera closed, or braun went away.
    Gone(String),
}

impl Braun {
    fn open(camera: &Camera) -> Result<Self, String> {
        let reply = ask_broker(&Message::CameraConnect).ok_or("the device broker did not answer")?;
        let fd = match reply {
            (Message::CameraSession, fds) => fds.into_iter().next().ok_or("no braun session")?,
            (Message::CameraUnavailable(reason), _) => return Err(reason),
            _ => return Err("the device broker answered something else".into()),
        };
        let socket = SeqPacket::from_owned_fd(fd).map_err(|error| error.to_string())?;
        socket
            .send_record(&braun::Request::Open(camera.id.clone()).encode(), &[])
            .map_err(|error| error.to_string())?;
        let mut bytes = vec![0; braun::MAX_MESSAGE];
        let deadline = Instant::now() + FRAME_WAIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !wait_readable(socket.as_fd(), millis(remaining)).map_err(|error| error.to_string())? {
                return Err("braun did not open the camera".into());
            }
            let record = socket.receive_record_into(&mut bytes).map_err(|error| error.to_string())?;
            match Reply::decode(record.bytes) {
                Some(Reply::Opened { slots, slot_bytes, .. }) => {
                    let memory = record.descriptors.into_iter().next().ok_or("no preview memory")?;
                    let slot_bytes = slot_bytes as usize;
                    let previews = Mapping::new(memory.as_fd(), slots as usize * slot_bytes, false)
                        .map_err(|error| error.to_string())?;
                    return Ok(Self { socket, previews, slot_bytes, bytes });
                }
                Some(Reply::Failed(0, reason)) => return Err(reason),
                _ => {}
            }
        }
    }

    fn send(&self, request: &braun::Request, fds: &[BorrowedFd<'_>]) -> bool {
        self.socket.send_record(&request.encode(), fds).is_ok()
    }

    /// The next reply, waiting `timeout` at most.
    fn receive(&mut self, timeout: Duration) -> Result<Option<Reply>, Lost> {
        let gone = |error: IpcError| Lost::Gone(error.to_string());
        if !wait_readable(self.socket.as_fd(), millis(timeout)).map_err(|error| Lost::Gone(error.to_string()))? {
            return Ok(None);
        }
        let record = self.socket.receive_record_into(&mut self.bytes).map_err(gone)?;
        Ok(Reply::decode(record.bytes))
    }

    fn preview(&self, preview: Preview) -> Option<&[u8]> {
        let length = preview.width as usize * preview.height as usize * 4;
        let start = preview.slot as usize * self.slot_bytes;
        (length <= self.slot_bytes).then(|| self.previews.bytes().get(start..start + length)).flatten()
    }
}

impl Drop for Braun {
    fn drop(&mut self) {
        self.send(&braun::Request::Close, &[]);
    }
}

fn millis(duration: Duration) -> i32 {
    i32::try_from(duration.as_millis()).unwrap_or(i32::MAX)
}

/// The session's thread: requests in, buffers and results out.
struct Worker {
    shared: Arc<Shared>,
    callback: Strong<dyn ICameraDeviceCallback>,
    camera: Camera,
    control: Control,
    braun: Option<Braun>,
    next_shot: u32,
    /// The camera was lost: every request fails from now on.
    lost: bool,
}

impl Worker {
    fn run(mut self) {
        loop {
            let request = {
                let mut state = lock(&self.shared.state);
                if state.closed {
                    break;
                }
                let request = state.queue.pop_front();
                state.busy = request.is_some();
                let photos = !state.photos.is_empty();
                if request.is_none() && !(photos && self.braun.is_some()) {
                    // Nothing to do until a request comes.
                    drop(self.shared.changed.wait_timeout(state, Duration::from_millis(200)));
                    continue;
                }
                request
            };
            match request {
                Some(request) => self.process(request),
                // Photos are developing: hear when they are written.
                None => {
                    if let Err(Lost::Gone(reason)) = self.pump(Duration::from_millis(50)) {
                        self.lose(&reason);
                    }
                }
            }
            let mut state = lock(&self.shared.state);
            state.busy = false;
            self.shared.changed.notify_all();
        }
    }

    fn lose(&mut self, reason: &str) {
        eprintln!("droidloom camera {}: {reason}", self.camera.id);
        self.braun = None;
        if !self.lost {
            self.lost = true;
            notify_error(&*self.callback, 0, -1, ErrorCode::ERROR_DEVICE);
        }
        let photos: Vec<Photo> = lock(&self.shared.state).photos.drain(..).collect();
        for photo in &photos {
            fail_photo(&*self.callback, photo);
        }
    }

    /// Handles braun's replies for `timeout`; returns the newest preview,
    /// releasing older ones.
    fn pump(&mut self, timeout: Duration) -> Result<Option<Preview>, Lost> {
        let deadline = Instant::now() + timeout;
        let mut newest: Option<Preview> = None;
        loop {
            let Some(braun) = self.braun.as_mut() else {
                return Err(Lost::Gone("braun closed".into()));
            };
            // Once a preview is in, take only what is already waiting.
            let wait = if newest.is_some() {
                Duration::ZERO
            } else {
                deadline.saturating_duration_since(Instant::now())
            };
            let Some(reply) = braun.receive(wait)? else {
                return Ok(newest);
            };
            match reply {
                Reply::Frame { slot, width, height, .. } => {
                    if let Some(old) = newest.replace(Preview { slot, width, height }) {
                        braun.send(&braun::Request::Release(old.slot), &[]);
                    }
                }
                Reply::Saved(shot) => self.photo_done(shot, None),
                Reply::Failed(0, reason) => return Err(Lost::Gone(reason)),
                Reply::Failed(shot, reason) => self.photo_done(shot, Some(&reason)),
                Reply::Opened { .. } | Reply::Captured(_) => {}
            }
        }
    }

    fn next_preview(&mut self) -> Result<Preview, Lost> {
        self.pump(FRAME_WAIT)?.ok_or(Lost::Late)
    }

    fn process(&mut self, request: Request) {
        if self.lost {
            fail_request(&*self.callback, &request);
            return;
        }
        if self.braun.is_none() {
            match Braun::open(&self.camera) {
                Ok(braun) => self.braun = Some(braun),
                Err(reason) => {
                    self.lose(&reason);
                    fail_request(&*self.callback, &request);
                    return;
                }
            }
        }
        let (actions, settings) = self.control.request(request.settings.clone());
        if let Some(braun) = &self.braun {
            if let Some(zoom) = actions.zoom {
                braun.send(&braun::Request::Zoom(zoom.min(MAX_ZOOM)), &[]);
            }
            if let Some((x, y)) = actions.focus {
                braun.send(&braun::Request::Focus(x, y), &[]);
            }
        }
        let preview = match self.next_preview() {
            Ok(preview) => preview,
            Err(Lost::Late) => {
                eprintln!("droidloom camera {}: no preview in time", self.camera.id);
                fail_request(&*self.callback, &request);
                return;
            }
            Err(Lost::Gone(reason)) => {
                self.lose(&reason);
                fail_request(&*self.callback, &request);
                return;
            }
        };
        let timestamp = boottime_ns();
        let shutter = NotifyMsg::Shutter(ShutterMsg {
            frameNumber: request.frame,
            timestamp,
            readoutTimestamp: timestamp,
            ..ShutterMsg::default()
        });
        if let Err(error) = self.callback.notify(&[shutter]) {
            eprintln!("droidloom camera: shutter not delivered: {error}");
        }

        let streams = lock(&self.shared.state).streams.clone();
        let mirrored = self.camera.facing == Facing::Front;
        let mut returned = Vec::new();
        let mut errors = Vec::new();
        let Request { frame: frame_number, outputs, .. } = request;
        for output in outputs {
            let configured = streams.get(&output.stream).copied();
            if configured.is_some_and(|stream| stream.blob) {
                let buffer_size = configured.map_or(0, |stream| stream.buffer_size);
                let photo_size = configured.and_then(|stream| stream.photo_size);
                let degrees = Control::jpeg_orientation(&settings);
                let orientation = exif::orientation(degrees, mirrored);
                if let Err(output) = self.shoot(frame_number, output, buffer_size, photo_size, orientation) {
                    errors.push(output.stream);
                    returned.push(output.returned(false));
                }
                continue;
            }
            let filled = output.ready()
                && output.buffer.as_ref().is_some_and(|buffer| {
                    let Some(bgra) = self.braun.as_ref().and_then(|braun| braun.preview(preview)) else {
                        return false;
                    };
                    let source = Source { bgra, width: preview.width as usize, height: preview.height as usize };
                    buffer.write(|memory, layout| {
                        let planes: Vec<Plane> = layout
                            .buffer
                            .planes
                            .iter()
                            .map(|plane| Plane { offset: plane.offset as usize, stride: plane.stride as usize })
                            .collect();
                        let mut target = Target {
                            fourcc: layout.buffer.format.fourcc,
                            width: layout.buffer.width as usize,
                            height: layout.buffer.height as usize,
                            planes: &planes,
                            memory,
                        };
                        // Android's buffers are what the sensor sees; braun
                        // mirrors the front camera's preview.
                        frame::fill(&source, &mut target, mirrored).is_ok()
                    })
                });
            if !filled {
                errors.push(output.stream);
            }
            returned.push(output.returned(filled));
        }
        if let Some(braun) = &self.braun {
            braun.send(&braun::Request::Release(preview.slot), &[]);
        }
        for stream in errors {
            notify_error(&*self.callback, frame_number, stream, ErrorCode::ERROR_BUFFER);
        }
        let result = self.control.result(settings, timestamp);
        send_result(
            &*self.callback,
            CaptureResult {
                frameNumber: frame_number,
                result: CameraMetadata { metadata: result.encode() },
                outputBuffers: returned,
                inputBuffer: no_input(),
                partialResult: 1,
                ..CaptureResult::default()
            },
        );
    }

    /// Asks braun for a photo into `output`'s buffer.
    fn shoot(
        &mut self,
        frame: i32,
        output: Output,
        buffer_size: usize,
        size: Option<(u32, u32)>,
        orientation: u16,
    ) -> Result<(), Output> {
        // SAFETY: the name is a C string; the descriptor is checked below.
        let raw = unsafe { libc::memfd_create(c"droidloom-photo".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(output);
        }
        // SAFETY: a new descriptor this process owns.
        let file = unsafe { OwnedFd::from_raw_fd(raw) };
        let shot = self.next_shot;
        self.next_shot = self.next_shot.wrapping_add(1).max(1);
        let sent = self
            .braun
            .as_ref()
            .is_some_and(|braun| braun.send(&braun::Request::Shoot(shot, size), &[file.as_fd()]));
        if !sent {
            return Err(output);
        }
        lock(&self.shared.state).photos.push_back(Photo { shot, frame, output, file, buffer_size, orientation });
        Ok(())
    }

    /// braun wrote photo `shot`, or could not.
    fn photo_done(&self, shot: u32, failure: Option<&str>) {
        let photo = {
            let mut state = lock(&self.shared.state);
            let Some(index) = state.photos.iter().position(|photo| photo.shot == shot) else {
                // Flushed meanwhile.
                return;
            };
            state.photos.remove(index)
        };
        let Some(photo) = photo else {
            return;
        };
        if let Some(reason) = failure {
            eprintln!("droidloom camera {}: photo failed: {reason}", self.camera.id);
            fail_photo(&*self.callback, &photo);
            return;
        }
        let written = photo.output.ready()
            && read_all(&photo.file)
                .and_then(|jpeg| exif::with_orientation(&jpeg, photo.orientation))
                .is_some_and(|jpeg| {
                    photo.output.buffer.as_ref().is_some_and(|buffer| {
                        buffer.write(|memory, _| write_blob(memory, &jpeg, photo.buffer_size))
                    })
                });
        if !written {
            fail_photo(&*self.callback, &photo);
            return;
        }
        send_result(&*self.callback, buffers_result(photo.frame, vec![photo.output.returned(true)]));
    }
}

fn read_all(fd: &OwnedFd) -> Option<Vec<u8>> {
    let file = File::from(fd.try_clone().ok()?);
    let length = descriptor_size(fd.as_fd()).ok()?;
    let mut bytes = vec![0; length];
    file.read_exact_at(&mut bytes, 0).ok()?;
    Some(bytes)
}

/// The JPEG at the start of a BLOB buffer and its `CameraBlob` at the end
/// of the size the camera service allocated.
fn write_blob(memory: &mut [u8], jpeg: &[u8], declared: usize) -> bool {
    let size = if declared > 0 { declared.min(memory.len()) } else { memory.len() };
    let Ok(length) = i32::try_from(jpeg.len()) else {
        return false;
    };
    if jpeg.len() + 8 > size {
        return false;
    }
    memory[..jpeg.len()].copy_from_slice(jpeg);
    memory[size - 8..size - 4].copy_from_slice(&BLOB_JPEG.to_le_bytes());
    memory[size - 4..size].copy_from_slice(&length.to_le_bytes());
    true
}

/// What the camera service may configure on `camera`.
fn check_stream(camera: &Camera, stream: &Stream) -> Option<Configured> {
    if stream.streamType != StreamType::OUTPUT || stream.rotation != StreamRotation::ROTATION_0 {
        return None;
    }
    let size = (u32::try_from(stream.width).ok()?, u32::try_from(stream.height).ok()?);
    let format = stream.format.0;
    if format == tags::format::BLOB {
        let full = size == (camera.width, camera.height);
        return (full || stream_sizes(camera).contains(&size)).then_some(Configured {
            blob: true,
            photo_size: (!full).then_some(size),
            buffer_size: usize::try_from(stream.bufferSize).unwrap_or(0),
        });
    }
    let known = format == tags::format::IMPLEMENTATION_DEFINED || format == tags::format::YCBCR_420_888;
    (known && stream_sizes(camera).contains(&size))
        .then_some(Configured { blob: false, photo_size: None, buffer_size: 0 })
}

fn check_configuration(camera: &Camera, configuration: &StreamConfiguration) -> Option<Vec<Configured>> {
    if configuration.operationMode == StreamConfigurationMode::CONSTRAINED_HIGH_SPEED_MODE {
        return None;
    }
    let streams = configuration
        .streams
        .iter()
        .map(|stream| check_stream(camera, stream))
        .collect::<Option<Vec<_>>>()?;
    let photos = streams.iter().filter(|stream| stream.blob).count();
    (photos <= 1 && streams.len() - photos <= MAX_PROCESSED_STREAMS && !streams.is_empty()).then_some(streams)
}

struct Session {
    camera: Camera,
    shared: Arc<Shared>,
    callback: Strong<dyn ICameraDeviceCallback>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Interface for Session {}

impl Session {
    fn flush_all(&self) {
        let requests: Vec<Request> = lock(&self.shared.state).queue.drain(..).collect();
        for request in &requests {
            fail_request(&*self.callback, request);
        }
        let mut state = lock(&self.shared.state);
        let deadline = Instant::now() + Duration::from_secs(2);
        while state.busy {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let photos: Vec<Photo> = state.photos.drain(..).collect();
        drop(state);
        for photo in &photos {
            fail_photo(&*self.callback, photo);
        }
    }

    fn stop(&self) {
        self.flush_all();
        lock(&self.shared.state).closed = true;
        self.shared.changed.notify_all();
        if let Some(worker) = lock(&self.worker).take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

impl ICameraDeviceSession for Session {
    fn close(&self) -> binder::Result<()> {
        self.stop();
        Ok(())
    }

    fn configureStreams(&self, configuration: &StreamConfiguration) -> binder::Result<Vec<HalStream>> {
        let Some(configured) = check_configuration(&self.camera, configuration) else {
            return illegal();
        };
        let mut state = lock(&self.shared.state);
        state.streams = configuration.streams.iter().map(|stream| stream.id).zip(configured).collect();
        let streams = state.streams.clone();
        state.buffers.retain(|(stream, _), _| streams.contains_key(stream));
        drop(state);
        Ok(configuration
            .streams
            .iter()
            .map(|stream| {
                let blob = stream.format.0 == tags::format::BLOB;
                let video = stream.usage.0 & BufferUsage::VIDEO_ENCODER.0 != 0;
                let format = if stream.format.0 != tags::format::IMPLEMENTATION_DEFINED {
                    stream.format
                } else if video {
                    PixelFormat::YCBCR_420_888
                } else {
                    PixelFormat::RGBA_8888
                };
                HalStream {
                    id: stream.id,
                    overrideFormat: format,
                    producerUsage: BufferUsage::CPU_WRITE_OFTEN,
                    consumerUsage: BufferUsage(0),
                    maxBuffers: if blob { MAX_PHOTO_BUFFERS } else { MAX_BUFFERS },
                    overrideDataSpace: stream.dataSpace,
                    ..HalStream::default()
                }
            })
            .collect())
    }

    fn configureStreamsV2(&self, configuration: &StreamConfiguration) -> binder::Result<ConfigureStreamsRet> {
        Ok(ConfigureStreamsRet { halStreams: self.configureStreams(configuration)?, ..ConfigureStreamsRet::default() })
    }

    fn constructDefaultRequestSettings(&self, template: RequestTemplate) -> binder::Result<CameraMetadata> {
        match default_request(&self.camera, template.0) {
            Some(settings) => Ok(CameraMetadata { metadata: settings.encode() }),
            None => illegal(),
        }
    }

    fn flush(&self) -> binder::Result<()> {
        self.flush_all();
        Ok(())
    }

    fn getCaptureRequestMetadataQueue(&self) -> binder::Result<MQDescriptor<i8, SynchronizedReadWrite>> {
        // No queue: settings come inline.
        Ok(MQDescriptor::default())
    }

    fn getCaptureResultMetadataQueue(&self) -> binder::Result<MQDescriptor<i8, SynchronizedReadWrite>> {
        Ok(MQDescriptor::default())
    }

    fn isReconfigurationRequired(&self, _old: &CameraMetadata, _new: &CameraMetadata) -> binder::Result<bool> {
        Ok(false)
    }

    fn processCaptureRequest(&self, requests: &[CaptureRequest], caches: &[BufferCache]) -> binder::Result<i32> {
        let mut state = lock(&self.shared.state);
        for cache in caches {
            state.buffers.remove(&(cache.streamId, cache.bufferId));
        }
        let mut accepted = 0;
        for request in requests {
            let Ok(settings) = Metadata::decode(&request.settings.metadata) else {
                return illegal();
            };
            // Settings come inline: there is no metadata queue.
            let first_without_settings = settings.is_empty() && !state.has_settings;
            if first_without_settings || request.fmqSettingsSize != 0 || request.inputBuffer.streamId != -1 {
                return illegal();
            }
            state.has_settings = true;
            let mut outputs = Vec::with_capacity(request.outputBuffers.len());
            for output in &request.outputBuffers {
                if !state.streams.contains_key(&output.streamId) {
                    return illegal();
                }
                let key = (output.streamId, output.bufferId);
                if !output.buffer.fds.is_empty() {
                    match Buffer::import(&output.buffer) {
                        Ok(buffer) => {
                            state.buffers.insert(key, Arc::new(buffer));
                        }
                        Err(error) => eprintln!("droidloom camera: buffer not imported: {error}"),
                    }
                }
                let acquire = output.acquireFence.fds.first().and_then(|fence| fence.as_ref().try_clone().ok());
                outputs.push(Output {
                    stream: output.streamId,
                    buffer_id: output.bufferId,
                    buffer: state.buffers.get(&key).cloned(),
                    acquire,
                });
            }
            state.queue.push_back(Request { frame: request.frameNumber, settings, outputs });
            accepted += 1;
        }
        self.shared.changed.notify_all();
        Ok(accepted)
    }

    fn signalStreamFlush(&self, _streams: &[i32], _counter: i32) -> binder::Result<()> {
        Ok(())
    }

    fn switchToOffline(
        &self,
        _streams: &[i32],
        _info: &mut CameraOfflineSessionInfo,
    ) -> binder::Result<Strong<dyn ICameraOfflineSession>> {
        unsupported()
    }

    fn repeatingRequestEnd(&self, _frame: i32, _streams: &[i32]) -> binder::Result<()> {
        Ok(())
    }
}

struct Device {
    camera: Camera,
    conflicting: Vec<String>,
}

impl Interface for Device {}

impl ICameraDevice for Device {
    fn getCameraCharacteristics(&self) -> binder::Result<CameraMetadata> {
        Ok(CameraMetadata { metadata: characteristics(&self.camera).encode() })
    }

    fn getPhysicalCameraCharacteristics(&self, _id: &str) -> binder::Result<CameraMetadata> {
        illegal()
    }

    fn getResourceCost(&self) -> binder::Result<CameraResourceCost> {
        Ok(CameraResourceCost { resourceCost: 100, conflictingDevices: self.conflicting.clone() })
    }

    fn isStreamCombinationSupported(&self, streams: &StreamConfiguration) -> binder::Result<bool> {
        Ok(check_configuration(&self.camera, streams).is_some())
    }

    fn open(&self, callback: &Strong<dyn ICameraDeviceCallback>) -> binder::Result<Strong<dyn ICameraDeviceSession>> {
        let shared = Arc::new(Shared { state: Mutex::new(State::default()), changed: Condvar::new() });
        let worker = Worker {
            shared: shared.clone(),
            callback: callback.clone(),
            camera: self.camera.clone(),
            control: Control::new(self.camera.width, self.camera.height, self.camera.lens),
            braun: None,
            next_shot: 1,
            lost: false,
        };
        let worker = thread::Builder::new()
            .name("camera-session".into())
            .spawn(move || worker.run())
            .map_err(|_| Status::new_service_specific_error(CameraStatus::INTERNAL_ERROR.0, None))?;
        let session = Session {
            camera: self.camera.clone(),
            shared,
            callback: callback.clone(),
            worker: Mutex::new(Some(worker)),
        };
        Ok(BnCameraDeviceSession::new_binder(session, BinderFeatures::default()))
    }

    fn openInjectionSession(
        &self,
        _callback: &Strong<dyn ICameraDeviceCallback>,
    ) -> binder::Result<Strong<dyn ICameraInjectionSession>> {
        unsupported()
    }

    fn setTorchMode(&self, _on: bool) -> binder::Result<()> {
        unsupported()
    }

    fn turnOnTorchWithStrengthLevel(&self, _strength: i32) -> binder::Result<()> {
        unsupported()
    }

    fn getTorchStrengthLevel(&self) -> binder::Result<i32> {
        unsupported()
    }

    fn constructDefaultRequestSettings(&self, template: RequestTemplate) -> binder::Result<CameraMetadata> {
        match default_request(&self.camera, template.0) {
            Some(settings) => Ok(CameraMetadata { metadata: settings.encode() }),
            None => illegal(),
        }
    }

    fn isStreamCombinationWithSettingsSupported(&self, streams: &StreamConfiguration) -> binder::Result<bool> {
        Ok(check_configuration(&self.camera, streams).is_some())
    }

    fn getSessionCharacteristics(&self, _configuration: &StreamConfiguration) -> binder::Result<CameraMetadata> {
        let mut session = Metadata::new();
        session.set_f32(tags::SCALER_AVAILABLE_MAX_DIGITAL_ZOOM, &[MAX_ZOOM]);
        Ok(CameraMetadata { metadata: session.encode() })
    }

    fn warmUp(&self) -> binder::Result<()> {
        Ok(())
    }
}

struct ProviderState {
    cameras: Vec<Camera>,
    callback: Option<Strong<dyn ICameraProviderCallback>>,
}

struct Provider(Arc<Mutex<ProviderState>>);

impl Interface for Provider {}

impl Provider {
    /// Asks the broker again while no camera is listed, and tells the
    /// camera service about the cameras once there are.
    fn watch(state: Arc<Mutex<ProviderState>>) {
        for _ in 0..RETRIES {
            if !lock(&state).cameras.is_empty() {
                return;
            }
            thread::sleep(RETRY_EVERY);
            let cameras = list_cameras();
            if cameras.is_empty() {
                continue;
            }
            eprintln!("droidloom camera: {} cameras", cameras.len());
            let count = cameras.len();
            let callback = {
                let mut state = lock(&state);
                state.cameras = cameras;
                state.callback.clone()
            };
            if let Some(callback) = callback {
                for index in 0..count {
                    let _ = callback.cameraDeviceStatusChange(&device_name(index), CameraDeviceStatus::PRESENT);
                }
            }
            return;
        }
    }
}

impl ICameraProvider for Provider {
    fn setCallback(&self, callback: &Strong<dyn ICameraProviderCallback>) -> binder::Result<()> {
        lock(&self.0).callback = Some(callback.clone());
        Ok(())
    }

    fn getVendorTags(&self) -> binder::Result<Vec<VendorTagSection>> {
        Ok(Vec::new())
    }

    fn getCameraIdList(&self) -> binder::Result<Vec<String>> {
        Ok((0..lock(&self.0).cameras.len()).map(device_name).collect())
    }

    fn getCameraDeviceInterface(&self, name: &str) -> binder::Result<Strong<dyn ICameraDevice>> {
        let state = lock(&self.0);
        let index = name
            .strip_prefix("device@1.1/internal/")
            .and_then(|index| index.parse::<usize>().ok())
            .filter(|index| *index < state.cameras.len());
        let Some(index) = index else {
            return illegal();
        };
        let conflicting = (0..state.cameras.len()).filter(|other| *other != index).map(device_name).collect();
        let device = Device { camera: state.cameras[index].clone(), conflicting };
        Ok(BnCameraDevice::new_binder(device, BinderFeatures::default()))
    }

    fn notifyDeviceStateChange(&self, _state: i64) -> binder::Result<()> {
        Ok(())
    }

    fn getConcurrentCameraIds(&self) -> binder::Result<Vec<ConcurrentCameraIdCombination>> {
        Ok(Vec::new())
    }

    fn isConcurrentStreamCombinationSupported(&self, _configs: &[CameraIdAndStreamCombination]) -> binder::Result<bool> {
        Ok(false)
    }
}

fn main() {
    let cameras = list_cameras();
    eprintln!("droidloom camera: {} cameras", cameras.len());
    let state = Arc::new(Mutex::new(ProviderState { cameras, callback: None }));
    let watched = state.clone();
    thread::spawn(move || Provider::watch(watched));
    let provider = BnCameraProvider::new_binder(Provider(state), BinderFeatures::default());
    ProcessState::start_thread_pool();
    binder::add_service(SERVICE_NAME, provider.as_binder()).expect("register the camera provider");
    ProcessState::join_thread_pool();
}
