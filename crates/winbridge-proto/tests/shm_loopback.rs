// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared-memory transport tests without a VM: the host and the agent endpoints run on one
//! in-memory region, the serial channel is a pair of in-process queues, and the workload goes
//! through the real `Assembler` / `send_files` path.
//!
//! Run with `cargo test --release -p winbridge-proto --test shm_loopback -- --nocapture` to see
//! the latency numbers. `WB_SHM_TEST_MB` changes the size of the bulk file (default 300).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use winbridge_proto::clip::{Assembler, ClipBundle, FilesEvent};
use winbridge_proto::ring::{
    answer_offer, create_region, Endpoint, Inbox, LinkConfig, Pop, Routed, DRAIN_WINDOW, LANE_BULK_H2G, REGION_LEN, TAG_MESSAGE,
};
use winbridge_proto::{encode_frame, Message};
use winbridge_shm::{HeapMapping, Mapping, Region};

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

fn now_us() -> u64 {
    EPOCH.elapsed().as_micros() as u64
}

struct Node {
    ep: Arc<Endpoint>,
}

struct Rig {
    host: Node,
    guest: Node,
    map: HeapMapping,
    /// Every message that crossed the serial channel, `(from_host, message)`.
    serial_log: Arc<Mutex<Vec<(bool, Message)>>>,
}

fn serial_end(
    tx: Sender<Message>,
    log: Arc<Mutex<Vec<(bool, Message)>>>,
    from_host: bool,
) -> Arc<dyn Fn(Message) + Send + Sync> {
    let tx = Mutex::new(tx);
    Arc::new(move |m: Message| {
        log.lock().unwrap().push((from_host, m.clone()));
        let _ = tx.lock().unwrap().send(m);
    })
}

/// Host and guest endpoints joined by in-process serial queues; the rings are not yet active.
fn rig(cfg: LinkConfig) -> Rig {
    let (h2g_tx, h2g_rx) = channel::<Message>();
    let (g2h_tx, g2h_rx) = channel::<Message>();
    let log = Arc::new(Mutex::new(Vec::new()));
    let host = Endpoint::new(serial_end(h2g_tx, log.clone(), true), Inbox::new(256, 1024, 8), cfg);
    let guest = Endpoint::new(serial_end(g2h_tx, log.clone(), false), Inbox::new(256, 1024, 8), cfg);
    for (rx, to) in [(h2g_rx, guest.clone()), (g2h_rx, host.clone())] {
        std::thread::spawn(move || {
            for m in rx {
                // Negotiation is driven by the tests themselves.
                let _ = to.on_serial(m);
            }
        });
    }
    Rig {
        host: Node { ep: host },
        guest: Node { ep: guest },
        map: HeapMapping::new(REGION_LEN as usize),
        serial_log: log,
    }
}

impl Rig {
    /// Host creates the region and activates, the agent answers the offer.
    fn activate(&self) -> Region<HeapMapping> {
        let region = create_region(self.map.clone(), 1).unwrap();
        let map = self.map.clone();
        let reply = answer_offer(&self.guest.ep, winbridge_shm::VERSION, REGION_LEN, move |_| Ok(map));
        assert!(matches!(reply, Message::ShmReady { ok: true, .. }), "{reply:?}");
        self.host.ep.activate(&region).unwrap();
        assert!(self.host.ep.is_active() && self.guest.ep.is_active());
        region
    }
}

fn send(ep: &Endpoint, m: Message) {
    if let Routed::Serial(m) = ep.route(m) {
        ep.send_serial(m);
    }
}

fn pct(v: &[u64], p: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    s[((s.len() - 1) as f64 * p) as usize]
}

fn mix(h: u64, w: u64) -> u64 {
    (h ^ w).wrapping_mul(0x0000_0100_0000_01b3).rotate_left(5)
}

