// SPDX-License-Identifier: GPL-3.0-or-later
//! `gen` subcommand: synthesise frames, encode them with every candidate, verify the reference decode,
//! measure PSNR / size / host encode time and write the streams plus `gen.jsonl`.

mod enc;
mod frames;

use crate::dec::tiles::diff_count;
use crate::dec::{decoders_for, Scratch, STREAM_CODECS};
use crate::run::CLASSES;
use crate::stream::{stream_path, StreamWriter};
use crate::util::*;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

struct FrameResult {
    payload: Vec<u8>,
    bytes: usize,
    hash: u64,
    enc_ms: f64,
    mse: f64,
    bad_frac: f64,
    changed_tiles: Option<usize>,
}

fn is_diff(codec: &str) -> bool {
    codec.ends_with("-diff")
}

fn encode(codec: &str, frame: &[u8], prev: Option<&[u8]>) -> Vec<u8> {
    match codec {
        "nv12" => enc::nv12(frame),
        "bc1" => enc::bc1(frame),
        "bc7" => enc::bc7(frame),
        "lz4-full" => enc::lossless(frame, None, None, false),
        "lz4-diff" => enc::lossless(frame, prev, None, true),
        "zstd1-full" => enc::lossless(frame, None, Some(1), false),
        "zstd1-diff" => enc::lossless(frame, prev, Some(1), true),
        "zstd3-full" => enc::lossless(frame, None, Some(3), false),
        "zstd3-diff" => enc::lossless(frame, prev, Some(3), true),
        "mjpeg444" => enc::mjpeg(frame, false, false),
        "mjpeg420" => enc::mjpeg(frame, true, false),
        "mjpeg444s8" => enc::mjpeg(frame, false, true),
        "mjpeg420s8" => enc::mjpeg(frame, true, true),
        "raw-diff" => enc::raw_diff(frame, prev),
        "raw-full" => Vec::new(),
        _ => die(&format!("unknown codec {codec}")),
    }
}

/// (MSE over B,G,R, fraction of pixels with any channel off by more than 24).
fn compare(a: &[u8], b: &[u8]) -> (f64, f64) {
    let (mut se, mut bad) = (0u64, 0usize);
    for (p, q) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        let mut worst = 0;
        for c in 0..3 {
            let d = (p[c] as i32 - q[c] as i32).unsigned_abs();
            se += (d * d) as u64;
            worst = worst.max(d);
        }
        bad += (worst > 24) as usize;
    }
    (se as f64 / (a.len() / 4 * 3) as f64, bad as f64 / (a.len() / 4) as f64)
}

fn process(src: &frames::Source, class: &str, codec: &str, range: std::ops::Range<usize>, workers: usize) -> Vec<FrameResult> {
    let results: Vec<Mutex<Option<FrameResult>>> = range.clone().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(range.start);
    let diff = is_diff(codec);
    let reference = if codec.starts_with("raw") { None } else { decoders_for(codec).into_iter().next() };
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                let mut cur = vec![0u8; FRAME_BYTES];
                let mut prev = vec![0u8; FRAME_BYTES];
                let mut recon = vec![0u8; FRAME_BYTES];
                let mut scratch = Scratch::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= range.end {
                        return;
                    }
                    src.render(class, i, &mut cur);
                    let has_prev = diff && i > 0;
                    if has_prev {
                        src.render(class, i - 1, &mut prev);
                    }
                    let t0 = Instant::now();
                    let payload = encode(codec, &cur, has_prev.then_some(prev.as_slice()));
                    let enc_ms = t0.elapsed().as_secs_f64() * 1e3;
                    let (mse, bad_frac, hash, bytes) = match &reference {
                        Some(d) => {
                            if has_prev {
                                recon.copy_from_slice(&prev);
                            } else {
                                recon.fill(0);
                            }
                            d.decode(&payload, 0, 1, &mut scratch, Dst(recon.as_mut_ptr()));
                            let (mse, bad) = compare(&cur, &recon);
                            if codec.starts_with("lz4") || codec.starts_with("zstd") {
                                assert!(mse == 0.0, "{codec} {class} frame {i}: lossless mismatch");
                            }
                            (mse, bad, hash_bytes(&recon), payload.len())
                        }
                        None => (0.0, 0.0, hash_bytes(&cur), if codec == "raw-full" { FRAME_BYTES } else { payload.len() }),
                    };
                    let changed_tiles = diff.then(|| {
                        (0..GROUPS).map(|g| diff_count(crate::dec::tiles::record(&payload, g))).sum::<usize>()
                    });
                    let keep = if codec.starts_with("raw") { Vec::new() } else { payload };
                    *results[i - range.start].lock().unwrap() =
                        Some(FrameResult { payload: keep, bytes, hash, enc_ms, mse, bad_frac, changed_tiles });
                }
            });
        }
    });
    results.into_iter().map(|m| m.into_inner().unwrap().unwrap()).collect()
}

