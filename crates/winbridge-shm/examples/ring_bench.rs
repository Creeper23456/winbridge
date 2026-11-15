// SPDX-License-Identifier: GPL-3.0-or-later

//! Host-only ring throughput on a /dev/shm file (the stand-in for the BAR), to be
//! compared later with the guest numbers.
//!
//! ```text
//! cargo run --release -p winbridge-shm --example ring_bench [-- GIB_PER_SIZE]
//! ```
//!
//! For 4 KiB, 64 KiB and 1 MiB records it prints:
//! * `memcpy` into / out of a mapped shm buffer (upper bound, no ring);
//! * single-threaded ring fill (`try_push` until full) and drain (`try_pop`);
//! * two threads, producer and consumer running concurrently (end to end).
//!
//! Each byte is copied once into the ring and once out of it ("single copy" per side).

#[cfg(unix)]
fn main() {
    imp::run();
}

#[cfg(not(unix))]
fn main() {
    eprintln!("ring_bench needs a unix host (it maps a /dev/shm file)");
}

#[cfg(unix)]
mod imp {
    use std::fs::OpenOptions;
    use std::hint::black_box;
    use std::thread;
    use std::time::Instant;

    use winbridge_shm::*;

    struct Map(memmap2::MmapMut);

    // SAFETY: valid and unchanged for the lifetime of the value.
    unsafe impl Mapping for Map {
        fn base(&self) -> *mut u8 {
            self.0.as_ptr() as *mut u8
        }
        fn len(&self) -> usize {
            self.0.len()
        }
    }

    const RING: usize = 16 << 20;

    fn pair() -> (Region<Map>, Region<Map>, *mut u8) {
        let dir = if std::path::Path::new("/dev/shm").is_dir() {
            std::path::PathBuf::from("/dev/shm")
        } else {
            std::env::temp_dir()
        };
        let path = dir.join(format!("winbridge-shm-bench-{}", std::process::id()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("create backing file");
        file.set_len((RING + (1 << 16)) as u64).unwrap();
        // SAFETY: private file mapped only by this process.
        let a = unsafe { memmap2::MmapMut::map_mut(&file) }.unwrap();
        let base = a.as_ptr() as *mut u8;
        let b = unsafe { memmap2::MmapMut::map_mut(&file) }.unwrap();
        let _ = std::fs::remove_file(&path);
        let host = Region::create(
            Map(a),
            0,
            Role::GuestAgent,
            &[LaneSpec {
                id: 1,
                direction: Direction::HostToGuest,
                priority: Priority::Bulk,
                flags: 0,
                data_size: RING,
            }],
        )
        .unwrap();
        (host, Region::attach(Map(b), Side::Guest).unwrap(), base)
    }

    fn mibs(bytes: u64, secs: f64) -> f64 {
        bytes as f64 / 1048576.0 / secs
    }

    pub fn run() {
        let gib: u64 = std::env::args()
            .nth(1)
            .and_then(|a| a.parse().ok())
            .unwrap_or(2);
        println!(
            "ring {} MiB data area, {gib} GiB per size and test; MiB/s (higher is better)",
            RING >> 20
        );
        println!(
            "{:>8} | {:>12} {:>12} | {:>12} {:>12} | {:>12}",
            "record", "memcpy in", "memcpy out", "ring push", "ring pop", "2 threads"
        );
        for size in [4 << 10, 64 << 10, 1 << 20] {
            let total = gib << 30;
            let count = total / size as u64;
            let (host, guest, base) = pair();
            let mut tx = host.producer(1).unwrap();
            let mut rx = guest.consumer(1).unwrap();
            let src = vec![0xA5u8; size];
            let mut sink = Vec::with_capacity(size);

            // memcpy baseline into / out of shm memory, cycling over the ring area.
            let lane = host.lanes()[0];
            // SAFETY: the lane's data area is RING bytes of valid mapped memory; we
            // are the only user of it during the baseline.
            let area = unsafe {
                std::slice::from_raw_parts_mut(
                    base.add(lane.offset + RING_HEADER_LEN),
                    RING,
                )
            };
            area.fill(0); // fault the pages in
            let slots = RING / size;
            let t = Instant::now();
            for i in 0..count as usize {
                let o = (i % slots) * size;
                area[o..o + size].copy_from_slice(&src);
                black_box(&area[o]);
            }
            let memcpy_in = mibs(count * size as u64, t.elapsed().as_secs_f64());
            let mut dst = vec![0u8; size];
            let t = Instant::now();
            for i in 0..count as usize {
                let o = (i % slots) * size;
                dst.copy_from_slice(&area[o..o + size]);
                black_box(&dst[0]);
            }
            let memcpy_out = mibs(count * size as u64, t.elapsed().as_secs_f64());

            // Single thread: fill the ring, then drain it, repeat.
            let per_round = RING / (size + REC_HEADER_LEN);
            let rounds = (count as usize / per_round).max(1);
            let (mut push_s, mut pop_s) = (0f64, 0f64);
            for r in 0..rounds + 1 {
                let t = Instant::now();
                let mut n = 0;
                while tx.try_push(&src).is_ok() {
                    n += 1;
                }
                let ps = t.elapsed().as_secs_f64();
                let t = Instant::now();
                while rx.try_pop(&mut sink).unwrap().is_some() {}
                let qs = t.elapsed().as_secs_f64();
                if r > 0 {
                    // round 0 is warm-up
                    push_s += ps;
                    pop_s += qs;
                }
                black_box(n);
            }
            let moved = (rounds * per_round * size) as u64;
            let (push, pop) = (mibs(moved, push_s), mibs(moved, pop_s));

            // Two threads, concurrent.
            let t = Instant::now();
            let h = thread::spawn(move || {
                let mut buf = Vec::with_capacity(size);
                let mut got = 0u64;
                while got < count {
                    if rx.try_pop(&mut buf).unwrap().is_some() {
                        got += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });
            let mut sent = 0u64;
            while sent < count {
                if tx.try_push(&src).is_ok() {
                    sent += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
            h.join().unwrap();
            let e2e = mibs(count * size as u64, t.elapsed().as_secs_f64());

            let label = if size >= 1 << 20 {
                format!("{} MiB", size >> 20)
            } else {
                format!("{} KiB", size >> 10)
            };
            println!(
                "{label:>8} | {memcpy_in:>12.0} {memcpy_out:>12.0} | {push:>12.0} {pop:>12.0} | {e2e:>12.0}"
            );
        }
    }
}
