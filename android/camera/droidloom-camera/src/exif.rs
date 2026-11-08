//! The orientation of a JPEG, written as an EXIF segment right after its
//! start-of-image marker. braun's photos are upright and carry none, so a
//! viewer turns them as this says: the turn Android asked for and, for the
//! front camera, the mirror braun applied.

/// The EXIF orientation of a picture to turn `degrees` clockwise when
/// shown, mirrored left to right first if `mirrored`.
pub fn orientation(degrees: i32, mirrored: bool) -> u16 {
    match (degrees.rem_euclid(360) / 90, mirrored) {
        (0, false) => 1,
        (1, false) => 6,
        (2, false) => 3,
        (3, false) => 8,
        (0, true) => 2,
        (1, true) => 7,
        (2, true) => 4,
        _ => 5,
    }
}

/// An APP1 segment holding only the orientation: little-endian TIFF, one
/// IFD of one entry.
fn segment(orientation: u16) -> Vec<u8> {
    let mut tiff = Vec::with_capacity(26);
    tiff.extend_from_slice(b"II*\0");
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&1u16.to_le_bytes());
    // Orientation, SHORT, one value.
    tiff.extend_from_slice(&0x0112u16.to_le_bytes());
    tiff.extend_from_slice(&3u16.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&orientation.to_le_bytes());
    tiff.extend_from_slice(&[0, 0]);
    // No next IFD.
    tiff.extend_from_slice(&0u32.to_le_bytes());
    let length = u16::try_from(2 + 6 + tiff.len()).expect("short segment");
    let mut out = vec![0xff, 0xe1];
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(b"Exif\0\0");
    out.extend_from_slice(&tiff);
    out
}

/// `jpeg` with its orientation set, or `None` if it is not a JPEG. An EXIF
/// segment already there is left alone and this one goes before it, which
/// is what readers take.
pub fn with_orientation(jpeg: &[u8], orientation: u16) -> Option<Vec<u8>> {
    let rest = jpeg.strip_prefix(&[0xff, 0xd8])?;
    let segment = segment(orientation);
    let mut out = Vec::with_capacity(jpeg.len() + segment.len());
    out.extend_from_slice(&[0xff, 0xd8]);
    out.extend_from_slice(&segment);
    out.extend_from_slice(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orientations() {
        assert_eq!(orientation(0, false), 1);
        assert_eq!(orientation(90, false), 6);
        assert_eq!(orientation(-90, false), 8);
        assert_eq!(orientation(180, true), 4);
        assert_eq!(orientation(270, true), 5);
    }

    #[test]
    fn segment_goes_after_start_of_image() {
        let jpeg = [0xff, 0xd8, 0xff, 0xdb, 0, 2, 0xff, 0xd9];
        let out = with_orientation(&jpeg, 6).unwrap();
        assert_eq!(&out[..4], &[0xff, 0xd8, 0xff, 0xe1]);
        let length = usize::from(u16::from_be_bytes([out[4], out[5]]));
        assert_eq!(&out[6..12], b"Exif\0\0");
        assert_eq!(&out[4 + length..], &jpeg[2..]);
        // The orientation value, in the IFD's one entry.
        let tiff = &out[12..];
        assert_eq!(u16::from_le_bytes([tiff[18], tiff[19]]), 6);
        assert!(with_orientation(b"PNG", 1).is_none());
    }
}
