//! Bounded, task-scoped layer transactions. All geometry is in physical task
//! pixels; source rectangles use unsigned 24.8 fixed point. The broker alone
//! delegates these sockets, so packets cannot choose a different task.

use crate::{Plane, WireError, read_u32, read_u64};

/// Maximum visible layers in a transaction; larger scenes use client composition.
pub const MAX_LAYERS: usize = 64;
/// Maximum cached source allocations in a task.
pub const MAX_BUFFERS: usize = 128;
/// Maximum imports awaiting compositor confirmation per task.
pub const MAX_IMPORTS: usize = 16;
/// Stream supports synchronized subsurfaces, alpha, and release sync-files.
/// Requiring both bits makes older peers select normal composition fallback.
pub const SUPPORTED: u32 =
    3 | ASYNC_STAGE | CONFIG_EVENTS | ASYNC_COMMIT | ASYNC_IMPORT | SCENE_BATCH;
/// Bounded chunks; the final chunk and commit share one packet.
pub const SCENE_BATCH: u32 = 1 << 6;
/// Maximum layers in one scene chunk; the transport still permits only four FDs.
pub const MAX_BATCH_LAYERS: usize = 16;
/// Header, commit fields and sixteen indexed layer records.
pub const MAX_LAYER_PACKET: usize = 16 + 28 + MAX_BATCH_LAYERS * 76;
/// `StageAsync` records have no reply; Commit reports the complete scene result.
pub const ASYNC_STAGE: u32 = 1 << 2;
/// Subscribe to revisioned configuration updates on the completion socket.
pub const CONFIG_EVENTS: u32 = 1 << 3;
/// Scene-ID commits report acceptance on the event socket, independently of
/// source release fences. Senders allow at most one unacknowledged scene.
pub const ASYNC_COMMIT: u32 = 1 << 4;
/// Allocation import results arrive on the event socket, identified by attempt
/// and allocation. Scenes may reference only successfully imported sources.
pub const ASYNC_IMPORT: u32 = 1 << 5;

/// Last published task configuration. Revision also orders changes to visibility
/// and capabilities that do not increment Android's configure serial.
#[derive(Default)]
pub struct ConfigPublisher {
    revision: u64,
    current: [u32; 4],
    subscribed: bool,
}

impl ConfigPublisher {
    fn refresh(&mut self, current: [u32; 4]) -> Result<bool, WireError> {
        if self.revision != 0 && self.current == current {
            return Ok(false);
        }
        self.revision = self.revision.checked_add(1).ok_or(WireError::Field)?;
        self.current = current;
        Ok(true)
    }

    #[allow(clippy::cast_possible_truncation)] // Wire revision is split into low/high words.
    fn snapshot(&self) -> [u32; 6] {
        [
            self.revision as u32,
            (self.revision >> 32) as u32,
            self.current[0],
            self.current[1],
            self.current[2],
            self.current[3],
        ]
    }

    /// Subscribe and return the initial reply, even if nothing changed.
    ///
    /// # Errors
    /// Returns `WireError::Field` if the revision would overflow.
    pub fn subscribe(&mut self, current: [u32; 4]) -> Result<[u32; 6], WireError> {
        self.refresh(current)?;
        self.subscribed = true;
        Ok(self.snapshot())
    }

    /// Emit only changed snapshots and only after a peer explicitly subscribed.
    ///
    /// # Errors
    /// Returns `WireError::Field` if the revision would overflow.
    pub fn update(&mut self, current: [u32; 4]) -> Result<Option<[u32; 6]>, WireError> {
        let changed = self.refresh(current)?;
        Ok((changed && self.subscribed).then(|| self.snapshot()))
    }
}

/// Immutable producer allocation; descriptors carry only image planes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Buffer {
    /// Stable allocation or layer identity.
    pub id: u64,
    /// Allocation width in pixels.
    pub width: u32,
    /// Allocation height in pixels.
    pub height: u32,
    /// Explicit DRM pixel format.
    pub fourcc: u32,
    /// Explicit DRM layout modifier.
    pub modifier: u64,
    /// Image planes in descriptor order.
    pub planes: Vec<Plane>,
}

/// One layer, in bottom-to-top transaction order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layer {
    /// Stable allocation or layer identity.
    pub id: u64,
    /// Zero selects a premultiplied ARGB solid pixel.
    pub buffer: u64,
    /// Unique source-buffer read identity, reused only while still attached.
    pub ticket: u64,
    /// Left, top, width and height in physical task pixels.
    pub destination: [i32; 4],
    /// Source rectangle in transformed buffer coordinates, in 24.8 units.
    pub source: [u32; 4],
    /// Wayland buffer transform enum, not Android transform flags.
    pub transform: u32,
    /// Wayland alpha multiplier, with maximum u32 representing one.
    pub alpha: u32,
    /// Proven full opaque coverage of this layer.
    pub opaque: bool,
    /// Premultiplied ARGB value for a solid layer.
    pub color: u32,
}

