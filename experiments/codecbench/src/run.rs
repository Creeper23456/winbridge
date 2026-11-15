// SPDX-License-Identifier: GPL-3.0-or-later
//! `decode` subcommand: time every decoder variant into the output surface.

use crate::dec::lossless::{Lossless, RawDiff, RawFull};
use crate::dec::tiles::{assemble, diff_record_bytes, pack_group};
use crate::dec::{decoders_for, Decoder, Scratch, STREAM_CODECS};
use crate::pool::Pool;
use crate::stream::{stream_path, Stream};
use crate::surface::Surface;
use crate::util::*;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

pub const CLASSES: &[&str] = &["ui", "uiscroll", "photo", "game", "mixed"];

struct Frames<'a> {
    data: Vec<&'a [u8]>,
    hashes: Vec<u64>,
}

pub struct Opts {
    pub label: String,
    pub threads: Vec<usize>,
    pub warmup: usize,
    pub frames: usize,
}

fn features() -> String {
    let mut f = Vec::new();
    #[cfg(target_arch = "x86_64")]
    {
        macro_rules! probe {
            ($($n:tt),*) => { $(if is_x86_feature_detected!($n) { f.push($n); })* };
        }
        probe!("sse2", "ssse3", "sse4.1", "avx", "avx2", "avx512f");
    }
    f.join("+")
}

fn emit(out: &mut Option<std::fs::File>, line: String) {
    println!("{line}");
    if let Some(f) = out {
        writeln!(f, "{line}").expect("write results");
    }
}

pub fn run(args: &Args) {
    let dir = args.get("data").unwrap_or_else(|| die("--data DIR required")).to_string();
    let dir = Path::new(&dir);
    let classes = args.list("classes").unwrap_or_else(|| CLASSES.iter().map(|s| s.to_string()).collect());
    let codec_filter = args.list("codecs");
    let opts = Opts {
        label: args.get("label").unwrap_or("run").to_string(),
        threads: args.list("threads").map_or(vec![1, 2, 4], |l| l.iter().map(|t| t.parse().expect("threads")).collect()),
        warmup: args.num("warmup", 20),
        frames: args.num("frames", usize::MAX),
    };
    let mut out = args
        .get("out")
        .map(|p| OpenOptions::new().create(true).append(true).open(p).expect("open --out"));
    emit(
        &mut out,
        format!(
            "{{\"kind\":\"env\",\"label\":\"{}\",\"os\":\"{}\",\"surface\":\"{}\",\"cpu_features\":\"{}\",\"logical_cpus\":{}}}",
            json_escape(&opts.label),
            std::env::consts::OS,
            Surface::kind(),
            features(),
            std::thread::available_parallelism().map_or(0, |n| n.get())
        ),
    );
    let surface = Surface::new();
    let wanted = |c: &str| codec_filter.as_ref().is_none_or(|l| l.iter().any(|x| x == c));

    for class in &classes {
        // Raw baselines are built from the lossless stream so no extra data is transferred.
        let src_path = stream_path(dir, class, "zstd1-full");
        let streams_present: Vec<&str> = STREAM_CODECS.iter().copied().filter(|c| stream_path(dir, class, c).exists()).collect();
        if streams_present.is_empty() {
            eprintln!("skip class {class}: no streams in {}", dir.display());
            continue;
        }
        if (wanted("raw-full") || wanted("raw-diff")) && src_path.exists() {
            let (raw, hashes) = inflate_all(&src_path, &opts, &surface);
            let n = hashes.len();
            if wanted("raw-full") {
                let f = Frames { data: (0..n).map(|i| &raw[i * FRAME_BYTES..(i + 1) * FRAME_BYTES]).collect(), hashes: hashes.clone() };
                bench(&RawFull, "raw-full", class, &f, &surface, &opts, &mut out);
            }
            // photo/game change every tile every frame; their changed-tile payloads would just duplicate the raw frames in RAM.
            if wanted("raw-diff") && class != "photo" && class != "game" {
                let payloads: Vec<Vec<u8>> = (0..n)
                    .map(|i| {
                        let cur = &raw[i * FRAME_BYTES..(i + 1) * FRAME_BYTES];
                        let prev = (i > 0).then(|| &raw[(i - 1) * FRAME_BYTES..i * FRAME_BYTES]);
                        let recs: Vec<Vec<u8>> = (0..GROUPS)
                            .map(|g| {
                                let (idx, data) = pack_group(cur, prev, g);
                                diff_record_bytes(&idx, &data)
                            })
                            .collect();
                        assemble(&recs)
                    })
                    .collect();
                let f = Frames { data: payloads.iter().map(Vec::as_slice).collect(), hashes: hashes.clone() };
                bench(&RawDiff, "raw-diff", class, &f, &surface, &opts, &mut out);
            }
        }
        for codec in streams_present {
            if !wanted(codec) {
                continue;
            }
            let s = Stream::load(&stream_path(dir, class, codec)).expect("load stream");
            let n = s.len().min(opts.frames);
            let f = Frames { data: (0..n).map(|i| s.frame(i)).collect(), hashes: (0..n).map(|i| s.hash(i)).collect() };
            for d in decoders_for(codec) {
                bench(d.as_ref(), codec, class, &f, &surface, &opts, &mut out);
            }
        }
    }
}

