// SPDX-License-Identifier: GPL-3.0-or-later

//! Small helpers: argument parsing, deterministic pattern generator, statistics, waiting with a deadline.

use std::str::FromStr;
use std::time::{Duration, Instant};

/// Parsed command line after the role and subcommand.
pub struct Args(Vec<String>);

impl Args {
    pub fn new(v: Vec<String>) -> Self {
        Self(v)
    }

    pub fn flag(&self, name: &str) -> bool {
        self.0.iter().any(|a| a == name)
    }

    /// All values of a repeatable `--name VALUE` option.
    pub fn all(&self, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 1 < self.0.len() {
            if self.0[i] == name {
                out.push(self.0[i + 1].clone());
                i += 1;
            }
            i += 1;
        }
        out
    }

    pub fn opt(&self, name: &str) -> Option<String> {
        self.all(name).into_iter().next()
    }

    pub fn get<T: FromStr>(&self, name: &str, default: T) -> T {
        match self.opt(name) {
            Some(v) => v.parse().unwrap_or_else(|_| {
                eprintln!("bad value for {name}: {v}");
                std::process::exit(2);
            }),
            None => default,
        }
    }
}

/// splitmix64 finaliser: the pattern word for position `x` under `seed`.
#[inline]
pub fn mix(x: u64, seed: u64) -> u64 {
    let mut z = x.wrapping_add(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub fn mib_s(bytes: u64, secs: f64) -> f64 {
    if secs <= 0.0 {
        return 0.0;
    }
    bytes as f64 / (1024.0 * 1024.0) / secs
}

/// Poll `f` (spinning, with a short sleep once it has been false for a while)
/// until it is true or `timeout` elapses.
pub fn wait_for(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    let mut n = 0u32;
    loop {
        if f() {
            return true;
        }
        n = n.wrapping_add(1);
        if n % 256 == 0 {
            if start.elapsed() > timeout {
                return false;
            }
            if n > 4096 {
                std::thread::sleep(Duration::from_micros(200));
            }
        }
        std::hint::spin_loop();
    }
}

/// Percentile (0..=100) of an ascending-sorted slice.
pub fn pct(sorted: &[u32], p: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p / 100.0).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

/// Process CPU time (user + kernel) of the whole process.
pub fn cpu_time() -> Duration {
    crate::dev::cpu_time()
}
