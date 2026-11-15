// SPDX-License-Identifier: GPL-3.0-or-later
//! MJPEG decode with zune-jpeg straight into the BGRA surface. A frame is either one JPEG or
//! `GROUPS` independent horizontal strips (so strips can be decoded on separate threads).

use super::tiles::record;
use super::{Decoder, Scratch};
use crate::util::*;
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

pub struct Mjpeg {
    pub strips: bool,
}

fn decode_jpeg(data: &[u8], out: &mut [u8]) {
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::BGRA);
    let mut d = JpegDecoder::new_with_options(ZCursor::new(data), opts);
    d.decode_into(out).expect("jpeg decode");
}

impl Decoder for Mjpeg {
    fn name(&self) -> String {
        "zune".into()
    }
    fn max_parts(&self) -> usize {
        if self.strips {
            GROUPS
        } else {
            1
        }
    }
    fn decode(&self, frame: &[u8], part: usize, parts: usize, _: &mut Scratch, dst: Dst) {
        if !self.strips {
            // SAFETY: a single part owns the whole surface.
            decode_jpeg(frame, unsafe { dst.rows(0, H) });
            return;
        }
        let (a, b) = split(GROUPS, part, parts);
        for g in a..b {
            let (y0, y1) = group_rows(g);
            // SAFETY: strips own disjoint row ranges.
            decode_jpeg(record(frame, g), unsafe { dst.rows(y0, y1) });
        }
    }
}
