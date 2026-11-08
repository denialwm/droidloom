//! braun's previews into Android's buffers: the middle of the frame with
//! the buffer's aspect, scaled bilinearly, as RGBA or full-range BT.601
//! YCbCr (JFIF), the encoding Android's camera YUV streams carry.

/// DRM fourcc codes minigbm allocates for the formats the camera fills.
pub mod fourcc {
    #![allow(missing_docs)]
    const fn code(name: [u8; 4]) -> u32 {
        u32::from_le_bytes(name)
    }
    pub const ABGR8888: u32 = code(*b"AB24");
    pub const XBGR8888: u32 = code(*b"XB24");
    pub const ARGB8888: u32 = code(*b"AR24");
    pub const XRGB8888: u32 = code(*b"XR24");
    pub const NV12: u32 = code(*b"NV12");
    pub const NV21: u32 = code(*b"NV21");
    pub const YVU420: u32 = code(*b"YV12");
    pub const YUV420: u32 = code(*b"YU12");
    pub const R8: u32 = code(*b"R8  ");
}

/// A BGRA frame, rows packed.
pub struct Source<'a> {
    /// `width` x `height` x 4 bytes.
    pub bgra: &'a [u8],
    /// Pixels across.
    pub width: usize,
    /// Rows.
    pub height: usize,
}

/// Where a plane starts and how far apart its rows are, in bytes.
#[derive(Clone, Copy, Debug)]
pub struct Plane {
    /// From the start of the buffer.
    pub offset: usize,
    /// Row pitch.
    pub stride: usize,
}

/// A buffer to fill.
pub struct Target<'a> {
    /// DRM fourcc.
    pub fourcc: u32,
    /// Pixels across.
    pub width: usize,
    /// Rows.
    pub height: usize,
    /// Planes in fourcc order.
    pub planes: &'a [Plane],
    /// The whole mapped buffer.
    pub memory: &'a mut [u8],
}

/// Why a buffer was not filled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Unfilled {
    /// A format this converter does not write.
    Format,
    /// Planes that do not fit the buffer, or an empty frame.
    Bounds,
}

/// One axis of the scale: for each output position, the first source
/// sample and the weight of the next, in 256ths.
#[allow(clippy::cast_precision_loss)]
fn axis(output: usize, source_first: usize, source_length: usize, source_limit: usize) -> Vec<(usize, u32)> {
    let scale = source_length as f32 / output as f32;
    (0..output)
        .map(|index| {
            let at = ((index as f32 + 0.5) * scale - 0.5).max(0.0);
            let whole = at.floor();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let first = (source_first + whole as usize).min(source_limit - 1);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let weight = ((at - whole) * 256.0) as u32;
            (first, weight)
        })
        .collect()
}

/// The middle of `width` x `height` with the aspect of `out_width` x `out_height`.
fn crop(width: usize, height: usize, out_width: usize, out_height: usize) -> (usize, usize, usize, usize) {
    if width * out_height > height * out_width {
        let seen = (height * out_width / out_height).max(1);
        ((width - seen) / 2, 0, seen, height)
    } else {
        let seen = (width * out_height / out_width).max(1);
        (0, (height - seen) / 2, width, seen)
    }
}

struct Sampler<'a> {
    source: &'a Source<'a>,
    columns: Vec<(usize, u32)>,
    rows: Vec<(usize, u32)>,
    flip: bool,
}

impl Sampler<'_> {
    /// R, G, B of output pixel (`x`, `y`).
    #[inline]
    fn rgb(&self, x: usize, y: usize) -> [u32; 3] {
        let x = if self.flip { self.columns.len() - 1 - x } else { x };
        let (x0, wx) = self.columns[x];
        let (y0, wy) = self.rows[y];
        let x1 = (x0 + 1).min(self.source.width - 1);
        let y1 = (y0 + 1).min(self.source.height - 1);
        let row0 = y0 * self.source.width * 4;
        let row1 = y1 * self.source.width * 4;
        let bgra = self.source.bgra;
        let mut rgb = [0; 3];
        for (channel, out) in rgb.iter_mut().enumerate() {
            // BGRA: blue first.
            let index = 2 - channel;
            let top = u32::from(bgra[row0 + x0 * 4 + index]) * (256 - wx)
                + u32::from(bgra[row0 + x1 * 4 + index]) * wx;
            let bottom = u32::from(bgra[row1 + x0 * 4 + index]) * (256 - wx)
                + u32::from(bgra[row1 + x1 * 4 + index]) * wx;
            *out = (top * (256 - wy) + bottom * wy + (1 << 15)) >> 16;
        }
        rgb
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_possible_wrap)]
fn luma([r, g, b]: [u32; 3]) -> u8 {
    ((77 * r + 150 * g + 29 * b + 128) >> 8).min(255) as u8
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_possible_wrap)]
fn chroma([r, g, b]: [u32; 3]) -> (u8, u8) {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    let cb = ((-43 * r - 85 * g + 128 * b + 128) >> 8) + 128;
    let cr = ((128 * r - 107 * g - 21 * b + 128) >> 8) + 128;
    (cb.clamp(0, 255) as u8, cr.clamp(0, 255) as u8)
}

