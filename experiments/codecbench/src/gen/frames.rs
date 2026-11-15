// SPDX-License-Identifier: GPL-3.0-or-later
//! Deterministic 1920x1080 BGRA frame synthesis for the five frame classes.
//!
//! ui       static desktop screenshot (tiled to 1080p) with an 800x500 window scrolling guest-screenshot
//!          text 3 px/frame and a 64x64 status tile that changes every 4 frames (~25 % of tiles change).
//! uiscroll the whole screen scrolls a stack of guest screenshots 5 px/frame (worst case for tile diff).
//! photo    wallpapers scaled up and panned/cut every 60 frames, plus +-1 LSB sensor grain on every pixel.
//! game     procedural: moving gradients, hard-edged circles/rectangles, glyph-like HUD, +-4 noise per pixel.
//! mixed    ui layout (660x700 scrolling text window) plus a 960x540 `game` video rectangle.

use crate::util::*;

pub struct Img {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

fn load(path: &str) -> Img {
    let im = image::open(path).unwrap_or_else(|e| die(&format!("{path}: {e}"))).to_rgba8();
    let (w, h) = (im.width() as usize, im.height() as usize);
    let mut px = im.into_raw();
    for p in px.chunks_exact_mut(4) {
        p.swap(0, 2);
        p[3] = 255;
    }
    Img { w, h, px }
}

/// Nearest-neighbour tiling of `img` over a 1920x1080 canvas (keeps text pixel-sharp).
fn tiled(img: &Img) -> Vec<u8> {
    let mut out = vec![0u8; FRAME_BYTES];
    for y in 0..H {
        for x in 0..W {
            let s = ((y % img.h) * img.w + x % img.w) * 4;
            out[y * STRIDE + x * 4..y * STRIDE + x * 4 + 4].copy_from_slice(&img.px[s..s + 4]);
        }
    }
    out
}

pub struct Source {
    base: Vec<u8>,
    doc: Vec<u8>,
    doc_h: usize,
    walls: Vec<Img>,
}

const WALL_W: usize = 2400;
const WALL_H: usize = 1350;

impl Source {
    pub fn new(shots: &[String], walls: &[String]) -> Source {
        if shots.len() < 2 || walls.is_empty() {
            die("--shots needs >= 2 images and --walls >= 1 image");
        }
        let base = tiled(&load(&shots[0]));
        let mut doc = Vec::new();
        for s in &shots[1..] {
            doc.extend_from_slice(&tiled(&load(s)));
        }
        let doc_h = doc.len() / STRIDE;
        let walls = walls
            .iter()
            .map(|p| {
                let im = load(p);
                // The loader swapped to BGRA; resize on a plain RGBA view (channel order is irrelevant to the filter).
                let buf = image::RgbaImage::from_raw(im.w as u32, im.h as u32, im.px).unwrap();
                let r = image::imageops::resize(&buf, WALL_W as u32, WALL_H as u32, image::imageops::FilterType::Lanczos3);
                Img { w: WALL_W, h: WALL_H, px: r.into_raw() }
            })
            .collect();
        Source { base, doc, doc_h, walls }
    }

    pub fn render(&self, class: &str, i: usize, out: &mut [u8]) {
        match class {
            "ui" => self.ui(i, out),
            "uiscroll" => self.uiscroll(i, out),
            "photo" => self.photo(i, out),
            "game" => game(i, out),
            "mixed" => self.mixed(i, out),
            _ => die(&format!("unknown class {class}")),
        }
    }

    fn scroll_window(&self, i: usize, speed: usize, rect: (usize, usize, usize, usize), out: &mut [u8]) {
        let (x0, y0, w, h) = rect;
        for r in 0..h {
            let s = ((i * speed + r) % self.doc_h) * STRIDE + x0 * 4;
            let d = (y0 + r) * STRIDE + x0 * 4;
            out[d..d + w * 4].copy_from_slice(&self.doc[s..s + w * 4]);
        }
    }

    fn status_tile(i: usize, out: &mut [u8]) {
        let c = ((i / 4) * 37 % 256) as u8;
        for y in 1000..1064 {
            for x in 1792..1856 {
                out[y * STRIDE + x * 4..y * STRIDE + x * 4 + 4].copy_from_slice(&[c, 255 - c, c / 2, 255]);
            }
        }
    }

    fn ui(&self, i: usize, out: &mut [u8]) {
        out.copy_from_slice(&self.base);
        self.scroll_window(i, 3, (350, 200, 800, 500), out);
        Self::status_tile(i, out);
    }

    fn uiscroll(&self, i: usize, out: &mut [u8]) {
        for y in 0..H {
            let s = ((i * 5 + y) % self.doc_h) * STRIDE;
            out[y * STRIDE..(y + 1) * STRIDE].copy_from_slice(&self.doc[s..s + STRIDE]);
        }
    }

