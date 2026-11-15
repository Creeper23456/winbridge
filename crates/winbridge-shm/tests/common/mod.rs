// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared helpers for the integration tests.
#![allow(dead_code)]

use winbridge_shm::*;

/// Seeded xorshift64* generator: deterministic, no dependencies.
pub struct Rng(pub u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform-ish value in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    pub fn fill(&mut self, buf: &mut [u8]) {
        for b in buf {
            *b = self.next() as u8;
        }
    }
}

pub fn spec(id: u16, direction: Direction, priority: Priority, data_size: usize) -> LaneSpec {
    LaneSpec {
        id,
        direction,
        priority,
        flags: 0,
        data_size,
    }
}

/// Host region created on a heap block plus a guest region attached to a second
/// handle of the same block, and the raw block for tampering.
pub fn heap_pair(
    len: usize,
    specs: &[LaneSpec],
) -> (Region<HeapMapping>, Region<HeapMapping>, HeapMapping) {
    let bar = HeapMapping::new(len);
    let host = Region::create(bar.clone(), 0x5, Role::GuestAgent, specs).unwrap();
    let guest = Region::attach(bar.clone(), Side::Guest).unwrap();
    (host, guest, bar)
}

pub unsafe fn poke64(m: &HeapMapping, off: usize, v: u64) {
    assert!(off + 8 <= m.len());
    (m.base().add(off) as *mut u64).write(v.to_le());
}

pub unsafe fn poke32(m: &HeapMapping, off: usize, v: u32) {
    assert!(off + 4 <= m.len());
    (m.base().add(off) as *mut u32).write(v.to_le());
}

pub unsafe fn poke8(m: &HeapMapping, off: usize, v: u8) {
    assert!(off < m.len());
    m.base().add(off).write(v);
}

pub unsafe fn peek64(m: &HeapMapping, off: usize) -> u64 {
    assert!(off + 8 <= m.len());
    u64::from_le((m.base().add(off) as *const u64).read())
}

/// Memory-mapped file standing in for the BAR (not available under Miri).
#[cfg(all(unix, not(miri)))]
pub mod mm {
    use super::*;
    use std::fs::OpenOptions;
    use std::sync::atomic::{AtomicU32, Ordering};

    pub struct MmapMapping(memmap2::MmapMut);

    // SAFETY: the mapping stays valid and unchanged while the value lives.
    unsafe impl Mapping for MmapMapping {
        fn base(&self) -> *mut u8 {
            self.0.as_ptr() as *mut u8
        }
        fn len(&self) -> usize {
            self.0.len()
        }
    }

    /// Back file in /dev/shm when present (like the real launcher), otherwise the
    /// temp dir. Two independent mappings of one file; the file is unlinked as
    /// soon as both exist.
    pub fn file_pair(len: usize) -> (MmapMapping, MmapMapping) {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = if std::path::Path::new("/dev/shm").is_dir() {
            std::path::PathBuf::from("/dev/shm")
        } else {
            std::env::temp_dir()
        };
        let path = dir.join(format!(
            "winbridge-shm-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.set_len(len as u64).unwrap();
        // SAFETY: private test file, only this process maps it.
        let a = unsafe { memmap2::MmapMut::map_mut(&file) }.unwrap();
        let b = unsafe { memmap2::MmapMut::map_mut(&file) }.unwrap();
        let _ = std::fs::remove_file(&path);
        (MmapMapping(a), MmapMapping(b))
    }
}
