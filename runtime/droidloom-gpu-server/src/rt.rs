//! Server runtime: one Android process's connection, its handle table and
//! the loop that runs records against the host driver.

use core::ffi::{c_int, c_void};
use core::sync::atomic::Ordering;
use std::collections::HashMap;
use std::os::fd::{AsFd, IntoRawFd, OwnedFd};

use droidloom_gpu_protocol::generated::vk::{PFN_vkGetInstanceProcAddr, VkDeviceMemory, VkFence, VkResult, VkSemaphore};
use droidloom_gpu_protocol::layout::{Control, Header, RING_OFFSET, RING_SIZE};
use droidloom_gpu_protocol::sys::{self, Drm, Shm};

use crate::generated::{Fns, TABLE};
use crate::probe::Shareable;

/// Handler for one record. Returns where a stream continues (null ends it);
/// ring records ignore the result.
pub type Handler = unsafe fn(&mut Server, *mut u8) -> *mut u8;

/// Dispatch table size: a power of two above every opcode, so the masked
/// opcode indexes it without a bounds check.
pub const SLOTS: usize = 1024;

/// Ids the client may use; the table maps each to a driver handle.
const MAX_IDS: usize = 1 << 24;

#[repr(C)]
pub struct Base {
    pub sType: i32,
    pub pNext: *mut Base,
}

/// A queue's timeline: fenced submissions signal consecutive points.
pub struct Timeline {
    /// The device the queue belongs to.
    pub device: usize,
    pub family: u32,
    /// Exportable binary semaphore for Android release fences.
    pub release: VkSemaphore,
    /// Exportable fence for submissions that signal timeline semaphores
    /// without a fence of their own.
    pub fence: VkFence,
    pub handle: u32,
    pub scratch: u32,
    pub point: u64,
}

pub struct Server {
    table: *mut u64,
    /// Command buffer whose stream is being replayed.
    pub cb: usize,
    pub fns: Fns,
    pub base: *mut u8,
    socket: OwnedFd,
    pub drm: Drm,
    pub gipa: PFN_vkGetInstanceProcAddr,
    pub instance: usize,
    /// Devices created on this connection.
    pub devices: usize,
    pub timelines: HashMap<u64, Timeline>,
    /// Property flags of the device's memory types, as Android sees them.
    pub memory_types: [u32; 32],
    /// Host-visible system-memory allocations, backed by memfds shared with
    /// the client.
    pub shared: HashMap<u64, Shm>,
    /// Physical devices shown to Android, from every instance, each with
    /// its place among the GPUs shown.
    pub shown: HashMap<usize, usize>,
    /// Per shown device: the memory types whose mappings a client can share.
    pub host_memory: Vec<Shareable>,
    /// The (DRM format, modifier) pairs the compositor imports, which the
    /// modifiers Android sees are kept to (layouts.rs); None for all.
    pub imported: Option<Vec<(u32, u64)>>,
    /// The PCI address of the one GPU shown to Android.
    pub gpu: String,
    /// The client is Android's loader; see `Hello::android`.
    pub android: bool,
    /// Gralloc buffers imported at image creation, freed with their image.
    pub image_memory: HashMap<u64, VkDeviceMemory>,
    /// Host timeline semaphores and the client's kernel timeline for each.
    pub semaphore_timelines: HashMap<VkSemaphore, u32>,
    /// Window buffers the application draws into through a shadow.
    pub shadows: HashMap<u64, crate::shadow::Shadow>,
    pub foreign: HashMap<u64, crate::shadow::Foreign>,
    /// Per device, the ETC2 and ASTC decoder.
    pub decoders: HashMap<usize, crate::compressed::Decoder>,
    /// ETC2 and ASTC images by host handle, with their blocks.
    pub compressed: HashMap<u64, crate::compressed::Compressed>,
    /// Command pools, and whether their queue family runs compute.
    pub pools: HashMap<u64, bool>,
    /// Per command buffer, the compute state a decode puts back.
    pub streams: HashMap<usize, Box<crate::compressed::Stream>>,
    /// The stream of `cb`.
    pub stream: *mut crate::compressed::Stream,
    _shm: Shm,
}

impl Server {
    pub fn new(shm: Shm, socket: OwnedFd, drm: Drm, gipa: PFN_vkGetInstanceProcAddr, gpu: String, host_memory: Vec<Shareable>, imported: Option<Vec<(u32, u64)>>, android: bool) -> Self {
        // SAFETY: anonymous private mapping, zero-filled on touch; id 0 maps
        // to the null handle with no special case.
        let table = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                MAX_IDS * 8,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if table == libc::MAP_FAILED {
            fail("map the handle table");
        }
        let mut fns = Fns::missing();
        // SAFETY: the loader's global commands.
        unsafe { fns.load_global(gipa, 0) };
        Self {
            table: table.cast(),
            cb: 0,
            fns,
            base: shm.base,
            socket,
            drm,
            gipa,
            instance: 0,
            devices: 0,
            timelines: HashMap::new(),
            memory_types: [0; 32],
            shared: HashMap::new(),
            shown: HashMap::new(),
            host_memory,
            imported,
            gpu,
            android,
            image_memory: HashMap::new(),
            semaphore_timelines: HashMap::new(),
            shadows: HashMap::new(),
            foreign: HashMap::new(),
            decoders: HashMap::new(),
            compressed: HashMap::new(),
            pools: HashMap::new(),
            streams: HashMap::new(),
            stream: core::ptr::null_mut(),
            _shm: shm,
        }
    }