pub fn run(args: &Args) {
    let out = Path::new(args.get("out").unwrap_or_else(|| die("--out DIR required")));
    fs::create_dir_all(out).expect("create out dir");
    let shots = args.list("shots").unwrap_or_else(|| die("--shots a.png,b.png,... required (guest screenshots)"));
    let walls = args.list("walls").unwrap_or_else(|| die("--walls a.jpg,b.png,... required (wallpapers)"));
    let n = args.num("frames", 240);
    let workers = args.num("workers", 6);
    let classes = args.list("classes").unwrap_or_else(|| CLASSES.iter().map(|s| s.to_string()).collect());
    let mut codecs = args.list("codecs").unwrap_or_else(|| {
        let mut v: Vec<String> = STREAM_CODECS.iter().map(|s| s.to_string()).collect();
        v.extend(["raw-full".to_string(), "raw-diff".to_string()]);
        v
    });
    codecs.retain(|c| c != "none");
    let src = frames::Source::new(&shots, &walls);
    let mut stats = fs::OpenOptions::new()
        .create(true)
        .append(args.get("append").is_some())
        .write(true)
        .truncate(args.get("append").is_none())
        .open(out.join("gen.jsonl"))
        .expect("open gen.jsonl");
    for class in &classes {
        for codec in &codecs {
            // Every tile changes every frame in these classes, so the diff variants would only repeat the full ones.
            if is_diff(codec) && !codec.starts_with("raw") && (class == "photo" || class == "game") {
                continue;
            }
            let t0 = Instant::now();
            let mut writer = (!codec.starts_with("raw")).then(|| StreamWriter::create(&stream_path(out, class, codec), n).expect("create stream"));
            let mut res: Vec<FrameResult> = Vec::with_capacity(n);
            for start in (0..n).step_by(workers * 3) {
                let mut batch = process(&src, class, codec, start..(start + workers * 3).min(n), workers);
                for r in &mut batch {
                    if let Some(w) = &mut writer {
                        w.push(&r.payload, r.hash).expect("write stream");
                    }
                    r.payload = Vec::new();
                }
                res.extend(batch);
            }
            if let Some(w) = writer {
                w.finish().expect("finish stream");
            }
            let bytes: Vec<f64> = res.iter().map(|r| r.bytes as f64).collect();
            let mean_bytes = bytes.iter().sum::<f64>() / n as f64;
            let max_bytes = bytes.iter().cloned().fold(0.0, f64::max);
            let mut enc: Vec<f64> = res.iter().map(|r| r.enc_ms).collect();
            let es = summarize(&mut enc);
            let psnrs: Vec<f64> = res.iter().map(|r| if r.mse == 0.0 { 999.0 } else { 10.0 * (255.0f64 * 255.0 / r.mse).log10() }).collect();
            let psnr_mean = psnrs.iter().sum::<f64>() / n as f64;
            let psnr_min = psnrs.iter().cloned().fold(f64::MAX, f64::min);
            let bad = res.iter().map(|r| r.bad_frac).sum::<f64>() / n as f64 * 100.0;
            let changed = if is_diff(codec) {
                // Frame 0 is a keyframe; the steady-state fraction excludes it.
                let tiles = (TILES_X * TILES_Y) as f64;
                res.iter().skip(1).filter_map(|r| r.changed_tiles).sum::<usize>() as f64 / (n - 1) as f64 / tiles * 100.0
            } else {
                -1.0
            };
            let line = format!(
                "{{\"kind\":\"gen\",\"class\":\"{class}\",\"codec\":\"{codec}\",\"frames\":{n},\"bytes_mean\":{mean_bytes:.0},\"bytes_max\":{max_bytes:.0},\"mbps60\":{:.2},\"psnr_mean\":{psnr_mean:.2},\"psnr_min\":{psnr_min:.2},\"bad_pct\":{bad:.3},\"enc_ms_p50\":{:.2},\"enc_ms_p95\":{:.2},\"changed_tiles_pct\":{changed:.1}}}",
                mean_bytes * 60.0 / 1e6,
                es.p50,
                es.p95
            );
            println!("{line}  [{:.0}s]", t0.elapsed().as_secs_f64());
            writeln!(stats, "{line}").unwrap();
        }
    }
}
