// SPDX-License-Identifier: GPL-3.0-or-later
//! BC1 and BC7 (mode 6 only) -> BGRA. Own scalar / SSSE3 / SSE2 / AVX2 decoders plus the
//! `texture2ddecoder` crate (generic, all BC7 modes) for comparison. All own variants are
//! bit-identical (checked through the per-frame output hash).

use super::{Decoder, Scratch};
use crate::util::*;
use std::sync::LazyLock;

const BW: usize = W / 4;
const BH: usize = H / 4;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Scalar,
    #[cfg(target_arch = "x86_64")]
    Sse,
    #[cfg(target_arch = "x86_64")]
    Avx2,
    Crate,
}

pub struct Bc {
    seven: bool,
    kind: Kind,
}

pub fn bc1_variants(v: &mut Vec<Box<dyn Decoder>>) {
    v.push(Box::new(Bc { seven: false, kind: Kind::Scalar }));
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("ssse3") {
        v.push(Box::new(Bc { seven: false, kind: Kind::Sse }));
    }
    v.push(Box::new(Bc { seven: false, kind: Kind::Crate }));
}

pub fn bc7_variants(v: &mut Vec<Box<dyn Decoder>>) {
    v.push(Box::new(Bc { seven: true, kind: Kind::Scalar }));
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("sse2") {
            v.push(Box::new(Bc { seven: true, kind: Kind::Sse }));
        }
        if is_x86_feature_detected!("avx2") {
            v.push(Box::new(Bc { seven: true, kind: Kind::Avx2 }));
        }
    }
    v.push(Box::new(Bc { seven: true, kind: Kind::Crate }));
}

// ---------------------------------------------------------------- BC1

const fn pack(b: u32, g: u32, r: u32, a: u32) -> u32 {
    b | g << 8 | r << 16 | a << 24
}

fn expand565(c: u16) -> (u32, u32, u32) {
    let (r, g, b) = ((c >> 11) as u32 & 31, (c >> 5) as u32 & 63, c as u32 & 31);
    (r << 3 | r >> 2, g << 2 | g >> 4, b << 3 | b >> 2)
}

/// BC1 palette in BGRA (D3D rules: c0 <= c1 selects the 3-colour mode with transparent black).
pub fn bc1_palette(blk: &[u8]) -> [u32; 4] {
    let c0 = u16::from_le_bytes([blk[0], blk[1]]);
    let c1 = u16::from_le_bytes([blk[2], blk[3]]);
    let (r0, g0, b0) = expand565(c0);
    let (r1, g1, b1) = expand565(c1);
    let mix = |a: u32, b: u32, wa: u32, wb: u32, d: u32| (a * wa + b * wb) / d;
    let (p2, p3) = if c0 > c1 {
        (
            pack(mix(b0, b1, 2, 1, 3), mix(g0, g1, 2, 1, 3), mix(r0, r1, 2, 1, 3), 255),
            pack(mix(b0, b1, 1, 2, 3), mix(g0, g1, 1, 2, 3), mix(r0, r1, 1, 2, 3), 255),
        )
    } else {
        (pack(mix(b0, b1, 1, 1, 2), mix(g0, g1, 1, 1, 2), mix(r0, r1, 1, 1, 2), 255), 0)
    };
    [pack(b0, g0, r0, 255), pack(b1, g1, r1, 255), p2, p3]
}

/// pshufb masks: byte `b` holds four 2-bit indices; output pixel k = palette[(b >> 2k) & 3].
static SHUF: LazyLock<[[u8; 16]; 256]> = LazyLock::new(|| {
    let mut t = [[0u8; 16]; 256];
    for (b, m) in t.iter_mut().enumerate() {
        for k in 0..4 {
            let idx = (b >> (2 * k)) & 3;
            for c in 0..4 {
                m[k * 4 + c] = (idx * 4 + c) as u8;
            }
        }
    }
    t
});

