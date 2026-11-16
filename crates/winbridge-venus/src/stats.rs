// SPDX-License-Identifier: GPL-3.0-or-later

//! Counters for the serve loop, reported once per second with `--stats`.
//!
//! The counters answer one question: of the time between a guest command and its result, how
//! much does this process spend, and how much is the channel or the guest? Every figure is a sum
//! over the interval; the report divides where an average is meant.

use std::time::{Duration, Instant};
use tracing::info;

/// Message kinds are small integers (`proto.rs`); one slot each, the rest share the last.
const KINDS: usize = 12;
/// Fence latency buckets: < 0.2 ms, < 1 ms, < 5 ms, < 20 ms, slower.
const BUCKETS: [Duration; 4] = [Duration::from_micros(200), Duration::from_millis(1), Duration::from_millis(5), Duration::from_millis(20)];
const PHASES: [&str; 5] = ["wait", "handle", "poll", "retire", "scan"];
const KIND_NAMES: [&str; KINDS] = ["?", "hello", "shm_create", "shm_destroy", "push", "shm_role", "submit", "sync_signal", "lost", "status_clear", "bo_create", "ping"];

#[derive(Default, Clone)]
struct Window {
    iters: u64,
    phase: [Duration; 5],
    msgs: [u64; KINDS],
    handle: [Duration; KINDS],
    bytes_in: u64,
    submits: u64,
    submit_bytes: u64,
    submit_time: Duration,
    submit_max: Duration,
    fences: u64,
    fence_time: Duration,
    fence_max: Duration,
    fence_hist: [u64; 5],
    scans: u64,
    scan_msgs: u64,
    scan_bytes: u64,
    compare_calls: u64,
    compare_bytes: u64,
    mirror: [(u64, u64); 3],
    out_msgs: u64,
    out_bytes: u64,
    send_time: Duration,
    send_max: Duration,
    wakes: u64,
}

pub struct Stats {
    on: bool,
    started: Instant,
    last: Instant,
    w: Window,
}

fn kib(n: u64) -> f64 {
    n as f64 / 1024.0
}

fn avg(sum: Duration, n: u64) -> Duration {
    if n == 0 {
        Duration::ZERO
    } else {
        sum / n as u32
    }
}

impl Stats {
    pub fn new() -> Stats {
        let now = Instant::now();
        Stats { on: false, started: now, last: now, w: Window::default() }
    }

    pub fn enable(&mut self) {
        self.on = true;
    }

    pub fn loop_phases(&mut self, phases: &[Duration; 5]) {
        self.w.iters += 1;
        for (acc, p) in self.w.phase.iter_mut().zip(phases) {
            *acc += *p;
        }
    }

    pub fn message(&mut self, kind: u32, bytes: usize, took: Duration) {
        let k = (kind as usize).min(KINDS - 1);
        self.w.msgs[k] += 1;
        self.w.handle[k] += took;
        self.w.bytes_in += bytes as u64;
    }

    pub fn submit(&mut self, bytes: usize, took: Duration) {
        self.w.submits += 1;
        self.w.submit_bytes += bytes as u64;
        self.w.submit_time += took;
        self.w.submit_max = self.w.submit_max.max(took);
    }

    pub fn fence(&mut self, latency: Duration) {
        self.w.fences += 1;
        self.w.fence_time += latency;
        self.w.fence_max = self.w.fence_max.max(latency);
        let slot = BUCKETS.iter().position(|b| latency < *b).unwrap_or(BUCKETS.len());
        self.w.fence_hist[slot] += 1;
    }

    pub fn scan(&mut self, msgs: u64, bytes: u64) {
        self.w.scans += 1;
        self.w.scan_msgs += msgs;
        self.w.scan_bytes += bytes;
    }

    /// True when the next `tick` will report.
    pub fn due(&self) -> bool {
        self.on && self.last.elapsed() >= Duration::from_secs(1)
    }

    /// How much memory is mirrored (count, bytes) as ring, other shmem, device memory.
    pub fn mirrored(&mut self, now: [(u64, u64); 3]) {
        self.w.mirror = now;
    }

    /// A scan pass compared `bytes` of mirrored memory with its copy.
    pub fn compared(&mut self, bytes: u64) {
        self.w.compare_calls += 1;
        self.w.compare_bytes += bytes;
    }

    pub fn sent(&mut self, bytes: usize, took: Duration) {
        self.w.out_msgs += 1;
        self.w.out_bytes += bytes as u64;
        self.w.send_time += took;
        self.w.send_max = self.w.send_max.max(took);
    }

    pub fn wake(&mut self) {
        self.w.wakes += 1;
    }

    /// Logs the interval and starts a new one, once a second has passed.
    pub fn tick(&mut self) {
        if !self.on || self.last.elapsed() < Duration::from_secs(1) {
            return;
        }
        let w = std::mem::take(&mut self.w);
        let span = self.last.elapsed();
        self.last = Instant::now();
        if w.iters == 0 || (w.msgs.iter().sum::<u64>() == 0 && w.fences == 0) {
            return;
        }
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        let phases: Vec<String> = PHASES.iter().zip(&w.phase).map(|(n, d)| format!("{n} {:.0}", ms(*d))).collect();
        let msgs: Vec<String> = (1..KINDS).filter(|k| w.msgs[*k] > 0).map(|k| format!("{} {} ({:.1} ms)", KIND_NAMES[k], w.msgs[k], ms(w.handle[k]))).collect();
        info!(target: "stats", "t={:.1}s span {:.2}s: {} loops; ms in {}", self.started.elapsed().as_secs_f64(), span.as_secs_f64(), w.iters, phases.join(", "));
        info!(target: "stats", "  in: {:.0} KiB; {}", kib(w.bytes_in), msgs.join(", "));
        info!(
            target: "stats",
            "  submit_cmd {} x avg {:.3} ms max {:.2} ms, {:.0} KiB; wakes {}",
            w.submits, ms(avg(w.submit_time, w.submits)), ms(w.submit_max), kib(w.submit_bytes), w.wakes
        );
        info!(
            target: "stats",
            "  fences {} avg {:.2} ms max {:.2} ms; <0.2/<1/<5/<20/more ms: {:?}",
            w.fences, ms(avg(w.fence_time, w.fences)), ms(w.fence_max), w.fence_hist
        );
        info!(
            target: "stats",
            "  out: {} msgs {:.0} KiB, write {:.1} ms total max {:.2} ms; {} scans made {} msgs {:.0} KiB; compared {} x {:.0} KiB",
            w.out_msgs, kib(w.out_bytes), ms(w.send_time), ms(w.send_max), w.scans, w.scan_msgs, kib(w.scan_bytes), w.compare_calls, kib(w.compare_bytes)
        );
        info!(
            target: "stats",
            "  mirrored now: ring {} x {:.0} KiB, other shmem {} x {:.0} KiB, device memory {} x {:.0} KiB",
            w.mirror[0].0, kib(w.mirror[0].1), w.mirror[1].0, kib(w.mirror[1].1), w.mirror[2].0, kib(w.mirror[2].1)
        );
    }
}
