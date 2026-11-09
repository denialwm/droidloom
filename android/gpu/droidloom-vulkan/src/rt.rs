//! Client runtime: the connection, the request ring, command-buffer streams,
//! handle ids and the encoding sinks the generated code writes through.
//!
//! The per-call path is a bump-pointer reservation and plain stores. Nothing
//! here validates what the application passed: the host driver sees exactly
//! what the application wrote, and a bad call fails there.

use core::ffi::{c_char, c_int, c_void};
use core::sync::atomic::{AtomicU64, Ordering};
use std::cell::Cell;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::{Mutex, MutexGuard, OnceLock};

use droidloom_gpu_protocol::PROTOCOL;
use droidloom_gpu_protocol::layout::{
    self, passed, Control, Header, Hello, Welcome, CHUNK_COUNT, CHUNK_OFFSET, CHUNK_SIZE, MAGIC, STOP_SIZE, RING_OFFSET,
    RING_SIZE,
};
use droidloom_gpu_protocol::sys::{self, Drm, Shm};

pub use droidloom_gpu_protocol::layout::align;

/// Which `vkGet*ProcAddr` may return a command.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Global,
    Instance,
    Physical,
    Device,
}

/// Shape shared by every structure that can be chained.
#[repr(C)]
pub struct Base {
    pub sType: i32,
    pub pNext: *const c_void,
}

/// What the loader expects at the start of every dispatchable object.
const ICD_LOADER_MAGIC: usize = 0x01CD_C0DE;

/// Header of every dispatchable object: the loader's word, then our id.
#[repr(C)]
pub struct Object {
    pub loader: usize,
    pub id: u64,
}

#[inline(always)]
pub fn id(handle: usize) -> u64 {
    // SAFETY: every dispatchable handle we hand out points at an `Object`.
    unsafe { (*(handle as *const Object)).id }
}

pub fn fail(message: &str) -> ! {
    report(message);
    std::process::abort()
}

/// Android applications drop stderr, so there the message goes to the log.
pub fn report(message: &str) {
    #[cfg(target_os = "android")]
    {
        #[link(name = "log")]
        unsafe extern "C" {
            fn __android_log_write(priority: i32, tag: *const core::ffi::c_char, text: *const core::ffi::c_char) -> i32;
        }
        let text = std::ffi::CString::new(message.replace('\0', " ")).unwrap_or_default();
        // SAFETY: both strings are NUL-terminated and outlive the call.
        unsafe { __android_log_write(6, c"droidloom-vulkan".as_ptr(), text.as_ptr()) };
    }
    #[cfg(not(target_os = "android"))]
    eprintln!("droidloom-vulkan: {message}");
}

/// A pNext structure the forwarding does not know yet. Dropped from the
/// chain; reported once per type in debug builds so it can be added.
pub fn skipped(s_type: i32) {
    if cfg!(debug_assertions) {
        static SEEN: Mutex<Vec<i32>> = Mutex::new(Vec::new());
        let mut seen = SEEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !seen.contains(&s_type) {
            seen.push(s_type);
            eprintln!("droidloom-vulkan: dropped unknown pNext structure {s_type}");
        }
    }
}

// ---------------------------------------------------------------- connection

pub struct Connection {
    shm: Shm,
    socket: OwnedFd,
    ring: Mutex<u32>,
    fds: Mutex<()>,
    chunks: Mutex<Vec<u32>>,
    next_chunk: AtomicU64,
    pub drm: Drm,
}

static CONNECTION: OnceLock<Connection> = OnceLock::new();

/// Android finds the host's socket in the cell; desktop clients in the
/// user's runtime directory.
fn socket_path() -> String {
    if cfg!(target_os = "android") {
        // A development build for a cell without the forwarding pairing
        // names the path the cell binds instead.
        return option_env!("DROIDLOOM_CELL_GPU_SOCKET").unwrap_or("/dev/socket/droidloom/gpu").into();
    }
    std::env::var("DROIDLOOM_GPU_SOCKET").unwrap_or_else(|_| {
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        format!("{runtime}/droidloom-gpu.sock")
    })
}

