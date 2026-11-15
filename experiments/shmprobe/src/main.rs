// SPDX-License-Identifier: GPL-3.0-or-later

//! shmprobe: measures whether the third-party ivshmem Windows driver and the `winbridge-shm` ring
//! are usable, and which BAR cache mode to use.
//!
//! ```text
//! shmprobe guest info
//! shmprobe guest identify
//! shmprobe guest bench   [--dev N] [--modes 0,1,2] [--quick] [--only OP] [--out FILE]
//! shmprobe guest pattern [--dev N] [--modes ...] [--span-mib 64]
//! shmprobe guest order   [--dev N] [--modes ...] [--secs 1.5]
//! shmprobe guest ring    [--dev N] [--modes ...] [--records N] [--pings N] [--sleep-pings N] [--ring-mib 64]
//! shmprobe guest single  [--dev N]            (Windows: single-mapping behaviour)
//! shmprobe guest open-test                    (Windows: can this user open the devices; prints the device DACL)
//! shmprobe guest harden [--sddl S]            (Windows: restrict the device objects, default SYSTEM + Administrators)
//! shmprobe host identify --file A [--file B]
//! shmprobe host bench    --file A [--quick] [--out FILE]
//! shmprobe host pattern  --file A [--span-mib 64]
//! shmprobe host order    --file A
//! shmprobe host ring     --file A [--rounds 3] [same ring options]
//! ```
//!
//! The guest role maps the ivshmem device with the driver (Windows). On Unix it takes `--file PATH`
//! instead, which makes the whole tool testable against two processes on one machine.

#[cfg(not(target_arch = "x86_64"))]
compile_error!("shmprobe measures x86-64 memory instructions");

mod bench;
mod ctl;
mod dev;
mod kernels;
mod order;
mod pattern;
mod ring;
mod util;
#[cfg(windows)]
mod wintests;
#[cfg(windows)]
mod winimp;

use std::fs::OpenOptions;
use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use dev::{Dev, Mode};
use util::Args;
use winbridge_shm::{publish_fence, Mapping};

type Res = Result<bool, String>;

fn pick(a: &Args) -> Result<Dev, String> {
    let idx: usize = a.get("--dev", 0);
    let mut v = dev::list(&a.all("--file"))?;
    if idx >= v.len() {
        return Err(format!("--dev {idx}: only {} device(s)", v.len()));
    }
    Ok(v.swap_remove(idx))
}

fn ring_params(a: &Args) -> ring::Params {
    ring::Params {
        records: a.get("--records", 1_000_000),
        pings: a.get("--pings", 200_000),
        sleep_pings: a.get("--sleep-pings", 2_000),
        ring_mib: a.get("--ring-mib", 64),
    }
}

fn csv_sink(a: &Args) -> Result<Option<std::fs::File>, String> {
    match a.opt("--out") {
        Some(p) => Ok(Some(OpenOptions::new().create(true).append(true).open(&p).map_err(|e| format!("{p}: {e}"))?)),
        None => Ok(None),
    }
}

fn run_bench(bar: *mut u8, len: usize, devname: &str, mode: &str, a: &Args, out: &mut Option<std::fs::File>) -> Result<(), String> {
    let only = a.opt("--only");
    bench::run(bar, len, devname, mode, a.flag("--quick"), only.as_deref(), &mut |row| {
        println!("{row}");
        if let Some(f) = out.as_mut() {
            let _ = writeln!(f, "{row}");
        }
    })
}

fn banner() {
    println!("CPU: {} [{}]", kernels::cpu_brand(), kernels::cpu_features());
}

