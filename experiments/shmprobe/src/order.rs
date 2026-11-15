// SPDX-License-Identifier: GPL-3.0-or-later

//! Publication-order test: does the host ever see the index of a record before the record's data?
//!
//! The guest repeatedly writes a 1 KiB block (every 64-bit word carries `round << 10 | word`), optionally
//! executes a fence (none, `sfence` or `mfence`, per run), then stores the round number in the index word (a different page). The host
//! polls the index and, whenever it changed, reads the block: a word whose round is *older* than the
//! index is a violation (index visible before data). Store kinds are the ones a ring producer may use:
//! scalar stores, `rep movsb` and non-temporal SSE2 stores. Runs are limited by time, per cache mode.

use std::arch::asm;
use std::arch::x86_64::{__m128i, _mm_load_si128, _mm_mfence, _mm_sfence, _mm_stream_si128};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use winbridge_shm::Mapping;

use crate::ctl::*;
use crate::dev::{Dev, Map, Mode};
use crate::util::wait_for;

const DATA_OFF: usize = 0x1000;
const IDX_OFF: usize = 0x2000;
const WORDS: usize = 128;
const TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy)]
enum Store {
    Scalar,
    Movsb,
    NonTemporal,
}

const STORES: [(Store, &str); 3] = [(Store::Scalar, "scalar64"), (Store::Movsb, "rep_movsb"), (Store::NonTemporal, "nt_sse2")];

#[repr(align(64))]
struct Block([u64; WORDS]);

/// Write round `r` into the data block with the given store kind.
///
/// # Safety
/// `data` must point to 1 KiB of writable, 64-byte aligned memory.
unsafe fn write_round(kind: Store, data: *mut u8, tmp: &mut Block, r: u64) {
    match kind {
        Store::Scalar => {
            for w in 0..WORDS {
                ptr::write_volatile((data as *mut u64).add(w), (r << 10) | w as u64);
            }
        }
        Store::Movsb => {
            for (w, t) in tmp.0.iter_mut().enumerate() {
                *t = (r << 10) | w as u64;
            }
            asm!("rep movsb", inout("rcx") WORDS * 8 => _, inout("rdi") data => _, inout("rsi") tmp.0.as_ptr() => _,
                 options(nostack, preserves_flags));
        }
        Store::NonTemporal => {
            for (w, t) in tmp.0.iter_mut().enumerate() {
                *t = (r << 10) | w as u64;
            }
            let (s, d) = (tmp.0.as_ptr() as *const __m128i, data as *mut __m128i);
            for i in 0..WORDS / 2 {
                _mm_stream_si128(d.add(i), _mm_load_si128(s.add(i)));
            }
        }
    }
}

const FENCES: [&str; 3] = ["none", "sfence", "mfence"];

fn label(mode: Mode, store: usize, fence: usize) -> u64 {
    mode as u64 | (store as u64) << 8 | (fence as u64) << 16
}

fn describe(l: u64) -> String {
    format!("mode={} store={} fence={}", Mode::from_u8(l as u8).name(), STORES[((l >> 8) & 0xff) as usize].1, FENCES[((l >> 16) & 0xff) as usize])
}