fn connect() -> Result<Connection, String> {
    let path = socket_path();
    let stream = std::os::unix::net::UnixStream::connect(&path).map_err(|error| format!("connect {path}: {error}"))?;
    let socket = OwnedFd::from(stream);
    let hello = Hello { magic: MAGIC, protocol: PROTOCOL, android: u64::from(cfg!(target_os = "android")) };
    // SAFETY: `Hello` is plain data.
    let bytes = unsafe { core::slice::from_raw_parts((&raw const hello).cast::<u8>(), size_of::<Hello>()) };
    sys::send(socket.as_fd(), bytes, None).map_err(|error| error.to_string())?;
    let mut welcome = [0u8; size_of::<Welcome>()];
    let (size, memory) = sys::recv(socket.as_fd(), &mut welcome).map_err(|error| error.to_string())?;
    // SAFETY: the buffer holds one `Welcome` when the size matches.
    let welcome: Welcome = unsafe { welcome.as_ptr().cast::<Welcome>().read_unaligned() };
    if size != size_of::<Welcome>() || welcome.magic != MAGIC {
        return Err("the GPU server refused the connection".into());
    }
    if welcome.protocol != PROTOCOL {
        return Err(format!("protocol {:016x} on the host, {PROTOCOL:016x} here: rebuild both from one tree", welcome.protocol));
    }
    let memory = memory.ok_or("the GPU server sent no shared memory")?;
    let shm = Shm::map(memory.as_raw_fd(), welcome.shm_size as usize).map_err(|error| error.to_string())?;
    // Syncobjs are not tied to a GPU: any render node carries the timelines.
    // The cell has exactly one, whichever GPU it belongs to.
    let node = if cfg!(target_os = "android") {
        b"/dev/dri/renderD128".to_vec()
    } else {
        let node = welcome.render_node.split(|&b| b == 0).next().unwrap_or_default();
        std::env::var("DROIDLOOM_GPU_RENDER_NODE").map_or_else(|_| node.to_vec(), String::into_bytes)
    };
    let node = std::ffi::CString::new(node).map_err(|_| "bad render node name")?;
    let drm = Drm::open(&node).map_err(|error| format!("open {}: {error}", node.to_string_lossy()))?;
    Ok(Connection {
        shm,
        socket,
        ring: Mutex::new(0),
        fds: Mutex::new(()),
        chunks: Mutex::new(Vec::new()),
        next_chunk: AtomicU64::new(0),
        drm,
    })
}

#[inline(always)]
pub fn conn() -> &'static Connection {
    CONNECTION.get().unwrap_or_else(|| fail("Vulkan used before vkCreateInstance"))
}

/// Connect once; later instances share the connection.
pub fn ensure_connected() -> Result<(), String> {
    if CONNECTION.get().is_none() {
        let connection = connect()?;
        let _ = CONNECTION.set(connection);
    }
    Ok(())
}

impl Connection {
    fn control(&self) -> &Control {
        // SAFETY: the control block is at the start of the region.
        unsafe { &*self.shm.base.cast::<Control>() }
    }

    fn base(&self) -> *mut u8 {
        self.shm.base
    }

    /// A chunk of the region for a stream or a reply area.
    pub fn chunk(&self) -> u32 {
        if let Some(chunk) = self.chunks.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pop() {
            return chunk;
        }
        self.fresh_chunks(1)
    }

    /// `count` adjacent chunks never handed out before; the first's index.
    fn fresh_chunks(&self, count: usize) -> u32 {
        let first = self.next_chunk.fetch_add(count as u64, Ordering::Relaxed);
        if first as usize + count > CHUNK_COUNT {
            fail("shared memory exhausted");
        }
        first as u32
    }

    pub fn release_chunks(&self, chunks: &mut Vec<u32>) {
        self.chunks.lock().unwrap_or_else(std::sync::PoisonError::into_inner).append(chunks);
    }

    pub fn chunk_ptr(&self, chunk: u32) -> *mut u8 {
        // SAFETY: chunk indices stay below CHUNK_COUNT.
        unsafe { self.base().add(CHUNK_OFFSET + chunk as usize * CHUNK_SIZE) }
    }

    pub fn offset_of(&self, pointer: *const u8) -> u64 {
        pointer as u64 - self.base() as u64
    }

    pub fn send_fd(&self, fd: c_int) {
        if sys::send(self.socket.as_fd(), &[0], Some(fd)).is_err() {
            fail("the GPU server went away");
        }
    }

    pub fn recv_fd(&self) -> c_int {
        let mut byte = [0u8];
        match sys::recv(self.socket.as_fd(), &mut byte) {
            Ok((_, Some(fd))) => {
                use std::os::fd::IntoRawFd;
                fd.into_raw_fd()
            }
            _ => fail("the GPU server sent no descriptor"),
        }
    }
}