fn guest(cmd: &str, a: &Args) -> Res {
    match cmd {
        "info" => {
            banner();
            let devs = dev::list(&a.all("--file"))?;
            for (i, d) in devs.iter().enumerate() {
                println!("DEVICE {i}: {}", d.describe());
                #[cfg(windows)]
                wintests::vectors(d);
            }
            Ok(true)
        }
        "identify" => {
            let devs = dev::list(&a.all("--file"))?;
            let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            for (i, d) in devs.iter().enumerate() {
                let map = d.map(Mode::Cached)?;
                let tag = format!("WBPROBE-DEV-{i}-{nonce:x}");
                let mut buf = [0u8; 64];
                buf[..tag.len()].copy_from_slice(tag.as_bytes());
                // SAFETY: offset 0x800..0x840 lies inside the mapping.
                unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), map.base().add(0x800), buf.len()) };
                publish_fence();
                println!("IDENTIFY dev={i} location=[{}] tag={tag}", d.label());
            }
            Ok(true)
        }
        "bench" => {
            banner();
            let d = pick(a)?;
            let mut out = csv_sink(a)?;
            let name = d.label();
            for mode in Mode::parse_list(a.opt("--modes")) {
                let map = d.map(mode)?;
                println!("BENCH mapping dev=[{name}] mode={} base={:p} len={} MiB", mode.name(), map.base(), map.len() >> 20);
                run_bench(map.base(), map.len(), &name, mode.name(), a, &mut out)?;
            }
            Ok(true)
        }
        "pattern" => {
            let d = pick(a)?;
            pattern::guest(&d, &Mode::parse_list(a.opt("--modes")), a.get("--span-mib", 64))
        }
        "order" => {
            banner();
            let d = pick(a)?;
            let v = order::guest(&d, &Mode::parse_list(a.opt("--modes")), a.get("--secs", 1.5))?;
            println!("ORDER total violations reported by the host: {v}");
            Ok(true)
        }
        "ring" => {
            banner();
            let d = pick(a)?;
            ring::guest(&d, &Mode::parse_list(a.opt("--modes")), ring_params(a))
        }
        #[cfg(windows)]
        "single" => wintests::single(&pick(a)?, a.get("--dev", 0)),
        #[cfg(windows)]
        "harden" => wintests::harden(a),
        #[cfg(windows)]
        "open-test" => wintests::open_test(),
        #[cfg(windows)]
        "trymap" => wintests::trymap(&pick(a)?, a),
        _ => Err(format!("unknown guest command `{cmd}`")),
    }
}

fn host_map(a: &Args) -> Result<dev::Map, String> {
    let f = a.opt("--file").ok_or("--file PATH required")?;
    host_file(&f)
}

#[cfg(unix)]
fn host_file(path: &str) -> Result<dev::Map, String> {
    dev::map_file(path)
}

#[cfg(not(unix))]
fn host_file(_path: &str) -> Result<dev::Map, String> {
    Err("the host role runs on Linux".into())
}

fn host(cmd: &str, a: &Args) -> Res {
    match cmd {
        "identify" => {
            for f in a.all("--file") {
                let map = host_file(&f)?;
                let mut b = [0u8; 64];
                // SAFETY: offset 0x800..0x840 lies inside the mapping.
                unsafe { std::ptr::copy_nonoverlapping(map.base().add(0x800), b.as_mut_ptr(), b.len()) };
                let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                println!("IDENTIFY file={f} tag={}", String::from_utf8_lossy(&b[..end]));
            }
            Ok(true)
        }
        "bench" => {
            banner();
            let map = host_map(a)?;
            let mut out = csv_sink(a)?;
            run_bench(map.base(), map.len(), "host", "host", a, &mut out)?;
            Ok(true)
        }
        "pattern" => pattern::host(&host_map(a)?, a.get("--span-mib", 64)),
        "order" => {
            let v = order::host(&host_map(a)?)?;
            println!("ORDER host total violations: {v}");
            Ok(true)
        }
        "ring" => ring::host(Arc::new(host_map(a)?), a.get("--rounds", 3), ring_params(a)),
        _ => Err(format!("unknown host command `{cmd}`")),
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.len() < 2 {
        eprintln!("usage: shmprobe (guest|host) COMMAND [options]; see the module documentation in src/main.rs");
        return ExitCode::from(2);
    }
    let a = Args::new(argv[2..].to_vec());
    let r = match argv[0].as_str() {
        "guest" => guest(&argv[1], &a),
        "host" => host(&argv[1], &a),
        other => Err(format!("unknown role `{other}`")),
    };
    match r {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            println!("RESULT: FAILED");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
