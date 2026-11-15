// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared constants, output-buffer handle, hashing, statistics and CLI helpers.

use std::collections::HashMap;

pub const W: usize = 1920;
pub const H: usize = 1080;
pub const STRIDE: usize = W * 4;
pub const FRAME_BYTES: usize = STRIDE * H;

pub const TILE: usize = 64;
pub const TILES_X: usize = W / TILE;
#[cfg(feature = "gen")]
pub const TILES_Y: usize = H.div_ceil(TILE);
pub const TILE_ROW_BYTES: usize = TILE * 4;

/// Independent slices of a frame (tile-row groups): the unit of parallelism for entropy-coded streams.
pub const GROUPS: usize = 8;
const GROUP_BOUNDS: [usize; GROUPS + 1] = [0, 2, 4, 6, 9, 11, 13, 15, 17];

/// Tile-row range `[ty0, ty1)` of group `g`.
pub fn group_tile_rows(g: usize) -> (usize, usize) {
    (GROUP_BOUNDS[g], GROUP_BOUNDS[g + 1])
}

/// Pixel-row range `[y0, y1)` of group `g`.
pub fn group_rows(g: usize) -> (usize, usize) {
    let (a, b) = group_tile_rows(g);
    (a * TILE, (b * TILE).min(H))
}

/// Height in pixel rows of tile row `ty`.
pub fn tile_height(ty: usize) -> usize {
    TILE.min(H - ty * TILE)
}

/// Splits `n` items into `parts` contiguous ranges and returns the range of `part`.
pub fn split(n: usize, part: usize, parts: usize) -> (usize, usize) {
    (n * part / parts, n * (part + 1) / parts)
}

/// Raw pointer to the (DIB or Vec) BGRA output surface. Threads write disjoint row bands.
#[derive(Clone, Copy)]
pub struct Dst(pub *mut u8);
unsafe impl Send for Dst {}
unsafe impl Sync for Dst {}

impl Dst {
    /// # Safety
    /// Rows `[y0, y1)` must be inside the surface and not aliased by another live slice.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn rows(&self, y0: usize, y1: usize) -> &mut [u8] {
        std::slice::from_raw_parts_mut(self.0.add(y0 * STRIDE), (y1 - y0) * STRIDE)
    }
}

/// Fast 4-lane multiply-xor hash of a buffer (verification only, never timed).
pub fn hash_bytes(data: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut lanes = [0x1234_5678_9ABC_DEF0u64; 4];
    let mut chunks = data.chunks_exact(32);
    for c in &mut chunks {
        for (l, lane) in lanes.iter_mut().enumerate() {
            let w = u64::from_le_bytes(c[l * 8..l * 8 + 8].try_into().unwrap());
            *lane = (*lane ^ w).wrapping_mul(K).rotate_left(29);
        }
    }
    let mut h = 0u64;
    for l in lanes {
        h = (h ^ l).wrapping_mul(K).rotate_left(31);
    }
    for &b in chunks.remainder() {
        h = (h ^ b as u64).wrapping_mul(K);
    }
    h
}

/// Summary of a set of samples (nanoseconds or any unit).
pub struct Summary {
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
    pub mean: f64,
}

pub fn summarize(samples: &mut [f64]) -> Summary {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = samples.len();
    let pick = |q: f64| samples[(((n - 1) as f64) * q).round() as usize];
    Summary {
        p50: pick(0.5),
        p95: pick(0.95),
        max: samples[n - 1],
        mean: samples.iter().sum::<f64>() / n as f64,
    }
}

/// Minimal `--key value` / `--flag` argument parser.
pub struct Args(HashMap<String, String>);

impl Args {
    pub fn parse(args: &[String]) -> Args {
        let mut m = HashMap::new();
        let mut i = 0;
        while i < args.len() {
            if let Some(k) = args[i].strip_prefix("--") {
                if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                    m.insert(k.to_string(), args[i + 1].clone());
                    i += 1;
                } else {
                    m.insert(k.to_string(), String::new());
                }
            }
            i += 1;
        }
        Args(m)
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.0.get(k).map(String::as_str)
    }

    pub fn list(&self, k: &str) -> Option<Vec<String>> {
        self.get(k)
            .map(|v| v.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect())
    }

    pub fn num(&self, k: &str, default: usize) -> usize {
        self.get(k).map_or(default, |v| v.parse().unwrap_or_else(|_| die(&format!("--{k}: not a number"))))
    }
}

pub fn die(msg: &str) -> ! {
    eprintln!("codecbench: {msg}");
    std::process::exit(2)
}

pub fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Extracts a scalar field from one of our flat JSON lines.
pub fn json_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":");
    let start = line.find(&pat)? + pat.len();
    let rest = &line[start..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim().trim_matches('"'))
}
