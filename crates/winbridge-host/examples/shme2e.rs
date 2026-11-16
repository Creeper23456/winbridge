// SPDX-License-Identifier: GPL-3.0-or-later

//! Host-side end-to-end driver for the shared-memory transport, without the GTK viewer.
//!
//! It plays the host role against a real guest agent: it connects to the QEMU chardev socket of
//! `org.winbridge.0`, performs the normal handshake, optionally negotiates the shared-memory lanes
//! (`ShmOffer`) over the ivshmem backing file, then measures
//!
//! 1. control round trips (`Ping`/`Pong`) with the bulk lanes idle,
//! 2. a host-to-guest file stream (drag-and-drop messages, acknowledged by `DropResult`) while
//!    pinging on the control lane,
//! 3. a guest-to-host file stream (`ClipFilesRequest` for a guest clipboard copy) while pinging,
//!
//! and prints throughput and latency percentiles plus SHA-256 hashes. `--mode serial` skips the
//! negotiation (the same agent stays on the serial channel) for the baseline.
//!
//! Not the viewer: no GTK, no D-Bus. Usage:
//!
//! ```text
//! cargo run --release -p winbridge-host --example shme2e -- \
//!     --socket /tmp/wbtest.sock --shm-file /dev/shm/wbtest-ctl --mode shm --bulk-mb 1024
//! ```

#[path = "../src/shmfile.rs"]
#[allow(dead_code)]
mod shmfile;

use std::io::{BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use winbridge_proto::ring::{create_region, Endpoint, Inbox, LinkConfig, Pop, Routed, REGION_LEN};
use winbridge_proto::{caps, read_frame, write_frame, ClipboardFormat, Message};

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
const CHUNK: usize = 256 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Shm,
    Serial,
}

struct Args {
    socket: PathBuf,
    shm_file: PathBuf,
    mode: Mode,
    bulk_mb: usize,
    pings: usize,
    work_dir: PathBuf,
    wait_secs: u64,
    clip_wait_secs: u64,
    skip_h2g: bool,
    /// Transport-only host -> guest test: chunks for an unknown transfer id, which the guest
    /// drops without touching the disk (0 = off).
    sink_mb: usize,
    skip_g2h: bool,
    /// Seconds of silence after the idle latency phase, before the restart measurement (0 = off).
    idle_secs: u64,
}

fn parse_args() -> Args {
    let mut a = Args {
        socket: PathBuf::from("/tmp/wbtest.sock"),
        shm_file: PathBuf::from("/dev/shm/wbtest-ctl"),
        mode: Mode::Shm,
        bulk_mb: 1024,
        pings: 2000,
        work_dir: PathBuf::from("/var/tmp/wbe2e"),
        wait_secs: 240,
        clip_wait_secs: 300,
        skip_h2g: false,
        sink_mb: 0,
        skip_g2h: false,
        idle_secs: 0,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().unwrap_or_else(|| die(&format!("{name} needs a value")));
        match arg.as_str() {
            "--socket" => a.socket = val("--socket").into(),
            "--shm-file" => a.shm_file = val("--shm-file").into(),
            "--mode" => {
                a.mode = match val("--mode").as_str() {
                    "shm" => Mode::Shm,
                    "serial" => Mode::Serial,
                    other => die(&format!("unknown mode {other} (shm|serial)")),
                }
            }
            "--bulk-mb" => a.bulk_mb = val("--bulk-mb").parse().unwrap_or_else(|_| die("bad --bulk-mb")),
            "--pings" => a.pings = val("--pings").parse().unwrap_or_else(|_| die("bad --pings")),
            "--work-dir" => a.work_dir = val("--work-dir").into(),
            "--wait" => a.wait_secs = val("--wait").parse().unwrap_or_else(|_| die("bad --wait")),
            "--clip-wait" => a.clip_wait_secs = val("--clip-wait").parse().unwrap_or_else(|_| die("bad --clip-wait")),
            "--skip-h2g" => a.skip_h2g = true,
            "--skip-g2h" => a.skip_g2h = true,
            "--sink-mb" => a.sink_mb = val("--sink-mb").parse().unwrap_or_else(|_| die("bad --sink-mb")),
            "--idle-secs" => a.idle_secs = val("--idle-secs").parse().unwrap_or_else(|_| die("bad --idle-secs")),
            "--help" | "-h" => {
                println!(
                    "shme2e --socket PATH --shm-file PATH --mode shm|serial [--bulk-mb N] [--pings N] \
                     [--work-dir DIR] [--wait SECS] [--clip-wait SECS] [--skip-h2g] [--skip-g2h]"
                );
                std::process::exit(0);
            }
            other => die(&format!("unknown argument {other}")),
        }
    }
    a
}

