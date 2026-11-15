// SPDX-License-Identifier: GPL-3.0-or-later
//! Decoders: every one writes BGRA into a pre-allocated 1080p surface, optionally split into parts that
//! run on separate threads.

pub mod bc;
pub mod jpeg;
pub mod lossless;
pub mod nv12;
pub mod tiles;

use crate::util::Dst;

/// Per-thread reusable state (decompression contexts, tile staging buffer).
pub struct Scratch {
    pub buf: Vec<u8>,
    pub zstd: zstd::bulk::Decompressor<'static>,
}

impl Scratch {
    pub fn new() -> Scratch {
        Scratch {
            // Largest tile group is 3 tile rows x 30 tiles x 16 KiB.
            buf: vec![0u8; 3 * crate::util::TILES_X * 64 * 64 * 4],
            zstd: zstd::bulk::Decompressor::new().expect("zstd context"),
        }
    }
}

pub trait Decoder: Sync {
    fn name(&self) -> String;
    /// Largest number of threads that can decode one frame concurrently.
    fn max_parts(&self) -> usize {
        crate::util::GROUPS
    }
    /// Decodes slice `part` of `parts` of `frame` into `dst`.
    fn decode(&self, frame: &[u8], part: usize, parts: usize, scratch: &mut Scratch, dst: Dst);
}

/// Stream codec names written by `gen` (each is one `<class>.<codec>.cbs` file).
pub const STREAM_CODECS: &[&str] = &[
    "nv12", "bc1", "bc7", "lz4-full", "lz4-diff", "zstd1-full", "zstd1-diff", "zstd3-full", "zstd3-diff",
    "mjpeg444", "mjpeg420", "mjpeg444s8", "mjpeg420s8",
];

/// Decoder variants that apply to a stream codec on this CPU (runtime feature detection).
pub fn decoders_for(codec: &str) -> Vec<Box<dyn Decoder>> {
    let mut v: Vec<Box<dyn Decoder>> = Vec::new();
    match codec {
        "nv12" => nv12::variants(&mut v),
        "bc1" => bc::bc1_variants(&mut v),
        "bc7" => bc::bc7_variants(&mut v),
        c if c.starts_with("lz4-") => v.push(Box::new(lossless::Lossless { zstd: false, diff: c.ends_with("diff") })),
        c if c.starts_with("zstd") => v.push(Box::new(lossless::Lossless { zstd: true, diff: c.ends_with("diff") })),
        c if c.starts_with("mjpeg") => v.push(Box::new(jpeg::Mjpeg { strips: c.ends_with("s8") })),
        _ => {}
    }
    v
}