    /// Driver handle for a client id.
    #[inline(always)]
    pub fn h(&self, id: u64) -> u64 {
        // SAFETY: client ids stay below MAX_IDS.
        unsafe { *self.table.add(id as usize) }
    }

    #[inline(always)]
    pub fn set(&mut self, id: u64, handle: u64) {
        // SAFETY: as for `h`.
        unsafe { *self.table.add(id as usize) = handle };
    }

    /// The next descriptor the client sent ahead of the current record.
    pub fn take_fd(&mut self) -> c_int {
        let mut byte = [0u8];
        match sys::recv(self.socket.as_fd(), &mut byte) {
            Ok((_, Some(fd))) => fd.into_raw_fd(),
            _ => fail("expected a descriptor from the client"),
        }
    }

    /// Send a descriptor the driver produced, then drop ours.
    pub fn send_fd(&mut self, fd: c_int) {
        if fd < 0 {
            return;
        }
        if sys::send(self.socket.as_fd(), &[0], Some(fd)).is_err() {
            fail("client went away");
        }
        // SAFETY: the driver handed this descriptor to us.
        unsafe { libc::close(fd) };
    }


    /// Process ring records until the client disconnects.
    pub fn run(&mut self) -> ! {
        // SAFETY: the control block heads the region.
        let control = unsafe { &*self.base.cast::<Control>() };
        let ring = unsafe { self.base.add(RING_OFFSET) };
        let mut tail = 0u32;
        loop {
            let head = control.head.0.load(Ordering::Acquire);
            if head == tail {
                idle(control, tail);
                continue;
            }
            while tail != head {
                // SAFETY: the client published every record before `head`.
                unsafe {
                    let record = ring.add(tail as usize & (RING_SIZE - 1));
                    let header = *record.cast::<Header>();
                    TABLE[header.op as usize & (SLOTS - 1)](self, record);
                    tail = tail.wrapping_add(header.size);
                }
                control.tail.0.store(tail, Ordering::SeqCst);
                if control.client_waiters.0.load(Ordering::SeqCst) != 0 {
                    sys::futex_wake(&control.tail.0);
                }
            }
        }
    }
}

/// How long the host spins for the next record before it sleeps.
const SPIN_NS: i64 = 300_000;

/// Nothing to do: spin briefly, then sleep until the client wakes us.
fn idle(control: &Control, tail: u32) {
    let deadline = sys::monotonic_ns() + SPIN_NS;
    loop {
        for _ in 0..64 {
            if control.head.0.load(Ordering::Acquire) != tail {
                return;
            }
            core::hint::spin_loop();
        }
        if sys::monotonic_ns() > deadline {
            break;
        }
    }
    control.host_sleeping.0.store(1, Ordering::SeqCst);
    if control.head.0.load(Ordering::SeqCst) == tail {
        sys::futex_wait(&control.head.0, tail);
    }
    control.host_sleeping.0.store(0, Ordering::SeqCst);
}

/// Pointer for a record offset; 0 is null.
#[inline(always)]
pub unsafe fn at<T>(record: *mut u8, offset: i64) -> *mut T {
    if offset == 0 { core::ptr::null_mut() } else { unsafe { record.offset(offset as isize).cast() } }
}

#[inline(always)]
pub unsafe fn next(record: *mut u8) -> *mut u8 {
    unsafe { record.add((*record.cast::<Header>()).size as usize) }
}

pub fn fail(message: &str) -> ! {
    eprintln!("droidloom-gpu-server: {message}");
    std::process::abort()
}

pub fn failed(command: &str, result: VkResult) -> ! {
    fail(&format!("{command} failed with {result}"))
}

pub fn missing(command: &str) -> ! {
    fail(&format!("the host driver has no {command}"))
}

pub unsafe fn unknown(_: &mut Server, record: *mut u8) -> *mut u8 {
    fail(&format!("unknown record opcode {}", unsafe { (*record.cast::<Header>()).op }))
}

/// Look up an instance-level driver entry point.
pub unsafe fn instance_proc(s: &Server, name: &core::ffi::CStr) -> *const c_void {
    unsafe { (s.gipa)(s.instance, name.as_ptr()).map_or(core::ptr::null(), |f| f as *const c_void) }
}