fn die(msg: &str) -> ! {
    eprintln!("shme2e: {msg}");
    std::process::exit(2);
}

fn now_us() -> u64 {
    EPOCH.elapsed().as_micros() as u64
}

fn pct(v: &[u64], p: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    s[((s.len() - 1) as f64 * p) as usize]
}

fn report(label: &str, rtts: &[u64]) {
    println!(
        "LATENCY {label}: n={} p50={} us p90={} us p99={} us max={} us",
        rtts.len(),
        pct(rtts, 0.5),
        pct(rtts, 0.9),
        pct(rtts, 0.99),
        pct(rtts, 1.0)
    );
}

/// Deterministic test content: xorshift64, little-endian words (the guest generates the same).
fn ensure_file(path: &Path, mib: usize) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() == mib as u64 * (1 << 20)) {
        return;
    }
    let mut f = BufWriter::new(std::fs::File::create(path).unwrap_or_else(|e| die(&format!("{}: {e}", path.display()))));
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut block = vec![0u8; 1 << 20];
    for _ in 0..mib {
        for c in block.chunks_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            c.copy_from_slice(&x.to_le_bytes());
        }
        f.write_all(&block).unwrap();
    }
    f.flush().unwrap();
}

fn sha256(path: &Path) -> String {
    let out = Command::new("sha256sum").arg(path).output().unwrap_or_else(|e| die(&format!("sha256sum: {e}")));
    String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or("").to_string()
}

/// Everything the dispatcher thread learns from the guest.
#[derive(Default)]
struct Shared {
    status: Mutex<Option<String>>,
    ready: Mutex<Option<(bool, String)>>,
    pongs: Mutex<Vec<(u64, u64)>>,
    drop_result: Mutex<Option<(bool, String, Instant)>>,
    clip_offer: Mutex<Option<u32>>,
    g2h: Mutex<G2h>,
}

#[derive(Default)]
struct G2h {
    begun: Option<Instant>,
    ended: Option<(Instant, bool, Option<String>)>,
    bytes: u64,
    chunks: u64,
}

fn dispatcher(ep: Arc<Endpoint>, sh: Arc<Shared>, recv_path: PathBuf, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut out = BufWriter::with_capacity(4 << 20, std::fs::File::create(&recv_path).unwrap());
        let mut next_offset = 0u64;
        while !stop.load(Ordering::Relaxed) {
            let Pop::Msg(msg) = ep.inbox().pop(Duration::from_millis(100)) else { continue };
            match msg {
                Message::Pong { seq, timestamp_ms } => {
                    sh.pongs.lock().unwrap().push((seq, now_us().saturating_sub(timestamp_ms)));
                }
                Message::StatusReply { os_description, .. } => {
                    *sh.status.lock().unwrap() = Some(os_description);
                }
                Message::DropResult { success, message } => {
                    *sh.drop_result.lock().unwrap() = Some((success, message, Instant::now()));
                }
                Message::ClipboardOffer { clip_id, formats } if formats.contains(&ClipboardFormat::FileList) => {
                    *sh.clip_offer.lock().unwrap() = Some(clip_id);
                }
                Message::FilesBegin { total_bytes, .. } => {
                    let mut g = sh.g2h.lock().unwrap();
                    *g = G2h::default();
                    g.begun = Some(Instant::now());
                    println!("G2H stream begins: {} MiB", total_bytes >> 20);
                    out.get_mut().set_len(0).ok();
                    next_offset = 0;
                }
                Message::FileChunk { offset, data, .. } => {
                    if offset != next_offset {
                        eprintln!("G2H chunk out of order: offset {offset}, expected {next_offset}");
                    }
                    out.write_all(&data).unwrap();
                    next_offset = offset + data.len() as u64;
                    let mut g = sh.g2h.lock().unwrap();
                    g.bytes += data.len() as u64;
                    g.chunks += 1;
                }
                Message::FilesEnd { success, error, .. } => {
                    out.flush().unwrap();
                    sh.g2h.lock().unwrap().ended = Some((Instant::now(), success, error));
                }
                _ => {}
            }
        }
    });
}

