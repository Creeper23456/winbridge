// SPDX-License-Identifier: GPL-3.0-or-later
//! `report` subcommand: merges `gen.jsonl` with decode result files into markdown tables.

use crate::run::CLASSES;
use crate::util::*;
use std::collections::BTreeMap;
use std::fs;

struct Gen {
    bytes: f64,
    mbps: f64,
    psnr_mean: f64,
    psnr_min: f64,
    bad: f64,
    enc_ms: f64,
    changed: f64,
}

struct Dec {
    p50: f64,
    p95: f64,
    max: f64,
    verify: String,
}

fn num(line: &str, k: &str) -> f64 {
    json_field(line, k).and_then(|v| v.parse().ok()).unwrap_or(f64::NAN)
}

fn lines(path: &str) -> Vec<String> {
    fs::read_to_string(path).unwrap_or_else(|e| die(&format!("{path}: {e}"))).lines().map(str::to_string).collect()
}

/// (class, codec, decoder, threads) -> one entry per result file.
type DecMap = BTreeMap<(String, String, String, usize), Vec<Dec>>;

fn load_dec(files: &[String]) -> DecMap {
    let mut m = DecMap::new();
    for f in files {
        for l in lines(f).iter().filter(|l| l.contains("\"kind\":\"decode\"")) {
            let key = (
                json_field(l, "class").unwrap().to_string(),
                json_field(l, "codec").unwrap().to_string(),
                json_field(l, "decoder").unwrap().to_string(),
                num(l, "threads") as usize,
            );
            m.entry(key).or_default().push(Dec {
                p50: num(l, "p50_ms"),
                p95: num(l, "p95_ms"),
                max: num(l, "max_ms"),
                verify: json_field(l, "verify").unwrap().to_string(),
            });
        }
    }
    m
}

fn mean(v: impl Iterator<Item = f64>) -> f64 {
    let v: Vec<f64> = v.collect();
    v.iter().sum::<f64>() / v.len() as f64
}

fn cell(m: &DecMap, class: &str, codec: &str, dec: &str, t: usize) -> String {
    match m.get(&(class.into(), codec.into(), dec.into(), t)) {
        None => "-".into(),
        Some(v) => format!(
            "{:.2}/{:.2}/{:.2}",
            mean(v.iter().map(|d| d.p50)),
            mean(v.iter().map(|d| d.p95)),
            v.iter().map(|d| d.max).fold(0.0, f64::max)
        ),
    }
}

/// Largest relative spread of the p50 between result files over all thread counts.
fn spread(m: &DecMap, class: &str, codec: &str, dec: &str) -> String {
    let mut worst: f64 = 0.0;
    let mut runs = 0;
    for t in [1, 2, 4] {
        if let Some(v) = m.get(&(class.into(), codec.into(), dec.into(), t)) {
            runs = runs.max(v.len());
            if v.len() > 1 {
                let (lo, hi) = v.iter().fold((f64::MAX, 0.0f64), |(l, h), d| (l.min(d.p50), h.max(d.p50)));
                worst = worst.max((hi - lo) / mean(v.iter().map(|d| d.p50)) * 100.0);
            }
        }
    }
    if runs > 1 {
        format!("{worst:.0}%")
    } else {
        "n/a".into()
    }
}

fn verify(m: &DecMap, class: &str, codec: &str, dec: &str) -> &'static str {
    let bad = m
        .iter()
        .filter(|(k, _)| k.0 == class && k.1 == codec && k.2 == dec)
        .any(|(_, v)| v.iter().any(|d| d.verify != "ok"));
    if bad {
        " (HASH MISMATCH)"
    } else {
        ""
    }
}

pub fn run(args: &Args) {
    let gen: Vec<String> = lines(args.get("gen").unwrap_or_else(|| die("--gen gen.jsonl required")));
    let guest = load_dec(&args.list("guest").unwrap_or_default());
    let host = load_dec(&args.list("host").unwrap_or_default());
    let mut g = BTreeMap::new();
    for l in gen.iter().filter(|l| l.contains("\"kind\":\"gen\"")) {
        g.insert(
            (json_field(l, "class").unwrap().to_string(), json_field(l, "codec").unwrap().to_string()),
            Gen {
                bytes: num(l, "bytes_mean"),
                mbps: num(l, "mbps60"),
                psnr_mean: num(l, "psnr_mean"),
                psnr_min: num(l, "psnr_min"),
                bad: num(l, "bad_pct"),
                enc_ms: num(l, "enc_ms_p50"),
                changed: num(l, "changed_tiles_pct"),
            },
        );
    }
    println!("| codec / decoder | class | bytes/frame | MB/s @60 fps | PSNR mean/min dB | px err>24 % | host enc ms p50 | guest 1T p50/p95/max ms | guest 2T | guest 4T | run spread |");
    println!("|---|---|---:|---:|---|---:|---:|---|---|---|---|");
    for class in CLASSES {
        for (key, gs) in g.iter().filter(|(k, _)| k.0 == *class) {
            let codec = key.1.as_str();
            let decs: Vec<&str> = guest
                .keys()
                .filter(|k| k.0 == *class && k.1 == codec)
                .map(|k| k.2.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let psnr = if gs.psnr_min >= 999.0 {
                "lossless".to_string()
            } else {
                format!("{:.1}/{:.1}", gs.psnr_mean, gs.psnr_min)
            };
            let tiles = if gs.changed >= 0.0 { format!(" ({:.0}% tiles)", gs.changed) } else { String::new() };
            for d in decs {
                println!(
                    "| {codec} / {d}{} | {class} | {:.0}{tiles} | {:.1} | {psnr} | {:.2} | {:.1} | {} | {} | {} | {} |",
                    verify(&guest, class, codec, d),
                    gs.bytes,
                    gs.mbps,
                    gs.bad,
                    gs.enc_ms,
                    cell(&guest, class, codec, d, 1),
                    cell(&guest, class, codec, d, 2),
                    cell(&guest, class, codec, d, 4),
                    spread(&guest, class, codec, d),
                );
            }
        }
    }
    if !host.is_empty() {
        println!("\nNative Linux host reference (same decode code):\n");
        println!("| codec / decoder | class | host 1T p50/p95/max ms | 2T | 4T |");
        println!("|---|---|---|---|---|");
        for (k, _) in host.iter().filter(|(k, _)| k.3 == 1) {
            println!(
                "| {} / {}{} | {} | {} | {} | {} |",
                k.1,
                k.2,
                verify(&host, &k.0, &k.1, &k.2),
                k.0,
                cell(&host, &k.0, &k.1, &k.2, 1),
                cell(&host, &k.0, &k.1, &k.2, 2),
                cell(&host, &k.0, &k.1, &k.2, 4),
            );
        }
    }
}
