// SPDX-License-Identifier: GPL-3.0-or-later
//! Host-side encoders (CPU stand-ins for the planned GPU compute-shader encoder).

use crate::dec::tiles::{assemble, diff_record_bytes, pack_group};
use crate::util::*;

fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// BGRA -> NV12, BT.709 limited range, 2x2 box-filtered chroma.
pub fn nv12(frame: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; W * H * 3 / 2];
    let (yp, uvp) = out.split_at_mut(W * H);
    for y in 0..H {
        for x in 0..W {
            let p = &frame[y * STRIDE + x * 4..];
            let (b, g, r) = (p[0] as i32, p[1] as i32, p[2] as i32);
            yp[y * W + x] = clamp8(((47 * r + 157 * g + 16 * b + 128) >> 8) + 16);
        }
    }
    for y in 0..H / 2 {
        for x in 0..W / 2 {
            let (mut sb, mut sg, mut sr) = (2, 2, 2);
            for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let p = &frame[(y * 2 + dy) * STRIDE + (x * 2 + dx) * 4..];
                sb += p[0] as i32;
                sg += p[1] as i32;
                sr += p[2] as i32;
            }
            let (b, g, r) = (sb >> 2, sg >> 2, sr >> 2);
            uvp[y * W + x * 2] = clamp8(((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128);
            uvp[y * W + x * 2 + 1] = clamp8(((112 * r - 102 * g - 10 * b + 128) >> 8) + 128);
        }
    }
    out
}

fn bgra_to_rgba(frame: &[u8]) -> Vec<u8> {
    let mut v = frame.to_vec();
    for p in v.chunks_exact_mut(4) {
        p.swap(0, 2);
    }
    v
}

/// BC1 with texpresso (cluster fit, uniform RGB weights).
pub fn bc1(frame: &[u8]) -> Vec<u8> {
    use texpresso::{Algorithm, Format, Params, COLOUR_WEIGHTS_UNIFORM};
    let rgba = bgra_to_rgba(frame);
    let mut out = vec![0u8; Format::Bc1.compressed_size(W, H)];
    let params = Params { algorithm: Algorithm::ClusterFit, weights: COLOUR_WEIGHTS_UNIFORM, weigh_colour_by_alpha: false };
    Format::Bc1.compress(&rgba, W, H, params, &mut out);
    out
}

// ------------------------------------------------------------------ BC7 mode 6

