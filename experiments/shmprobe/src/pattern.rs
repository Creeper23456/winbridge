// SPDX-License-Identifier: GPL-3.0-or-later

//! Correctness of the data path in both directions, per cache mode.
//!
//! For each mode the guest maps the device, fills a span with a position-dependent pattern,
//! publishes an epoch (after `sfence`) in the mailbox and waits for the host to verify it. The host
//! then overwrites the same span (lines the guest has just cached) with a different pattern; the
//! guest has to read the new data, which also shows whether a cached mapping stays coherent.

use std::ptr;
use std::time::{Duration, Instant};

use winbridge_shm::{publish_fence, Mapping};

use crate::bench::Local;
use crate::ctl::*;
use crate::dev::{Dev, Map, Mode};
use crate::util::{mib_s, mix, wait_for};

const SPAN_OFF: usize = 4096;
const BLOCK: usize = 1 << 20;
const TIMEOUT: Duration = Duration::from_secs(180);

fn seed_g(mode: u64) -> u64 {
    0x1000 + mode
}
fn seed_h(mode: u64) -> u64 {
    0x2000 + mode
}

fn fill(buf: *mut u8, bytes: usize, first_word: u64, seed: u64) {
    for i in 0..bytes / 8 {
        // SAFETY: caller passes a buffer of `bytes` bytes, 8-aligned.
        unsafe { (buf as *mut u64).add(i).write(mix(first_word + i as u64, seed)) };
    }
}

/// Write the pattern through ordinary copies; returns seconds.
fn write_span(bar: *mut u8, span: usize, seed: u64, local: &Local) -> f64 {
    let t = Instant::now();
    let mut off = 0;
    while off < span {
        let n = BLOCK.min(span - off);
        fill(local.ptr, n, (off / 8) as u64, seed);
        // SAFETY: SPAN_OFF + span <= mapping length (checked by the callers).
        unsafe { ptr::copy_nonoverlapping(local.ptr, bar.add(SPAN_OFF + off), n) };
        off += n;
    }
    publish_fence();
    t.elapsed().as_secs_f64()
}

/// Verify the pattern; returns (mismatching 64-bit words, first bad offset, seconds).
fn verify_span(bar: *mut u8, span: usize, seed: u64, local: &Local, expect: &Local) -> (u64, Option<usize>, f64) {
    let t = Instant::now();
    let (mut bad, mut first) = (0u64, None);
    let mut off = 0;
    while off < span {
        let n = BLOCK.min(span - off);
        // SAFETY: as in `write_span`.
        unsafe { ptr::copy_nonoverlapping(bar.add(SPAN_OFF + off), local.ptr, n) };
        fill(expect.ptr, n, (off / 8) as u64, seed);
        for i in 0..n / 8 {
            // SAFETY: in bounds of both buffers.
            let (a, b) = unsafe { ((local.ptr as *const u64).add(i).read(), (expect.ptr as *const u64).add(i).read()) };
            if a != b {
                bad += 1;
                first.get_or_insert(off + i * 8);
            }
        }
        off += n;
    }
    (bad, first, t.elapsed().as_secs_f64())
}

fn check_span(len: usize, span_mib: usize) -> Result<usize, String> {
    let span = span_mib << 20;
    if SPAN_OFF + span > len {
        return Err(format!("span of {span_mib} MiB does not fit the {} MiB mapping", len >> 20));
    }
    Ok(span)
}

/// Guest role. Returns true if every check passed.
pub fn guest(dev: &Dev, modes: &[Mode], span_mib: usize) -> Result<bool, String> {
    let (local, expect) = (Local::new(BLOCK), Local::new(BLOCK));
    let mut ok = true;
    for (i, &mode) in modes.iter().enumerate() {
        let map = dev.map(mode)?;
        let span = check_span(map.len(), span_mib)?;
        // SAFETY: `map` outlives `ctl` within this iteration.
        let ctl = unsafe { Ctl::new(map.base()) };
        if !wait_for(TIMEOUT, || ctl.get(HOST_READY) == READY_MAGIC) {
            return Err("host did not publish the mailbox (is `shmprobe host pattern` running?)".into());
        }
        let e = i as u64 + 1;
        let tw = write_span(map.base(), span, seed_g(mode as u64), &local);
        ctl.set(G_MODE, mode as u64);
        ctl.set(G_EPOCH, e);
        if !wait_for(TIMEOUT, || ctl.get(H_ACK) == e) {
            return Err(format!("host did not acknowledge epoch {e}"));
        }
        let host_bad = ctl.get(H_BAD);
        if !wait_for(TIMEOUT, || ctl.get(H_EPOCH) == e) {
            return Err(format!("host did not publish its pattern for epoch {e}"));
        }
        let (bad, first, tr) = verify_span(map.base(), span, seed_h(mode as u64), &local, &expect);
        ctl.set(G_BAD, bad);
        ctl.set(G_ACK, e);
        if i + 1 == modes.len() {
            ctl.set(G_DONE, 1);
        }
        println!(
            "PATTERN mode={} span={}MiB guest->host: host_bad_words={host_bad} (write {:.0} MiB/s) | host->guest: guest_bad_words={bad}{} (read-back {:.0} MiB/s)",
            mode.name(),
            span >> 20,
            mib_s(span as u64, tw),
            first.map(|f| format!(" first_bad_off=0x{f:x}")).unwrap_or_default(),
            mib_s(span as u64, tr)
        );
        ok &= host_bad == 0 && bad == 0;
    }
    Ok(ok)
}

/// Host role: serve epochs until the guest reports it is done.
pub fn host(map: &Map, span_mib: usize) -> Result<bool, String> {
    let span = check_span(map.len(), span_mib)?;
    let (local, expect) = (Local::new(BLOCK), Local::new(BLOCK));
    // SAFETY: `map` outlives `ctl`.
    let ctl = unsafe { Ctl::new(map.base()) };
    for off in (0..0x100).step_by(8) {
        ctl.set(off, 0);
    }
    ctl.set(HOST_READY, READY_MAGIC);
    println!("host: mailbox ready, waiting for the guest");
    let mut ok = true;
    let mut e = 1u64;
    loop {
        let start = Instant::now();
        loop {
            if ctl.get(G_EPOCH) == e {
                break;
            }
            if ctl.get(G_DONE) == 1 {
                ctl.set(HOST_READY, 0);
                return Ok(ok);
            }
            if start.elapsed() > TIMEOUT {
                return Err(format!("guest did not publish epoch {e}"));
            }
            std::thread::sleep(Duration::from_micros(100));
        }
        let mode = ctl.get(G_MODE);
        let (bad, first, tv) = verify_span(map.base(), span, seed_g(mode), &local, &expect);
        ctl.set(H_BAD, bad);
        ctl.set(H_ACK, e);
        let tw = write_span(map.base(), span, seed_h(mode), &local);
        ctl.set(H_EPOCH, e);
        if !wait_for(TIMEOUT, || ctl.get(G_ACK) == e) {
            return Err(format!("guest did not acknowledge epoch {e}"));
        }
        println!(
            "PATTERN host epoch={e} mode={} guest->host: bad_words={bad}{} (read {:.0} MiB/s) | host->guest: guest_bad_words={} (write {:.0} MiB/s)",
            Mode::from_u8(mode as u8).name(),
            first.map(|f| format!(" first_bad_off=0x{f:x}")).unwrap_or_default(),
            mib_s(span as u64, tv),
            ctl.get(G_BAD),
            mib_s(span as u64, tw)
        );
        ok &= bad == 0 && ctl.get(G_BAD) == 0;
        e += 1;
    }
}