impl Layer {
    /// Validate geometry without inspecting pixels or application identity.
    pub fn validate(&self, width: u32, height: u32, buffer: Option<&Buffer>) -> bool {
        let [x, y, w, h] = self.destination;
        if self.id == 0
            || x < 0
            || y < 0
            || w <= 0
            || h <= 0
            || self.transform > 7
            || (x as u32).checked_add(w as u32).is_none_or(|v| v > width)
            || (y as u32).checked_add(h as u32).is_none_or(|v| v > height)
        {
            return false;
        }
        if self.buffer == 0 {
            return self.ticket == 0 && buffer.is_none();
        }
        let Some(buffer) = buffer else {
            return false;
        };
        let (bw, bh) = if matches!(self.transform, 1 | 3 | 5 | 7) {
            (buffer.height, buffer.width)
        } else {
            (buffer.width, buffer.height)
        };
        let [sx, sy, sw, sh] = self.source;
        self.ticket != 0
            && buffer.id == self.buffer
            && sw > 0
            && sh > 0
            && sx
                .checked_add(sw)
                .is_some_and(|v| u64::from(v) <= u64::from(bw) * 256)
            && sy
                .checked_add(sh)
                .is_some_and(|v| u64::from(v) <= u64::from(bh) * 256)
    }
}

/// One request. Stage never changes visible state; Commit validates the whole scene.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request {
    /// Query capabilities and current task configure.
    Config,
    /// Subscribe to configuration events; initial revisioned snapshot is replied
    /// on the request socket and later changes use the event socket.
    SubscribeConfig,
    /// Register image planes once.
    Register(Buffer),
    /// Pipeline registration without a request-socket reply. Requires
    /// `ASYNC_IMPORT`; at most `MAX_IMPORTS` attempts may await a result.
    RegisterAsync {
        /// Strictly increasing nonzero import attempt, scoped to this stream.
        attempt: u64,
        /// Allocation metadata and image planes.
        buffer: Buffer,
    },
    /// Retire an idle cached allocation.
    Unregister(u64),
    /// Append one layer to an invisible pending transaction.
    Stage(Layer),
    /// Append a layer without a reply; requires the `ASYNC_STAGE` capability.
    StageAsync(Layer),
    /// Append a bounded invisible chunk, without a reply.
    StageBatch(Vec<BatchLayer>),
    /// Append the last chunk and atomically commit the complete scene.
    CommitBatch {
        /// Strictly increasing, nonzero stream identity.
        scene: u64,
        /// Current configure serial.
        serial: u32,
        /// Physical task width.
        width: u32,
        /// Physical task height.
        height: u32,
        /// Total count including preceding chunks.
        count: u32,
        /// Last invisible chunk.
        layers: Vec<BatchLayer>,
    },
    /// Publish all staged layers under one current configure.
    Commit {
        /// Configure serial obtained from Config.
        serial: u32,
        /// Physical task width.
        width: u32,
        /// Physical task height.
        height: u32,
        /// Exact staged layer count.
        count: u32,
    },
    /// Same validation as Commit; completion uses event 0x8006, never a
    /// request-socket reply. Requires the `ASYNC_COMMIT` capability.
    CommitAsync {
        /// Strictly increasing, nonzero identity scoped to this stream.
        scene: u64,
        /// Current configure serial.
        serial: u32,
        /// Physical task width.
        width: u32,
        /// Physical task height.
        height: u32,
        /// Exact staged layer count.
        count: u32,
    },
    /// Discard staged metadata without changing visible content.
    Abort,
}

/// Layer metadata with an optional descriptor index scoped to this packet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchLayer {
    /// The unchanged layer wire semantics.
    pub layer: Layer,
    /// Each attached descriptor belongs to exactly one layer.
    pub fence_index: Option<usize>,
}

fn decode_layer(p: &[u8], fenced: bool) -> Result<Layer, WireError> {
    let flags = read_u32(p, 64)?;
    let layer = Layer {
        id: read_u64(p, 0)?,
        buffer: read_u64(p, 8)?,
        ticket: read_u64(p, 16)?,
        destination: [
            read_u32(p, 24)? as i32,
            read_u32(p, 28)? as i32,
            read_u32(p, 32)? as i32,
            read_u32(p, 36)? as i32,
        ],
        source: [
            read_u32(p, 40)?,
            read_u32(p, 44)?,
            read_u32(p, 48)?,
            read_u32(p, 52)?,
        ],
        transform: read_u32(p, 56)?,
        alpha: read_u32(p, 60)?,
        opaque: flags == 1,
        color: read_u32(p, 68)?,
    };
    if flags > 1 || (layer.buffer == 0 && fenced) {
        return Err(WireError::Field);
    }
    Ok(layer)
}

