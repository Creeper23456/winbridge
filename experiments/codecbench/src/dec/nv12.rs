// SPDX-License-Identifier: GPL-3.0-or-later
//! NV12 (BT.709, limited range, 4:2:0 nearest chroma upsampling) -> BGRA.
//!
//! One integer formula shared by all variants so SIMD output is bit-identical to the scalar one:
//!   yy = (max(y - 16, 0) * 298) >> 8
//!   r = yy + ((v' * 459) >> 8) + 1,   b = yy + ((u' * 541) >> 8) + 1,
//!   g = yy + ((u' * -55) >> 8) + ((v' * -136) >> 8) + 1,   u' = u - 128, v' = v - 128; clamp to 0..255.
//! (>> is an arithmetic shift, i.e. what `pmulhw` computes on value << 8.)
//! Layout: Y plane (W x H) followed by the interleaved UV plane (W x H/2).

use super::{Decoder, Scratch};
use crate::util::*;

#[derive(Clone, Copy)]
enum Kind {
    Scalar,
    #[cfg(target_arch = "x86_64")]
    Sse2,
    #[cfg(target_arch = "x86_64")]
    Avx2,
}

pub struct Nv12 {
    kind: Kind,
}

pub fn variants(v: &mut Vec<Box<dyn Decoder>>) {
    v.push(Box::new(Nv12 { kind: Kind::Scalar }));
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("sse2") {
            v.push(Box::new(Nv12 { kind: Kind::Sse2 }));
        }
        if is_x86_feature_detected!("avx2") {
            v.push(Box::new(Nv12 { kind: Kind::Avx2 }));
        }
    }
}

fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

