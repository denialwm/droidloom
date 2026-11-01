//! Lookup tables for the ASTC decoder (shaders/astc.comp), ported from
//! Granite's texcompress_astc_luts.cpp as Mesa carries it.
//!
//! Copyright (c) 2017-2022 Hans-Kristian Arntzen
//!
//! Permission is hereby granted, free of charge, to any person obtaining
//! a copy of this software and associated documentation files (the
//! "Software"), to deal in the Software without restriction, including
//! without limitation the rights to use, copy, modify, merge, publish,
//! distribute, sublicense, and/or sell copies of the Software, and to
//! permit persons to whom the Software is furnished to do so, subject to
//! the following conditions:
//!
//! The above copyright notice and this permission notice shall be
//! included in all copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
//! EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
//! MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
//! IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
//! CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
//! TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
//! SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

#[derive(Clone, Copy)]
struct Mode {
    bits: u32,
    trits: u32,
    quints: u32,
}

const fn m(bits: u32, trits: u32, quints: u32) -> Mode {
    Mode { bits, trits, quints }
}

/// Endpoint quantization modes, best first.
const QUANTIZATION: [Mode; 17] = [
    m(8, 0, 0), m(6, 1, 0), m(5, 0, 1), m(7, 0, 0), m(5, 1, 0), m(4, 0, 1),
    m(6, 0, 0), m(4, 1, 0), m(3, 0, 1), m(5, 0, 0), m(3, 1, 0), m(2, 0, 1),
    m(4, 0, 0), m(2, 1, 0), m(1, 0, 1), m(3, 0, 0), m(1, 1, 0),
];

/// Weight quantization by the block mode's range field; 0, 1, 8, 9 invalid.
const WEIGHTS: [Mode; 16] = [
    m(0, 0, 0), m(0, 0, 0), m(1, 0, 0), m(0, 1, 0), m(2, 0, 0), m(0, 0, 1),
    m(1, 1, 0), m(3, 0, 0), m(0, 0, 0), m(0, 0, 0), m(1, 0, 1), m(2, 1, 0),
    m(4, 0, 0), m(2, 0, 1), m(3, 1, 0), m(5, 0, 0),
];

/// The tables shared by every block size, in the shader's binding order.
pub struct Luts {
    /// Binding 2, R16G16B16A16_UINT: [pairs - 1][remaining bits] to
    /// (bits, trits, quints, unquantize offset).
    pub endpoint_modes: Vec<u16>,
    /// Binding 3, R8_UINT.
    pub endpoint_unquantize: Vec<u8>,
    /// Binding 4, R8G8B8A8_UINT: (bits, trits, quints, unquantize offset).
    pub weight_modes: Vec<u8>,
    /// Binding 5, R8_UINT.
    pub weight_unquantize: Vec<u8>,
    /// Binding 6, R16_UINT: 256 trit then 128 quint blocks.
    pub trits_quints: Vec<u16>,
}

fn value_range(mode: Mode) -> u32 {
    let mut range = 1 << mode.bits;
    if mode.trits != 0 {
        range *= 3;
    }
    if mode.quints != 0 {
        range *= 5;
    }
    if range == 1 { 0 } else { range }
}

fn unquantize_weight(i: u32, mode: Mode) -> u8 {
    let mut v = if mode.trits == 0 && mode.quints == 0 {
        match mode.bits {
            1 => i * 63,
            2 => i * 0x15,
            3 => i * 9,
            4 => (i << 2) | (i >> 2),
            5 => (i << 1) | (i >> 4),
            _ => 0,
        }
    } else if mode.bits == 0 {
        if mode.trits != 0 { 32 * i } else { 16 * i }
    } else {
        let b = (i >> 1) & 1;
        let c = (i >> 2) & 1;
        let a = 0x7f * (i & 1);
        let d = i >> mode.bits;
        let (cs, bs) = if mode.trits != 0 {
            ([50, 23, 11][mode.bits as usize - 1], match mode.bits {
                2 => 0x45 * b,
                3 => 0x21 * b + 0x42 * c,
                _ => 0,
            })
        } else {
            ([28, 13][mode.bits as usize - 1], if mode.bits == 2 { 0x42 * b } else { 0 })
        };
        let unq = (d * cs + bs) ^ a;
        (a & 0x20) | (unq >> 2)
    } & 0xff;
    // Expand [0, 63] to [0, 64].
    if mode.bits != 0 && v > 32 {
        v += 1;
    }
    v as u8
}

