// SPDX-License-Identifier: GPL-3.0-or-later
// codecbench: guest-side decode cost of candidate frame-return codecs (1080p BGRA).
//
//   gen    (host)  codecbench gen --out DIR --shots a.png,b.png,.. --walls w1.jpg,w2.png [--frames 240]
//          Synthesises the frame classes, encodes with every candidate, prints size/PSNR/encode time.
//   decode (any)   codecbench decode --data DIR [--threads 1,2,4] [--out results.jsonl] [--label NAME]
//          Times each decoder into a pre-allocated 1080p BGRA surface (DIB section on Windows).
//   tail   (win)   codecbench tail [--out results.jsonl]   memcpy-into-DIB / BitBlt chain tail.
//   report         codecbench report --gen gen.jsonl --guest g1.jsonl[,g2.jsonl] [--host h.jsonl]

mod dec;
#[cfg(feature = "gen")]
mod gen;
mod pool;
mod report;
mod run;
mod stream;
mod surface;
#[cfg(windows)]
mod tail;
mod util;

use util::{die, Args};

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = argv.first() else {
        die("usage: codecbench gen|decode|tail|report [options]");
    };
    let args = Args::parse(&argv[1..]);
    match cmd.as_str() {
        #[cfg(feature = "gen")]
        "gen" => gen::run(&args),
        "decode" => run::run(&args),
        #[cfg(windows)]
        "tail" => tail::run(&args),
        "report" => report::run(&args),
        other => die(&format!("unknown or unavailable subcommand {other:?}")),
    }
}
