//! Android's `camera_metadata_t` as the camera AIDL carries it in
//! `CameraMetadata.metadata`: a header, sorted 16-byte entries, then data.
//! A value of four bytes or fewer sits in its entry; a longer one in the
//! data, aligned to eight bytes.

use std::collections::BTreeMap;

const HEADER_BYTES: usize = 48;
const ENTRY_BYTES: usize = 16;
const DATA_ALIGNMENT: usize = 8;
const VERSION: u32 = 1;
const FLAG_SORTED: u32 = 1;
const NO_VENDOR: u64 = u64::MAX;

/// Element type of an entry, as `libcamera_metadata` numbers them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Type {
    /// `u8`, also enums.
    Byte = 0,
    /// `i32`.
    Int32 = 1,
    /// `f32`.
    Float = 2,
    /// `i64`.
    Int64 = 3,
    /// `f64`.
    Double = 4,
    /// Two `i32`: numerator, denominator.
    Rational = 5,
}

impl Type {
    fn size(self) -> usize {
        match self {
            Type::Byte => 1,
            Type::Int32 | Type::Float => 4,
            Type::Int64 | Type::Double | Type::Rational => 8,
        }
    }

    fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Type::Byte,
            1 => Type::Int32,
            2 => Type::Float,
            3 => Type::Int64,
            4 => Type::Double,
            5 => Type::Rational,
            _ => return None,
        })
    }
}

/// One entry's values, little-endian as on the phone.
#[derive(Clone, Debug, PartialEq)]
pub struct Value {
    /// Element type.
    pub kind: Type,
    /// `count` elements of `kind`.
    pub bytes: Vec<u8>,
}

impl Value {
    fn count(&self) -> usize {
        self.bytes.len() / self.kind.size()
    }
}

/// A metadata buffer that does not decode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Malformed(pub &'static str);

/// Camera metadata: tag to values.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Metadata {
    entries: BTreeMap<u32, Value>,
}