fn send(ep: &Endpoint, m: Message) {
    if let Routed::Serial(m) = ep.route(m) {
        ep.send_serial(m);
    }
}

/// Pings every millisecond until `stop`; returns the sequence range used.
fn ping_until(ep: Arc<Endpoint>, first: u64, stop: Arc<AtomicBool>, limit: Option<u64>) -> std::thread::JoinHandle<(u64, u64)> {
    std::thread::spawn(move || {
        let mut seq = first;
        while !stop.load(Ordering::Relaxed) && limit.map_or(true, |l| seq - first < l) {
            send(&ep, Message::Ping { seq, timestamp_ms: now_us() });
            seq += 1;
            std::thread::sleep(Duration::from_millis(1));
        }
        (first, seq)
    })
}

fn rtts(sh: &Shared, range: (u64, u64)) -> Vec<u64> {
    sh.pongs.lock().unwrap().iter().filter(|(s, _)| *s >= range.0 && *s < range.1).map(|(_, v)| *v).collect()
}

fn main() {
    let args = parse_args();
    std::fs::create_dir_all(&args.work_dir).unwrap_or_else(|e| die(&format!("{}: {e}", args.work_dir.display())));

    // --- connect (the VM may still be booting) -------------------------------------------------
    let deadline = Instant::now() + Duration::from_secs(args.wait_secs);
    let stream = loop {
        match UnixStream::connect(&args.socket) {
            Ok(s) => break s,
            Err(e) if Instant::now() < deadline => {
                eprintln!("waiting for {}: {e}", args.socket.display());
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(e) => die(&format!("cannot connect to {}: {e}", args.socket.display())),
        }
    };
    let writer = Arc::new(Mutex::new(stream.try_clone().unwrap()));
    let inbox = Inbox::new(256, 1024, 8);
    let ep = Endpoint::new(
        {
            let w = writer.clone();
            Arc::new(move |m| {
                let _ = write_frame(&mut *w.lock().unwrap(), &m);
            })
        },
        inbox,
        LinkConfig::default(),
    );
    let sh = Arc::new(Shared::default());
    let stop = Arc::new(AtomicBool::new(false));
    let recv_path = args.work_dir.join("recv.bin");
    dispatcher(ep.clone(), sh.clone(), recv_path.clone(), stop.clone());
    {
        let ep = ep.clone();
        let sh = sh.clone();
        let mut reader = stream;
        std::thread::spawn(move || loop {
            match read_frame(&mut reader) {
                Ok(m) => match ep.on_serial(m) {
                    Some(Message::ShmReady { ok, reason }) => *sh.ready.lock().unwrap() = Some((ok, reason)),
                    Some(other) => eprintln!("unexpected negotiation message {other:?}"),
                    None => {}
                },
                Err(e) => {
                    eprintln!("serial reader ended: {e}");
                    std::process::exit(3);
                }
            }
        });
    }

    // --- handshake: wait for the agent's status (it announces its capabilities there) ------------
    while sh.status.lock().unwrap().is_none() {
        if Instant::now() > deadline {
            die("the agent did not answer QueryStatus");
        }
        send(&ep, Message::QueryStatus);
        std::thread::sleep(Duration::from_secs(2));
    }
    let status = sh.status.lock().unwrap().clone().unwrap();
    println!("AGENT {status}");
    let cap = caps::parse(&status);

    // --- negotiation -------------------------------------------------------------------------
    let mut _region = None;
    match args.mode {
        Mode::Serial => println!("TRANSPORT serial (baseline, no ShmOffer sent)"),
        Mode::Shm => {
            let Some(cap) = cap else { die("the agent did not announce the shared-memory capability") };
            println!("agent capability: shm protocol {}, instance {:08x}", cap.shm_protocol, cap.instance);
            let map = shmfile::open(&args.shm_file, REGION_LEN).unwrap_or_else(|e| die(&e));
            let region = create_region(map, 1).unwrap_or_else(|e| die(&format!("cannot create the region: {e}")));
            let t0 = Instant::now();
            ep.send_serial(Message::ShmOffer { shm_version: winbridge_shm::VERSION, region_len: REGION_LEN });
            let until = Instant::now() + Duration::from_secs(20);
            let ready = loop {
                if let Some(r) = sh.ready.lock().unwrap().clone() {
                    break r;
                }
                if Instant::now() > until {
                    die("no ShmReady within 20 s");
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            if !ready.0 {
                die(&format!("the agent declined shared memory: {}", ready.1));
            }
            ep.activate(&region).unwrap_or_else(|e| die(&format!("cannot start the link: {e}")));
            println!("TRANSPORT shm active (negotiation {} ms)", t0.elapsed().as_millis());
            _region = Some(region);
        }
    }

    // --- phase 1: control latency, bulk idle -----------------------------------------------------
    let idle = {
        let h = ping_until(ep.clone(), 0, Arc::new(AtomicBool::new(false)), Some(args.pings as u64));
        let range = h.join().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        rtts(&sh, range)
    };
    report("bulk idle", &idle);

    // --- phase 1a: go idle, then restart (first ping, then the steady state) ----------------------
    if args.idle_secs > 0 {
        std::thread::sleep(Duration::from_secs(args.idle_secs));
        let base = args.pings as u64;
        let range = ping_until(ep.clone(), base, Arc::new(AtomicBool::new(false)), Some(args.pings as u64)).join().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let after = rtts(&sh, range);
        println!("IDLE {} s: first ping {} us", args.idle_secs, after.first().copied().unwrap_or_default());
        report("after idle", &after);
    }

    let bulk = args.work_dir.join("bulk.bin");
    let mut h2g_ok = None;
    let mut g2h_ok = None;
    let expect_sha;
    if args.skip_h2g && args.skip_g2h {
        expect_sha = String::new();
    } else {
        ensure_file(&bulk, args.bulk_mb);
        expect_sha = sha256(&bulk);
        println!("SOURCE_SHA256 {expect_sha}");
    }

    // --- phase 1b: transport-only host -> guest stream ---------------------------------------------
    // Chunks for a transfer the guest does not know: it decodes and drops them (no disk), then
    // answers the closing `DropFileFinish` with a failed `DropResult`, which marks the end.
    if args.sink_mb > 0 {
        *sh.drop_result.lock().unwrap() = None;
        let chunk = vec![0x5au8; CHUNK];
        let chunks = args.sink_mb * (1 << 20) / CHUNK;
        let t0 = Instant::now();
        for i in 0..chunks {
            send(&ep, Message::DropFileChunk { transfer_id: 0xdead_beef, offset: (i * CHUNK) as u64, data: chunk.clone() });
        }
        let sent_at = t0.elapsed();
        send(&ep, Message::DropFileFinish { transfer_id: 0xdead_beef, success: true, error: None });
        let until = Instant::now() + Duration::from_secs(600);
        let at = loop {
            if let Some((_, _, at)) = sh.drop_result.lock().unwrap().clone() {
                break at;
            }
            if Instant::now() > until {
                die("no DropResult after the sink stream");
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        let secs = at.duration_since(t0).as_secs_f64();
        println!(
            "SINK H2G {} MiB in {:.3} s = {:.0} MiB/s (all queued after {:.3} s)",
            args.sink_mb,
            secs,
            args.sink_mb as f64 / secs,
            sent_at.as_secs_f64()
        );
        *sh.drop_result.lock().unwrap() = None;
    }

    // --- phase 2: host -> guest stream while pinging --------------------------------------------
    if !args.skip_h2g {
        let total = std::fs::metadata(&bulk).unwrap().len();
        let stop_ping = Arc::new(AtomicBool::new(false));
        let pinger = ping_until(ep.clone(), 1_000_000, stop_ping.clone(), None);
        let t0 = Instant::now();
        send(&ep, Message::DropBegin { transfer_id: 1, x: 0, y: 0, file_count: 1 });
        send(&ep, Message::DropFileStart { transfer_id: 1, operation_id: 1, filename: "wbe2e-bulk.bin".into(), total_bytes: total });
        let mut f = std::fs::File::open(&bulk).unwrap();
        let mut buf = vec![0u8; CHUNK];
        let mut offset = 0u64;
        loop {
            let n = f.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            send(&ep, Message::DropFileChunk { transfer_id: 1, offset, data: buf[..n].to_vec() });
            offset += n as u64;
        }
        let sent_at = t0.elapsed();
        send(&ep, Message::DropFileFinish { transfer_id: 1, success: true, error: None });
        let until = Instant::now() + Duration::from_secs(600);
        let (ok, msg, at) = loop {
            if let Some(r) = sh.drop_result.lock().unwrap().clone() {
                break r;
            }
            if Instant::now() > until {
                die("no DropResult from the guest");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let secs = at.duration_since(t0).as_secs_f64();
        stop_ping.store(true, Ordering::Relaxed);
        let range = pinger.join().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        println!(
            "H2G {} MiB in {:.2} s = {:.0} MiB/s (all queued after {:.2} s) result ok={ok} {msg}",
            total >> 20,
            secs,
            (total >> 20) as f64 / secs,
            sent_at.as_secs_f64()
        );
        report("H2G bulk saturated", &rtts(&sh, range));
        println!("H2G_GUEST_PATH {}", msg.strip_prefix("Saved to ").unwrap_or(&msg));
        h2g_ok = Some(ok);
    }

    // --- phase 3: guest -> host stream while pinging ----------------------------------------------
    if !args.skip_g2h {
        println!("WAIT_CLIPBOARD copy the guest file now");
        let until = Instant::now() + Duration::from_secs(args.clip_wait_secs);
        let clip_id = loop {
            if let Some(id) = *sh.clip_offer.lock().unwrap() {
                break id;
            }
            if Instant::now() > until {
                die("no clipboard file offer from the guest");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let stop_ping = Arc::new(AtomicBool::new(false));
        let pinger = ping_until(ep.clone(), 2_000_000, stop_ping.clone(), None);
        let t0 = Instant::now();
        send(&ep, Message::ClipFilesRequest { clip_id });
        let until = Instant::now() + Duration::from_secs(600);
        let (end_at, ok, err) = loop {
            if let Some(e) = sh.g2h.lock().unwrap().ended.clone() {
                break e;
            }
            if Instant::now() > until {
                die("the guest file stream did not finish");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        stop_ping.store(true, Ordering::Relaxed);
        let range = pinger.join().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let (bytes, begun) = {
            let g = sh.g2h.lock().unwrap();
            (g.bytes, g.begun.unwrap_or(t0))
        };
        let secs = end_at.duration_since(begun).as_secs_f64();
        println!(
            "G2H {} MiB in {:.2} s = {:.0} MiB/s (request to begin {:.2} s) stream ok={ok} {}",
            bytes >> 20,
            secs,
            (bytes >> 20) as f64 / secs,
            begun.duration_since(t0).as_secs_f64(),
            err.unwrap_or_default()
        );
        report("G2H bulk saturated", &rtts(&sh, range));
        let got = sha256(&recv_path);
        let same = got == expect_sha;
        println!("G2H_SHA256 {got} {}", if same { "MATCH" } else { "MISMATCH" });
        g2h_ok = Some(ok && same);
    }

    if let Some(stats) = ep.stats() {
        println!("RING stats {stats:?}");
    }
    println!("RESULT h2g={h2g_ok:?} g2h={g2h_ok:?} (H2G hash: compare H2G_GUEST_PATH with SOURCE_SHA256 in the guest)");
    stop.store(true, Ordering::Relaxed);
    if g2h_ok == Some(false) || h2g_ok == Some(false) {
        std::process::exit(1);
    }
}
