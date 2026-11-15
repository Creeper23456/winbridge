// SPDX-License-Identifier: GPL-3.0-or-later
//! Persistent worker pool: `run(f)` executes `f(tid)` on `n` threads (tid 0 on the caller) and returns
//! when all have finished. Workers spin briefly then yield, so per-frame dispatch costs ~1 us, not a
//! futex round-trip. With n = 1 nothing is spawned and `f(0)` runs inline.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

type Job = *const (dyn Fn(usize) + Sync);

struct Shared {
    job: UnsafeCell<Option<Job>>,
    generation: AtomicUsize,
    done: AtomicUsize,
    quit: AtomicBool,
}
unsafe impl Sync for Shared {}
unsafe impl Send for Shared {}

pub struct Pool {
    n: usize,
    shared: Arc<Shared>,
    handles: Vec<JoinHandle<()>>,
}

impl Pool {
    pub fn new(n: usize) -> Pool {
        let shared = Arc::new(Shared {
            job: UnsafeCell::new(None),
            generation: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
            quit: AtomicBool::new(false),
        });
        let handles = (1..n)
            .map(|tid| {
                let s = shared.clone();
                thread::spawn(move || worker(tid, &s))
            })
            .collect();
        Pool { n, shared, handles }
    }

    pub fn run(&self, f: &(dyn Fn(usize) + Sync)) {
        if self.n == 1 {
            f(0);
            return;
        }
        // SAFETY: the pointer is only dereferenced by workers between the generation bump and the
        // `done` barrier below, both inside this call, so `f` outlives every use.
        let job: Job = unsafe { std::mem::transmute::<&(dyn Fn(usize) + Sync), Job>(f) };
        unsafe { *self.shared.job.get() = Some(job) };
        self.shared.done.store(0, Ordering::Relaxed);
        self.shared.generation.fetch_add(1, Ordering::Release);
        f(0);
        let mut spins = 0u32;
        while self.shared.done.load(Ordering::Acquire) != self.n - 1 {
            backoff(&mut spins);
        }
    }
}

fn backoff(spins: &mut u32) {
    *spins += 1;
    if *spins < 20_000 {
        std::hint::spin_loop();
    } else {
        thread::yield_now();
    }
}

fn worker(tid: usize, s: &Shared) {
    let mut seen = 0usize;
    loop {
        let mut spins = 0u32;
        let mut g = s.generation.load(Ordering::Acquire);
        while g == seen {
            backoff(&mut spins);
            g = s.generation.load(Ordering::Acquire);
        }
        seen = g;
        if s.quit.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: published before the generation bump (Release/Acquire pair above).
        let job = unsafe { (*s.job.get()).unwrap() };
        unsafe { (*job)(tid) };
        s.done.fetch_add(1, Ordering::AcqRel);
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Release);
        self.shared.generation.fetch_add(1, Ordering::Release);
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}