pub fn send_fd(fd: c_int) {
    conn().send_fd(fd);
}

pub fn recv_fd() -> c_int {
    conn().recv_fd()
}

/// Held around a call that returns descriptors, so they arrive in order.
pub fn fd_lock() -> MutexGuard<'static, ()> {
    conn().fds.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------- ring

pub struct Ring {
    head: MutexGuard<'static, u32>,
    end: u32,
}

pub fn ring() -> Ring {
    Ring { head: conn().ring.lock().unwrap_or_else(std::sync::PoisonError::into_inner), end: 0 }
}

impl Ring {
    /// Room for one record of `size` bytes, header written.
    #[inline]
    pub fn reserve(&mut self, op: u32, size: usize) -> *mut u8 {
        let connection = conn();
        let size = align(size) as u32;
        if size as usize > RING_SIZE / 2 {
            fail("request larger than the ring");
        }
        let mut head = *self.head;
        let offset = head as usize & (RING_SIZE - 1);
        let room = (RING_SIZE - offset) as u32;
        if size > room {
            // Records never wrap: pad to the start.
            wait_for_space(head, room);
            // SAFETY: `offset` is inside the ring.
            unsafe { write_header(connection.base().add(RING_OFFSET + offset), layout::op::PAD, room) };
            head = head.wrapping_add(room);
        }
        wait_for_space(head, size);
        let record = unsafe { connection.base().add(RING_OFFSET + (head as usize & (RING_SIZE - 1))) };
        // SAFETY: the record lies in free ring space.
        unsafe { write_header(record, op, size) };
        *self.head = head;
        self.end = head.wrapping_add(size);
        record
    }

    /// Make the reserved record visible. Wakes the host only when asked:
    /// calls that need no answer let requests pile up until one does.
    pub fn publish(&mut self, wake: bool) -> u32 {
        *self.head = self.end;
        let control = conn().control();
        control.head.0.store(self.end, Ordering::SeqCst);
        if wake {
            wake_host(control);
        }
        self.end
    }
}

fn wake_host(control: &Control) {
    if control.host_sleeping.0.load(Ordering::SeqCst) != 0 {
        control.host_sleeping.0.store(0, Ordering::SeqCst);
        sys::futex_wake(&control.head.0);
    }
}

unsafe fn write_header(record: *mut u8, op: u32, size: u32) {
    unsafe { record.cast::<Header>().write(Header { op, size }) };
}

fn wait_for_space(head: u32, size: u32) {
    let control = conn().control();
    while head.wrapping_add(size).wrapping_sub(control.tail.0.load(Ordering::Acquire)) as usize > RING_SIZE {
        wake_host(control);
        wait_tail(control, |tail| head.wrapping_add(size).wrapping_sub(tail) as usize <= RING_SIZE);
    }
}

/// How long a waiting thread spins before it sleeps. A reply usually takes
/// what the call costs natively, time the thread would spend in the driver.
const SPIN_NS: i64 = 300_000;

fn wait_tail(control: &Control, done: impl Fn(u32) -> bool) {
    let deadline = sys::monotonic_ns() + SPIN_NS;
    loop {
        for _ in 0..64 {
            if done(control.tail.0.load(Ordering::Acquire)) {
                return;
            }
            core::hint::spin_loop();
        }
        if sys::monotonic_ns() > deadline {
            break;
        }
    }
    control.client_waiters.0.fetch_add(1, Ordering::SeqCst);
    loop {
        let tail = control.tail.0.load(Ordering::SeqCst);
        if done(tail) {
            break;
        }
        // Long waits check now and then that the server is still there.
        sys::futex_wait_for(&control.tail.0, tail, 100_000_000);
        if !done(control.tail.0.load(Ordering::SeqCst)) && sys::hung_up(conn().socket.as_fd()) {
            fail("the GPU server went away");
        }
    }
    control.client_waiters.0.fetch_sub(1, Ordering::SeqCst);
}

/// Wait until the host has finished every request up to `end`.
pub fn wait(end: u32) {
    let control = conn().control();
    wake_host(control);
    wait_tail(control, |tail| passed(tail, end));
}

pub fn passed_now(end: u32) -> bool {
    passed(conn().control().tail.0.load(Ordering::Acquire), end)
}