/// Decodes the whole lossless stream (untimed) into one big raw buffer.
fn inflate_all(path: &Path, opts: &Opts, surface: &Surface) -> (Vec<u8>, Vec<u64>) {
    let s = Stream::load(path).expect("load stream");
    let n = s.len().min(opts.frames);
    let dec = Lossless { zstd: true, diff: false };
    let mut scratch = Scratch::new();
    let mut raw = Vec::with_capacity(n * FRAME_BYTES);
    for i in 0..n {
        dec.decode(s.frame(i), 0, 1, &mut scratch, surface.dst());
        raw.extend_from_slice(surface.bytes());
    }
    (raw, (0..n).map(|i| s.hash(i)).collect())
}

fn bench(dec: &dyn Decoder, codec: &str, class: &str, f: &Frames, surface: &Surface, opts: &Opts, out: &mut Option<std::fs::File>) {
    let n = f.data.len();
    let dst = surface.dst();
    // Untimed correctness pass: output hash of every frame against the host reference.
    let mut scratch = Scratch::new();
    let mut bad = 0;
    for i in 0..n {
        dec.decode(f.data[i], 0, 1, &mut scratch, dst);
        if hash_bytes(surface.bytes()) != f.hashes[i] {
            bad += 1;
        }
    }
    let verify = if bad == 0 { "ok".to_string() } else { format!("mismatch:{bad}/{n}") };
    let mean_bytes = f.data.iter().map(|d| d.len()).sum::<usize>() as f64 / n as f64;
    for &t in &opts.threads {
        if t > dec.max_parts() {
            continue;
        }
        let pool = Pool::new(t);
        let scratches: Vec<Mutex<Scratch>> = (0..t).map(|_| Mutex::new(Scratch::new())).collect();
        let mut ns = Vec::with_capacity(n);
        for i in 0..n {
            let frame = f.data[i];
            let t0 = Instant::now();
            pool.run(&|tid| dec.decode(frame, tid, t, &mut scratches[tid].lock().unwrap(), dst));
            let dt = t0.elapsed().as_nanos() as f64;
            if i >= opts.warmup {
                ns.push(dt);
            }
        }
        if ns.is_empty() {
            continue;
        }
        let samples = ns.len();
        let s = summarize(&mut ns);
        emit(
            out,
            format!(
                "{{\"kind\":\"decode\",\"label\":\"{}\",\"os\":\"{}\",\"class\":\"{class}\",\"codec\":\"{codec}\",\"decoder\":\"{}\",\"threads\":{t},\"samples\":{samples},\"p50_ms\":{:.3},\"p95_ms\":{:.3},\"max_ms\":{:.3},\"mean_ms\":{:.3},\"in_bytes\":{:.0},\"verify\":\"{verify}\"}}",
                json_escape(&opts.label),
                std::env::consts::OS,
                dec.name(),
                s.p50 / 1e6,
                s.p95 / 1e6,
                s.max / 1e6,
                s.mean / 1e6,
                mean_bytes
            ),
        );
    }
}