fn decode_batch(p: &[u8], descriptors: usize) -> Result<Vec<BatchLayer>, WireError> {
    let count = read_u32(p, 0)? as usize;
    if count == 0 || count > MAX_BATCH_LAYERS || p.len() != 4 + count * 76 {
        return Err(WireError::Length);
    }
    if descriptors > 4 {
        return Err(WireError::Descriptors);
    }
    let mut used = [false; 4];
    let mut layers = Vec::with_capacity(count);
    for p in p[4..].chunks_exact(76) {
        let index = read_u32(p, 72)?;
        let fence_index = if index == u32::MAX {
            None
        } else {
            let index = index as usize;
            if index >= descriptors || used[index] {
                return Err(WireError::Descriptors);
            }
            used[index] = true;
            Some(index)
        };
        layers.push(BatchLayer {
            layer: decode_layer(p, fence_index.is_some())?,
            fence_index,
        });
    }
    if used[..descriptors].iter().any(|v| !v) {
        return Err(WireError::Descriptors);
    }
    Ok(layers)
}

/// Bounded scene staging. Overflow invalidates the whole transaction, including
/// when the sender does not wait for individual stage acknowledgements.
pub struct Staging<T> {
    layers: Vec<(Layer, Option<T>)>,
    failed: bool,
}

impl<T> Default for Staging<T> {
    fn default() -> Self {
        Self {
            layers: Vec::new(),
            failed: false,
        }
    }
}

impl<T> Staging<T> {
    /// Append one layer, retaining descriptor ownership until commit or abort.
    pub fn push(&mut self, layer: Layer, acquire: Option<T>) -> bool {
        if self.failed || self.layers.len() == MAX_LAYERS {
            self.failed = true;
            return false;
        }
        self.layers.push((layer, acquire));
        true
    }

    /// Whether any stage in the current scene exceeded its resource bound.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Borrow the staged layers for complete-scene validation.
    pub fn layers(&self) -> &[(Layer, Option<T>)] {
        &self.layers
    }

    /// Abort or reject a scene, closing owned resources and retaining capacity.
    pub fn clear(&mut self) {
        self.layers.clear();
        self.failed = false;
    }

    /// Take a validated scene; callers restore its empty allocation after use.
    pub fn take(&mut self) -> Vec<(Layer, Option<T>)> {
        self.failed = false;
        std::mem::take(&mut self.layers)
    }

    /// Return a drained scene allocation for reuse on the next frame.
    pub fn recycle(&mut self, mut layers: Vec<(Layer, Option<T>)>) {
        layers.clear();
        self.layers = layers;
        self.failed = false;
    }
}

/// Immutable information about a cached source and its current read.
pub struct SourceState<'a> {
    /// Validated allocation metadata.
    pub buffer: &'a Buffer,
    /// Whether the Wayland compositor confirmed the import.
    pub imported: bool,
    /// Outstanding source read, if any.
    pub ticket: Option<u64>,
    /// Whether another read can start while `ticket` is outstanding: the
    /// allocation has a timeline with no read in flight, or can add one.
    pub spare_timeline: bool,
}

/// Validate an entire scene before issuing any Wayland surface mutations.
pub fn validate_scene<'a>(
    layers: impl ExactSizeIterator<Item = (&'a Layer, bool)>,
    count: u32,
    width: u32,
    height: u32,
    last_ticket: u64,
    source: impl FnMut(u64) -> Option<SourceState<'a>>,
    attached: impl FnMut(u64) -> Option<(u64, u64)>,
) -> bool {
    assess_scene(layers, count, width, height, last_ticket, source, attached) == SceneStatus::Ready
}

/// Whether a complete scene can be submitted, must wait for a preceding GPU
/// read, or is invalid. A waiting scene has passed every other validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SceneStatus {
    /// All source allocations can be attached now.
    Ready,
    /// At least one new read needs its allocation's preceding read to finish:
    /// that read is not shown by the layer now asking for the allocation, and
    /// the allocation has no spare timeline for another read.
    Busy,
    /// The scene violates the transport or current attachment contract.
    Invalid,
}

/// Check a complete scene without modifying allocations or surface state.
pub fn assess_scene<'a>(
    layers: impl ExactSizeIterator<Item = (&'a Layer, bool)>,
    count: u32,
    width: u32,
    height: u32,
    last_ticket: u64,
    mut source: impl FnMut(u64) -> Option<SourceState<'a>>,
    mut attached: impl FnMut(u64) -> Option<(u64, u64)>,
) -> SceneStatus {
    use std::collections::BTreeSet;
    if count == 0 || count as usize != layers.len() || layers.len() > MAX_LAYERS {
        return SceneStatus::Invalid;
    }
    let mut ids = BTreeSet::new();
    let mut buffers = BTreeSet::new();
    let mut tickets = BTreeSet::new();
    let mut busy = false;
    for (l, has_acquire) in layers {
        let allocation = source(l.buffer);
        if !ids.insert(l.id) || !l.validate(width, height, allocation.as_ref().map(|b| b.buffer)) {
            return SceneStatus::Invalid;
        }
        if l.buffer == 0 {
            if has_acquire {
                return SceneStatus::Invalid;
            }
            continue;
        }
        let Some(b) = allocation else {
            return SceneStatus::Invalid;
        };
        if !b.imported || !buffers.insert(l.buffer) {
            return SceneStatus::Invalid;
        }
        if b.ticket == Some(l.ticket) {
            if has_acquire || attached(l.id) != Some((l.buffer, l.ticket)) {
                return SceneStatus::Invalid;
            }
        } else {
            if l.ticket <= last_ticket || !tickets.insert(l.ticket) {
                return SceneStatus::Invalid;
            }
            // The producer cannot have rewritten an allocation whose read is
            // outstanding, so a new ticket names unchanged content. A new
            // ticket for the read this same layer still shows relabels it:
            // the attachment stays and the new ticket ends with that read.
            // Any other new read starts on a spare timeline. Waiting would
            // deadlock in both cases: a compositor may release a detached
            // read only with its next frame, which this scene provides.
            busy |= !b.spare_timeline
                && b.ticket
                    .is_some_and(|held| attached(l.id) != Some((l.buffer, held)));
        }
    }
    if busy {
        SceneStatus::Busy
    } else {
        SceneStatus::Ready
    }
}

