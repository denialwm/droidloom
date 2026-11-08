//! braun's session protocol, as Phonewave's braun speaks it on the socket
//! its `Connect` returns: `SOCK_SEQPACKET`, one message per packet,
//! little-endian, the first byte the kind. Descriptors travel beside a
//! message: the preview memory with `Opened`, the JPEG file with `Shoot`.

/// The largest message braun sends.
pub const MAX_MESSAGE: usize = 4096;

/// What the camera service asks braun.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// Open camera `id`, closing the one open.
    Open(String),
    /// Close the camera.
    Close,
    /// Focus on this point of the preview, 0 to 1 across and down.
    Focus(f32, f32),
    /// Crop the middle `1 / zoom` of the frame.
    Zoom(f32),
    /// Take photo `shot` into the JPEG file beside the message.
    Shoot(u32),
    /// The preview in this slot has been read.
    Release(u32),
}

/// What braun says.
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    /// The camera is open: `width` x `height` upright, focusing if it has a
    /// `lens`, previews in `slots` slots of `slot_bytes` in the memory
    /// beside the message.
    Opened {
        /// Frame width, upright.
        width: u32,
        /// Frame height, upright.
        height: u32,
        /// Whether it focuses.
        lens: bool,
        /// Preview slots.
        slots: u32,
        /// Bytes per slot.
        slot_bytes: u32,
    },
    /// A preview in `slot`: BGRA, `width` x `height`, rows packed.
    Frame {
        /// Slot to read, then release.
        slot: u32,
        /// Pixels across.
        width: u32,
        /// Rows.
        height: u32,
        /// Counts frames.
        sequence: u64,
    },
    /// Photo `shot`'s burst is in.
    Captured(u32),
    /// Photo `shot` is written.
    Saved(u32),
    /// Photo `shot` failed or, for 0, the camera did and is closed.
    Failed(u32, String),
}

impl Request {
    /// The message bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Request::Open(id) => {
                out.push(1);
                out.extend_from_slice(id.as_bytes());
            }
            Request::Close => out.push(2),
            Request::Focus(x, y) => {
                out.push(3);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            Request::Zoom(zoom) => {
                out.push(4);
                out.extend_from_slice(&zoom.to_le_bytes());
            }
            Request::Shoot(shot) => {
                out.push(5);
                out.extend_from_slice(&shot.to_le_bytes());
            }
            Request::Release(slot) => {
                out.push(6);
                out.extend_from_slice(&slot.to_le_bytes());
            }
        }
        out
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_at_checked(N)?;
        self.0 = rest;
        head.try_into().ok()
    }
    fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_le_bytes)
    }
}

impl Reply {
    /// The reply in `bytes`, or `None` for one this service does not know.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (&kind, rest) = bytes.split_first()?;
        let mut reader = Reader(rest);
        Some(match kind {
            1 => Reply::Opened {
                width: reader.u32()?,
                height: reader.u32()?,
                lens: reader.take::<1>()?[0] != 0,
                slots: reader.u32()?,
                slot_bytes: reader.u32()?,
            },
            2 => Reply::Frame {
                slot: reader.u32()?,
                width: reader.u32()?,
                height: reader.u32()?,
                sequence: reader.u64()?,
            },
            3 => Reply::Captured(reader.u32()?),
            4 => Reply::Saved(reader.u32()?),
            5 => Reply::Failed(reader.u32()?, String::from_utf8_lossy(reader.0).into_owned()),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_match_braun() {
        assert_eq!(Request::Open("imx890".into()).encode(), b"\x01imx890");
        assert_eq!(Request::Close.encode(), [2]);
        assert_eq!(Request::Zoom(2.0).encode(), [4, 0, 0, 0, 0x40]);
        assert_eq!(Request::Shoot(7).encode(), [5, 7, 0, 0, 0]);
        assert_eq!(Request::Release(2).encode(), [6, 2, 0, 0, 0]);
        let focus = Request::Focus(0.25, 0.75).encode();
        assert_eq!(focus[0], 3);
        assert_eq!(&focus[5..9], &0.75f32.to_le_bytes());
    }

    #[test]
    fn replies_decode() {
        let mut opened = vec![1];
        for word in [3072u32, 4096] {
            opened.extend_from_slice(&word.to_le_bytes());
        }
        opened.push(1);
        for word in [3u32, 1 << 20] {
            opened.extend_from_slice(&word.to_le_bytes());
        }
        assert_eq!(
            Reply::decode(&opened),
            Some(Reply::Opened { width: 3072, height: 4096, lens: true, slots: 3, slot_bytes: 1 << 20 })
        );
        let mut frame = vec![2];
        for word in [1u32, 768, 1024] {
            frame.extend_from_slice(&word.to_le_bytes());
        }
        frame.extend_from_slice(&(1u64 << 40).to_le_bytes());
        assert_eq!(
            Reply::decode(&frame),
            Some(Reply::Frame { slot: 1, width: 768, height: 1024, sequence: 1 << 40 })
        );
        assert_eq!(Reply::decode(b"\x05\0\0\0\0taken"), Some(Reply::Failed(0, "taken".into())));
        assert_eq!(Reply::decode(&[4, 9, 0, 0, 0]), Some(Reply::Saved(9)));
        assert_eq!(Reply::decode(&[2, 1]), None);
        assert_eq!(Reply::decode(&[9]), None);
    }
}