fn hash_file(path: &Path) -> u64 {
    use std::io::Read;
    let mut f = std::fs::File::open(path).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    loop {
        let n = f.read(&mut buf).unwrap();
        if n == 0 {
            return h;
        }
        for c in buf[..n].chunks(8) {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            h = mix(h, u64::from_le_bytes(w));
        }
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("wb-shm-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn make_file(path: &Path, mib: usize) {
    use std::io::Write;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
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

/// What the guest dispatcher saw of a file stream.
#[derive(Default)]
struct Outcome {
    done: Option<Vec<PathBuf>>,
    error: Option<String>,
    bytes: u64,
}

/// The agent's dispatch loop in miniature: control first, `Ping` answered, files assembled.
fn guest_dispatcher(
    ep: Arc<Endpoint>,
    staging: PathBuf,
    out: Arc<Mutex<Outcome>>,
    stop: Arc<AtomicBool>,
    chunk_delay: Duration,
) {
    std::thread::spawn(move || {
        let mut asm = Assembler::new(staging);
        while !stop.load(Ordering::Relaxed) {
            let Pop::Msg(msg) = ep.inbox().pop(Duration::from_millis(50)) else { continue };
            match &msg {
                Message::Ping { seq, timestamp_ms } => {
                    send(&ep, Message::Pong { seq: *seq, timestamp_ms: *timestamp_ms })
                }
                Message::ClipboardOffer { .. } | Message::ClipFilesManifest { .. } | Message::ClipboardItem { .. } => {
                    asm.handle(&msg).unwrap();
                }
                Message::FileChunk { data, .. } => {
                    std::thread::sleep(chunk_delay);
                    out.lock().unwrap().bytes += data.len() as u64;
                    if let Err(e) = asm.handle_files(&msg) {
                        out.lock().unwrap().error.get_or_insert(e);
                    }
                }
                Message::FilesBegin { .. } | Message::FileEntry { .. } | Message::FilesEnd { .. } => {
                    match asm.handle_files(&msg) {
                        Ok(FilesEvent::Done { paths, .. }) => out.lock().unwrap().done = Some(paths),
                        Ok(_) => {}
                        Err(e) => {
                            out.lock().unwrap().error.get_or_insert(e);
                        }
                    }
                }
                _ => {}
            }
        }
    });
}

/// The host dispatch loop: collects Pong round trips as `(seq, microseconds)`.
fn host_dispatcher(ep: Arc<Endpoint>, rtts: Arc<Mutex<Vec<(u64, u64)>>>, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if let Pop::Msg(Message::Pong { seq, timestamp_ms }) = ep.inbox().pop(Duration::from_millis(50)) {
                rtts.lock().unwrap().push((seq, now_us().saturating_sub(timestamp_ms)));
            }
        }
    });
}

