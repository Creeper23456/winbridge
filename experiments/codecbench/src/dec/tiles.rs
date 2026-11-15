// SPDX-License-Identifier: GPL-3.0-or-later
//! 64x64 tile geometry and the per-group record layout shared by the raw, LZ4 and zstd paths.
//!
//! Frame payload: `u32 offs[GROUPS + 1]` (byte offsets from the start of the payload), then one record
//! per tile-row group. A full record is just the (compressed) rows of the group. A diff record is
//! `u16 n, u16 idx[n], data` where `idx` is the tile number inside the group (`row * TILES_X + col`)
//! and `data` is the (compressed) concatenation of the changed tiles, each tile row-major, 256 B per row.

use crate::util::*;

pub const HDR_BYTES: usize = (GROUPS + 1) * 4;

pub fn record(payload: &[u8], g: usize) -> &[u8] {
    let o = |i: usize| u32::from_le_bytes(payload[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
    &payload[o(g)..o(g + 1)]
}

/// Splits a diff record into (tile indices, data).
pub fn diff_record(rec: &[u8]) -> (impl Iterator<Item = u16> + '_, &[u8]) {
    let n = u16::from_le_bytes([rec[0], rec[1]]) as usize;
    let idx = rec[2..2 + n * 2].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]));
    (idx, &rec[2 + n * 2..])
}

pub fn diff_count(rec: &[u8]) -> usize {
    u16::from_le_bytes([rec[0], rec[1]]) as usize
}

/// Raw bytes of the tiles listed in `rec`'s index.
pub fn diff_raw_len(rec: &[u8], g: usize) -> usize {
    let ty0 = group_tile_rows(g).0;
    diff_record(rec).0.map(|i| tile_height(ty0 + i as usize / TILES_X) * TILE_ROW_BYTES).sum()
}

/// Copies packed tiles from `src` into the surface.
pub fn scatter(idx: impl Iterator<Item = u16>, src: &[u8], g: usize, dst: Dst) {
    let ty0 = group_tile_rows(g).0;
    let mut off = 0;
    for i in idx {
        let (ty, tx) = (ty0 + i as usize / TILES_X, i as usize % TILES_X);
        let th = tile_height(ty);
        for r in 0..th {
            // SAFETY: the tile lies inside the surface; groups (hence threads) own disjoint rows.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    src.as_ptr().add(off),
                    dst.0.add((ty * TILE + r) * STRIDE + tx * TILE_ROW_BYTES),
                    TILE_ROW_BYTES,
                );
            }
            off += TILE_ROW_BYTES;
        }
    }
}

/// Changed tiles of group `g` between `cur` and `prev` (`None` = all tiles): tile indices and the packed
/// tile bytes. Used by the encoder side and to build the raw-diff baseline in memory.
pub fn pack_group(cur: &[u8], prev: Option<&[u8]>, g: usize) -> (Vec<u16>, Vec<u8>) {
    let (ty0, ty1) = group_tile_rows(g);
    let mut idx = Vec::new();
    let mut data = Vec::new();
    for ty in ty0..ty1 {
        let th = tile_height(ty);
        for tx in 0..TILES_X {
            let off = |r: usize| (ty * TILE + r) * STRIDE + tx * TILE_ROW_BYTES;
            let changed = match prev {
                None => true,
                Some(p) => (0..th).any(|r| cur[off(r)..off(r) + TILE_ROW_BYTES] != p[off(r)..off(r) + TILE_ROW_BYTES]),
            };
            if changed {
                idx.push(((ty - ty0) * TILES_X + tx) as u16);
                for r in 0..th {
                    data.extend_from_slice(&cur[off(r)..off(r) + TILE_ROW_BYTES]);
                }
            }
        }
    }
    (idx, data)
}

/// `u16 n, u16 idx[n]` followed by `data`.
pub fn diff_record_bytes(idx: &[u16], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + idx.len() * 2 + data.len());
    out.extend_from_slice(&(idx.len() as u16).to_le_bytes());
    for i in idx {
        out.extend_from_slice(&i.to_le_bytes());
    }
    out.extend_from_slice(data);
    out
}

/// Prefixes the per-group offset table to the concatenated records.
pub fn assemble(records: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HDR_BYTES + records.iter().map(Vec::len).sum::<usize>());
    let mut off = HDR_BYTES as u32;
    for r in records {
        out.extend_from_slice(&off.to_le_bytes());
        off += r.len() as u32;
    }
    out.extend_from_slice(&off.to_le_bytes());
    for r in records {
        out.extend_from_slice(r);
    }
    out
}