/// Decode a complete record with exact ancillary descriptor accounting.
pub fn decode(bytes: &[u8], descriptors: usize) -> Result<Request, WireError> {
    if bytes.len() < 16
        || bytes.len() > MAX_LAYER_PACKET
        || &bytes[..4] != b"DLL1"
        || read_u32(bytes, 8)? as usize != bytes.len() - 16
        || read_u32(bytes, 12)? as usize != descriptors
    {
        return Err(WireError::Header);
    }
    let p = &bytes[16..];
    match read_u32(bytes, 4)? {
        1 if p.is_empty() && descriptors == 0 => Ok(Request::Config),
        8 if p.is_empty() && descriptors == 0 => Ok(Request::SubscribeConfig),
        opcode @ (2 | 10) => {
            let (attempt, p) = if opcode == 10 {
                let attempt = read_u64(p, 0)?;
                if attempt == 0 {
                    return Err(WireError::Field);
                }
                (Some(attempt), &p[8..])
            } else {
                (None, p)
            };
            if p.len() < 40 {
                return Err(WireError::Length);
            }
            let n = read_u32(p, 36)? as usize;
            if n == 0 || n > 4 || n != descriptors || p.len() != 40 + n * 8 {
                return Err(WireError::Descriptors);
            }
            let b = Buffer {
                id: read_u64(p, 0)?,
                width: read_u32(p, 8)?,
                height: read_u32(p, 12)?,
                fourcc: read_u32(p, 16)?,
                modifier: read_u64(p, 24)?,
                planes: (0..n)
                    .map(|i| {
                        Ok(Plane {
                            offset: read_u32(p, 40 + i * 8)?,
                            stride: read_u32(p, 44 + i * 8)?,
                        })
                    })
                    .collect::<Result<_, WireError>>()?,
            };
            if b.id == 0
                || b.width == 0
                || b.height == 0
                || b.width > 16384
                || b.height > 16384
                || b.fourcc == 0
                || b.modifier == u64::MAX
                || read_u32(p, 20)? != 0
                || read_u32(p, 32)? != 0
                || b.planes.iter().any(|p| p.stride == 0)
            {
                return Err(WireError::Field);
            }
            Ok(if let Some(attempt) = attempt {
                Request::RegisterAsync { attempt, buffer: b }
            } else {
                Request::Register(b)
            })
        }
        3 if p.len() == 8 && descriptors == 0 => Ok(Request::Unregister(read_u64(p, 0)?)),
        opcode @ (4 | 7) if p.len() == 72 && descriptors <= 1 => {
            let l = decode_layer(p, descriptors != 0)?;
            Ok(if opcode == 7 {
                Request::StageAsync(l)
            } else {
                Request::Stage(l)
            })
        }
        11 => Ok(Request::StageBatch(decode_batch(p, descriptors)?)),
        12 if p.len() >= 28 && read_u64(p, 0)? != 0 => Ok(Request::CommitBatch {
            scene: read_u64(p, 0)?,
            serial: read_u32(p, 8)?,
            width: read_u32(p, 12)?,
            height: read_u32(p, 16)?,
            count: read_u32(p, 20)?,
            layers: decode_batch(&p[24..], descriptors)?,
        }),
        5 if p.len() == 16 && descriptors == 0 => Ok(Request::Commit {
            serial: read_u32(p, 0)?,
            width: read_u32(p, 4)?,
            height: read_u32(p, 8)?,
            count: read_u32(p, 12)?,
        }),
        9 if p.len() == 24 && descriptors == 0 && read_u64(p, 0)? != 0 => {
            Ok(Request::CommitAsync {
                scene: read_u64(p, 0)?,
                serial: read_u32(p, 8)?,
                width: read_u32(p, 12)?,
                height: read_u32(p, 16)?,
                count: read_u32(p, 20)?,
            })
        }
        6 if p.is_empty() && descriptors == 0 => Ok(Request::Abort),
        _ => Err(WireError::Length),
    }
}