fn unquantize_endpoint(i: u32, mode: Mode) -> u8 {
    if mode.trits == 0 && mode.quints == 0 {
        // Bit replication.
        return (match mode.bits {
            1 => i * 0xff,
            2 => i * 0x55,
            3 => (i << 5) | (i << 2) | (i >> 1),
            4 => i * 0x11,
            5 => (i << 3) | (i >> 2),
            6 => (i << 2) | (i >> 4),
            7 => (i << 1) | (i >> 6),
            _ => i,
        }) as u8;
    }
    let bit = |n: u32| (i >> n) & 1;
    let (b, c, d, e, f) = (bit(1), bit(2), bit(3), bit(4), bit(5));
    let a = (i & 1) * 0x1ff;
    let dd = i >> mode.bits;
    let (cs, bs) = if mode.trits != 0 {
        ([204, 93, 44, 22, 11, 5][mode.bits as usize - 1], match mode.bits {
            2 => b * 0x116,
            3 => b * 0x85 + c * 0x10a,
            4 => b * 0x41 + c * 0x82 + d * 0x104,
            5 => b * 0x20 + c * 0x40 + d * 0x81 + e * 0x102,
            6 => b * 0x10 + c * 0x20 + d * 0x40 + e * 0x80 + f * 0x101,
            _ => 0,
        })
    } else {
        ([113, 54, 26, 13, 6][mode.bits as usize - 1], match mode.bits {
            2 => b * 0x10c,
            3 => b * 0x82 + c * 0x105,
            4 => b * 0x40 + c * 0x81 + d * 0x102,
            5 => b * 0x20 + c * 0x40 + d * 0x80 + e * 0x101,
            _ => 0,
        })
    };
    let unq = (dd * cs + bs) ^ a;
    ((a & 0x80) | (unq >> 2)) as u8
}

pub fn luts() -> Luts {
    let mut endpoint_unquantize = Vec::new();
    let mut offsets = [0u16; QUANTIZATION.len()];
    for (i, &mode) in QUANTIZATION.iter().enumerate() {
        offsets[i] = endpoint_unquantize.len() as u16;
        endpoint_unquantize.extend((0..value_range(mode)).map(|v| unquantize_endpoint(v, mode)));
    }

    // At most 9 endpoint pairs: 18 values.
    let mut endpoint_modes = vec![0u16; 9 * 128 * 4];
    for pairs in 1..=9u32 {
        for remaining in 0..128u32 {
            let values = pairs * 2;
            let found = QUANTIZATION.iter().position(|mode| {
                mode.bits * values + (mode.quints * 7 * values + 2) / 3 + (mode.trits * 8 * values + 4) / 5 <= remaining
            });
            if let Some(i) = found {
                let mode = QUANTIZATION[i];
                let at = (((pairs - 1) * 128 + remaining) * 4) as usize;
                endpoint_modes[at..at + 4].copy_from_slice(&[mode.bits as u16, mode.trits as u16, mode.quints as u16, offsets[i]]);
            }
        }
    }

    let mut weight_modes = Vec::with_capacity(WEIGHTS.len() * 4);
    let mut weight_unquantize = Vec::new();
    for &mode in &WEIGHTS {
        weight_modes.extend([mode.bits as u8, mode.trits as u8, mode.quints as u8, weight_unquantize.len() as u8]);
        weight_unquantize.extend((0..value_range(mode)).map(|v| unquantize_weight(v, mode)));
    }
    assert!(weight_unquantize.len() <= 256);

    Luts { endpoint_modes, endpoint_unquantize, weight_modes, weight_unquantize, trits_quints: trits_quints() }
}