fn plane_fits(plane: Plane, row_bytes: usize, rows: usize, length: usize) -> bool {
    rows == 0
        || plane.stride >= row_bytes
            && plane
                .stride
                .checked_mul(rows - 1)
                .and_then(|span| span.checked_add(plane.offset))
                .and_then(|start| start.checked_add(row_bytes))
                .is_some_and(|end| end <= length)
}

/// Fills `target` with the middle of `source`, mirrored across if `flip`.
///
/// # Errors
/// [`Unfilled`] for a format it does not write or planes that do not fit.
pub fn fill(source: &Source<'_>, target: &mut Target<'_>, flip: bool) -> Result<(), Unfilled> {
    let (width, height) = (target.width, target.height);
    if width == 0
        || height == 0
        || source.width == 0
        || source.height == 0
        || source.bgra.len() < source.width * source.height * 4
    {
        return Err(Unfilled::Bounds);
    }
    let (left, top, seen_x, seen_y) = crop(source.width, source.height, width, height);
    let sampler = Sampler {
        source,
        columns: axis(width, left, seen_x, source.width),
        rows: axis(height, top, seen_y, source.height),
        flip,
    };
    let length = target.memory.len();
    let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
    match target.fourcc {
        fourcc::ABGR8888 | fourcc::XBGR8888 | fourcc::ARGB8888 | fourcc::XRGB8888 => {
            let plane = *target.planes.first().ok_or(Unfilled::Bounds)?;
            if !plane_fits(plane, width * 4, height, length) {
                return Err(Unfilled::Bounds);
            }
            let bgr = matches!(target.fourcc, fourcc::ARGB8888 | fourcc::XRGB8888);
            for y in 0..height {
                let row = &mut target.memory[plane.offset + y * plane.stride..][..width * 4];
                for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
                    #[allow(clippy::cast_possible_truncation)]
                    let [r, g, b] = sampler.rgb(x, y).map(|value| value as u8);
                    pixel.copy_from_slice(&if bgr { [b, g, r, 255] } else { [r, g, b, 255] });
                }
            }
        }
        fourcc::NV12 | fourcc::NV21 | fourcc::YVU420 | fourcc::YUV420 => {
            let luma_plane = *target.planes.first().ok_or(Unfilled::Bounds)?;
            if !plane_fits(luma_plane, width, height, length) {
                return Err(Unfilled::Bounds);
            }
            for y in 0..height {
                let row = &mut target.memory[luma_plane.offset + y * luma_plane.stride..][..width];
                for (x, value) in row.iter_mut().enumerate() {
                    *value = luma(sampler.rgb(x, y));
                }
            }
            let semi = matches!(target.fourcc, fourcc::NV12 | fourcc::NV21);
            let planes = if semi { 2 } else { 3 };
            if target.planes.len() < planes {
                return Err(Unfilled::Bounds);
            }
            let row_bytes = if semi { chroma_width * 2 } else { chroma_width };
            if !target.planes[1..planes]
                .iter()
                .all(|plane| plane_fits(*plane, row_bytes, chroma_height, length))
            {
                return Err(Unfilled::Bounds);
            }
            for cy in 0..chroma_height {
                for cx in 0..chroma_width {
                    let (x, y) = (cx * 2, cy * 2);
                    let mut sum = [0; 3];
                    let mut count = 0;
                    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        if x + dx < width && y + dy < height {
                            let rgb = sampler.rgb(x + dx, y + dy);
                            for channel in 0..3 {
                                sum[channel] += rgb[channel];
                            }
                            count += 1;
                        }
                    }
                    let (cb, cr) = chroma(sum.map(|value| value / count));
                    match target.fourcc {
                        fourcc::NV12 | fourcc::NV21 => {
                            let plane = target.planes[1];
                            let at = plane.offset + cy * plane.stride + cx * 2;
                            let pair = if target.fourcc == fourcc::NV12 { [cb, cr] } else { [cr, cb] };
                            target.memory[at..at + 2].copy_from_slice(&pair);
                        }
                        _ => {
                            let (u, v) = if target.fourcc == fourcc::YUV420 { (1, 2) } else { (2, 1) };
                            let (pu, pv) = (target.planes[u], target.planes[v]);
                            target.memory[pu.offset + cy * pu.stride + cx] = cb;
                            target.memory[pv.offset + cy * pv.stride + cx] = cr;
                        }
                    }
                }
            }
        }
        _ => return Err(Unfilled::Format),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: usize, height: usize, bgra: [u8; 4]) -> Vec<u8> {
        bgra.repeat(width * height)
    }

    #[test]
    fn rgba_keeps_colour_and_order() {
        let frame = solid(8, 6, [10, 20, 30, 255]);
        let source = Source { bgra: &frame, width: 8, height: 6 };
        let mut memory = vec![0; 64 * 3];
        let planes = [Plane { offset: 0, stride: 64 }];
        let mut target = Target { fourcc: fourcc::ABGR8888, width: 4, height: 3, planes: &planes, memory: &mut memory };
        fill(&source, &mut target, false).unwrap();
        assert_eq!(&memory[..8], &[30, 20, 10, 255, 30, 20, 10, 255]);
        assert_eq!(&memory[64..68], &[30, 20, 10, 255]);
        assert_eq!(memory[16], 0, "the stride's padding is left alone");
    }

    #[test]
    fn nv12_of_grey_and_red() {
        let grey = solid(4, 4, [128, 128, 128, 255]);
        let source = Source { bgra: &grey, width: 4, height: 4 };
        let mut memory = vec![0; 16 + 8];
        let planes = [Plane { offset: 0, stride: 4 }, Plane { offset: 16, stride: 4 }];
        let mut target = Target { fourcc: fourcc::NV12, width: 4, height: 4, planes: &planes, memory: &mut memory };
        fill(&source, &mut target, false).unwrap();
        assert!(memory[..16].iter().all(|y| *y == 128));
        assert!(memory[16..].iter().all(|c| *c == 128));

        let red = solid(4, 4, [0, 0, 255, 255]);
        let source = Source { bgra: &red, width: 4, height: 4 };
        let mut target = Target { fourcc: fourcc::NV21, width: 4, height: 4, planes: &planes, memory: &mut memory };
        fill(&source, &mut target, false).unwrap();
        assert_eq!(memory[0], 77);
        // NV21: Cr first, then Cb.
        assert_eq!(&memory[16..18], &[255, 85]);
    }

    #[test]
    fn mirror_and_crop() {
        // A frame white on the left half, black on the right.
        let mut frame = Vec::new();
        for _ in 0..4 {
            frame.extend_from_slice(&[255, 255, 255, 255].repeat(4));
            frame.extend_from_slice(&[0, 0, 0, 255].repeat(4));
        }
        let source = Source { bgra: &frame, width: 8, height: 4 };
        let planes = [Plane { offset: 0, stride: 2 }];
        let mut memory = vec![0; 4];
        let mut target = Target { fourcc: fourcc::YUV420, width: 2, height: 2, planes: &planes, memory: &mut memory };
        // Two planes missing: refused before writing chroma.
        assert_eq!(fill(&source, &mut target, false), Err(Unfilled::Bounds));
        let planes = [Plane { offset: 0, stride: 2 }, Plane { offset: 4, stride: 1 }, Plane { offset: 5, stride: 1 }];
        let mut memory = vec![0; 6];
        let mut target = Target { fourcc: fourcc::YUV420, width: 2, height: 2, planes: &planes, memory: &mut memory };
        fill(&source, &mut target, true).unwrap();
        // Square out of 2:1, so the middle 4x4; mirrored, black comes first.
        assert!(memory[0] < 64 && memory[1] > 192, "{memory:?}");
    }

    #[test]
    fn unknown_formats_and_short_buffers() {
        let frame = solid(2, 2, [0; 4]);
        let source = Source { bgra: &frame, width: 2, height: 2 };
        let planes = [Plane { offset: 0, stride: 8 }];
        let mut memory = vec![0; 8];
        let mut target = Target { fourcc: fourcc::R8, width: 2, height: 2, planes: &planes, memory: &mut memory };
        assert_eq!(fill(&source, &mut target, false), Err(Unfilled::Format));
        let mut target = Target { fourcc: fourcc::ABGR8888, width: 2, height: 2, planes: &planes, memory: &mut memory };
        assert_eq!(fill(&source, &mut target, false), Err(Unfilled::Bounds));
    }
}
