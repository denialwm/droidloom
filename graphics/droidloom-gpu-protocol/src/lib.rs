//! Droidloom GPU forwarding: what the Android-side Vulkan driver and the
//! host server share.
//!
//! The two ends share one memory region and one Unix socket. Requests are
//! records in a ring inside the region; command buffers are record streams
//! in chunks of the same region; replies go to a per-thread area. The
//! socket only carries file descriptors and the opening handshake. Both
//! ends run the same kernel, so DMA-BUFs, sync_files and syncobjs move as
//! descriptors and are never copied.

pub mod generated;
pub mod layout;
pub mod sys;

/// What both ends must agree on: the generated commands and the transport
/// layout. Any change to either refuses builds from another tree.
pub const PROTOCOL: u64 = generated::PROTOCOL ^ fnv(include_bytes!("layout.rs"));

const fn fnv(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut i = 0;
    while i < bytes.len() {
        hash = (hash ^ bytes[i] as u64).wrapping_mul(0x0100_0000_01b3);
        i += 1;
    }
    hash
}
