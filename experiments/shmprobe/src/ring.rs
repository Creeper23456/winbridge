// SPDX-License-Identifier: GPL-3.0-or-later

//! End-to-end test of the `winbridge-shm` ring over a device: one region per cache mode, created
//! by the host and attached by the guest, with four lanes:
//!
//! | id | direction | use |
//! |---|---|---|
//! | 1 | guest to host | bulk records of mixed size with checksums |
//! | 2 | host to guest | same, other direction |
//! | 3 | guest to host | ping (8 bytes) |
//! | 4 | host to guest | pong (echo) |
//!
//! Phases per round: A guest produces / host consumes, B host produces / guest consumes,
//! C ping-pong with both sides spinning, D ping-pong where the host sleeps 1 ms between polls.
//! The two sides run the same script; the region's capability word carries the round number so a
//! guest never attaches to the previous round's region.

use std::sync::Arc;
use std::time::{Duration, Instant};

use winbridge_shm::{
    Consumer, Direction, LaneSpec, Mapping, PopError, Priority, Producer, PushError, Region, Role, Side,
};

use crate::dev::{Dev, Map, Mode, Shared};
use crate::util::{cpu_time, mib_s, mix, pct, wait_for};

const L_G2H: u16 = 1;
const L_H2G: u16 = 2;
const L_PING: u16 = 3;
const L_PONG: u16 = 4;
const POOL: usize = 1 << 20;
const MAX_PAYLOAD: usize = 64 << 10;
const FIRST_TIMEOUT: Duration = Duration::from_secs(180);
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
pub struct Params {
    pub records: u64,
    pub pings: u32,
    pub sleep_pings: u32,
    pub ring_mib: usize,
}

fn specs(p: &Params) -> [LaneSpec; 4] {
    let bulk = p.ring_mib << 20;
    let lane = |id, direction, priority, data_size| LaneSpec { id, direction, priority, flags: 0, data_size };
    [
        lane(L_G2H, Direction::GuestToHost, Priority::Bulk, bulk),
        lane(L_H2G, Direction::HostToGuest, Priority::Bulk, bulk),
        lane(L_PING, Direction::GuestToHost, Priority::Control, 64 << 10),
        lane(L_PONG, Direction::HostToGuest, Priority::Control, 64 << 10),
    ]
}

/// Deterministic record source/checker. Record `seq` of a stream has a length drawn from
/// 80 % 16 B..1 KiB, 15 % 1..16 KiB, 5 % 16..64 KiB; the payload is
/// `seq (8) | body | checksum(body) ^ seq (8)`, the tag is `seq as u16`.
struct Gen {
    pool: Vec<u8>,
    seed: u64,
}

fn csum(b: &[u8]) -> u64 {
    let mut a = [0u64; 4];
    let mut chunks = b.chunks_exact(32);
    for c in &mut chunks {
        for (i, acc) in a.iter_mut().enumerate() {
            let w = u64::from_le_bytes(c[i * 8..i * 8 + 8].try_into().unwrap());
            *acc = (*acc ^ w).rotate_left(13).wrapping_add(w);
        }
    }
    let mut tail = 0u64;
    for (i, &x) in chunks.remainder().iter().enumerate() {
        tail = tail.rotate_left(8) ^ x as u64 ^ (i as u64) << 56;
    }
    a[0] ^ a[1].rotate_left(1) ^ a[2].rotate_left(2) ^ a[3].rotate_left(3) ^ tail ^ b.len() as u64
}

impl Gen {
    fn new(seed: u64) -> Self {
        let mut pool = vec![0u8; POOL];
        for (i, c) in pool.chunks_exact_mut(8).enumerate() {
            c.copy_from_slice(&mix(i as u64, 0xAB).to_le_bytes());
        }
        Self { pool, seed }
    }

    /// (payload length, pool offset of the body)
    fn plan(&self, seq: u64) -> (usize, usize) {
        let r = mix(seq, self.seed);
        let class = r % 100;
        let x = (r >> 8) as usize;
        let len = if class < 80 {
            16 + x % (1024 - 16)
        } else if class < 95 {
            1024 + x % (15 * 1024)
        } else {
            16 * 1024 + x % (48 * 1024 - 16)
        };
        let off = ((r >> 32) as usize % (POOL - MAX_PAYLOAD)) & !7;
        (len, off)
    }