// ---------------------------------------------------------------- ids and objects

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static FREE_IDS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
/// The host's handle table holds this many entries.
pub const MAX_IDS: u64 = 1 << 24;

pub fn new_id() -> u64 {
    if let Some(id) = FREE_IDS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pop() {
        return id;
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    if id >= MAX_IDS {
        fail("out of object ids");
    }
    id
}

pub fn free_id(id: u64) {
    FREE_IDS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(id);
}

pub fn object(id: u64) -> Object {
    Object { loader: ICD_LOADER_MAGIC, id }
}

pub fn new_dispatchable(id: u64) -> usize {
    Box::into_raw(Box::new(object(id))) as usize
}

pub fn free_dispatchable(handle: usize) {
    free_id(id(handle));
    // SAFETY: plain dispatchable objects come from `new_dispatchable`.
    drop(unsafe { Box::from_raw(handle as *mut Object) });
}

// ---------------------------------------------------------------- sinks

/// Where generated encoders put a record's data.
pub trait Sink {
    /// Reserve `size` bytes of record data; the offset from the record start.
    fn alloc(&mut self, size: usize) -> i64;
    /// Reserve reply space in the calling thread's reply area.
    fn reply_alloc(&mut self, size: usize) -> i64;
    unsafe fn put<T>(&mut self, at: i64, value: T);
    unsafe fn copy(&mut self, at: i64, source: *const u8, size: usize);
    /// Hand a descriptor the application gave up to the host.
    unsafe fn send_fd(&mut self, fd: c_int);
    /// Give the host a copy of a descriptor the caller keeps.
    fn lend_fd(&mut self, fd: c_int);
}

/// First pass: only sizes.
pub struct Measure(usize);

impl Measure {
    pub fn new(frame: usize) -> Self {
        Self(frame)
    }

    pub fn size(&self) -> usize {
        self.0
    }
}

impl Sink for Measure {
    #[inline(always)]
    fn alloc(&mut self, size: usize) -> i64 {
        let at = self.0;
        self.0 += align(size);
        at as i64
    }
    #[inline(always)]
    fn reply_alloc(&mut self, _size: usize) -> i64 {
        0
    }
    #[inline(always)]
    unsafe fn put<T>(&mut self, _at: i64, _value: T) {}
    #[inline(always)]
    unsafe fn copy(&mut self, _at: i64, _source: *const u8, _size: usize) {}
    #[inline(always)]
    unsafe fn send_fd(&mut self, _fd: c_int) {}
    #[inline(always)]
    fn lend_fd(&mut self, _fd: c_int) {}
}

thread_local! {
    /// The thread's reply area: start and end.
    static REPLY: Cell<(*mut u8, *mut u8)> = const { Cell::new((core::ptr::null_mut(), core::ptr::null_mut())) };
}

/// The thread's reply area, one chunk until a reply needs more. A call's
/// first reply takes the area from its start; a reply that does not fit
/// moves the thread to adjacent chunks it fits in. The area left behind
/// may still hold this call's earlier replies, so it is not reused.
fn reply_area(size: usize, first: bool) -> (*mut u8, *mut u8) {
    REPLY.with(|area| {
        let (start, end) = area.get();
        if first && end as usize - start as usize >= size {
            return (start, end);
        }
        let connection = conn();
        let count = size.div_ceil(CHUNK_SIZE).max(1);
        let start = connection.chunk_ptr(if count == 1 { connection.chunk() } else { connection.fresh_chunks(count) });
        // SAFETY: the chunks are adjacent and inside the region.
        let end = unsafe { start.add(count * CHUNK_SIZE) };
        area.set((start, end));
        (start, end)
    })
}

/// Second pass: the bytes, into a reserved record.
pub struct Write {
    record: *mut u8,
    position: usize,
    reply: *mut u8,
    reply_end: *mut u8,
}

impl Write {
    #[inline(always)]
    pub fn new(record: *mut u8, frame: usize) -> Self {
        Self { record, position: frame, reply: core::ptr::null_mut(), reply_end: core::ptr::null_mut() }
    }

    /// Address of `at`, an offset from the record start.
    #[inline(always)]
    pub fn at<T>(&self, at: i64) -> *mut T {
        unsafe { self.record.offset(at as isize).cast() }
    }
}

impl Sink for Write {
    #[inline(always)]
    fn alloc(&mut self, size: usize) -> i64 {
        let at = self.position;
        self.position += align(size);
        at as i64
    }
    #[inline(always)]
    fn reply_alloc(&mut self, size: usize) -> i64 {
        let size = align(size);
        if self.reply.is_null() || (self.reply_end as usize - self.reply as usize) < size {
            (self.reply, self.reply_end) = reply_area(size, self.reply.is_null());
        }
        let at = self.reply as i64 - self.record as i64;
        self.reply = unsafe { self.reply.add(size) };
        at
    }
    #[inline(always)]
    unsafe fn put<T>(&mut self, at: i64, value: T) {
        unsafe { self.record.offset(at as isize).cast::<T>().write_unaligned(value) };
    }
    #[inline(always)]
    unsafe fn copy(&mut self, at: i64, source: *const u8, size: usize) {
        unsafe { core::ptr::copy_nonoverlapping(source, self.record.offset(at as isize), size) };
    }
    unsafe fn send_fd(&mut self, fd: c_int) {
        // An import takes the descriptor; the host now holds its copy.
        conn().send_fd(fd);
        unsafe { libc::close(fd) };
    }
    fn lend_fd(&mut self, fd: c_int) {
        conn().send_fd(fd);
    }
}

pub unsafe fn enc_pod<T, W: Sink>(w: &mut W, source: *const T, count: usize) -> i64 {
    let size = size_of::<T>() * count;
    let at = w.alloc(size);
    unsafe { w.copy(at, source.cast(), size) };
    at
}

pub unsafe fn enc_bytes<W: Sink>(w: &mut W, source: *const u8, size: usize) -> i64 {
    let at = w.alloc(size);
    unsafe { w.copy(at, source, size) };
    at
}

pub unsafe fn enc_str<W: Sink>(w: &mut W, string: *const c_char) -> i64 {
    let size = unsafe { libc::strlen(string) } + 1;
    unsafe { enc_bytes(w, string.cast(), size) }
}

pub unsafe fn enc_strs<W: Sink>(w: &mut W, strings: *const *const c_char, count: usize) -> i64 {
    let at = w.alloc(8 * count);
    for i in 0..count {
        let string = unsafe { enc_str(w, *strings.add(i)) };
        unsafe { w.put(at + 8 * i as i64, string) };
    }
    at
}

pub unsafe fn enc_ids<W: Sink>(w: &mut W, handles: *const usize, count: usize) -> i64 {
    let at = w.alloc(8 * count);
    for i in 0..count {
        unsafe { w.put(at + 8 * i as i64, id(*handles.add(i))) };
    }
    at
}

// ---------------------------------------------------------------- streams

/// A command buffer: an id, then a record stream in region chunks.
#[repr(C)]
pub struct CommandBuffer {
    pub object: Object,
    cursor: *mut u8,
    end: *mut u8,
    first: u64,
    chunks: Vec<u32>,
    used: usize,
    /// Ring position of the last RECORD, once the host may still read us.
    recorded: Option<u32>,
}

#[inline(always)]
pub fn stream(handle: usize) -> &'static mut CommandBuffer {
    // SAFETY: command-buffer handles point at `CommandBuffer`; Vulkan makes
    // the application synchronize access to each one.
    unsafe { &mut *(handle as *mut CommandBuffer) }
}

