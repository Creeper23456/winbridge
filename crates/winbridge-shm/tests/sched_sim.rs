// SPDX-License-Identifier: GPL-3.0-or-later

//! Deterministic lane-scheduler simulation on a virtual clock (microseconds).
//! The simulated link moves 1 MiB per millisecond, so one 1 MiB bulk block takes
//! exactly 1000 us ("one block time").

use std::collections::VecDeque;
use std::time::Duration;

use winbridge_shm::sched::{decode_credit, encode_credit, CREDIT_RECORD_LEN};
use winbridge_shm::{Class, NewestWins, SchedConfig, Scheduler};

const MIB: usize = 1 << 20;
const BLOCK_US: u64 = 1000;

fn service_us(bytes: usize) -> u64 {
    (bytes as u64 * 1000 / MIB as u64).max(1)
}

struct Cfg {
    /// Control record every `period` us (with deterministic jitter), size.
    ctrl: Option<(u64, usize)>,
    /// Frame every `period` us, size.
    frames: Option<(u64, usize)>,
    frame_cap: usize,
    bulk: bool,
    credit_rtt_us: u64,
    duration_us: u64,
    sched: SchedConfig,
}

impl Default for Cfg {
    fn default() -> Self {
        Self {
            ctrl: None,
            frames: None,
            frame_cap: 2,
            bulk: false,
            credit_rtt_us: 200,
            duration_us: 1_000_000,
            sched: SchedConfig::default(),
        }
    }
}

#[derive(Default)]
struct Stats {
    ctrl_lat: Vec<u64>,
    frame_lat: Vec<u64>,
    frames_delivered: Vec<u64>,
    frames_produced: u64,
    frames_dropped: u64,
    bulk_blocks: u64,
    bulk_done_at: Vec<u64>,
    max_in_flight: u32,
}

fn pct(v: &[u64], p: f64) -> u64 {
    let mut s = v.to_vec();
    s.sort_unstable();
    s[((s.len() as f64 * p) as usize).min(s.len() - 1)]
}

fn run(cfg: Cfg) -> Stats {
    let mut sched = Scheduler::new(cfg.sched);
    let ctrl_lane = sched.add_lane(Class::Control);
    let frame_lane = sched.add_lane(Class::Normal);
    let bulk_lane = sched.add_lane(Class::Bulk);

    let mut ctrlq: VecDeque<u64> = VecDeque::new();
    let mut frameq: NewestWins<(u64, u64)> = NewestWins::new(cfg.frame_cap);
    let mut credits: VecDeque<u64> = VecDeque::new();
    let mut st = Stats::default();
    let mut jitter = 0x2545_F491_4F6C_DD1Du64;
    let mut t = 0u64;
    let mut next_ctrl = 0u64;
    let mut next_frame = 0u64;
    let mut seq = 0u64;

    while t < cfg.duration_us {
        if let Some((period, _)) = cfg.ctrl {
            while next_ctrl <= t {
                ctrlq.push_back(next_ctrl);
                jitter ^= jitter << 13;
                jitter ^= jitter >> 7;
                jitter ^= jitter << 17;
                next_ctrl += period / 2 + jitter % period;
            }
        }
        if let Some((period, _)) = cfg.frames {
            while next_frame <= t {
                st.frames_produced += 1;
                frameq.push((next_frame, seq));
                seq += 1;
                next_frame += period;
            }
        }
        while credits.front().is_some_and(|&c| c <= t) {
            credits.pop_front();
            sched.return_credit(bulk_lane, 1).unwrap();
        }
        st.max_in_flight = st.max_in_flight.max(sched.in_flight(bulk_lane));

        let pick = sched.next(Duration::from_micros(t), |i| {
            if i == ctrl_lane {
                !ctrlq.is_empty()
            } else if i == frame_lane {
                !frameq.is_empty()
            } else {
                cfg.bulk
            }
        });
        match pick {
            Some(p) if p.lane == ctrl_lane => {
                let arrived = ctrlq.pop_front().unwrap();
                t += service_us(cfg.ctrl.unwrap().1);
                st.ctrl_lat.push(t - arrived);
            }
            Some(p) if p.lane == frame_lane => {
                let (arrived, s) = frameq.pop().unwrap();
                t += service_us(cfg.frames.unwrap().1);
                st.frame_lat.push(t - arrived);
                st.frames_delivered.push(s);
            }
            Some(p) => {
                assert_eq!(p.max_len, MIB);
                t += service_us(p.max_len);
                st.bulk_blocks += 1;
                st.bulk_done_at.push(t);
                credits.push_back(t + cfg.credit_rtt_us);
            }
            None => {
                // Idle: jump to the next event.
                let mut nxt = cfg.duration_us;
                if cfg.ctrl.is_some() {
                    nxt = nxt.min(next_ctrl);
                }
                if cfg.frames.is_some() {
                    nxt = nxt.min(next_frame);
                }
                if let Some(&c) = credits.front() {
                    nxt = nxt.min(c);
                }
                t = nxt.max(t + 1);
            }
        }
    }
    st.frames_dropped = frameq.dropped();
    st
}