/// Guest role (writer). Returns the total number of violations the host reported.
pub fn guest(dev: &Dev, modes: &[Mode], secs: f64) -> Result<u64, String> {
    let mut run = 0u64;
    let mut total = 0u64;
    let mut tmp = Block([0; WORDS]);
    for (mi, &mode) in modes.iter().enumerate() {
        let map = dev.map(mode)?;
        // SAFETY: the mapping outlives `ctl`, `data` and `idx` within this iteration.
        let ctl = unsafe { Ctl::new(map.base()) };
        let data = unsafe { map.base().add(DATA_OFF) };
        let idx = unsafe { &*(map.base().add(IDX_OFF) as *const AtomicU64) };
        if !wait_for(TIMEOUT, || ctl.get(HOST_READY) == READY_MAGIC) {
            return Err("host did not publish the mailbox (is `shmprobe host order` running?)".into());
        }
        for si in 0..STORES.len() {
            for fence in 0..FENCES.len() {
                run += 1;
                let l = label(mode, si, fence);
                for w in 0..WORDS {
                    // SAFETY: in bounds of the data block.
                    unsafe { ptr::write_volatile((data as *mut u64).add(w), 0) };
                }
                idx.store(0, Ordering::Release);
                winbridge_shm::publish_fence();
                ctl.set(O_LABEL, l);
                ctl.set(O_RUN, run);
                if !wait_for(TIMEOUT, || ctl.get(O_SEEN) == run) {
                    return Err(format!("host did not pick up run {run}"));
                }
                let start = Instant::now();
                let mut r = 0u64;
                loop {
                    r += 1;
                    // SAFETY: `data` is 1 KiB inside the mapping, page aligned.
                    unsafe {
                        write_round(STORES[si].0, data, &mut tmp, r);
                        match fence {
                            1 => _mm_sfence(),
                            2 => _mm_mfence(),
                            _ => {}
                        }
                    }
                    idx.store(r, Ordering::Release);
                    if r % 256 == 0 && start.elapsed().as_secs_f64() >= secs {
                        break;
                    }
                }
                ctl.set(O_ROUNDS, r);
                ctl.set(O_DONE, run);
                if !wait_for(TIMEOUT, || ctl.get(O_HDONE) == run) {
                    return Err(format!("host did not finish run {run}"));
                }
                let viol = ctl.get(O_VIOL);
                total += viol;
                println!(
                    "ORDER {} rounds={r} host_samples={} violations={viol} torn_words={} max_lag_rounds={} final_check_bad={}",
                    describe(l),
                    ctl.get(O_SAMPLES),
                    ctl.get(O_TORN),
                    ctl.get(O_LAG),
                    ctl.get(O_FINAL_BAD)
                );
            }
        }
        if mi + 1 == modes.len() {
            ctl.set(O_ALL, 1);
        }
    }
    Ok(total)
}

/// Host role (reader): follow the guest's runs until it reports completion.
pub fn host(map: &Map) -> Result<u64, String> {
    // SAFETY: the mapping outlives `ctl`, `data`, `idx`.
    let ctl = unsafe { Ctl::new(map.base()) };
    let data = unsafe { map.base().add(DATA_OFF) } as *const u64;
    let idx = unsafe { &*(map.base().add(IDX_OFF) as *const AtomicU64) };
    for off in (0..0x100).step_by(8) {
        ctl.set(off, 0);
    }
    ctl.set(HOST_READY, READY_MAGIC);
    println!("host: mailbox ready, waiting for the guest");
    let mut last_run = 0u64;
    let mut total = 0u64;
    loop {
        let t0 = Instant::now();
        let run = loop {
            let r = ctl.get(O_RUN);
            if r > last_run {
                break r;
            }
            if ctl.get(O_ALL) == 1 {
                ctl.set(HOST_READY, 0);
                return Ok(total);
            }
            if t0.elapsed() > TIMEOUT {
                return Err("guest did not start the next run".into());
            }
            std::thread::sleep(Duration::from_micros(200));
        };
        last_run = run;
        let l = ctl.get(O_LABEL);
        ctl.set(O_SEEN, run);
        let (mut samples, mut viol, mut torn, mut lag) = (0u64, 0u64, 0u64, 0u64);
        let mut last_idx = 0u64;
        let check = |i: u64, viol: &mut u64, torn: &mut u64, lag: &mut u64| -> bool {
            let mut bad = false;
            for w in 0..WORDS {
                // SAFETY: inside the 1 KiB data block.
                let v = unsafe { ptr::read_volatile(data.add(w)) };
                if v & 1023 != w as u64 {
                    *torn += 1;
                }
                if (v >> 10) < i {
                    bad = true;
                    *lag = (*lag).max(i - (v >> 10));
                }
            }
            if bad {
                *viol += 1;
            }
            bad
        };
        while ctl.get(O_DONE) != run {
            let i = idx.load(Ordering::Acquire);
            if i != last_idx {
                last_idx = i;
                samples += 1;
                check(i, &mut viol, &mut torn, &mut lag);
            }
        }
        // Settled state: after the writer stopped, index and data must agree.
        std::thread::sleep(Duration::from_millis(2));
        let i = idx.load(Ordering::Acquire);
        let (mut v2, mut t2, mut l2) = (0, 0, 0);
        let final_bad = check(i, &mut v2, &mut t2, &mut l2) as u64;
        ctl.set(O_SAMPLES, samples);
        ctl.set(O_VIOL, viol);
        ctl.set(O_TORN, torn);
        ctl.set(O_LAG, lag);
        ctl.set(O_FINAL_BAD, final_bad);
        ctl.set(O_HDONE, run);
        total += viol;
        println!(
            "ORDER host run={run} {} rounds={} samples={samples} violations={viol} torn_words={torn} max_lag_rounds={lag} final_check_bad={final_bad}",
            describe(l),
            ctl.get(O_ROUNDS)
        );
    }
}