impl CommandBuffer {
    pub fn new(id: u64) -> usize {
        Box::into_raw(Box::new(Self {
            object: object(id),
            cursor: core::ptr::null_mut(),
            end: core::ptr::null_mut(),
            first: 0,
            chunks: Vec::new(),
            used: 0,
            recorded: None,
        })) as usize
    }

    pub fn free(handle: usize) {
        // SAFETY: from `new`.
        let mut buffer = unsafe { Box::from_raw(handle as *mut Self) };
        buffer.settle();
        conn().release_chunks(&mut buffer.chunks);
        free_id(buffer.object.id);
    }

    /// Wait until the host no longer reads this stream.
    fn settle(&mut self) {
        if let Some(end) = self.recorded.take() {
            if !passed_now(end) {
                wait(end);
            }
        }
    }

    /// Start a new stream, reusing this buffer's chunks.
    pub fn begin(&mut self) {
        self.settle();
        self.used = 0;
        self.next_chunk();
        self.first = conn().offset_of(self.cursor);
    }

    fn next_chunk(&mut self) {
        let connection = conn();
        if self.used == self.chunks.len() {
            self.chunks.push(connection.chunk());
        }
        self.cursor = connection.chunk_ptr(self.chunks[self.used]);
        // SAFETY: inside the chunk.
        self.end = unsafe { self.cursor.add(CHUNK_SIZE) };
        self.used += 1;
    }