    fn photo(&self, i: usize, out: &mut [u8]) {
        let wall = &self.walls[(i / 60) % self.walls.len()];
        let f = (i % 60) as i32;
        let tri = if f < 30 { f } else { 60 - f } as usize;
        let (ox, oy) = (tri * 16, tri * 9);
        for y in 0..H {
            let s = ((oy + y) * wall.w + ox) * 4;
            out[y * STRIDE..(y + 1) * STRIDE].copy_from_slice(&wall.px[s..s + STRIDE]);
        }
        for (k, p) in out.chunks_exact_mut(4).enumerate() {
            let h = mix(k as u32 ^ (i as u32).wrapping_mul(0x9E37_79B1));
            for (c, ch) in p.iter_mut().take(3).enumerate() {
                *ch = (*ch as i32 + ((h >> (2 * c)) & 3) as i32 - 1).clamp(0, 255) as u8;
            }
        }
    }

    fn mixed(&self, i: usize, out: &mut [u8]) {
        out.copy_from_slice(&self.base);
        self.scroll_window(i, 3, (40, 150, 660, 700), out);
        Self::status_tile(i, out);
        let mut g = vec![0u8; FRAME_BYTES];
        game(i, &mut g);
        for y in 150..690 {
            let o = y * STRIDE + 760 * 4;
            out[o..o + 960 * 4].copy_from_slice(&g[o..o + 960 * 4]);
        }
    }
}

fn mix(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7FEB_352D);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846C_A68B);
    x ^ (x >> 16)
}

fn tri8(a: i32) -> i32 {
    let a = a & 511;
    if a < 256 {
        a
    } else {
        511 - a
    }
}

fn bounce(v: i32, max: i32) -> i32 {
    let m = v.rem_euclid(2 * max);
    if m > max {
        2 * max - m
    } else {
        m
    }
}

fn put(out: &mut [u8], x: i32, y: i32, c: [u8; 3]) {
    if x >= 0 && y >= 0 && (x as usize) < W && (y as usize) < H {
        let o = y as usize * STRIDE + x as usize * 4;
        out[o..o + 3].copy_from_slice(&c);
    }
}

fn game(i: usize, out: &mut [u8]) {
    let t = i as i32;
    for y in 0..H as i32 {
        for x in 0..W as i32 {
            let o = y as usize * STRIDE + x as usize * 4;
            out[o] = (40 + tri8(x - 3 * y - 5 * t) * 3 / 5) as u8;
            out[o + 1] = (40 + tri8(2 * x + y + 3 * t) * 3 / 5) as u8;
            out[o + 2] = (40 + tri8(3 * x + 2 * y + 7 * t) * 3 / 5) as u8;
            out[o + 3] = 255;
        }
    }
    for k in 0..20i32 {
        let r = 30 + (k * 17) % 90;
        let cx = r + bounce(200 + k * 83 + t * (3 + k % 5), W as i32 - 2 * r);
        let cy = r + bounce(100 + k * 47 + t * (2 + k % 3), H as i32 - 2 * r);
        let col = [(60 + k * 37 % 190) as u8, (200 - k * 29 % 160) as u8, (90 + k * 53 % 160) as u8];
        let dark = col.map(|c| c / 3);
        for dy in -r..=r {
            let dx = ((r * r - dy * dy) as f32).sqrt() as i32;
            for x in cx - dx..=cx + dx {
                let edge = x - (cx - dx) < 2 || (cx + dx) - x < 2 || dy.abs() >= r - 1;
                put(out, x, cy + dy, if edge { dark } else { col });
            }
        }
    }
    for k in 0..12i32 {
        let (w, h) = (80 + k * 13, 50 + k * 9);
        let x0 = bounce(50 + k * 131 - t * (2 + k % 4), W as i32 - w);
        let y0 = bounce(30 + k * 71 + t * (1 + k % 3), H as i32 - h);
        let col = [(220 - k * 31 % 150) as u8, (80 + k * 23 % 150) as u8, (30 + k * 61 % 200) as u8];
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                let edge = x - x0 < 2 || y - y0 < 2 || x0 + w - x <= 2 || y0 + h - y <= 2;
                put(out, x, y, if edge { [20, 20, 20] } else { col });
            }
        }
    }
    // HUD: 40 pseudo-glyphs (5x7 cells, 2x scale) on a dark bar, re-rolled every 30 frames.
    for y in 20..52 {
        for x in 30..450 {
            put(out, x, y, [16, 16, 16]);
        }
    }
    for k in 0..40u32 {
        let bits = mix(k * 7919 + (i as u32 / 30) * 104_729) as u64 | (mix(k + 12345) as u64) << 32;
        for cell in 0..35u32 {
            if bits >> cell & 1 == 1 {
                let (cx, cy) = (30 + k as i32 * 10 + (cell % 5) as i32 * 2, 24 + (cell / 5) as i32 * 3);
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    put(out, cx + dx, cy + dy, [240, 240, 240]);
                }
            }
        }
    }
    for (k, p) in out.chunks_exact_mut(4).enumerate() {
        let h = mix(k as u32 ^ (i as u32).wrapping_mul(0x85EB_CA6B));
        for (c, ch) in p.iter_mut().take(3).enumerate() {
            *ch = (*ch as i32 + ((h >> (3 * c)) & 7) as i32 - 4).clamp(0, 255) as u8;
        }
    }
}