const W4: [i32; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

fn q7(v: f32) -> u8 {
    ((v - 1.0) * 0.5).round().clamp(0.0, 127.0) as u8
}

fn palette(q0: &[u8; 3], q1: &[u8; 3]) -> [[i32; 3]; 16] {
    let mut pal = [[0i32; 3]; 16];
    for (i, p) in pal.iter_mut().enumerate() {
        for c in 0..3 {
            let (a, b) = (q0[c] as i32 * 2 + 1, q1[c] as i32 * 2 + 1);
            p[c] = (a * (64 - W4[i]) + b * W4[i] + 32) >> 6;
        }
    }
    pal
}

/// Picks indices for the given endpoints; returns (indices, squared error).
fn assign(px: &[[f32; 3]; 16], q0: &[u8; 3], q1: &[u8; 3]) -> ([u8; 16], f32) {
    let pal = palette(q0, q1);
    let e0 = [q0[0] as f32 * 2.0 + 1.0, q0[1] as f32 * 2.0 + 1.0, q0[2] as f32 * 2.0 + 1.0];
    let d = [q1[0] as f32 * 2.0 + 1.0 - e0[0], q1[1] as f32 * 2.0 + 1.0 - e0[1], q1[2] as f32 * 2.0 + 1.0 - e0[2]];
    let dd = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
    let mut idx = [0u8; 16];
    let mut err = 0.0;
    for (k, p) in px.iter().enumerate() {
        let t = if dd > 0.0 { ((p[0] - e0[0]) * d[0] + (p[1] - e0[1]) * d[1] + (p[2] - e0[2]) * d[2]) / dd } else { 0.0 };
        let c = (t * 15.0).round().clamp(0.0, 15.0) as i32;
        let mut best = (f32::MAX, 0i32);
        for i in (c - 1).max(0)..=(c + 1).min(15) {
            let q = pal[i as usize];
            let e = (0..3).map(|ch| (p[ch] - q[ch] as f32).powi(2)).sum::<f32>();
            if e < best.0 {
                best = (e, i);
            }
        }
        idx[k] = best.1 as u8;
        err += best.0;
    }
    (idx, err)
}

fn encode_block_m6(px: &[[f32; 3]; 16]) -> [u8; 16] {
    let mut mean = [0f32; 3];
    for p in px {
        for c in 0..3 {
            mean[c] += p[c] / 16.0;
        }
    }
    let mut cov = [[0f32; 3]; 3];
    for p in px {
        for i in 0..3 {
            for j in 0..3 {
                cov[i][j] += (p[i] - mean[i]) * (p[j] - mean[j]);
            }
        }
    }
    let mut axis = [0.577f32; 3];
    for _ in 0..4 {
        let n = [0, 1, 2].map(|i| cov[i][0] * axis[0] + cov[i][1] * axis[1] + cov[i][2] * axis[2]);
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        if len < 1e-6 {
            break;
        }
        axis = n.map(|v| v / len);
    }
    let (mut tmin, mut tmax) = (f32::MAX, f32::MIN);
    for p in px {
        let t = (0..3).map(|c| (p[c] - mean[c]) * axis[c]).sum::<f32>();
        tmin = tmin.min(t);
        tmax = tmax.max(t);
    }
    let mut p0 = [0, 1, 2].map(|c| mean[c] + axis[c] * tmin);
    let mut p1 = [0, 1, 2].map(|c| mean[c] + axis[c] * tmax);
    let mut best: Option<([u8; 3], [u8; 3], [u8; 16], f32)> = None;
    for iter in 0..3 {
        let q0 = p0.map(q7);
        let q1 = p1.map(q7);
        let (idx, err) = assign(px, &q0, &q1);
        if best.as_ref().is_none_or(|b| err < b.3) {
            best = Some((q0, q1, idx, err));
        }
        if iter == 2 {
            break;
        }
        // Least-squares endpoint refit for the current indices.
        let (mut a00, mut a01, mut a11) = (0f32, 0f32, 0f32);
        let (mut b0, mut b1) = ([0f32; 3], [0f32; 3]);
        for (k, p) in px.iter().enumerate() {
            let w = W4[idx[k] as usize] as f32 / 64.0;
            let a = 1.0 - w;
            a00 += a * a;
            a01 += a * w;
            a11 += w * w;
            for c in 0..3 {
                b0[c] += a * p[c];
                b1[c] += w * p[c];
            }
        }
        let det = a00 * a11 - a01 * a01;
        if det.abs() < 1e-4 {
            break;
        }
        for c in 0..3 {
            p0[c] = (b0[c] * a11 - b1[c] * a01) / det;
            p1[c] = (a00 * b1[c] - a01 * b0[c]) / det;
        }
    }
    let (mut q0, mut q1, mut idx, _) = best.unwrap();
    if idx[0] >= 8 {
        std::mem::swap(&mut q0, &mut q1);
        for i in &mut idx {
            *i = 15 - *i;
        }
    }
    let mut v: u128 = 0x40;
    let mut put = |pos: u32, bits: u32, val: u128| v |= (val & ((1u128 << bits) - 1)) << pos;
    for c in 0..3 {
        put(7 + 14 * c as u32, 7, q0[c] as u128);
        put(14 + 14 * c as u32, 7, q1[c] as u128);
    }
    put(49, 7, 127);
    put(56, 7, 127);
    put(63, 1, 1);
    put(64, 1, 1);
    put(65, 3, idx[0] as u128);
    for (i, &x) in idx.iter().enumerate().skip(1) {
        put(64 + 4 * i as u32, 4, x as u128);
    }
    v.to_le_bytes()
}

/// BC7 using only mode 6 (single subset, 7.7.7.7 + shared-LSB p-bits forced to 1 for opaque alpha, 4-bit indices).
pub fn bc7(frame: &[u8]) -> Vec<u8> {
    let (bw, bh) = (W / 4, H / 4);
    let mut out = Vec::with_capacity(bw * bh * 16);
    for by in 0..bh {
        for bx in 0..bw {
            let mut px = [[0f32; 3]; 16];
            for (k, p) in px.iter_mut().enumerate() {
                let o = (by * 4 + k / 4) * STRIDE + (bx * 4 + k % 4) * 4;
                *p = [frame[o + 2] as f32, frame[o + 1] as f32, frame[o] as f32];
            }
            out.extend_from_slice(&encode_block_m6(&px));
        }
    }
    out
}

// ------------------------------------------------------------------ lossless

fn compress(data: &[u8], zstd_level: Option<i32>) -> Vec<u8> {
    match zstd_level {
        Some(l) => zstd::bulk::compress(data, l).expect("zstd"),
        None => lz4_flex::block::compress(data),
    }
}

/// LZ4 (`zstd_level = None`) or zstd, per tile-row group, full rows or changed tiles only.
pub fn lossless(frame: &[u8], prev: Option<&[u8]>, zstd_level: Option<i32>, diff: bool) -> Vec<u8> {
    let recs: Vec<Vec<u8>> = (0..GROUPS)
        .map(|g| {
            if diff {
                let (idx, data) = pack_group(frame, prev, g);
                let comp = if idx.is_empty() { Vec::new() } else { compress(&data, zstd_level) };
                diff_record_bytes(&idx, &comp)
            } else {
                let (y0, y1) = group_rows(g);
                compress(&frame[y0 * STRIDE..y1 * STRIDE], zstd_level)
            }
        })
        .collect();
    assemble(&recs)
}

/// Uncompressed changed-tile records (the raw-diff baseline's wire size).
pub fn raw_diff(frame: &[u8], prev: Option<&[u8]>) -> Vec<u8> {
    let recs: Vec<Vec<u8>> = (0..GROUPS)
        .map(|g| {
            let (idx, data) = pack_group(frame, prev, g);
            diff_record_bytes(&idx, &data)
        })
        .collect();
    assemble(&recs)
}

// ------------------------------------------------------------------ MJPEG

fn jpeg(rows: &[u8], h: usize, sub420: bool) -> Vec<u8> {
    use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
    let mut out = Vec::new();
    let mut e = Encoder::new(&mut out, 90);
    e.set_sampling_factor(if sub420 { SamplingFactor::F_2_2 } else { SamplingFactor::F_1_1 });
    e.encode(rows, W as u16, h as u16, ColorType::Bgra).expect("jpeg encode");
    out
}

pub fn mjpeg(frame: &[u8], sub420: bool, strips: bool) -> Vec<u8> {
    if !strips {
        return jpeg(frame, H, sub420);
    }
    let recs: Vec<Vec<u8>> = (0..GROUPS)
        .map(|g| {
            let (y0, y1) = group_rows(g);
            jpeg(&frame[y0 * STRIDE..y1 * STRIDE], y1 - y0, sub420)
        })
        .collect();
    assemble(&recs)
}