fn start_pings(ep: Arc<Endpoint>, first_seq: u64, count: Option<u64>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<u64> {
    std::thread::spawn(move || {
        let mut seq = first_seq;
        while !stop.load(Ordering::Relaxed) && count.map_or(true, |c| seq < first_seq + c) {
            send(&ep, Message::Ping { seq, timestamp_ms: now_us() });
            seq += 1;
            std::thread::sleep(Duration::from_millis(1));
        }
        seq
    })
}

fn rtts_between(r: &Mutex<Vec<(u64, u64)>>, lo: u64, hi: u64) -> Vec<u64> {
    r.lock().unwrap().iter().filter(|(s, _)| *s >= lo && *s < hi).map(|(_, v)| *v).collect()
}

fn mib() -> usize {
    std::env::var("WB_SHM_TEST_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(300)
}

/// Sends a file as the host does: snapshot offer, manifest, then the stream.
fn send_file(ep: &Arc<Endpoint>, clip_id: u32, src: &Path) -> Result<(), String> {
    let bundle = ClipBundle { files: vec![src.to_path_buf()], ..Default::default() };
    bundle.send(clip_id, &mut |m| {
        send(ep, m);
        Ok(())
    })?;
    winbridge_proto::clip::send_files(
        clip_id,
        &[src.to_path_buf()],
        &mut |m| {
            send(ep, m);
            Ok(())
        },
        |_, _| {},
    )
}

#[test]
fn control_latency_stays_bounded_under_a_bulk_stream() {
    let size_mib = mib();
    let dir = TempDir::new("a");
    let src = dir.0.join("big.bin");
    make_file(&src, size_mib);
    let want = hash_file(&src);

    let rig = rig(LinkConfig::default());
    let _region = rig.activate();
    let stop = Arc::new(AtomicBool::new(false));
    let outcome = Arc::new(Mutex::new(Outcome::default()));
    let rtts = Arc::new(Mutex::new(Vec::new()));
    guest_dispatcher(rig.guest.ep.clone(), dir.0.join("staging"), outcome.clone(), stop.clone(), Duration::ZERO);
    host_dispatcher(rig.host.ep.clone(), rtts.clone(), stop.clone());

    // Bulk off: 500 pings on an idle link.
    let idle_stop = Arc::new(AtomicBool::new(false));
    let end = start_pings(rig.host.ep.clone(), 0, Some(500), idle_stop).join().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let idle = rtts_between(&rtts, 0, end);
    assert_eq!(idle.len(), 500, "every idle ping must be answered");

    // Bulk saturated: the file streams while pings continue.
    let done_sending = Arc::new(AtomicBool::new(false));
    let pinger = start_pings(rig.host.ep.clone(), 1_000_000, None, done_sending.clone());
    let started = Instant::now();
    let host_ep = rig.host.ep.clone();
    let src2 = src.clone();
    let sender = std::thread::spawn(move || send_file(&host_ep, 7, &src2));
    sender.join().unwrap().unwrap();
    // The stream is complete once the guest assembled it.
    let deadline = Instant::now() + Duration::from_secs(120);
    while outcome.lock().unwrap().done.is_none() && outcome.lock().unwrap().error.is_none() {
        assert!(Instant::now() < deadline, "the transfer hung");
        std::thread::sleep(Duration::from_millis(5));
    }
    let secs = started.elapsed().as_secs_f64();
    done_sending.store(true, Ordering::Relaxed);
    let last = pinger.join().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let loaded = rtts_between(&rtts, 1_000_000, last);

    let out = outcome.lock().unwrap();
    assert!(out.error.is_none(), "transfer failed: {:?}", out.error);
    let staged = &out.done.as_ref().unwrap()[0];
    assert_eq!(hash_file(staged), want, "the staged file differs");
    assert_eq!(out.bytes, size_mib as u64 * (1 << 20));

    println!(
        "bulk {size_mib} MiB in {secs:.2} s = {:.0} MiB/s over the ring",
        size_mib as f64 / secs
    );
    println!(
        "control RTT, bulk idle  ({:>5} pings): p50 {} us, p99 {} us, max {} us",
        idle.len(),
        pct(&idle, 0.5),
        pct(&idle, 0.99),
        pct(&idle, 1.0)
    );
    println!(
        "control RTT, bulk 100%  ({:>5} pings): p50 {} us, p99 {} us, max {} us",
        loaded.len(),
        pct(&loaded, 0.5),
        pct(&loaded, 0.99),
        pct(&loaded, 1.0)
    );
    assert!(loaded.len() > 20, "too few pings answered under load: {}", loaded.len());
    // A control record never waits behind bulk data: the bound is a few scheduling quanta plus
    // the cost of one bulk chunk in the dispatcher, far below what a FIFO would give.
    assert!(
        pct(&loaded, 0.99) < 50_000,
        "control p99 under bulk load {} us",
        pct(&loaded, 0.99)
    );
    let stats = rig.host.ep.stats().unwrap();
    assert_eq!(stats.bad_records, 0);
    assert!(stats.tx_bulk > 1000);
    stop.store(true, Ordering::Relaxed);
}

#[test]
fn corrupt_lane_falls_back_and_the_transfer_ends_cleanly() {
    let dir = TempDir::new("b");
    let src = dir.0.join("f.bin");
    make_file(&src, 64);
    let rig = rig(LinkConfig::default());
    let region = rig.activate();
    let stop = Arc::new(AtomicBool::new(false));
    let outcome = Arc::new(Mutex::new(Outcome::default()));
    guest_dispatcher(rig.guest.ep.clone(), dir.0.join("staging"), outcome.clone(), stop.clone(), Duration::from_millis(2));

    let host_ep = rig.host.ep.clone();
    let src2 = src.clone();
    let (done_tx, done_rx) = channel();
    std::thread::spawn(move || {
        let r = send_file(&host_ep, 9, &src2);
        let _ = done_tx.send(r);
    });

    // Corrupt the bulk host-to-guest ring while the (slow) guest is far behind the producer: a
    // 1 MiB stretch of not yet consumed data becomes 0xFF, so the next record header the guest
    // reaches claims a length of 4 GiB. Published data is never rewritten by the producer, so
    // the damage cannot be repaired by a later index store.
    let lane = *region.lanes().iter().find(|l| l.id == LANE_BULK_H2G).unwrap();
    let word = |off: usize| {
        // SAFETY: header words of a lane inside the mapping (validated by `create`).
        unsafe { std::ptr::read_volatile(rig.map.base().add(lane.offset + off) as *const u64) }
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while word(0).saturating_sub(word(64)) < 6 << 20 {
        assert!(Instant::now() < deadline, "the producer never got ahead of the consumer");
        std::thread::sleep(Duration::from_millis(1));
    }
    let start = ((word(64) + (2 << 20)) as usize) & (lane.size - 1);
    assert!(start + (1 << 20) <= lane.size, "the damaged stretch wraps; rerun");
    // SAFETY: inside the lane's data area (checked above), which follows the 128-byte header.
    unsafe {
        std::ptr::write_bytes(rig.map.base().add(lane.offset + winbridge_shm::RING_HEADER_LEN + start), 0xFF, 1 << 20);
    }

    // Whatever happens next, nothing hangs: the sender finishes and the guest either assembled
    // the file (all data survived) or reports a clean error.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        {
            let o = outcome.lock().unwrap();
            if o.done.is_some() || o.error.is_some() {
                break;
            }
        }
        assert!(Instant::now() < deadline, "the transfer hung after the corruption");
        std::thread::sleep(Duration::from_millis(5));
    }
    match done_rx.recv_timeout(Duration::from_secs(60)) {
        Ok(_) | Err(RecvTimeoutError::Disconnected) => {}
        Err(RecvTimeoutError::Timeout) => panic!("the sender hung after the corruption"),
    }
    let announced = || {
        rig.serial_log
            .lock()
            .unwrap()
            .iter()
            .any(|(from_host, m)| !*from_host && matches!(m, Message::ShmFallback { .. }))
    };
    let wait = Instant::now() + Duration::from_secs(5);
    while !announced() && Instant::now() < wait {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(announced(), "the guest must announce ShmFallback on serial");
    std::thread::sleep(DRAIN_WINDOW / 4);
    assert!(!rig.guest.ep.is_active() && !rig.host.ep.is_active(), "both ends are back on serial");
    let o = outcome.lock().unwrap();
    if let Some(paths) = &o.done {
        assert_eq!(hash_file(&paths[0]), hash_file(&src), "a completed transfer must be intact");
    }
    println!("corruption outcome: done={} error={:?}", o.done.is_some(), o.error);
    // After the fallback new traffic flows over serial.
    send(&rig.host.ep, Message::Ping { seq: 5, timestamp_ms: 0 });
    stop.store(true, Ordering::Relaxed);
}

#[test]
fn invalid_records_are_counted_and_the_limit_ends_the_link() {
    let rig = rig(LinkConfig { bad_record_limit: 3, ..LinkConfig::default() });
    // The test plays the host's writer by hand; only the guest runs a link.
    let region = create_region(rig.map.clone(), 1).unwrap();
    let map = rig.map.clone();
    let reply = answer_offer(&rig.guest.ep, winbridge_shm::VERSION, REGION_LEN, move |_| Ok(map));
    assert!(matches!(reply, Message::ShmReady { ok: true, .. }));
    let mut ctl = region.producer(1).unwrap();
    let mut bulk = region.producer(3).unwrap();
    let good = encode_frame(&Message::QueryStatus).unwrap();

    let _ = ctl.try_push_tagged(TAG_MESSAGE, 0, b"not a frame").unwrap();
    // A bulk message on the control lane, and a control message on the bulk lane.
    let _ = ctl.try_push_tagged(TAG_MESSAGE, 0, &encode_frame(&Message::FilesEnd { stream_id: 1, success: true, error: None }).unwrap()).unwrap();
    let _ = bulk.try_push_tagged(TAG_MESSAGE, 0, &good).unwrap();
    let _ = ctl.try_push_tagged(TAG_MESSAGE, 0, &good).unwrap();
    let Pop::Msg(m) = rig.guest.ep.inbox().pop(Duration::from_secs(5)) else { panic!("no message") };
    assert_eq!(m, Message::QueryStatus, "valid records still arrive");
    assert!(rig.guest.ep.is_active(), "three bad records are within the limit");

    let _ = ctl.try_push_tagged(7, 0, b"unknown tag").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while rig.guest.ep.is_active() {
        assert!(Instant::now() < deadline, "the link must end after too many bad records");
        std::thread::sleep(Duration::from_millis(2));
    }
    let log = rig.serial_log.lock().unwrap();
    assert!(log.iter().any(|(h, m)| !*h && matches!(m, Message::ShmFallback { .. })));
}

#[test]
fn hostile_region_is_refused_without_panicking() {
    let rig = rig(LinkConfig::default());
    // Garbage where the header should be.
    let map = HeapMapping::new(1 << 20);
    // SAFETY: the heap block is at least 4096 bytes long.
    unsafe { std::ptr::write_bytes(map.base(), 0xA5, 4096) };
    let m = map.clone();
    let reply = answer_offer(&rig.guest.ep, winbridge_shm::VERSION, 1 << 20, move |_| Ok(m));
    assert!(matches!(reply, Message::ShmReady { ok: false, .. }), "{reply:?}");
    // An unwritten (host not finished) region.
    let blank = HeapMapping::new(1 << 20);
    let reply = answer_offer(&rig.guest.ep, winbridge_shm::VERSION, 1 << 20, move |_| Ok(blank));
    assert!(matches!(reply, Message::ShmReady { ok: false, .. }));
    // A layout version the agent does not know, and a device that cannot be mapped.
    let reply = answer_offer(&rig.guest.ep, 99, REGION_LEN, |_| -> Result<HeapMapping, String> { unreachable!() });
    assert!(matches!(reply, Message::ShmReady { ok: false, .. }));
    let reply = answer_offer(&rig.guest.ep, winbridge_shm::VERSION, REGION_LEN, |_| -> Result<HeapMapping, String> {
        Err("no device".into())
    });
    assert!(matches!(&reply, Message::ShmReady { ok: false, reason } if reason == "no device"));
    assert!(!rig.guest.ep.is_active());
}

#[test]
fn messages_take_the_lane_of_their_class() {
    let rig = rig(LinkConfig::default());
    // No link: the caller writes to serial.
    assert!(matches!(rig.guest.ep.route(Message::QueryStatus), Routed::Serial(_)));
    let region = create_region(rig.map.clone(), 1).unwrap();
    let map = rig.map.clone();
    let reply = answer_offer(&rig.guest.ep, winbridge_shm::VERSION, REGION_LEN, move |_| Ok(map));
    assert!(matches!(reply, Message::ShmReady { ok: true, .. }));
    assert!(matches!(rig.guest.ep.route(Message::QueryStatus), Routed::Sent));
    assert!(matches!(
        rig.guest.ep.route(Message::FilesEnd { stream_id: 1, success: true, error: None }),
        Routed::Sent
    ));
    // Negotiation messages and oversized control messages stay on serial.
    assert!(matches!(rig.guest.ep.route(Message::ShmKick), Routed::Serial(_)));
    let huge = Message::DropText { text: "x".repeat(1 << 20), x: 0, y: 0 };
    assert!(matches!(rig.guest.ep.route(huge), Routed::Serial(_)));
    // The host side of the region sees them on lanes 2 (control) and 4 (bulk).
    let mut ctl = region.consumer(2).unwrap();
    let mut bulk = region.consumer(4).unwrap();
    let (mut a, mut b) = (Vec::new(), Vec::new());
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut got_ctl, mut got_bulk) = (None, None);
    while (got_ctl.is_none() || got_bulk.is_none()) && Instant::now() < deadline {
        got_ctl = got_ctl.or(ctl.try_pop(&mut a).unwrap());
        got_bulk = got_bulk.or(bulk.try_pop(&mut b).unwrap());
    }
    assert_eq!(winbridge_proto::decode_frame(&a).unwrap(), Message::QueryStatus);
    assert!(matches!(winbridge_proto::decode_frame(&b).unwrap(), Message::FilesEnd { .. }));
}

#[test]
fn ping_fast_path_bypasses_a_busy_dispatcher() {
    let dir = TempDir::new("fast");
    let src = dir.0.join("f.bin");
    make_file(&src, 16);
    let rig = rig(LinkConfig::default());
    let _region = rig.activate();
    let guest = rig.guest.ep.clone();
    guest.set_fast_path(Arc::new({
        let g = guest.clone();
        move |m| match m {
            Message::Ping { seq, timestamp_ms } => {
                send(&g, Message::Pong { seq: *seq, timestamp_ms: *timestamp_ms });
                true
            }
            _ => false,
        }
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let outcome = Arc::new(Mutex::new(Outcome::default()));
    let rtts = Arc::new(Mutex::new(Vec::new()));
    // The dispatcher needs 20 ms per file chunk, like a disk that throttles writes.
    guest_dispatcher(guest.clone(), dir.0.join("staging"), outcome.clone(), stop.clone(), Duration::from_millis(20));
    host_dispatcher(rig.host.ep.clone(), rtts.clone(), stop.clone());
    let done = Arc::new(AtomicBool::new(false));
    let pinger = start_pings(rig.host.ep.clone(), 0, None, done.clone());
    send_file(&rig.host.ep, 3, &src).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while outcome.lock().unwrap().done.is_none() && outcome.lock().unwrap().error.is_none() {
        assert!(Instant::now() < deadline, "the transfer hung");
        std::thread::sleep(Duration::from_millis(5));
    }
    done.store(true, Ordering::Relaxed);
    let last = pinger.join().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let r = rtts_between(&rtts, 0, last);
    println!("fast path, dispatcher stalled 20 ms per chunk: {} pings, p50 {} us, p99 {} us, max {} us",
        r.len(), pct(&r, 0.5), pct(&r, 0.99), pct(&r, 1.0));
    assert!(r.len() > 200);
    assert!(pct(&r, 0.99) < 5_000, "pings must not wait for the busy dispatcher");
    assert!(outcome.lock().unwrap().error.is_none());
    stop.store(true, Ordering::Relaxed);
}

/// Ring plus framing only: no disk, no hashing. The receiver counts and drops the chunks.
#[test]
fn raw_bulk_throughput() {
    let mib: usize = std::env::var("WB_SHM_TEST_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(2048);
    let rig = rig(LinkConfig::default());
    let _region = rig.activate();
    const CHUNK: usize = 256 * 1024;
    let chunks = mib * (1 << 20) / CHUNK;
    let guest = rig.guest.ep.clone();
    let receiver = std::thread::spawn(move || {
        let mut got = 0u64;
        let mut n = 0usize;
        while n < chunks {
            if let Pop::Msg(Message::FileChunk { data, .. }) = guest.inbox().pop(Duration::from_millis(500)) {
                got += data.len() as u64;
                n += 1;
            }
        }
        got
    });
    let payload = vec![0xa5u8; CHUNK];
    let t0 = Instant::now();
    for i in 0..chunks {
        send(&rig.host.ep, Message::FileChunk { stream_id: 9, index: 0, offset: (i * CHUNK) as u64, data: payload.clone() });
    }
    let sent = t0.elapsed();
    let got = receiver.join().unwrap();
    let secs = t0.elapsed().as_secs_f64();
    println!(
        "raw bulk {} MiB in {:.3} s = {:.0} MiB/s (all queued after {:.3} s)",
        got >> 20,
        secs,
        (got >> 20) as f64 / secs,
        sent.as_secs_f64()
    );
    assert_eq!(got, (chunks * CHUNK) as u64);
}

/// A viewer that reconnects starts with `QueryStatus` on serial while the agent still holds the
/// old link: the agent must leave it, or its `StatusReply` would go into a ring nobody reads.
#[test]
fn query_status_on_serial_ends_a_stale_link() {
    let rig = rig(LinkConfig::default());
    let _region = rig.activate();
    assert!(rig.guest.ep.is_active());
    assert!(rig.guest.ep.on_serial(Message::QueryStatus).is_none());
    assert!(!rig.guest.ep.is_active());
    // The query itself still reaches the dispatcher, and the next offer can be answered.
    assert!(matches!(rig.guest.ep.inbox().pop(Duration::from_secs(1)), Pop::Msg(Message::QueryStatus)));
    let map = rig.map.clone();
    let reply = answer_offer(&rig.guest.ep, winbridge_shm::VERSION, REGION_LEN, move |_| Ok(map));
    assert!(matches!(reply, Message::ShmReady { ok: true, .. }), "{reply:?}");
}
