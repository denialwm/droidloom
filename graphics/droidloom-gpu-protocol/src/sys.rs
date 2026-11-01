//! The few kernel interfaces both ends use: futexes on the shared region,
//! the region itself, descriptor passing and DRM syncobj timelines.

use core::sync::atomic::AtomicU32;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 { Err(io::Error::last_os_error()) } else { Ok(result) }
}

/// Sleep while `word` holds `expected`. Shared, not private: the other end
/// is another process mapping the same pages.
pub fn futex_wait(word: &AtomicU32, expected: u32) {
    // SAFETY: the word lives in a mapping that outlives the call.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAIT,
            expected,
            core::ptr::null::<libc::timespec>(),
        );
    }
}

/// As `futex_wait`, giving up after `ns`.
pub fn futex_wait_for(word: &AtomicU32, expected: u32, ns: i64) {
    let timeout = libc::timespec { tv_sec: ns / 1_000_000_000, tv_nsec: ns % 1_000_000_000 };
    // SAFETY: as for `futex_wait`; the timeout is relative.
    unsafe {
        libc::syscall(libc::SYS_futex, word.as_ptr(), libc::FUTEX_WAIT, expected, &raw const timeout);
    }
}

/// True once the other end of `socket` has hung up.
pub fn hung_up(socket: BorrowedFd<'_>) -> bool {
    let mut poll = libc::pollfd { fd: socket.as_raw_fd(), events: libc::POLLRDHUP, revents: 0 };
    // SAFETY: one pollfd on a live descriptor, no waiting.
    unsafe { libc::poll(&raw mut poll, 1, 0) > 0 && poll.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0 }
}

/// Wake every thread waiting on `word`.
pub fn futex_wake(word: &AtomicU32) {
    // SAFETY: as for `futex_wait`.
    unsafe {
        libc::syscall(libc::SYS_futex, word.as_ptr(), libc::FUTEX_WAKE, i32::MAX);
    }
}

/// The shared region, mapped read-write.
pub struct Shm {
    /// First byte of the mapping.
    pub base: *mut u8,
    /// Length of the mapping.
    pub size: usize,
}

// SAFETY: the region is plain shared memory; access is coordinated by the
// ring protocol, not by Rust ownership.
unsafe impl Send for Shm {}
unsafe impl Sync for Shm {}

impl Shm {
    /// Create a region backed by a sealed-size memfd.
    pub fn create(size: usize) -> io::Result<(Self, OwnedFd)> {
        // SAFETY: plain syscalls on a fresh descriptor.
        unsafe {
            let fd = check(libc::memfd_create(c"droidloom-gpu".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING))?;
            let fd = OwnedFd::from_raw_fd(fd);
            check(libc::ftruncate(fd.as_raw_fd(), size as libc::off_t))?;
            check(libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL))?;
            Ok((Self::map(fd.as_raw_fd(), size)?, fd))
        }
    }

    /// Map a region received from the other end.
    pub fn map(fd: RawFd, size: usize) -> io::Result<Self> {
        // SAFETY: a shared mapping of a memfd of at least `size` bytes.
        let base = unsafe {
            libc::mmap(core::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0)
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { base: base.cast(), size })
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // SAFETY: unmapping the mapping created in `map`.
        unsafe { libc::munmap(self.base.cast(), self.size) };
    }
}