#[test]
fn a_saturating_bulk_does_not_delay_control_beyond_one_block() {
    let st = run(Cfg {
        ctrl: Some((3700, 256)),
        bulk: true,
        ..Cfg::default()
    });
    let max = *st.ctrl_lat.iter().max().unwrap();
    println!(
        "control latency with saturating bulk: p50 {} us, p99 {} us, max {} us (block = {BLOCK_US} us)",
        pct(&st.ctrl_lat, 0.5),
        pct(&st.ctrl_lat, 0.99),
        max
    );
    assert!(max <= BLOCK_US + service_us(256), "max {max}");
    // Bulk really was saturating the link and the credit window was exercised.
    assert!(st.bulk_blocks >= 950, "bulk blocks {}", st.bulk_blocks);
    assert!(st.max_in_flight <= SchedConfig::default().credit_window);
}

#[test]
fn b_frames_latency_stays_bounded_while_bulk_saturates() {
    let frame = (16_667, 64 * 1024);
    let without = run(Cfg {
        frames: Some(frame),
        ctrl: Some((5000, 256)),
        ..Cfg::default()
    });
    let with = run(Cfg {
        frames: Some(frame),
        ctrl: Some((5000, 256)),
        bulk: true,
        ..Cfg::default()
    });
    let (p50, p99, max) = (
        pct(&with.frame_lat, 0.5),
        pct(&with.frame_lat, 0.99),
        *with.frame_lat.iter().max().unwrap(),
    );
    println!(
        "frame latency bulk off: p99 {} us; bulk on: p50 {p50} us, p99 {p99} us, max {max} us",
        pct(&without.frame_lat, 0.99)
    );
    assert_eq!(with.frames_dropped, 0);
    // One block in progress + own service + a control record in front of it.
    assert!(max <= BLOCK_US + service_us(64 * 1024) + service_us(256) + 2, "max {max}");
    assert!(with.bulk_blocks >= 850);
}

#[test]
fn c_bulk_makes_progress_under_saturating_frames_and_control() {
    let st = run(Cfg {
        ctrl: Some((1000, 128)),
        frames: Some((800, MIB)), // 125% of the link
        bulk: true,
        duration_us: 2_000_000,
        ..Cfg::default()
    });
    let guard_us = 50_000;
    println!(
        "saturated frames+control: bulk blocks in 2 s = {}, frames delivered {}, dropped {}",
        st.bulk_blocks,
        st.frames_delivered.len(),
        st.frames_dropped
    );
    // One block per 50 ms guaranteed: at least 2 s / 50 ms - slack.
    assert!(st.bulk_blocks >= 38, "bulk blocks {}", st.bulk_blocks);
    let mut last = 0;
    for &d in &st.bulk_done_at {
        assert!(d - last <= guard_us + 2 * BLOCK_US, "gap {} us", d - last);
        last = d;
    }
    // The guard costs control at most the in-progress service plus one block.
    let max = *st.ctrl_lat.iter().max().unwrap();
    assert!(max <= 2 * BLOCK_US + 10, "control max {max}");
    // Bulk got only its guaranteed share, not the link.
    assert!(st.bulk_blocks <= 45, "bulk blocks {}", st.bulk_blocks);
}

#[test]
fn d_stale_frames_are_dropped_newest_wins() {
    let cfg = || Cfg {
        ctrl: Some((1000, 128)),
        frames: Some((800, MIB)),
        bulk: true,
        duration_us: 2_000_000,
        ..Cfg::default()
    };
    let st = run(cfg());
    assert!(st.frames_dropped > 100, "dropped {}", st.frames_dropped);
    // Order preserved, nothing duplicated.
    assert!(st.frames_delivered.windows(2).all(|w| w[0] < w[1]));
    // Every produced frame was delivered, dropped as stale, or is still queued
    // (at most `cap` of them) when the run ends.
    let accounted = st.frames_delivered.len() as u64 + st.frames_dropped;
    assert!(st.frames_produced >= accounted && st.frames_produced - accounted <= 2);
    // Delivered frames are fresh: waiting behind one frame in progress, one
    // queued frame, the guard block, and its own service.
    let max = *st.frame_lat.iter().max().unwrap();
    println!(
        "newest-wins (cap 2): delivered {} dropped {} max age {} us",
        st.frames_delivered.len(),
        st.frames_dropped,
        max
    );
    assert!(max <= 6 * BLOCK_US, "max age {max}");

    // Contrast: an unbounded FIFO would deliver every frame, hopelessly late.
    let fifo = run(Cfg {
        frame_cap: 1_000_000,
        ..cfg()
    });
    assert_eq!(fifo.frames_dropped, 0);
    assert!(*fifo.frame_lat.iter().max().unwrap() > 100 * BLOCK_US);
}

