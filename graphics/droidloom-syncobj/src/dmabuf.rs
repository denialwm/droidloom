//! Implicit synchronization through the DMA-BUF itself, for compositors
//! without `wp_linux_drm_syncobj_manager_v1`.
//!
//! The producer's `sync_file` becomes the buffer's write fence, so the
//! compositor's reads wait for it. Once the compositor releases the buffer,
//! the fences its reads left on the buffer come back as one `sync_file` for the
//! presenter's release point.

use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use crate::{IoctlRequest, SyncobjError, drm_error};

const DMA_BUF_BASE: u64 = b'b' as u64;
const SYNC_BASE: u64 = b'>' as u64;
const DMA_BUF_SYNC_WRITE: u32 = 2;
const IOC_WRITE: u64 = 1;
const IOC_READ_WRITE: u64 = 3;

const fn request(direction: u64, base: u64, number: u64, size: usize) -> IoctlRequest {
    ((direction << 30) | ((size as u64) << 16) | (base << 8) | number) as IoctlRequest
}

#[repr(C)]
struct DmaBufSyncFile {
    flags: u32,
    fd: i32,
}

#[repr(C)]
struct SyncMergeData {
    name: [u8; 32],
    fd2: i32,
    fence: i32,
    flags: u32,
    pad: u32,
}

/// Make `sync_file` the write fence of every plane: compositor reads wait for it.
///
/// # Errors
///
/// Propagates the kernel's refusal, such as a kernel before Linux 6.0.
pub fn attach_write_fence(
    planes: &[OwnedFd],
    sync_file: BorrowedFd<'_>,
) -> Result<(), SyncobjError> {
    const IMPORT: IoctlRequest = request(IOC_WRITE, DMA_BUF_BASE, 3, size_of::<DmaBufSyncFile>());
    for plane in planes {
        let mut argument = DmaBufSyncFile {
            flags: DMA_BUF_SYNC_WRITE,
            fd: sync_file.as_raw_fd(),
        };
        // SAFETY: IMPORT is DMA_BUF_IOCTL_IMPORT_SYNC_FILE for this exact
        // payload; both descriptors stay live for the call.
        if unsafe { libc::ioctl(plane.as_raw_fd(), IMPORT, std::ptr::from_mut(&mut argument)) } != 0
        {
            return Err(drm_error(
                "attach write fence to DMA-BUF",
                io::Error::last_os_error(),
            ));
        }
    }
    Ok(())
}

/// Every fence a new writer of the planes must wait for: the reads the
/// compositor queued and the last write, merged into one `sync_file`.
///
/// # Errors
///
/// Propagates export and merge failures.
pub fn export_fences(planes: &[OwnedFd]) -> Result<OwnedFd, SyncobjError> {
    const EXPORT: IoctlRequest =
        request(IOC_READ_WRITE, DMA_BUF_BASE, 2, size_of::<DmaBufSyncFile>());
    let mut merged: Option<OwnedFd> = None;
    for plane in planes {
        let mut argument = DmaBufSyncFile {
            flags: DMA_BUF_SYNC_WRITE,
            fd: -1,
        };
        // SAFETY: EXPORT is DMA_BUF_IOCTL_EXPORT_SYNC_FILE for this exact
        // payload; on success the kernel stores a new descriptor in `fd`.
        if unsafe { libc::ioctl(plane.as_raw_fd(), EXPORT, std::ptr::from_mut(&mut argument)) } != 0
        {
            return Err(drm_error(
                "export DMA-BUF fences",
                io::Error::last_os_error(),
            ));
        }
        // SAFETY: the successful export transferred ownership of `fd`.
        let fence = unsafe { OwnedFd::from_raw_fd(argument.fd) };
        merged = Some(match merged {
            None => fence,
            Some(previous) => merge(&previous, &fence)?,
        });
    }
    merged.ok_or_else(|| {
        drm_error(
            "export DMA-BUF fences",
            io::Error::from(io::ErrorKind::InvalidInput),
        )
    })
}

fn merge(first: &OwnedFd, second: &OwnedFd) -> Result<OwnedFd, SyncobjError> {
    const MERGE: IoctlRequest = request(IOC_READ_WRITE, SYNC_BASE, 3, size_of::<SyncMergeData>());
    let mut argument = SyncMergeData {
        name: [0; 32],
        fd2: second.as_raw_fd(),
        fence: -1,
        flags: 0,
        pad: 0,
    };
    argument.name[..9].copy_from_slice(b"droidloom");
    // SAFETY: MERGE is SYNC_IOC_MERGE for this exact payload; on success the
    // kernel stores a new descriptor in `fence`.
    if unsafe { libc::ioctl(first.as_raw_fd(), MERGE, std::ptr::from_mut(&mut argument)) } != 0 {
        return Err(drm_error("merge sync_files", io::Error::last_os_error()));
    }
    // SAFETY: the successful merge transferred ownership of `fence`.
    Ok(unsafe { OwnedFd::from_raw_fd(argument.fence) })
}