impl Metadata {
    /// No entries.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether there are no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The tags present, ascending.
    pub fn tags(&self) -> impl Iterator<Item = u32> + '_ {
        self.entries.keys().copied()
    }

    /// The values of `tag`.
    pub fn get(&self, tag: u32) -> Option<&Value> {
        self.entries.get(&tag)
    }

    /// Sets `tag` to `value`.
    pub fn set(&mut self, tag: u32, value: Value) {
        self.entries.insert(tag, value);
    }

    /// Removes `tag`.
    pub fn remove(&mut self, tag: u32) {
        self.entries.remove(&tag);
    }

    /// Every entry of `other`, over these.
    pub fn merge(&mut self, other: &Metadata) {
        for (tag, value) in &other.entries {
            self.entries.insert(*tag, value.clone());
        }
    }

    /// Sets bytes.
    pub fn set_u8(&mut self, tag: u32, values: &[u8]) {
        self.set(
            tag,
            Value {
                kind: Type::Byte,
                bytes: values.to_vec(),
            },
        );
    }

    /// Sets `i32`s.
    pub fn set_i32(&mut self, tag: u32, values: &[i32]) {
        self.set(tag, Value::from_le(Type::Int32, values.iter().map(|v| v.to_le_bytes())));
    }

    /// Sets `f32`s.
    pub fn set_f32(&mut self, tag: u32, values: &[f32]) {
        self.set(tag, Value::from_le(Type::Float, values.iter().map(|v| v.to_le_bytes())));
    }

    /// Sets `i64`s.
    pub fn set_i64(&mut self, tag: u32, values: &[i64]) {
        self.set(tag, Value::from_le(Type::Int64, values.iter().map(|v| v.to_le_bytes())));
    }

    /// Sets rationals.
    pub fn set_rational(&mut self, tag: u32, values: &[(i32, i32)]) {
        self.set(
            tag,
            Value::from_le(
                Type::Rational,
                values.iter().map(|(numerator, denominator)| {
                    let mut bytes = [0; 8];
                    bytes[..4].copy_from_slice(&numerator.to_le_bytes());
                    bytes[4..].copy_from_slice(&denominator.to_le_bytes());
                    bytes
                }),
            ),
        );
    }

    /// The first byte of `tag`.
    pub fn u8(&self, tag: u32) -> Option<u8> {
        self.typed(tag, Type::Byte)?.first().copied()
    }

    /// The `i32`s of `tag`.
    pub fn i32s(&self, tag: u32) -> Option<Vec<i32>> {
        Some(
            self.typed(tag, Type::Int32)?
                .chunks_exact(4)
                .map(|chunk| i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect(),
        )
    }

    /// The first `f32` of `tag`.
    pub fn f32(&self, tag: u32) -> Option<f32> {
        let bytes = self.typed(tag, Type::Float)?;
        Some(f32::from_le_bytes(bytes.get(..4)?.try_into().ok()?))
    }

    fn typed(&self, tag: u32, kind: Type) -> Option<&[u8]> {
        let value = self.entries.get(&tag)?;
        (value.kind == kind).then_some(value.bytes.as_slice())
    }

    /// The `camera_metadata_t` bytes, entries sorted by tag.
    pub fn encode(&self) -> Vec<u8> {
        let entries_start = HEADER_BYTES;
        let entries_bytes = self.entries.len() * ENTRY_BYTES;
        let data_start = align(entries_start + entries_bytes, DATA_ALIGNMENT);
        let data_count: usize = self
            .entries
            .values()
            .map(|value| data_bytes(value.bytes.len()))
            .sum();
        let size = align(data_start + data_count, DATA_ALIGNMENT);
        let mut out = vec![0u8; size];
        let header = [
            size,
            VERSION as usize,
            FLAG_SORTED as usize,
            self.entries.len(),
            self.entries.len(),
            entries_start,
            data_count,
            data_count,
            data_start,
            0,
        ];
        for (index, field) in header.iter().enumerate() {
            put_u32(&mut out, index * 4, *field);
        }
        out[40..48].copy_from_slice(&NO_VENDOR.to_le_bytes());
        let mut data_offset = 0;
        for (index, (tag, value)) in self.entries.iter().enumerate() {
            let at = entries_start + index * ENTRY_BYTES;
            out[at..at + 4].copy_from_slice(&tag.to_le_bytes());
            put_u32(&mut out, at + 4, value.count());
            if value.bytes.len() <= 4 {
                out[at + 8..at + 8 + value.bytes.len()].copy_from_slice(&value.bytes);
            } else {
                put_u32(&mut out, at + 8, data_offset);
                let to = data_start + data_offset;
                out[to..to + value.bytes.len()].copy_from_slice(&value.bytes);
                data_offset += data_bytes(value.bytes.len());
            }
            out[at + 12] = value.kind as u8;
        }
        out
    }

    /// The metadata in `bytes`; none in an empty buffer.
    ///
    /// # Errors
    /// [`Malformed`] when the header, an entry or its data is out of bounds.
    pub fn decode(bytes: &[u8]) -> Result<Self, Malformed> {
        let mut metadata = Metadata::new();
        if bytes.is_empty() {
            return Ok(metadata);
        }
        if bytes.len() < HEADER_BYTES {
            return Err(Malformed("short header"));
        }
        let field = |index: usize| get_u32(bytes, index * 4);
        let size = field(0);
        let entry_count = field(3);
        let entries_start = field(5);
        let data_count = field(6);
        let data_start = field(8);
        if size > bytes.len() || field(1) != VERSION as usize {
            return Err(Malformed("bad size or version"));
        }
        let entries_end = entry_count
            .checked_mul(ENTRY_BYTES)
            .and_then(|length| length.checked_add(entries_start))
            .ok_or(Malformed("entries overflow"))?;
        let data_end = data_start
            .checked_add(data_count)
            .ok_or(Malformed("data overflow"))?;
        if entries_end > size || data_end > size || entries_start % 4 != 0 {
            return Err(Malformed("entries or data out of bounds"));
        }
        for index in 0..entry_count {
            let at = entries_start + index * ENTRY_BYTES;
            let tag = u32::try_from(get_u32(bytes, at)).map_err(|_| Malformed("tag"))?;
            let count = get_u32(bytes, at + 4);
            let kind = Type::from_u8(bytes[at + 12]).ok_or(Malformed("unknown type"))?;
            let length = count
                .checked_mul(kind.size())
                .ok_or(Malformed("count overflow"))?;
            let value = if length <= 4 {
                bytes[at + 8..at + 8 + length].to_vec()
            } else {
                let from = data_start
                    .checked_add(get_u32(bytes, at + 8))
                    .ok_or(Malformed("offset overflow"))?;
                let to = from.checked_add(length).ok_or(Malformed("offset overflow"))?;
                if to > data_end {
                    return Err(Malformed("value out of bounds"));
                }
                bytes[from..to].to_vec()
            };
            metadata.set(tag, Value { kind, bytes: value });
        }
        Ok(metadata)
    }
}

