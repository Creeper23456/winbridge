// SPDX-License-Identifier: GPL-3.0-or-later
//! Raw copy baselines and the LZ4 / zstd lossless decoders (full-frame and changed-tile variants).

use super::tiles::*;
use super::{Decoder, Scratch};
use crate::util::*;

/// memcpy of a whole raw BGRA frame into the surface, split by rows.
pub struct RawFull;

impl Decoder for RawFull {
    fn name(&self) -> String {
        "raw-full".into()
    }
    fn max_parts(&self) -> usize {
        16
    }
    fn decode(&self, frame: &[u8], part: usize, parts: usize, _: &mut Scratch, dst: Dst) {
        let (a, b) = split(H, part, parts);
        // SAFETY: parts own disjoint row ranges.
        unsafe { dst.rows(a, b).copy_from_slice(&frame[a * STRIDE..b * STRIDE]) }
    }
}

/// Copy of the changed 64x64 tiles only (records hold uncompressed packed tiles).
pub struct RawDiff;

impl Decoder for RawDiff {
    fn name(&self) -> String {
        "raw-diff".into()
    }
    fn decode(&self, frame: &[u8], part: usize, parts: usize, _: &mut Scratch, dst: Dst) {
        let (a, b) = split(GROUPS, part, parts);
        for g in a..b {
            let rec = record(frame, g);
            let (idx, data) = diff_record(rec);
            scatter(idx, data, g, dst);
        }
    }
}

pub struct Lossless {
    pub zstd: bool,
    pub diff: bool,
}

impl Lossless {
    fn inflate(&self, src: &[u8], out: &mut [u8], scratch_zstd: &mut zstd::bulk::Decompressor<'static>) {
        let n = if self.zstd {
            scratch_zstd.decompress_to_buffer(src, out).expect("zstd")
        } else {
            lz4_flex::block::decompress_into(src, out).expect("lz4")
        };
        assert_eq!(n, out.len(), "short decompress");
    }
}

impl Decoder for Lossless {
    fn name(&self) -> String {
        format!("{}-{}", if self.zstd { "zstd" } else { "lz4" }, if self.diff { "diff" } else { "full" })
    }
    fn decode(&self, frame: &[u8], part: usize, parts: usize, scratch: &mut Scratch, dst: Dst) {
        let (a, b) = split(GROUPS, part, parts);
        for g in a..b {
            let rec = record(frame, g);
            if self.diff {
                if diff_count(rec) == 0 {
                    continue;
                }
                let (idx, comp) = diff_record(rec);
                let raw_len = diff_raw_len(rec, g);
                let Scratch { buf, zstd } = scratch;
                self.inflate(comp, &mut buf[..raw_len], zstd);
                scatter(idx, &buf[..raw_len], g, dst);
            } else {
                let (y0, y1) = group_rows(g);
                // SAFETY: groups own disjoint row ranges; we decompress straight into the surface.
                let out = unsafe { dst.rows(y0, y1) };
                self.inflate(rec, out, &mut scratch.zstd);
            }
        }
    }
}