#[test]
fn newest_wins_queue_semantics() {
    let mut q = NewestWins::new(2);
    assert_eq!(q.push(1), None);
    assert_eq!(q.push(2), None);
    assert_eq!(q.push(3), Some(1));
    assert_eq!(q.push(4), Some(2));
    assert_eq!(q.push(5), Some(3));
    assert_eq!((q.len(), q.dropped()), (2, 3));
    assert_eq!((q.pop(), q.pop(), q.pop()), (Some(4), Some(5), None));
    assert!(q.is_empty());
    let mut one = NewestWins::new(0); // clamped to 1
    assert_eq!(one.push('a'), None);
    assert_eq!(one.push('b'), Some('a'));
}

// ---- scheduler unit behaviour ------------------------------------------------

fn d(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

#[test]
fn control_is_strict_and_equal_lanes_round_robin() {
    let mut s = Scheduler::new(SchedConfig::default());
    let c = s.add_lane(Class::Control);
    let f1 = s.add_lane(Class::Normal);
    let f2 = s.add_lane(Class::Normal);
    let b = s.add_lane(Class::Bulk);
    let all = |_: usize| true;
    for _ in 0..5 {
        assert_eq!(s.next(d(1), all).unwrap().lane, c);
    }
    let not_c = |i: usize| i != c;
    let picks: Vec<usize> = (0..6).map(|_| s.next(d(1), not_c).unwrap().lane).collect();
    assert_eq!(picks, [f1, f2, f1, f2, f1, f2]);
    let only_bulk = |i: usize| i == b;
    assert_eq!(s.next(d(1), only_bulk).unwrap().class, Class::Bulk);
    assert_eq!(s.next(d(1), |_| false), None);
}

#[test]
fn credit_window_limits_blocks_in_flight() {
    let cfg = SchedConfig {
        credit_window: 3,
        ..SchedConfig::default()
    };
    let mut s = Scheduler::new(cfg);
    let b = s.add_lane(Class::Bulk);
    for _ in 0..3 {
        let p = s.next(d(1), |_| true).unwrap();
        assert_eq!((p.lane, p.max_len), (b, MIB));
    }
    assert_eq!(s.next(d(1), |_| true), None, "window exhausted");
    assert_eq!(s.in_flight(b), 3);
    s.return_credit(b, 2).unwrap();
    assert!(s.next(d(1), |_| true).is_some());
    // A hostile peer cannot return more than is in flight, nor credit a non-bulk lane.
    assert!(s.return_credit(b, 3).is_err());
    assert!(s.return_credit(99, 1).is_err());
    assert_eq!(s.in_flight(b), 2);
}

#[test]
fn guard_overrides_control_after_interval_and_idle_bulk_accrues_no_debt() {
    let mut s = Scheduler::new(SchedConfig::default());
    let c = s.add_lane(Class::Control);
    let b = s.add_lane(Class::Bulk);
    // Bulk idle for a long time: no debt, control wins until 50 ms after bulk appears.
    assert_eq!(s.next(d(1000), |i| i == c).unwrap().lane, c);
    assert_eq!(s.next(d(1001), |_| true).unwrap().lane, c);
    assert_eq!(s.next(d(1049), |_| true).unwrap().lane, c);
    let p = s.next(d(1051), |_| true).unwrap();
    assert_eq!(p.lane, b);
    s.return_credit(b, 1).unwrap();
    // Served just now: control wins again until the next interval.
    assert_eq!(s.next(d(1052), |_| true).unwrap().lane, c);
    assert_eq!(s.next(d(1101), |_| true).unwrap().lane, b);
}

#[test]
fn credit_record_codec() {
    let r = encode_credit(0x1234, 7);
    assert_eq!(r.len(), CREDIT_RECORD_LEN);
    assert_eq!(decode_credit(&r), Some((0x1234, 7)));
    assert_eq!(decode_credit(&r[..7]), None);
    assert_eq!(decode_credit(&[r.as_slice(), &[0]].concat()), None);
    assert_eq!(decode_credit(&encode_credit(1, 0)), None);
    let mut bad = r;
    bad[0] ^= 1;
    assert_eq!(decode_credit(&bad), None);
    let mut bad = r;
    bad[1] = 1;
    assert_eq!(decode_credit(&bad), None);
}