/// Send `bytes` and, optionally, one descriptor.
pub fn send(socket: BorrowedFd<'_>, bytes: &[u8], fd: Option<RawFd>) -> io::Result<()> {
    let mut iov = libc::iovec { iov_base: bytes.as_ptr().cast_mut().cast(), iov_len: bytes.len() };
    let mut control = [0u64; 4];
    // SAFETY: msghdr points at live locals; CMSG macros stay inside `control`.
    unsafe {
        let mut message: libc::msghdr = core::mem::zeroed();
        message.msg_iov = &raw mut iov;
        message.msg_iovlen = 1;
        if let Some(fd) = fd {
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = libc::CMSG_SPACE(size_of::<RawFd>() as u32) as _;
            let header = libc::CMSG_FIRSTHDR(&raw const message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
            libc::CMSG_DATA(header).cast::<RawFd>().write_unaligned(fd);
        }
        loop {
            let sent = libc::sendmsg(socket.as_raw_fd(), &raw const message, libc::MSG_NOSIGNAL);
            if sent >= 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

/// Receive into `bytes`, returning the byte count and any descriptor.
pub fn recv(socket: BorrowedFd<'_>, bytes: &mut [u8]) -> io::Result<(usize, Option<OwnedFd>)> {
    let mut iov = libc::iovec { iov_base: bytes.as_mut_ptr().cast(), iov_len: bytes.len() };
    let mut control = [0u64; 4];
    // SAFETY: as for `send`; a received descriptor is owned from here on.
    unsafe {
        let mut message: libc::msghdr = core::mem::zeroed();
        message.msg_iov = &raw mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = size_of_val(&control) as _;
        let received = loop {
            let received = libc::recvmsg(socket.as_raw_fd(), &raw mut message, libc::MSG_CMSG_CLOEXEC);
            if received >= 0 {
                break received as usize;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        };
        let header = libc::CMSG_FIRSTHDR(&raw const message);
        let fd = (!header.is_null() && (*header).cmsg_type == libc::SCM_RIGHTS)
            .then(|| OwnedFd::from_raw_fd(libc::CMSG_DATA(header).cast::<RawFd>().read_unaligned()));
        Ok((received, fd))
    }
}

const fn drm_iowr(nr: u64, size: usize) -> u64 {
    (3 << 30) | ((size as u64) << 16) | (0x64 << 8) | nr
}

#[repr(C)]
struct SyncobjCreate {
    handle: u32,
    flags: u32,
}

#[repr(C)]
struct SyncobjHandle {
    handle: u32,
    flags: u32,
    fd: i32,
    pad: u32,
}

#[repr(C)]
struct SyncobjTransfer {
    src_handle: u32,
    dst_handle: u32,
    src_point: u64,
    dst_point: u64,
    flags: u32,
    pad: u32,
}

#[repr(C)]
struct SyncobjTimelineWait {
    handles: u64,
    points: u64,
    timeout_nsec: i64,
    count_handles: u32,
    flags: u32,
    first_signaled: u32,
    pad: u32,
}

#[repr(C)]
struct SyncobjTimelineArray {
    handles: u64,
    points: u64,
    count_handles: u32,
    flags: u32,
}

/// [`Drm::wait`]: every point, not the first.
pub const WAIT_ALL: u32 = 1;
/// [`Drm::wait`]: also wait for points without a fence yet.
pub const WAIT_FOR_SUBMIT: u32 = 2;

/// A DRM node used only for syncobjs. Any node works: syncobjs are DRM core.
pub struct Drm(pub OwnedFd);

impl Drm {
    /// Open the node at `path`.
    pub fn open(path: &std::ffi::CStr) -> io::Result<Self> {
        // SAFETY: open(2) with a NUL-terminated path.
        let fd = check(unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) })?;
        // SAFETY: a fresh descriptor.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    fn ioctl<T>(&self, nr: u64, argument: &mut T) -> io::Result<()> {
        loop {
            // SAFETY: `nr` names the DRM ioctl whose payload is exactly `T`.
            let result = unsafe { libc::ioctl(self.0.as_raw_fd(), drm_iowr(nr, size_of::<T>()) as _, core::ptr::from_mut(argument)) };
            if result == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) {
                return Err(error);
            }
        }
    }

    /// A new syncobj.
    pub fn create(&self) -> io::Result<u32> {
        let mut argument = SyncobjCreate { handle: 0, flags: 0 };
        self.ioctl(0xBF, &mut argument)?;
        Ok(argument.handle)
    }

    /// Drop syncobj `handle`.
    pub fn destroy(&self, handle: u32) {
        let mut argument = SyncobjCreate { handle, flags: 0 };
        let _ = self.ioctl(0xC0, &mut argument);
    }

    /// Syncobj `handle` as a descriptor another process imports.
    pub fn export(&self, handle: u32) -> io::Result<OwnedFd> {
        let mut argument = SyncobjHandle { handle, flags: 0, fd: -1, pad: 0 };
        self.ioctl(0xC1, &mut argument)?;
        // SAFETY: the kernel returned a fresh descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(argument.fd) })
    }

    /// A handle for the syncobj descriptor `fd`.
    pub fn import(&self, fd: BorrowedFd<'_>) -> io::Result<u32> {
        let mut argument = SyncobjHandle { handle: 0, flags: 0, fd: fd.as_raw_fd(), pad: 0 };
        self.ioctl(0xC2, &mut argument)?;
        Ok(argument.handle)
    }

    /// Put a sync_file's fence at `point` of `timeline`, through `scratch`.
    pub fn attach(&self, timeline: u32, point: u64, scratch: u32, sync_file: BorrowedFd<'_>) -> io::Result<()> {
        let mut import = SyncobjHandle { handle: scratch, flags: 1, fd: sync_file.as_raw_fd(), pad: 0 };
        self.ioctl(0xC2, &mut import)?;
        let mut transfer = SyncobjTransfer { src_handle: scratch, dst_handle: timeline, src_point: 0, dst_point: point, flags: 0, pad: 0 };
        self.ioctl(0xCC, &mut transfer)
    }

    /// A sync_file for `point` of `timeline`, once that point has a fence.
    pub fn sync_file(&self, timeline: u32, point: u64) -> io::Result<OwnedFd> {
        let binary = self.create()?;
        let mut transfer =
            SyncobjTransfer { src_handle: timeline, dst_handle: binary, src_point: point, dst_point: 0, flags: WAIT_FOR_SUBMIT, pad: 0 };
        let mut export = SyncobjHandle { handle: binary, flags: 1, fd: -1, pad: 0 };
        let result = self.ioctl(0xCC, &mut transfer).and_then(|()| self.ioctl(0xC1, &mut export));
        self.destroy(binary);
        // SAFETY: on success the kernel returned a fresh descriptor.
        result.map(|()| unsafe { OwnedFd::from_raw_fd(export.fd) })
    }

    /// Signal `point` of `timeline` from the CPU.
    pub fn signal(&self, timeline: u32, point: u64) -> io::Result<()> {
        let mut argument = SyncobjTimelineArray {
            handles: (&raw const timeline) as u64,
            points: (&raw const point) as u64,
            count_handles: 1,
            flags: 0,
        };
        self.ioctl(0xCD, &mut argument)
    }

    /// Wait for timeline points. Returns false on timeout.
    pub fn wait(&self, handles: &[u32], points: &[u64], flags: u32, deadline_ns: i64) -> io::Result<bool> {
        let mut argument = SyncobjTimelineWait {
            handles: handles.as_ptr() as u64,
            points: points.as_ptr() as u64,
            timeout_nsec: deadline_ns,
            count_handles: handles.len() as u32,
            flags,
            first_signaled: 0,
            pad: 0,
        };
        match self.ioctl(0xCA, &mut argument) {
            Ok(()) => Ok(true),
            Err(error) if error.raw_os_error() == Some(libc::ETIME) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// The last signalled point of timeline `handle`.
    pub fn query(&self, handle: u32) -> io::Result<u64> {
        let mut point = 0u64;
        let mut argument = SyncobjTimelineArray {
            handles: (&raw const handle) as u64,
            points: (&raw mut point) as u64,
            count_handles: 1,
            flags: 0,
        };
        self.ioctl(0xCB, &mut argument)?;
        Ok(point)
    }
}

/// `CLOCK_MONOTONIC` now, in nanoseconds: the base of syncobj deadlines.
pub fn monotonic_ns() -> i64 {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes the local.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
    now.tv_sec * 1_000_000_000 + now.tv_nsec
}