    fn build(&self, seq: u64, buf: &mut Vec<u8>) -> u16 {
        let (len, off) = self.plan(seq);
        let body = &self.pool[off..off + len - 16];
        buf.clear();
        buf.extend_from_slice(&seq.to_le_bytes());
        buf.extend_from_slice(body);
        buf.extend_from_slice(&(csum(body) ^ seq).to_le_bytes());
        seq as u16
    }

    fn check(&self, seq: u64, tag: u16, p: &[u8]) -> Result<(), String> {
        let (len, _) = self.plan(seq);
        if p.len() != len {
            return Err(format!("record {seq}: length {} expected {len}", p.len()));
        }
        if tag != seq as u16 {
            return Err(format!("record {seq}: tag {tag}"));
        }
        let got_seq = u64::from_le_bytes(p[..8].try_into().unwrap());
        if got_seq != seq {
            return Err(format!("record {seq}: sequence field {got_seq}"));
        }
        let body = &p[8..len - 8];
        let want = csum(body) ^ seq;
        let have = u64::from_le_bytes(p[len - 8..].try_into().unwrap());
        if want != have {
            return Err(format!("record {seq}: checksum mismatch (len {len})"));
        }
        Ok(())
    }
}

struct Phase {
    records: u64,
    bytes: u64,
    secs: f64,
    cpu: f64,
}

fn report(side: &str, round: u64, mode: &str, name: &str, ph: &Phase) {
    println!(
        "RING {side} round={round} mode={mode} phase={name} records={} bytes={} secs={:.3} MiB/s={:.1} records/s={:.0} cpu%={:.0}",
        ph.records,
        ph.bytes,
        ph.secs,
        mib_s(ph.bytes, ph.secs),
        ph.records as f64 / ph.secs,
        100.0 * ph.cpu / ph.secs
    );
}

fn produce(tx: &mut Producer<Shared>, g: &Gen, n: u64) -> Result<Phase, String> {
    let mut buf = Vec::with_capacity(MAX_PAYLOAD + 64);
    let (t0, c0) = (Instant::now(), cpu_time());
    let mut bytes = 0u64;
    let mut stall = Instant::now();
    for seq in 0..n {
        let tag = g.build(seq, &mut buf);
        let mut spins = 0u32;
        loop {
            match tx.try_push_tagged(tag, 0, &buf) {
                Ok(_) => {
                    stall = Instant::now();
                    break;
                }
                Err(PushError::Full) => {
                    spins = spins.wrapping_add(1);
                    if spins % 4096 == 0 && stall.elapsed() > STALL_TIMEOUT {
                        return Err(format!("producer stalled at record {seq} (consumer gone?)"));
                    }
                    std::hint::spin_loop();
                }
                Err(e) => return Err(format!("push {seq}: {e}")),
            }
        }
        bytes += buf.len() as u64;
    }
    Ok(Phase { records: n, bytes, secs: t0.elapsed().as_secs_f64(), cpu: (cpu_time() - c0).as_secs_f64() })
}

fn consume(rx: &mut Consumer<Shared>, g: &Gen, n: u64) -> Result<Phase, String> {
    let mut buf = vec![0u8; MAX_PAYLOAD + 64];
    let mut started: Option<(Instant, Duration)> = None;
    let mut bytes = 0u64;
    let mut stall = Instant::now();
    for seq in 0..n {
        let mut spins = 0u32;
        loop {
            match rx.try_pop_into(&mut buf) {
                Ok(Some(m)) => {
                    started.get_or_insert_with(|| (Instant::now(), cpu_time()));
                    stall = Instant::now();
                    g.check(seq, m.tag, &buf[..m.len])?;
                    bytes += m.len as u64;
                    break;
                }
                Ok(None) => {
                    spins = spins.wrapping_add(1);
                    let limit = if started.is_some() { STALL_TIMEOUT } else { FIRST_TIMEOUT };
                    if spins % 4096 == 0 && stall.elapsed() > limit {
                        return Err(format!("consumer stalled waiting for record {seq} (producer gone?)"));
                    }
                    std::hint::spin_loop();
                }
                Err(PopError::BufferTooSmall { .. }) => return Err(format!("record {seq}: buffer too small")),
                Err(e) => return Err(format!("pop {seq}: {e}")),
            }
        }
    }
    let (t0, c0) = started.ok_or("no records")?;
    Ok(Phase { records: n, bytes, secs: t0.elapsed().as_secs_f64(), cpu: (cpu_time() - c0).as_secs_f64() })
}