/// From the specification.
fn trits_quints() -> Vec<u16> {
    let mut table = vec![0u16; 256 + 128];
    for t in 0..256u32 {
        let (c, t3, t4);
        if (t >> 2) & 7 == 7 {
            c = (((t >> 5) & 7) << 2) | (t & 3);
            t4 = 2;
            t3 = 2;
        } else {
            c = t & 0x1f;
            if (t >> 5) & 3 == 3 {
                t4 = 2;
                t3 = (t >> 7) & 1;
            } else {
                t4 = (t >> 7) & 1;
                t3 = (t >> 5) & 3;
            }
        }
        let (t0, t1, t2);
        if c & 3 == 3 {
            t2 = 2;
            t1 = (c >> 4) & 1;
            t0 = (((c >> 3) & 1) << 1) | (((c >> 2) & 1) & !((c >> 3) & 1));
        } else if (c >> 2) & 3 == 3 {
            t2 = 2;
            t1 = 2;
            t0 = c & 3;
        } else {
            t2 = (c >> 4) & 1;
            t1 = (c >> 2) & 3;
            t0 = (((c >> 1) & 1) << 1) | ((c & 1) & !((c >> 1) & 1));
        }
        table[t as usize] = (t0 | (t1 << 3) | (t2 << 6) | (t3 << 9) | (t4 << 12)) as u16;
    }
    for q in 0..128u32 {
        let (q0, q1, q2);
        if (q >> 1) & 3 == 3 && (q >> 5) & 3 == 0 {
            q2 = ((q & 1) << 2) | ((((q >> 4) & 1) & !(q & 1)) << 1) | (((q >> 3) & 1) & !(q & 1));
            q1 = 4;
            q0 = 4;
        } else {
            let c;
            if (q >> 1) & 3 == 3 {
                q2 = 4;
                c = (((q >> 3) & 3) << 3) | ((!(q >> 5) & 3) << 1) | (q & 1);
            } else {
                q2 = (q >> 5) & 3;
                c = q & 0x1f;
            }
            if c & 7 == 5 {
                q1 = 4;
                q0 = (c >> 3) & 3;
            } else {
                q1 = (c >> 3) & 3;
                q0 = c & 7;
            }
        }
        table[256 + q as usize] = (q0 | (q1 << 3) | (q2 << 6)) as u16;
    }
    table
}

fn hash52(mut p: u32) -> u32 {
    p ^= p >> 15;
    p = p.wrapping_sub(p << 17);
    p = p.wrapping_add(p << 7);
    p = p.wrapping_add(p << 4);
    p ^= p >> 5;
    p = p.wrapping_add(p << 16);
    p ^= p >> 7;
    p ^= p >> 3;
    p ^= p << 6;
    p ^= p >> 17;
    p
}

/// From the specification.
fn select_partition(seed: u32, mut x: u32, mut y: u32, partitions: u32, small: bool) -> u32 {
    if small {
        x <<= 1;
        y <<= 1;
    }
    let seed = seed + (partitions - 1) * 1024;
    let rnum = hash52(seed);
    let nibble = |shift: u32| {
        let s = (rnum >> shift) & 0xf;
        s * s
    };
    let mut s = [
        nibble(0), nibble(4), nibble(8), nibble(12), nibble(16), nibble(20), nibble(24), nibble(28),
        nibble(18), nibble(22), nibble(26), {
            let v = ((rnum >> 30) | (rnum << 2)) & 0xf;
            v * v
        },
    ];
    let (sh1, sh2) = if seed & 1 != 0 {
        (if seed & 2 != 0 { 4 } else { 5 }, if partitions == 3 { 6 } else { 5 })
    } else {
        (if partitions == 3 { 6 } else { 5 }, if seed & 2 != 0 { 4 } else { 5 })
    };
    let sh3 = if seed & 0x10 != 0 { sh1 } else { sh2 };
    for (i, v) in s.iter_mut().enumerate() {
        *v >>= match i {
            0 | 2 | 4 | 6 => sh1,
            1 | 3 | 5 | 7 => sh2,
            _ => sh3,
        };
    }
    // z is 0: 2D blocks only.
    let a = (s[0] * x + s[1] * y + (rnum >> 14)) & 0x3f;
    let b = (s[2] * x + s[3] * y + (rnum >> 10)) & 0x3f;
    let mut c = (s[4] * x + s[5] * y + (rnum >> 6)) & 0x3f;
    let mut d = (s[6] * x + s[7] * y + (rnum >> 2)) & 0x3f;
    if partitions < 4 {
        d = 0;
    }
    if partitions < 3 {
        c = 0;
    }
    if a >= b && a >= c && a >= d {
        0
    } else if b >= c && b >= d {
        1
    } else if c >= d {
        2
    } else {
        3
    }
}

/// Binding 7, R8_UINT: per seed and texel, the 2-, 3- and 4-partition
/// index, (32 x width) by (32 x height) texels.
pub fn partition_table(width: u32, height: u32) -> Vec<u8> {
    let small = width * height < 31;
    let lut_width = width * 32;
    let mut table = vec![0u8; (lut_width * height * 32) as usize];
    for seed_y in 0..32 {
        for seed_x in 0..32 {
            let seed = seed_y * 32 + seed_x;
            for y in 0..height {
                for x in 0..width {
                    let p2 = select_partition(seed, x, y, 2, small);
                    let p3 = select_partition(seed, x, y, 3, small);
                    let p4 = select_partition(seed, x, y, 4, small);
                    table[((seed_y * height + y) * lut_width + seed_x * width + x) as usize] = (p2 | (p3 << 2) | (p4 << 4)) as u8;
                }
            }
        }
    }
    table
}
