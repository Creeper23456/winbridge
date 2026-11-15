// SPDX-License-Identifier: GPL-3.0-or-later

//! Sequential throughput of the kernels in `kernels.rs` over a mapping, per block size.

use std::alloc::{alloc, Layout};
use std::hint::black_box;
use std::time::Instant;

use crate::kernels::{by_name, Op, OPS};
use crate::util::{mib_s, mix};

pub const SIZES: [usize; 7] = [4 << 10, 16 << 10, 64 << 10, 256 << 10, 1 << 20, 4 << 20, 16 << 20];
const QUICK_SIZES: [usize; 4] = [4 << 10, 64 << 10, 1 << 20, 16 << 20];
const REPS: usize = 5;
/// Wall time one repetition aims for.
const REP_SECS: f64 = 0.08;

/// 4096-aligned private buffer filled with a pattern (the source/sink of the copies).
pub struct Local {
    pub ptr: *mut u8,
}

impl Local {
    pub fn new(len: usize) -> Self {
        let layout = Layout::from_size_align(len, 4096).expect("layout");
        // SAFETY: non-zero size.
        let ptr = unsafe { alloc(layout) };
        assert!(!ptr.is_null(), "alloc");
        for i in 0..len / 8 {
            // SAFETY: in bounds, 8-aligned.
            unsafe { (ptr as *mut u64).add(i).write(mix(i as u64, 77)) };
        }
        Self { ptr }
    }
}

fn pass(op: &Op, bar: *mut u8, local: &Local, window: usize, bs: usize, blocks: usize, off: &mut usize) -> f64 {
    let mut sink = 0u64;
    let t = Instant::now();
    for _ in 0..blocks {
        // SAFETY: `*off + bs <= window <= mapping`, both pointers are 64-aligned and bs is a multiple of 64.
        sink ^= unsafe { (op.run)(bar.add(*off), local.ptr, bs) };
        *off += bs;
        if *off >= window {
            *off = 0;
        }
    }
    let s = t.elapsed().as_secs_f64();
    black_box(sink);
    s
}

/// Benchmark every available op at every block size and hand each result row to `emit`:
/// `ROW,dev,mode,op,block_bytes,median_MiB/s,best_MiB/s,min_MiB/s`.
pub fn run(bar: *mut u8, len: usize, dev: &str, mode: &str, quick: bool, only: Option<&str>, emit: &mut dyn FnMut(String)) -> Result<(), String> {
    if len < (16 << 20) {
        return Err("mapping smaller than 16 MiB".into());
    }
    let mut window = 128 << 20;
    while window > len {
        window >>= 1;
    }
    let local = Local::new(16 << 20);
    // Touch the whole window once so the host backing pages are committed before timing.
    let warm = by_name("w_sse2").expect("w_sse2 exists");
    let mut off = 0;
    let t = pass(warm, bar, &local, window, 16 << 20, window / (16 << 20), &mut off);
    emit(format!("WARMUP,{dev},{mode},{:.1}", mib_s(window as u64, t)));

    let sizes: &[usize] = if quick { &QUICK_SIZES } else { &SIZES };
    for op in OPS.iter().filter(|o| o.available() && only.map_or(true, |n| o.name == n)) {
        for &bs in sizes {
            let mut off = 0;
            let probe_blocks = ((1 << 20) / bs).max(1);
            let t = pass(op, bar, &local, window, bs, probe_blocks, &mut off).max(1e-9);
            let speed = (probe_blocks * bs) as f64 / t;
            let blocks = (((speed * REP_SECS) as usize) / bs).clamp(1, (256 << 20) / bs);
            let reps = if blocks as f64 * bs as f64 / speed > 1.0 { 3 } else { REPS };
            let mut v: Vec<f64> = (0..reps)
                .map(|_| mib_s((blocks * bs) as u64, pass(op, bar, &local, window, bs, blocks, &mut off)))
                .collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            emit(format!("ROW,{dev},{mode},{},{bs},{:.1},{:.1},{:.1}", op.name, v[v.len() / 2], v[v.len() - 1], v[0]));
        }
    }
    Ok(())
}