impl Value {
    fn from_le<const N: usize>(kind: Type, values: impl Iterator<Item = [u8; N]>) -> Self {
        Self {
            kind,
            bytes: values.flatten().collect(),
        }
    }
}

fn align(value: usize, alignment: usize) -> usize {
    value.div_ceil(alignment) * alignment
}

fn data_bytes(length: usize) -> usize {
    if length <= 4 { 0 } else { align(length, DATA_ALIGNMENT) }
}

fn put_u32(out: &mut [u8], at: usize, value: usize) {
    let value = u32::try_from(value).expect("metadata fits 32-bit offsets");
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn get_u32(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes")) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_round_trip_in_tag_order() {
        let mut metadata = Metadata::new();
        metadata.set_i32(0xd000a, &[34, 1024, 768, 0, 33, 3072, 4096, 0]);
        metadata.set_u8(0x10003, &[1]);
        metadata.set_f32(0x1002f, &[2.5]);
        metadata.set_i64(0xe0010, &[123_456_789_012]);
        metadata.set_rational(0x10016, &[(1, 3)]);
        let bytes = metadata.encode();
        assert_eq!(bytes.len() % 8, 0);
        let decoded = Metadata::decode(&bytes).unwrap();
        assert_eq!(decoded, metadata);
        assert_eq!(decoded.u8(0x10003), Some(1));
        assert_eq!(decoded.f32(0x1002f), Some(2.5));
        assert_eq!(decoded.i32s(0xd000a).unwrap()[2], 768);
        assert_eq!(decoded.tags().collect::<Vec<_>>(), vec![0x10003, 0x10016, 0x1002f, 0xd000a, 0xe0010]);
    }

    #[test]
    fn layout_matches_libcamera_metadata() {
        let mut metadata = Metadata::new();
        metadata.set_u8(0x10003, &[1]);
        metadata.set_i64(0xe0010, &[7]);
        let bytes = metadata.encode();
        // Header: size, version, sorted, 2 entries of 2, entries at 48,
        // 8 data bytes of 8, data at 80, padding, no vendor.
        let words: Vec<u32> = bytes[..40]
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes(chunk.try_into().unwrap()))
            .collect();
        assert_eq!(words, vec![88, 1, 1, 2, 2, 48, 8, 8, 80, 0]);
        // The byte sits in its entry; the i64 at data offset 0.
        assert_eq!(&bytes[48..64], &[3, 0, 1, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[64 + 8..64 + 13], &[0, 0, 0, 0, 3]);
        assert_eq!(&bytes[80..88], &7i64.to_le_bytes());
    }

    #[test]
    fn empty_and_broken_buffers() {
        assert!(Metadata::decode(&[]).unwrap().is_empty());
        let mut metadata = Metadata::new();
        metadata.set_i64(0xe0010, &[7]);
        let mut bytes = metadata.encode();
        // The entry's data offset, past the data.
        bytes[48 + 8] = 200;
        assert!(Metadata::decode(&bytes).is_err());
        assert!(Metadata::decode(&bytes[..20]).is_err());
    }
}