fn push_blocking(tx: &mut Producer<Shared>, payload: &[u8]) -> Result<(), String> {
    let t = Instant::now();
    loop {
        match tx.try_push(payload) {
            Ok(_) => return Ok(()),
            Err(PushError::Full) if t.elapsed() < STALL_TIMEOUT => std::hint::spin_loop(),
            Err(e) => return Err(format!("push: {e}")),
        }
    }
}

/// Guest side of C/D: send `n` pings, time each round trip (ns).
fn ping_client(tx: &mut Producer<Shared>, rx: &mut Consumer<Shared>, n: u32) -> Result<(Vec<u32>, f64, f64), String> {
    let mut buf = [0u8; 64];
    let mut out = Vec::with_capacity(n as usize);
    let (t0, c0) = (Instant::now(), cpu_time());
    for i in 0..n {
        let t = Instant::now();
        push_blocking(tx, &(i as u64).to_le_bytes())?;
        let mut spins = 0u32;
        loop {
            match rx.try_pop_into(&mut buf) {
                Ok(Some(m)) => {
                    if m.len != 8 || u64::from_le_bytes(buf[..8].try_into().unwrap()) != i as u64 {
                        return Err(format!("pong {i}: wrong payload"));
                    }
                    break;
                }
                Ok(None) => {
                    spins = spins.wrapping_add(1);
                    if spins % 65536 == 0 && t.elapsed() > STALL_TIMEOUT {
                        return Err(format!("pong {i} never arrived"));
                    }
                    std::hint::spin_loop();
                }
                Err(e) => return Err(format!("pong {i}: {e}")),
            }
        }
        out.push(t.elapsed().as_nanos().min(u32::MAX as u128) as u32);
    }
    Ok((out, t0.elapsed().as_secs_f64(), (cpu_time() - c0).as_secs_f64()))
}

/// Host side of C/D: echo `n` pings. `sleep` makes the host sleep 1 ms whenever the lane is empty.
fn ping_server(rx: &mut Consumer<Shared>, tx: &mut Producer<Shared>, n: u32, sleep: bool) -> Result<(f64, f64), String> {
    let mut buf = [0u8; 64];
    let (t0, c0) = (Instant::now(), cpu_time());
    let mut idle = Instant::now();
    let mut first = true;
    let mut done = 0;
    while done < n {
        match rx.try_pop_into(&mut buf) {
            Ok(Some(m)) => {
                if first {
                    first = false;
                }
                push_blocking(tx, &buf[..m.len])?;
                done += 1;
                idle = Instant::now();
            }
            Ok(None) => {
                let limit = if first { FIRST_TIMEOUT } else { STALL_TIMEOUT };
                if idle.elapsed() > limit {
                    return Err("ping client gone".into());
                }
                if sleep {
                    std::thread::sleep(Duration::from_millis(1));
                } else {
                    std::hint::spin_loop();
                }
            }
            Err(e) => return Err(format!("ping: {e}")),
        }
    }
    Ok((t0.elapsed().as_secs_f64(), (cpu_time() - c0).as_secs_f64()))
}

