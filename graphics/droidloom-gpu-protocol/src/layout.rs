//! The shared region and the records in it.
//!
//! ```text
//! 0          control block: ring positions and wake flags, one cache line each
//! RING       request ring: records, 8-byte aligned, never split at the end
//! CHUNKS     fixed-size chunks: command-buffer streams and reply areas
//! ```
//!
//! A record is a [`Header`], its command's frame and then its data. Ring
//! positions only grow; a record's end position is its sequence number, so
//! "the host has passed position P" means every earlier request is done.

use core::sync::atomic::AtomicU32;

/// Size of the shared region. Pages are only allocated when touched.
pub const SHM_SIZE: usize = 256 << 20;
/// The request ring follows the control block.
pub const RING_OFFSET: usize = 4096;
/// Power of two, so positions wrap with the `u32` counters.
pub const RING_SIZE: usize = 8 << 20;
/// Chunks fill the rest of the region.
pub const CHUNK_OFFSET: usize = RING_OFFSET + RING_SIZE;
/// One chunk: a part of a command-buffer stream, or a thread's reply area.
pub const CHUNK_SIZE: usize = 256 << 10;
/// Chunks in the region.
pub const CHUNK_COUNT: usize = (SHM_SIZE - CHUNK_OFFSET) / CHUNK_SIZE;

/// Space a stream keeps at the end of each chunk for its STOP.
pub const STOP_SIZE: usize = size_of::<Header>();

/// Transport records; command opcodes start at 16.
pub mod op {
    /// Filler to the end of the ring; the next record is at its start.
    pub const PAD: u32 = 0;
    /// End of a stream part. A full chunk is sent at once, so the host
    /// replays it while the application records the next.
    pub const STOP: u32 = 1;
    /// Replay one part of a command-buffer stream: [`super::Record`].
    pub const RECORD: u32 = 2;
}

/// Start of every record.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Header {
    /// A transport record ([`op`]) or a command.
    pub op: u32,
    /// Whole record, header included; a multiple of 8.
    pub size: u32,
}

/// [`op::RECORD`].
#[repr(C)]
pub struct Record {
    /// `op::RECORD`.
    pub header: Header,
    /// The command buffer's id.
    pub command_buffer: u64,
    /// Region offset of the part's first record.
    pub stream: u64,
}

/// A counter alone on its cache line.
#[repr(C, align(64))]
pub struct Line(pub AtomicU32);

/// Start of the region. Each word is a futex where something waits on it.
#[repr(C)]
pub struct Control {
    /// Client: end of the last published record. Host sleeps on it.
    pub head: Line,
    /// Host: end of the last finished record. Clients sleep on it.
    pub tail: Line,
    /// Host is (about to be) asleep on `head`.
    pub host_sleeping: Line,
    /// Number of client threads asleep on `tail`.
    pub client_waiters: Line,
}

/// The opening exchange on the socket.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Hello {
    /// [`MAGIC`].
    pub magic: u64,
    /// [`crate::PROTOCOL`] of the client's build.
    pub protocol: u64,
    /// Nonzero for Android's loader, which owns presentation and wants the
    /// device's swapchain support as `VK_ANDROID_native_buffer`.
    pub android: u64,
}

/// The host's answer, sent with the region's memfd.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Welcome {
    /// [`MAGIC`].
    pub magic: u64,
    /// [`crate::PROTOCOL`] of the host's build.
    pub protocol: u64,
    /// Size of the region.
    pub shm_size: u64,
    /// Render node of the host GPU, NUL-padded, for syncobj timelines.
    pub render_node: [u8; 64],
}

/// Opens both handshake messages.
pub const MAGIC: u64 = u64::from_le_bytes(*b"DLGPU\0\0\x01");

/// Round up to the record alignment.
pub const fn align(size: usize) -> usize {
    (size + 7) & !7
}

/// Distance from `from` to `to` on the wrapping ring counters.
pub const fn passed(position: u32, target: u32) -> bool {
    position.wrapping_sub(target) as i32 >= 0
}