unsafe fn bc1_band_scalar(src: &[u8], by0: usize, by1: usize, dst: Dst) {
    for by in by0..by1 {
        for bx in 0..BW {
            let blk = &src[(by * BW + bx) * 8..][..8];
            let pal = bc1_palette(blk);
            let bits = u32::from_le_bytes(blk[4..8].try_into().unwrap());
            for r in 0..4 {
                let row = dst.0.add((by * 4 + r) * STRIDE + bx * 16) as *mut u32;
                for c in 0..4 {
                    row.add(c).write_unaligned(pal[(bits >> (2 * (r * 4 + c)) & 3) as usize]);
                }
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn bc1_band_ssse3(src: &[u8], by0: usize, by1: usize, dst: Dst, shuf: &[[u8; 16]; 256]) {
    use std::arch::x86_64::*;
    for by in by0..by1 {
        for bx in 0..BW {
            let blk = &src[(by * BW + bx) * 8..][..8];
            let pal = bc1_palette(blk);
            let palv = _mm_loadu_si128(pal.as_ptr() as *const __m128i);
            let bits = u32::from_le_bytes(blk[4..8].try_into().unwrap());
            for r in 0..4 {
                let m = _mm_loadu_si128(shuf[(bits >> (8 * r)) as usize & 255].as_ptr() as *const __m128i);
                _mm_storeu_si128(dst.0.add((by * 4 + r) * STRIDE + bx * 16) as *mut __m128i, _mm_shuffle_epi8(palv, m));
            }
        }
    }
}

// ---------------------------------------------------------------- BC7 mode 6

const W4: [u32; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

const fn weight_vec8() -> [[i16; 8]; 8] {
    let mut t = [[0i16; 8]; 8];
    let mut k = 0;
    while k < 8 {
        let mut l = 0;
        while l < 8 {
            t[k][l] = W4[2 * k + l / 4] as i16;
            l += 1;
        }
        k += 1;
    }
    t
}
const fn weight_vec16() -> [[i16; 16]; 4] {
    let mut t = [[0i16; 16]; 4];
    let mut k = 0;
    while k < 4 {
        let mut l = 0;
        while l < 16 {
            t[k][l] = W4[4 * k + l / 4] as i16;
            l += 1;
        }
        k += 1;
    }
    t
}
static WV8: [[i16; 8]; 8] = weight_vec8();
static WV16: [[i16; 16]; 4] = weight_vec16();

/// Parsed mode-6 block: endpoints as BGRA bytes and 16 four-bit indices packed one per nibble.
struct M6 {
    e0: u32,
    e1: u32,
    nib: u64,
}

fn parse_m6(blk: &[u8]) -> M6 {
    let v = u128::from_le_bytes(blk.try_into().unwrap());
    debug_assert_eq!(v & 0x7f, 0x40, "not a mode 6 block");
    let f = |i: u32| ((v >> (7 + 7 * i)) & 0x7f) as u32;
    let (p0, p1) = ((v >> 63) as u32 & 1, (v >> 64) as u32 & 1);
    let ep = |r: u32, g: u32, b: u32, a: u32, p: u32| pack(b << 1 | p, g << 1 | p, r << 1 | p, a << 1 | p);
    let ix = (v >> 65) as u64;
    M6 {
        e0: ep(f(0), f(2), f(4), f(6), p0),
        e1: ep(f(1), f(3), f(5), f(7), p1),
        nib: (ix & 7) | ((ix >> 3) << 4),
    }
}

fn pal_entry(e0: u32, e1: u32, w: u32) -> u32 {
    let mut out = 0;
    for c in 0..4 {
        let (a, b) = ((e0 >> (8 * c)) & 255, (e1 >> (8 * c)) & 255);
        out |= ((a * (64 - w) + b * w + 32) >> 6) << (8 * c);
    }
    out
}

unsafe fn bc7_band_scalar(src: &[u8], by0: usize, by1: usize, dst: Dst) {
    for by in by0..by1 {
        for bx in 0..BW {
            let m = parse_m6(&src[(by * BW + bx) * 16..][..16]);
            let mut pal = [0u32; 16];
            for (i, p) in pal.iter_mut().enumerate() {
                *p = pal_entry(m.e0, m.e1, W4[i]);
            }
            for r in 0..4 {
                let row = dst.0.add((by * 4 + r) * STRIDE + bx * 16) as *mut u32;
                for c in 0..4 {
                    row.add(c).write_unaligned(pal[(m.nib >> (4 * (r * 4 + c)) & 15) as usize]);
                }
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn bc7_band_sse2(src: &[u8], by0: usize, by1: usize, dst: Dst) {
    use std::arch::x86_64::*;
    let zero = _mm_setzero_si128();
    let c32 = _mm_set1_epi16(32);
    let c64 = _mm_set1_epi16(64);
    let mut wv = [_mm_setzero_si128(); 8];
    for (k, w) in wv.iter_mut().enumerate() {
        *w = _mm_loadu_si128(WV8[k].as_ptr() as *const __m128i);
    }
    let mut pal = [0u32; 16];
    for by in by0..by1 {
        for bx in 0..BW {
            let m = parse_m6(&src[(by * BW + bx) * 16..][..16]);
            let e0 = _mm_unpacklo_epi8(_mm_cvtsi32_si128(m.e0 as i32), zero);
            let e1 = _mm_unpacklo_epi8(_mm_cvtsi32_si128(m.e1 as i32), zero);
            let e0 = _mm_unpacklo_epi64(e0, e0);
            let e1 = _mm_unpacklo_epi64(e1, e1);
            for (k, w) in wv.iter().enumerate() {
                let s = _mm_add_epi16(_mm_add_epi16(_mm_mullo_epi16(e0, _mm_sub_epi16(c64, *w)), _mm_mullo_epi16(e1, *w)), c32);
                let p = _mm_packus_epi16(_mm_srli_epi16(s, 6), zero);
                _mm_storel_epi64(pal.as_mut_ptr().add(k * 2) as *mut __m128i, p);
            }
            for r in 0..4 {
                let row = dst.0.add((by * 4 + r) * STRIDE + bx * 16) as *mut u32;
                let n = m.nib >> (16 * r);
                row.write_unaligned(*pal.get_unchecked((n & 15) as usize));
                row.add(1).write_unaligned(*pal.get_unchecked((n >> 4 & 15) as usize));
                row.add(2).write_unaligned(*pal.get_unchecked((n >> 8 & 15) as usize));
                row.add(3).write_unaligned(*pal.get_unchecked((n >> 12 & 15) as usize));
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn bc7_band_avx2(src: &[u8], by0: usize, by1: usize, dst: Dst) {
    use std::arch::x86_64::*;
    let c32 = _mm256_set1_epi16(32);
    let c64 = _mm256_set1_epi16(64);
    let shifts = _mm256_setr_epi32(0, 4, 8, 12, 16, 20, 24, 28);
    let mask = _mm256_set1_epi32(15);
    let mut wv = [_mm256_setzero_si256(); 4];
    for (k, w) in wv.iter_mut().enumerate() {
        *w = _mm256_loadu_si256(WV16[k].as_ptr() as *const __m256i);
    }
    for by in by0..by1 {
        for bx in 0..BW {
            let m = parse_m6(&src[(by * BW + bx) * 16..][..16]);
            let e0 = _mm256_cvtepu8_epi16(_mm_set1_epi32(m.e0 as i32));
            let e1 = _mm256_cvtepu8_epi16(_mm_set1_epi32(m.e1 as i32));
            let mut halves = [_mm256_setzero_si256(); 2];
            for (h, half) in halves.iter_mut().enumerate() {
                let mut s = [_mm256_setzero_si256(); 2];
                for (j, sj) in s.iter_mut().enumerate() {
                    let w = wv[h * 2 + j];
                    let t = _mm256_add_epi16(
                        _mm256_add_epi16(_mm256_mullo_epi16(e0, _mm256_sub_epi16(c64, w)), _mm256_mullo_epi16(e1, w)),
                        c32,
                    );
                    *sj = _mm256_srli_epi16(t, 6);
                }
                *half = _mm256_permute4x64_epi64(_mm256_packus_epi16(s[0], s[1]), 0xD8);
            }
            for g in 0..2 {
                let x = (m.nib >> (32 * g)) as u32;
                let idx = _mm256_and_si256(_mm256_srlv_epi32(_mm256_set1_epi32(x as i32), shifts), mask);
                let a = _mm256_permutevar8x32_epi32(halves[0], idx);
                let b = _mm256_permutevar8x32_epi32(halves[1], idx);
                let sel = _mm256_castsi256_ps(_mm256_slli_epi32(idx, 28));
                let res = _mm256_castps_si256(_mm256_blendv_ps(_mm256_castsi256_ps(a), _mm256_castsi256_ps(b), sel));
                let r0 = dst.0.add((by * 4 + g * 2) * STRIDE + bx * 16);
                _mm_storeu_si128(r0 as *mut __m128i, _mm256_castsi256_si128(res));
                _mm_storeu_si128(r0.add(STRIDE) as *mut __m128i, _mm256_extracti128_si256(res, 1));
            }
        }
    }
}

// ---------------------------------------------------------------- dispatch

impl Decoder for Bc {
    fn name(&self) -> String {
        let base = if self.seven { "bc7" } else { "bc1" };
        let suffix = match (self.seven, self.kind) {
            (_, Kind::Scalar) => "scalar",
            #[cfg(target_arch = "x86_64")]
            (false, Kind::Sse) => "ssse3",
            #[cfg(target_arch = "x86_64")]
            (true, Kind::Sse) => "sse2",
            #[cfg(target_arch = "x86_64")]
            (_, Kind::Avx2) => "avx2",
            (_, Kind::Crate) => "t2d",
        };
        format!("{base}-{suffix}")
    }
    fn max_parts(&self) -> usize {
        16
    }
    fn decode(&self, frame: &[u8], part: usize, parts: usize, _: &mut Scratch, dst: Dst) {
        let (by0, by1) = split(BH, part, parts);
        let bsz = if self.seven { 16 } else { 8 };
        // SAFETY: parts own disjoint block rows; `frame` holds BW*BH blocks.
        unsafe {
            match (self.seven, self.kind) {
                (false, Kind::Scalar) => bc1_band_scalar(frame, by0, by1, dst),
                #[cfg(target_arch = "x86_64")]
                (false, Kind::Sse) => bc1_band_ssse3(frame, by0, by1, dst, &SHUF),
                (true, Kind::Scalar) => bc7_band_scalar(frame, by0, by1, dst),
                #[cfg(target_arch = "x86_64")]
                (true, Kind::Sse) => bc7_band_sse2(frame, by0, by1, dst),
                #[cfg(target_arch = "x86_64")]
                (true, Kind::Avx2) => bc7_band_avx2(frame, by0, by1, dst),
                (_, Kind::Crate) => {
                    let rows = dst.rows(by0 * 4, by1 * 4);
                    let pixels = std::slice::from_raw_parts_mut(rows.as_mut_ptr() as *mut u32, rows.len() / 4);
                    let blocks = &frame[by0 * BW * bsz..by1 * BW * bsz];
                    let r = if self.seven {
                        texture2ddecoder::decode_bc7(blocks, W, (by1 - by0) * 4, pixels)
                    } else {
                        texture2ddecoder::decode_bc1(blocks, W, (by1 - by0) * 4, pixels)
                    };
                    r.expect("texture2ddecoder");
                }
                #[cfg(target_arch = "x86_64")]
                (false, Kind::Avx2) => unreachable!(),
            }
        }
    }
}