fn row_scalar(y: &[u8], uv: &[u8], out: &mut [u8]) {
    for x in 0..W {
        let yy = ((y[x].saturating_sub(16) as i32) * 298) >> 8;
        let u = uv[x & !1] as i32 - 128;
        let v = uv[x | 1] as i32 - 128;
        out[x * 4] = clamp8(yy + ((u * 541) >> 8) + 1);
        out[x * 4 + 1] = clamp8(yy + ((u * -55) >> 8) + ((v * -136) >> 8) + 1);
        out[x * 4 + 2] = clamp8(yy + ((v * 459) >> 8) + 1);
        out[x * 4 + 3] = 255;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn row_sse2(y: *const u8, uv: *const u8, out: *mut u8) {
    use std::arch::x86_64::*;
    let zero = _mm_setzero_si128();
    let c16 = _mm_set1_epi16(16);
    let c128 = _mm_set1_epi16(128);
    let one = _mm_set1_epi16(1);
    let alpha = _mm_set1_epi16(255);
    for x in (0..W).step_by(8) {
        let y16 = _mm_unpacklo_epi8(_mm_loadl_epi64(y.add(x) as *const __m128i), zero);
        let yy = _mm_add_epi16(_mm_mulhi_epu16(_mm_slli_epi16(_mm_subs_epu16(y16, c16), 8), _mm_set1_epi16(298)), one);
        let uvw = _mm_sub_epi16(_mm_unpacklo_epi8(_mm_cvtsi64_si128((uv.add(x) as *const i64).read_unaligned()), zero), c128);
        let u = _mm_slli_epi16(_mm_shufflehi_epi16(_mm_shufflelo_epi16(uvw, 0xA0), 0xA0), 8);
        let v = _mm_slli_epi16(_mm_shufflehi_epi16(_mm_shufflelo_epi16(uvw, 0xF5), 0xF5), 8);
        let r = _mm_add_epi16(yy, _mm_mulhi_epi16(v, _mm_set1_epi16(459)));
        let b = _mm_add_epi16(yy, _mm_mulhi_epi16(u, _mm_set1_epi16(541)));
        let g = _mm_add_epi16(
            yy,
            _mm_add_epi16(_mm_mulhi_epi16(u, _mm_set1_epi16(-55)), _mm_mulhi_epi16(v, _mm_set1_epi16(-136))),
        );
        let pb = _mm_packus_epi16(b, r);
        let pg = _mm_packus_epi16(g, alpha);
        let bg = _mm_unpacklo_epi8(pb, pg);
        let ra = _mm_unpackhi_epi8(pb, pg);
        _mm_storeu_si128(out.add(x * 4) as *mut __m128i, _mm_unpacklo_epi16(bg, ra));
        _mm_storeu_si128(out.add(x * 4 + 16) as *mut __m128i, _mm_unpackhi_epi16(bg, ra));
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn row_avx2(y: *const u8, uv: *const u8, out: *mut u8) {
    use std::arch::x86_64::*;
    let c16 = _mm256_set1_epi16(16);
    let c128 = _mm256_set1_epi16(128);
    let one = _mm256_set1_epi16(1);
    let alpha = _mm256_set1_epi16(255);
    for x in (0..W).step_by(16) {
        let y16 = _mm256_cvtepu8_epi16(_mm_loadu_si128(y.add(x) as *const __m128i));
        let yy = _mm256_add_epi16(
            _mm256_mulhi_epu16(_mm256_slli_epi16(_mm256_subs_epu16(y16, c16), 8), _mm256_set1_epi16(298)),
            one,
        );
        let uvw = _mm256_sub_epi16(_mm256_cvtepu8_epi16(_mm_loadu_si128(uv.add(x) as *const __m128i)), c128);
        let u = _mm256_slli_epi16(_mm256_shufflehi_epi16(_mm256_shufflelo_epi16(uvw, 0xA0), 0xA0), 8);
        let v = _mm256_slli_epi16(_mm256_shufflehi_epi16(_mm256_shufflelo_epi16(uvw, 0xF5), 0xF5), 8);
        let r = _mm256_add_epi16(yy, _mm256_mulhi_epi16(v, _mm256_set1_epi16(459)));
        let b = _mm256_add_epi16(yy, _mm256_mulhi_epi16(u, _mm256_set1_epi16(541)));
        let g = _mm256_add_epi16(
            yy,
            _mm256_add_epi16(
                _mm256_mulhi_epi16(u, _mm256_set1_epi16(-55)),
                _mm256_mulhi_epi16(v, _mm256_set1_epi16(-136)),
            ),
        );
        let pb = _mm256_packus_epi16(b, r);
        let pg = _mm256_packus_epi16(g, alpha);
        let bg = _mm256_unpacklo_epi8(pb, pg);
        let ra = _mm256_unpackhi_epi8(pb, pg);
        let lo = _mm256_unpacklo_epi16(bg, ra);
        let hi = _mm256_unpackhi_epi16(bg, ra);
        _mm256_storeu_si256(out.add(x * 4) as *mut __m256i, _mm256_permute2x128_si256(lo, hi, 0x20));
        _mm256_storeu_si256(out.add(x * 4 + 32) as *mut __m256i, _mm256_permute2x128_si256(lo, hi, 0x31));
    }
}

impl Decoder for Nv12 {
    fn name(&self) -> String {
        match self.kind {
            Kind::Scalar => "nv12-scalar",
            #[cfg(target_arch = "x86_64")]
            Kind::Sse2 => "nv12-sse2",
            #[cfg(target_arch = "x86_64")]
            Kind::Avx2 => "nv12-avx2",
        }
        .into()
    }
    fn max_parts(&self) -> usize {
        16
    }
    fn decode(&self, frame: &[u8], part: usize, parts: usize, _: &mut Scratch, dst: Dst) {
        let (p0, p1) = split(H / 2, part, parts);
        let uv_plane = &frame[W * H..];
        for y in p0 * 2..p1 * 2 {
            let yrow = &frame[y * W..(y + 1) * W];
            let uvrow = &uv_plane[(y / 2) * W..(y / 2 + 1) * W];
            // SAFETY: parts own disjoint rows; row slices are W (resp. STRIDE) bytes.
            let out = unsafe { dst.rows(y, y + 1) };
            match self.kind {
                Kind::Scalar => row_scalar(yrow, uvrow, out),
                #[cfg(target_arch = "x86_64")]
                Kind::Sse2 => unsafe { row_sse2(yrow.as_ptr(), uvrow.as_ptr(), out.as_mut_ptr()) },
                #[cfg(target_arch = "x86_64")]
                Kind::Avx2 => unsafe { row_avx2(yrow.as_ptr(), uvrow.as_ptr(), out.as_mut_ptr()) },
            }
        }
    }
}