    /// Room for one stream record. The only check on the recording path.
    #[inline(always)]
    pub fn reserve(&mut self, op: u32, size: usize) -> *mut u8 {
        if (self.end as usize - self.cursor as usize) < size + STOP_SIZE {
            self.grow(size);
        }
        let record = self.cursor;
        // SAFETY: the check above left room for the record and a STOP.
        unsafe {
            write_header(record, op, size as u32);
            self.cursor = record.add(size);
        }
        record
    }

    #[cold]
    fn grow(&mut self, size: usize) {
        if size + STOP_SIZE > CHUNK_SIZE {
            fail("command larger than a stream chunk");
        }
        // SAFETY: STOP_SIZE bytes were kept free for this.
        unsafe { write_header(self.cursor, layout::op::STOP, STOP_SIZE as u32) };
        self.send();
        self.next_chunk();
        self.first = conn().offset_of(self.cursor);
    }

    /// Hand the finished stream to the host for replay.
    pub fn finish(&mut self) {
        self.send();
    }

    /// Queue the part since `first` for replay, waking a sleeping host.
    fn send(&mut self) {
        let mut ring = ring();
        let record = ring.reserve(layout::op::RECORD, size_of::<layout::Record>());
        // SAFETY: a reserved RECORD.
        unsafe {
            let record = record.cast::<layout::Record>();
            (*record).command_buffer = self.object.id;
            (*record).stream = self.first;
        }
        self.recorded = Some(ring.publish(true));
    }
}

// ---------------------------------------------------------------- id tables

/// One `T` per possible id in lazily committed, zero-filled memory, so a
/// lookup is one index.
pub struct IdTable<T> {
    base: OnceLock<usize>,
    name: &'static str,
    /// Entries are reached through raw pointers; the table itself is shared.
    _entry: core::marker::PhantomData<fn() -> T>,
}

impl<T> IdTable<T> {
    pub const fn new(name: &'static str) -> Self {
        Self { base: OnceLock::new(), name, _entry: core::marker::PhantomData }
    }

    #[inline(always)]
    pub fn at(&self, id: u64) -> *mut T {
        let base = *self.base.get_or_init(|| {
            let size = MAX_IDS as usize * size_of::<T>();
            // SAFETY: an anonymous private mapping, zero-filled on touch.
            let base = unsafe {
                libc::mmap(core::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE, -1, 0)
            };
            if base == libc::MAP_FAILED {
                fail(&format!("map the {} table", self.name));
            }
            base as usize
        });
        // SAFETY: ids are below MAX_IDS.
        unsafe { (base as *mut T).add(id as usize) }
    }
}

// ---------------------------------------------------------------- memory

/// Host-visible memory: the memfd the host imported or the DMA-BUF it
/// exported, mapped here too, so vkMapMemory is a pointer and writes reach
/// the GPU without a copy.
pub struct Mapping {
    pub base: *mut u8,
    pub size: usize,
}

pub static MAPPINGS: IdTable<Mapping> = IdTable::new("memory");

/// The physical device each device was created on.
pub static PHYSICAL: IdTable<usize> = IdTable::new("device");

// ---------------------------------------------------------------- fences

/// Fence state lives here, not on the host: a fence is a point on its
/// queue's timeline syncobj, and waits go straight to the kernel.
pub struct FenceState {
    /// 0: signaled. `u64::MAX`: never submitted since the last reset.
    pub point: AtomicU64,
    pub timeline: AtomicU64,
}

pub const UNSUBMITTED: u64 = u64::MAX;

static FENCES: IdTable<FenceState> = IdTable::new("fence");

pub fn fence(id: u64) -> &'static FenceState {
    // SAFETY: entries are atomics.
    unsafe { &*FENCES.at(id) }
}

/// A queue: an id and the timeline its fenced submissions signal.
#[repr(C)]
pub struct Queue {
    pub object: Object,
    pub device: usize,
    pub timeline: u32,
    pub point: AtomicU64,
}

pub fn queue(handle: usize) -> &'static Queue {
    // SAFETY: queue handles point at `Queue`.
    unsafe { &*(handle as *const Queue) }
}

/// An instance and the physical devices it was given.
#[repr(C)]
pub struct Instance {
    pub object: Object,
    pub physical: Vec<usize>,
}