fn print_pings(name: &str, mode: &str, round: u64, mut v: Vec<u32>, wall: f64, cpu: f64) {
    let skip = (v.len() / 100).min(100);
    let worst_incl = v.iter().copied().max().unwrap_or(0);
    let mut s: Vec<u32> = v.split_off(skip);
    s.sort_unstable();
    let mean = s.iter().map(|&x| x as f64).sum::<f64>() / s.len().max(1) as f64;
    let us = |x: u32| x as f64 / 1000.0;
    println!(
        "PING guest round={round} mode={mode} phase={name} n={} min_us={:.2} p50_us={:.2} p90_us={:.2} p99_us={:.2} p99.9_us={:.2} max_us={:.2} max_incl_warmup_us={:.2} mean_us={:.2} cpu%={:.0}",
        s.len(),
        us(s.first().copied().unwrap_or(0)),
        us(pct(&s, 50.0)),
        us(pct(&s, 90.0)),
        us(pct(&s, 99.0)),
        us(pct(&s, 99.9)),
        us(s.last().copied().unwrap_or(0)),
        us(worst_incl),
        mean / 1000.0,
        100.0 * cpu / wall
    );
}

/// Guest role: one round per cache mode. Returns Ok(true) if every record verified.
pub fn guest(dev: &Dev, modes: &[Mode], p: Params) -> Result<bool, String> {
    let g_gen = Gen::new(0x6701);
    let h_gen = Gen::new(0x4801);
    for (i, &mode) in modes.iter().enumerate() {
        let round = i as u64 + 1;
        let m = mode.name();
        let map = Arc::new(dev.map(mode)?);
        let mut region = None;
        let deadline = FIRST_TIMEOUT;
        if !wait_for(deadline, || match Region::attach(Shared(map.clone()), Side::Guest) {
            Ok(r) if r.capabilities() == round => {
                region = Some(r);
                true
            }
            _ => false,
        }) {
            return Err(format!("round {round}: region of the host never appeared (is `shmprobe host ring` running?)"));
        }
        let region = region.expect("set by wait_for");
        let (mut tx, mut rx) = (region.producer(L_G2H).map_err(|e| e.to_string())?, region.consumer(L_H2G).map_err(|e| e.to_string())?);
        let (mut ptx, mut prx) = (region.producer(L_PING).map_err(|e| e.to_string())?, region.consumer(L_PONG).map_err(|e| e.to_string())?);
        let a = produce(&mut tx, &g_gen, p.records)?;
        report("guest", round, m, "guest->host (producer side)", &a);
        let b = consume(&mut rx, &h_gen, p.records)?;
        report("guest", round, m, "host->guest (consumer side)", &b);
        let (v, w, c) = ping_client(&mut ptx, &mut prx, p.pings)?;
        print_pings("spin", m, round, v, w, c);
        let (v, w, c) = ping_client(&mut ptx, &mut prx, p.sleep_pings)?;
        print_pings("host-sleep-1ms", m, round, v, w, c);
    }
    Ok(true)
}

/// Host role: `rounds` regions in a row on the same mapping.
pub fn host(map: Arc<Map>, rounds: u64, p: Params) -> Result<bool, String> {
    let g_gen = Gen::new(0x6701);
    let h_gen = Gen::new(0x4801);
    if map.len() < (p.ring_mib << 21) + (1 << 20) {
        return Err("mapping too small for the ring size".into());
    }
    for round in 1..=rounds {
        let region = Region::create(Shared(map.clone()), round, Role::GuestAgent, &specs(&p)).map_err(|e| e.to_string())?;
        println!("host: round {round} region created, waiting for the guest");
        let (mut rx, mut tx) = (region.consumer(L_G2H).map_err(|e| e.to_string())?, region.producer(L_H2G).map_err(|e| e.to_string())?);
        let (mut prx, mut ptx) = (region.consumer(L_PING).map_err(|e| e.to_string())?, region.producer(L_PONG).map_err(|e| e.to_string())?);
        let a = consume(&mut rx, &g_gen, p.records)?;
        report("host", round, "-", "guest->host (consumer side)", &a);
        let b = produce(&mut tx, &h_gen, p.records)?;
        report("host", round, "-", "host->guest (producer side)", &b);
        let (w, c) = ping_server(&mut prx, &mut ptx, p.pings, false)?;
        println!("PING host round={round} phase=spin echoed={} cpu%={:.0}", p.pings, 100.0 * c / w);
        let (w, c) = ping_server(&mut prx, &mut ptx, p.sleep_pings, true)?;
        println!("PING host round={round} phase=host-sleep-1ms echoed={} cpu%={:.0}", p.sleep_pings, 100.0 * c / w);
    }
    Ok(true)
}