/// Encode a small descriptor-free acknowledgement or release notification.
pub fn response(opcode: u32, words: &[u32]) -> Vec<u8> {
    let mut bytes = b"DLL1".to_vec();
    for value in [opcode, (words.len() * 4) as u32, 0] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    for value in words {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// A source's real compositor release fence (one SCM_RIGHTS sync-file).
/// This announces fence availability; 0x8003 still announces safe reuse.
pub fn release_fence(ticket: u64) -> Vec<u8> {
    let mut bytes = response(0x8004, &[ticket as u32, (ticket >> 32) as u32]);
    bytes[12..16].copy_from_slice(&1_u32.to_le_bytes());
    bytes
}

/// Scene rejection: the host cannot place this arrangement (EOPNOTSUPP). The
/// sender composes it until its layers, size or configure serial change.
pub const SCENE_UNPLACEABLE: i32 = -95;
/// Scene rejection for this scene only (EAGAIN); the next one may be accepted.
pub const SCENE_RETRY: i32 = -11;

/// Scene acceptance is metadata, and must never imply producer buffer release.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Split ID and signed wire status.
pub fn commit_result(scene: u64, code: i32) -> Vec<u8> {
    response(0x8006, &[scene as u32, (scene >> 32) as u32, code as u32])
}

/// Import confirmation reports allocation usability, never GPU completion.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Split IDs and signed wire status.
pub fn import_result(attempt: u64, buffer: u64, code: i32) -> Vec<u8> {
    response(
        0x8007,
        &[
            attempt as u32,
            (attempt >> 32) as u32,
            buffer as u32,
            (buffer >> 32) as u32,
            code as u32,
        ],
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn batched_scene_validates_fence_ownership_and_bounded_chunks() {
        use super::*;
        fn packet(op: u32, indices: &[u32], fds: u32) -> Vec<u8> {
            let mut words = if op == 12 {
                vec![7, 0, 9, 800, 600, 64]
            } else {
                Vec::new()
            };
            words.push(indices.len() as u32);
            for (i, index) in indices.iter().enumerate() {
                words.extend_from_slice(&[
                    i as u32 + 1,
                    0,
                    5,
                    0,
                    i as u32 + 1,
                    0,
                    0,
                    0,
                    800,
                    600,
                    0,
                    0,
                    800 * 256,
                    600 * 256,
                    0,
                    u32::MAX,
                    1,
                    0,
                    *index,
                ]);
            }
            let mut bytes = response(op, &words);
            bytes[12..16].copy_from_slice(&fds.to_le_bytes());
            bytes
        }
        let bytes = packet(12, &[u32::MAX, 1, 0, u32::MAX], 2);
        let Request::CommitBatch {
            scene,
            count,
            layers,
            ..
        } = decode(&bytes, 2).unwrap()
        else {
            panic!()
        };
        assert_eq!((scene, count), (7, 64));
        assert_eq!(
            layers.iter().map(|l| l.fence_index).collect::<Vec<_>>(),
            [None, Some(1), Some(0), None]
        );
        // Duplicate, out-of-range, unreferenced and excess descriptors all fail.
        for (indices, fds) in [
            (vec![0, 0], 1),
            (vec![1], 1),
            (vec![u32::MAX], 1),
            (vec![0, 1, 2, 3, 4], 5),
        ] {
            assert!(decode(&packet(11, &indices, fds), fds as usize).is_err());
        }
        assert!(decode(&bytes[..bytes.len() - 1], 2).is_err());
        assert!(decode(&packet(11, &[], 0), 0).is_err());
        assert!(decode(&packet(11, &[u32::MAX; 17], 0), 0).is_err());
        let mut solid_fence = packet(11, &[0], 1);
        solid_fence[28..36].fill(0); // Buffer ID at header + count + id.
        assert!(decode(&solid_fence, 1).is_err());
        let mut staging = Staging::<()>::default();
        for _ in 0..4 {
            let Request::StageBatch(layers) = decode(&packet(11, &[u32::MAX; 16], 0), 0).unwrap()
            else {
                panic!()
            };
            for layer in layers {
                assert!(staging.push(layer.layer, None));
            }
        }
        assert_eq!(staging.layers().len(), MAX_LAYERS);
        assert!(!staging.failed());
        assert!(!staging.push(layer(), None));
        assert!(staging.failed()); // A valid prefix cannot be committed.
    }

    #[test]
    fn async_import_preserves_allocation_metadata_and_descriptor_validation() {
        use super::*;
        let b = buffer();
        let words = [
            b.id as u32,
            0,
            b.width,
            b.height,
            b.fourcc,
            0,
            b.modifier as u32,
            (b.modifier >> 32) as u32,
            0,
            1,
            b.planes[0].offset,
            b.planes[0].stride,
        ];
        for (opcode, attempt) in [(2, None), (10, Some(0x1234_5678_0000_0001))] {
            let mut payload = Vec::new();
            if attempt.is_some() {
                payload.extend([1, 0x1234_5678]);
            }
            payload.extend(words);
            let mut packet = response(opcode, &payload);
            packet[12..16].copy_from_slice(&1_u32.to_le_bytes());
            let expected = attempt.map_or(Request::Register(b.clone()), |attempt| {
                Request::RegisterAsync {
                    attempt,
                    buffer: b.clone(),
                }
            });
            assert_eq!(decode(&packet, 1), Ok(expected));
            assert!(decode(&packet, 0).is_err());
            assert!(decode(&packet, 2).is_err());
            if opcode == 10 {
                packet[16..24].fill(0);
                assert!(decode(&packet, 1).is_err());
            }
        }
        for length in 0..48 {
            let mut packet = response(10, &[]);
            packet.resize(16 + length, 0);
            packet[8..12].copy_from_slice(&(length as u32).to_le_bytes());
            assert!(decode(&packet, 0).is_err());
        }
        assert_eq!(
            import_result(0x1234_5678_0000_0001, 0x9876_5432_0000_0002, -95),
            response(0x8007, &[1, 0x1234_5678, 2, 0x9876_5432, (-95_i32) as u32])
        );
        assert_eq!(SUPPORTED & ASYNC_IMPORT, ASYNC_IMPORT);
    }

    #[test]
    fn async_commit_keeps_scene_identity_separate_from_release_tickets() {
        use super::{Request, commit_result, decode, response};
        let scene = 0x1234_5678_0000_0001;
        assert_eq!(
            decode(&response(9, &[1, 0x1234_5678, 17, 800, 600, 2]), 0),
            Ok(Request::CommitAsync {
                scene,
                serial: 17,
                width: 800,
                height: 600,
                count: 2
            })
        );
        assert!(decode(&response(9, &[0, 0, 17, 800, 600, 2]), 0).is_err());
        assert!(decode(&response(9, &[1, 0, 17, 800, 600]), 0).is_err());
        assert!(decode(&response(9, &[1, 0, 17, 800, 600, 2]), 1).is_err());
        assert_eq!(
            commit_result(scene, -95),
            response(0x8006, &[1, 0x1234_5678, (-95_i32) as u32])
        );
        assert_ne!(
            &commit_result(scene, 0)[4..8],
            &super::release_fence(scene)[4..8]
        );
    }
    #[test]
    fn configuration_subscription_orders_changes_and_leaves_legacy_peers_silent() {
        let mut publisher = super::ConfigPublisher::default();
        let initial = [super::SUPPORTED, 9, 800, 600];
        assert_eq!(publisher.update(initial).unwrap(), None);
        assert_eq!(
            publisher.subscribe(initial).unwrap(),
            [1, 0, super::SUPPORTED, 9, 800, 600]
        );
        assert_eq!(publisher.update(initial).unwrap(), None);
        let resized = [super::SUPPORTED, 10, 1024, 768];
        assert_eq!(
            publisher.update(resized).unwrap(),
            Some([2, 0, super::SUPPORTED, 10, 1024, 768])
        );
        assert_eq!(
            publisher.update([0, 10, 1024, 768]).unwrap(),
            Some([3, 0, 0, 10, 1024, 768])
        );
        assert_eq!(
            publisher.update(resized).unwrap(),
            Some([4, 0, super::SUPPORTED, 10, 1024, 768])
        );
        assert_eq!(
            publisher.subscribe(resized).unwrap(),
            [4, 0, super::SUPPORTED, 10, 1024, 768]
        );
        publisher.revision = u64::MAX;
        assert!(publisher.update(initial).is_err());
        assert_eq!(publisher.current, resized);
        assert_eq!(publisher.revision, u64::MAX);
    }

    #[test]
    fn configuration_subscription_rejects_payload_and_descriptors() {
        use super::{Request, decode, response};
        assert_eq!(decode(&response(8, &[]), 0), Ok(Request::SubscribeConfig));
        assert!(decode(&response(8, &[1]), 0).is_err());
        assert!(decode(&response(8, &[]), 1).is_err());
    }
    use super::*;

    #[test]
    fn pipelined_stage_uses_the_same_layer_and_descriptor_contract() {
        let expected = layer();
        let words = [
            2,
            0,
            8,
            0,
            9,
            0,
            0,
            0,
            800,
            600,
            0,
            0,
            800 * 256,
            600 * 256,
            0,
            u32::MAX,
            1,
            0,
        ];
        for opcode in [4, 7] {
            let mut packet = response(opcode, &words);
            assert_eq!(packet.len(), 88);
            assert_eq!(
                decode(&packet, 0).unwrap(),
                if opcode == 7 {
                    Request::StageAsync(expected.clone())
                } else {
                    Request::Stage(expected.clone())
                }
            );
            packet[12..16].copy_from_slice(&1_u32.to_le_bytes());
            assert!(decode(&packet, 1).is_ok());
            assert!(decode(&packet, 0).is_err());
            packet[12..16].copy_from_slice(&2_u32.to_le_bytes());
            assert!(decode(&packet, 2).is_err());
            packet[12..16].copy_from_slice(&0_u32.to_le_bytes());
            packet[80..84].copy_from_slice(&2_u32.to_le_bytes());
            assert!(
                decode(&packet, 0).is_err(),
                "unknown layer flags must fail closed"
            );
        }
    }

    #[test]
    fn staging_overflow_is_sticky_and_releases_all_rejected_resources() {
        use std::{cell::Cell, rc::Rc};
        struct Resource(Rc<Cell<usize>>);
        impl Drop for Resource {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let dropped = Rc::new(Cell::new(0));
        let mut staging = Staging::default();
        for _ in 0..MAX_LAYERS {
            assert!(staging.push(layer(), Some(Resource(dropped.clone()))));
        }
        assert!(!staging.failed());
        assert!(!staging.push(layer(), Some(Resource(dropped.clone()))));
        assert_eq!(dropped.get(), 1);
        assert_eq!(staging.layers().len(), MAX_LAYERS);
        assert!(
            staging.failed(),
            "a commit claiming only the accepted prefix must fail"
        );
        assert!(!staging.push(layer(), Some(Resource(dropped.clone()))));
        assert_eq!(dropped.get(), 2);
        staging.clear();
        assert_eq!(dropped.get(), MAX_LAYERS + 2);
        assert!(!staging.failed());
        assert!(staging.push(layer(), None));
        assert_eq!(staging.layers().len(), 1);
    }

    #[test]
    fn completed_scene_recycles_storage_for_the_next_transaction() {
        let mut staging = Staging::<()>::default();
        for _ in 0..MAX_LAYERS {
            assert!(staging.push(layer(), None));
        }
        let mut layers = staging.take();
        let allocation = layers.as_ptr();
        layers.clear();
        staging.recycle(layers);
        assert!(staging.layers().is_empty());
        for _ in 0..MAX_LAYERS {
            assert!(staging.push(layer(), None));
        }
        assert_eq!(staging.layers().as_ptr(), allocation);
    }
    fn buffer() -> Buffer {
        Buffer {
            id: 8,
            width: 800,
            height: 600,
            fourcc: 1,
            modifier: 0,
            planes: vec![Plane {
                offset: 0,
                stride: 3200,
            }],
        }
    }
    fn layer() -> Layer {
        Layer {
            id: 2,
            buffer: 8,
            ticket: 9,
            destination: [0, 0, 800, 600],
            source: [0, 0, 800 * 256, 600 * 256],
            transform: 0,
            alpha: u32::MAX,
            opaque: true,
            color: 0,
        }
    }
    #[test]
    fn pending_and_failed_imports_cannot_be_used_in_a_scene() {
        let b = buffer();
        let l = layer();
        for imported in [false, true] {
            assert_eq!(
                validate_scene(
                    [(&l, false)].into_iter(),
                    1,
                    800,
                    600,
                    0,
                    |id| (id == b.id).then_some(SourceState {
                        buffer: &b,
                        imported,
                        ticket: None,
                        spare_timeline: true,
                    }),
                    |_| None,
                ),
                imported,
            );
        }
        assert!(!validate_scene(
            [(&l, false)].into_iter(),
            1,
            800,
            600,
            0,
            |_| None,
            |_| None
        ));
    }
    #[test]
    fn geometry_checks_rotated_sources_and_bounds() {
        let b = buffer();
        let mut l = layer();
        assert!(l.validate(800, 600, Some(&b)));
        l.destination[0] = 1;
        assert!(!l.validate(800, 600, Some(&b)));
        l.destination[0] = 0;
        l.transform = 1;
        assert!(!l.validate(800, 600, Some(&b)));
        l.source = [0, 0, 600 * 256, 800 * 256];
        assert!(l.validate(800, 600, Some(&b)));
        l.source[0] = u32::MAX;
        assert!(!l.validate(800, 600, Some(&b)));
        l.source[0] = 0;
        l.ticket = 0;
        assert!(!l.validate(800, 600, Some(&b)));
    }
    #[test]
    fn multilayer_transactions_preserve_reuse_and_reject_ambiguous_ownership() {
        let b = buffer();
        let mut overlay = layer();
        overlay.id = 3;
        overlay.buffer = 0;
        overlay.ticket = 0;
        overlay.alpha = u32::MAX / 2;
        let mut base = layer();
        let validate = |scene: &[Layer], busy: bool, attached_layer: u64, has_fence: bool| {
            validate_scene(
                scene.iter().map(|l| (l, l.buffer != 0 && has_fence)),
                scene.len() as u32,
                800,
                600,
                8,
                |id| {
                    (id == 8).then_some(SourceState {
                        buffer: &b,
                        imported: true,
                        ticket: busy.then_some(9),
                        spare_timeline: false,
                    })
                },
                |id| (id == attached_layer).then_some((8, 9)),
            )
        };
        assert!(validate(&[base.clone(), overlay.clone()], false, 0, true));
        assert!(validate(&[base.clone(), overlay.clone()], true, 2, false));
        assert!(!validate(&[base.clone(), overlay.clone()], true, 3, false));
        assert!(!validate(&[base.clone(), overlay.clone()], true, 2, true));
        assert!(!validate(&[base.clone(), base.clone()], false, 0, false));
        overlay.id = base.id;
        assert!(!validate(&[base.clone(), overlay], false, 0, false));
        base.ticket = 8;
        assert!(!validate(&[base], false, 0, false));
        assert!(!validate(&[], false, 0, false));
    }

    #[test]
    fn new_read_of_an_allocation_still_read_elsewhere_takes_a_spare_timeline_or_waits() {
        let b = buffer();
        let mut l = layer();
        l.ticket = 10;
        let assess = |ticket, has_acquire, attached, spare_timeline| {
            assess_scene(
                [(&l, has_acquire)].into_iter(),
                1,
                800,
                600,
                9,
                |_| {
                    Some(SourceState {
                        buffer: &b,
                        imported: true,
                        ticket,
                        spare_timeline,
                    })
                },
                |_| attached,
            )
        };
        for spare in [false, true] {
            // This layer shows ticket 9 of the allocation: ticket 10 relabels it.
            assert_eq!(
                assess(Some(9), true, Some((8, 9)), spare),
                SceneStatus::Ready
            );
            assert_eq!(assess(None, true, Some((8, 9)), spare), SceneStatus::Ready);
            assert_eq!(
                assess(Some(10), false, Some((8, 10)), spare),
                SceneStatus::Ready
            );
            assert_eq!(
                assess(Some(10), true, Some((8, 10)), spare),
                SceneStatus::Invalid
            );
            assert_eq!(assess(Some(10), false, None, spare), SceneStatus::Invalid);
        }
        // Ticket 9 was detached from this layer and is still being released.
        // A new read starts beside it, or waits once no timeline is spare.
        assert_eq!(assess(Some(9), true, None, true), SceneStatus::Ready);
        assert_eq!(
            assess(Some(9), true, Some((7, 9)), true),
            SceneStatus::Ready
        );
        assert_eq!(assess(Some(9), true, None, false), SceneStatus::Busy);
        assert_eq!(
            assess(Some(9), true, Some((7, 9)), false),
            SceneStatus::Busy
        );
    }

    #[test]
    fn busy_scene_must_still_validate_every_layer_before_it_can_wait() {
        let b = buffer();
        let base = layer();
        let mut overlay = base.clone();
        overlay.id = 3;
        overlay.buffer = 0;
        overlay.ticket = 0;
        let assess = |scene: &[Layer]| {
            assess_scene(
                scene.iter().map(|l| (l, l.buffer != 0)),
                scene.len() as u32,
                800,
                600,
                8,
                |id| {
                    (id == b.id).then_some(SourceState {
                        buffer: &b,
                        imported: true,
                        ticket: Some(8),
                        spare_timeline: false,
                    })
                },
                |_| None,
            )
        };
        assert_eq!(assess(&[base.clone(), overlay.clone()]), SceneStatus::Busy);
        overlay.destination[2] += 1;
        assert_eq!(assess(&[base.clone(), overlay]), SceneStatus::Invalid);
        assert_eq!(assess(&[base.clone(), base.clone()]), SceneStatus::Invalid);
        let mut stale = base;
        stale.ticket = 7;
        assert_eq!(assess(&[stale]), SceneStatus::Invalid);
    }

    #[test]
    fn waiting_scene_preserves_unchanged_peer_reads_and_rechecks_expired_tickets() {
        let a = buffer();
        let mut b = a.clone();
        b.id = 10;
        let mut changing = layer();
        changing.ticket = 12;
        let mut unchanged = changing.clone();
        unchanged.id = 3;
        unchanged.buffer = 10;
        unchanged.ticket = 11;
        let scene = [changing, unchanged];
        let assess = |changing_ticket, unchanged_ticket| {
            assess_scene(
                scene.iter().map(|l| (l, l.id == 2)),
                2,
                800,
                600,
                11,
                |id| {
                    Some(SourceState {
                        buffer: if id == 8 { &a } else { &b },
                        imported: true,
                        ticket: if id == 8 {
                            changing_ticket
                        } else {
                            unchanged_ticket
                        },
                        spare_timeline: false,
                    })
                },
                |id| (id == 3).then_some((10, 11)),
            )
        };
        assert_eq!(assess(Some(9), Some(11)), SceneStatus::Busy);
        assert_eq!(assess(None, Some(11)), SceneStatus::Ready);
        assert_eq!(assess(None, None), SceneStatus::Invalid);
    }

    #[test]
    fn malformed_or_cross_version_packets_are_rejected() {
        let mut p = response(1, &[]);
        assert_eq!(decode(&p, 0), Ok(Request::Config));
        assert!(decode(&p, 1).is_err());
        p[3] = b'2';
        assert!(decode(&p, 0).is_err());
        assert!(decode(&response(5, &[1, 800, 600, 2]), 0).is_ok());
        assert!(decode(&response(5, &[1, 800, 600]), 0).is_err());
    }
}
