// SPDX-License-Identifier: GPL-3.0-or-later

//! Hostile-peer tests: targeted index/record forgeries and seeded fuzzing of the
//! header, directory and ring memory. Nothing may panic or read out of bounds
//! (the blocks are exact-size heap allocations, so Miri catches any overrun).

mod common;

use common::*;
use winbridge_shm::*;

const H2G: Direction = Direction::HostToGuest;
const G2H: Direction = Direction::GuestToHost;

const SIZE: usize = 4096;

/// Host consumes lane 1 (guest-to-host); returns host consumer, ring offset, block.
fn host_consumer() -> (Consumer<HeapMapping>, usize, HeapMapping) {
    let (host, _guest, bar) = heap_pair(1 << 16, &[spec(1, G2H, Priority::High, SIZE)]);
    let off = host.lanes()[0].offset;
    (host.consumer(1).unwrap(), off, bar)
}

// ring header offsets
const HEAD: usize = 0;
const TAIL: usize = 64;
const DATA: usize = 128;

fn expect_corrupt(rx: &mut Consumer<HeapMapping>, want: Corruption) {
    let mut out = Vec::new();
    assert_eq!(
        rx.try_pop(&mut out).unwrap_err(),
        LaneError::Corrupt(want)
    );
    assert_eq!(out.capacity(), 0, "allocated before validation");
    assert!(rx.is_closed());
    assert_eq!(rx.corruption(), Some(want));
    // sticky
    assert_eq!(rx.try_pop(&mut out).unwrap_err(), LaneError::Closed);
    assert_eq!(rx.poll_ready().unwrap_err(), LaneError::Closed);
    let mut small = [0u8; 4];
    assert_eq!(
        rx.try_pop_into(&mut small).unwrap_err(),
        PopError::Lane(LaneError::Closed)
    );
}

#[test]
fn head_beyond_data_size_closes_lane() {
    let (mut rx, off, bar) = host_consumer();
    unsafe { poke64(&bar, off + HEAD, SIZE as u64 + 8) };
    expect_corrupt(&mut rx, Corruption::IndexOutOfRange);
}

#[test]
fn huge_head_closes_lane() {
    let (mut rx, off, bar) = host_consumer();
    unsafe { poke64(&bar, off + HEAD, u64::MAX & !7) };
    expect_corrupt(&mut rx, Corruption::IndexOutOfRange);
}

#[test]
fn misaligned_head_closes_lane() {
    let (mut rx, off, bar) = host_consumer();
    unsafe { poke64(&bar, off + HEAD, 5) };
    expect_corrupt(&mut rx, Corruption::IndexMisaligned);
}

#[test]
fn head_moving_backwards_closes_lane() {
    let (host, guest, bar) = heap_pair(1 << 16, &[spec(1, G2H, Priority::High, SIZE)]);
    let off = host.lanes()[0].offset;
    let mut rx = host.consumer(1).unwrap();
    let mut tx = guest.producer(1).unwrap();
    let _ = tx.try_push(&[1u8; 40]).unwrap();
    let _ = tx.try_push(&[2u8; 40]).unwrap();
    let mut out = Vec::new();
    assert!(rx.try_pop(&mut out).unwrap().is_some());
    // Producer "rewinds" its head to a value that is still >= our tail.
    unsafe { poke64(&bar, off + HEAD, 48) };
    expect_corrupt(&mut rx, Corruption::IndexBackwards);
}

#[test]
fn record_longer_than_cap_closes_lane_without_allocating() {
    let (mut rx, off, bar) = host_consumer();
    rx.set_max_record(1000);
    unsafe {
        poke64(&bar, off + DATA, 0xFFFF_FFFF); // len = 4 GiB - 1
        poke64(&bar, off + HEAD, 64);
    }
    expect_corrupt(&mut rx, Corruption::RecordTooLong);
}

#[test]
fn record_longer_than_published_bytes_closes_lane() {
    let (mut rx, off, bar) = host_consumer();
    unsafe {
        poke64(&bar, off + DATA, 100); // fits the cap, but only 16 bytes published
        poke64(&bar, off + HEAD, 16);
    }
    expect_corrupt(&mut rx, Corruption::RecordExceedsAvailable);
}

/// Advance the consumer's private tail to `SIZE - 16` with one legitimate record.
fn advance_to_end(
    host: &Region<HeapMapping>,
    guest: &Region<HeapMapping>,
) -> Consumer<HeapMapping> {
    let mut rx = host.consumer(1).unwrap();
    let mut tx = guest.producer(1).unwrap();
    let _ = tx.try_push(&vec![0u8; SIZE - 16 - REC_HEADER_LEN]).unwrap();
    let mut out = Vec::new();
    assert!(rx.try_pop(&mut out).unwrap().is_some());
    rx
}

#[test]
fn record_running_past_end_of_data_area_closes_lane() {
    let (host, guest, bar) = heap_pair(1 << 16, &[spec(1, G2H, Priority::High, SIZE)]);
    let off = host.lanes()[0].offset;
    let mut rx = advance_to_end(&host, &guest);
    let tail = (SIZE - 16) as u64;
    unsafe {
        poke64(&bar, off + DATA + SIZE - 16, 40); // total 48 > 16 bytes to the end
        poke64(&bar, off + HEAD, tail + 64);
    }
    expect_corrupt(&mut rx, Corruption::RecordWraps);
}

#[test]
fn bad_skip_markers_close_lane() {
    // Skip longer than what was published.
    let (host, guest, bar) = heap_pair(1 << 16, &[spec(1, G2H, Priority::High, SIZE)]);
    let off = host.lanes()[0].offset;
    let mut rx = advance_to_end(&host, &guest);
    unsafe {
        poke64(&bar, off + DATA + SIZE - 16, (FLAG_SKIP as u64) << 48);
        poke64(&bar, off + HEAD, (SIZE - 16 + 8) as u64);
    }
    expect_corrupt(&mut rx, Corruption::BadSkip);

    // Skip at position 0 (never emitted by an honest producer).
    let (mut rx, off, bar) = host_consumer();
    unsafe {
        poke64(&bar, off + DATA, (FLAG_SKIP as u64) << 48);
        poke64(&bar, off + HEAD, 8);
    }
    expect_corrupt(&mut rx, Corruption::BadSkip);
}

#[test]
fn producer_closes_lane_on_bad_tail() {
    let (host, guest, bar) = heap_pair(1 << 16, &[spec(1, H2G, Priority::High, 64)]);
    let off = host.lanes()[0].offset;
    let mut tx = host.producer(1).unwrap();
    let mut rx = guest.consumer(1).unwrap();
    let mut out = Vec::new();
    let _ = tx.try_push(&[0u8; 24]).unwrap();
    let _ = tx.try_push(&[0u8; 24]).unwrap();
    assert!(rx.try_pop(&mut out).unwrap().is_some()); // tail = 32
    let _ = tx.try_push(&[0u8; 24]).unwrap(); // refreshes tail_seen = 32
    // Backwards tail.
    unsafe { poke64(&bar, off + TAIL, 8) };
    assert_eq!(
        tx.try_push(&[0u8; 24]).unwrap_err(),
        PushError::Lane(LaneError::Corrupt(Corruption::IndexBackwards))
    );
    assert!(tx.is_closed());
    assert_eq!(
        tx.try_push(b"").unwrap_err(),
        PushError::Lane(LaneError::Closed)
    );
    drop(tx);

    // Tail beyond head (taking the lane again re-validates the shared state).
    let (host, _guest, bar) = heap_pair(1 << 16, &[spec(1, H2G, Priority::High, 64)]);
    let off = host.lanes()[0].offset;
    let mut tx = host.producer(1).unwrap();
    let _ = tx.try_push(&[0u8; 24]).unwrap();
    let _ = tx.try_push(&[0u8; 24]).unwrap();
    unsafe { poke64(&bar, off + TAIL, 4096) };
    assert_eq!(
        tx.try_push(&[0u8; 24]).unwrap_err(),
        PushError::Lane(LaneError::Corrupt(Corruption::IndexOutOfRange))
    );
    drop(tx);
    assert!(matches!(
        host.producer(1).err(),
        Some(RegionError::Lane(LaneError::Corrupt(_)))
    ));
}

#[test]
fn peer_close_flags_are_honoured() {
    let (host, guest, _) = heap_pair(1 << 16, &[spec(1, H2G, Priority::High, 256)]);
    let mut tx = host.producer(1).unwrap();
    let mut rx = guest.consumer(1).unwrap();
    let _ = tx.try_push(b"last").unwrap();
    tx.close();
    let mut out = Vec::new();
    // Remaining data is still delivered, then the lane reports Closed.
    assert!(rx.try_pop(&mut out).unwrap().is_some());
    assert_eq!(rx.try_pop(&mut out).unwrap_err(), LaneError::Closed);
    rx.close();
    drop(tx);
    let (host, guest, _) = heap_pair(1 << 16, &[spec(1, H2G, Priority::High, 256)]);
    let mut tx = host.producer(1).unwrap();
    let mut rx = guest.consumer(1).unwrap();
    rx.close();
    assert_eq!(
        tx.try_push(b"x").unwrap_err(),
        PushError::Lane(LaneError::Closed)
    );
}

// ---- fuzzing ---------------------------------------------------------------

fn iters(full: u32) -> u32 {
    if cfg!(miri) {
        (full / 100).max(3)
    } else {
        full
    }
}

fn three_lanes() -> Vec<LaneSpec> {
    vec![
        spec(1, H2G, Priority::Control, 256),
        spec(2, G2H, Priority::High, 4096),
        spec(3, G2H, Priority::Bulk, 1024),
    ]
}

/// Exercise every endpoint of an attached region with bounded work.
fn exercise(r: &Region<HeapMapping>, rng: &mut Rng) {
    let lanes = r.lanes().to_vec();
    // Every accepted lane lies inside the mapping and the set is disjoint.
    for (i, a) in lanes.iter().enumerate() {
        assert!(a.size.is_power_of_two() && a.size >= MIN_DATA_SIZE);
        assert!(a.offset % LANE_ALIGN == 0 && a.offset >= HEADER_LEN);
        for b in &lanes[i + 1..] {
            assert!(
                a.offset + RING_HEADER_LEN + a.size <= b.offset
                    || b.offset + RING_HEADER_LEN + b.size <= a.offset
            );
        }
    }
    for l in lanes {
        if l.direction.producer() == r.side() {
            if let Ok(mut p) = r.producer(l.id) {
                for _ in 0..8 {
                    let mut buf = vec![0u8; rng.below(300) as usize];
                    rng.fill(&mut buf);
                    let _ = p.try_push(&buf);
                }
            }
        } else if let Ok(mut c) = r.consumer(l.id) {
            drain(&mut c);
        }
    }
}

fn drain(c: &mut Consumer<HeapMapping>) {
    let mut out = Vec::new();
    let mut small = [0u8; 16];
    for i in 0..4096 {
        let res = if i % 4 == 3 {
            match c.try_pop_into(&mut small) {
                Err(PopError::Lane(l)) => Err(l),
                // Too small for this record: take it with the growable variant.
                Err(PopError::BufferTooSmall { .. }) => c.try_pop(&mut out),
                Ok(v) => Ok(v),
            }
        } else {
            c.try_pop(&mut out)
        };
        match res {
            Ok(Some(m)) => {
                assert!(m.len <= c.max_record());
                assert!(out.len() <= c.max_record());
                assert!(out.capacity() <= 2 * c.max_record() + 64);
                assert_eq!(m.flags & FLAG_SKIP, 0);
            }
            Ok(None) => break,
            Err(_) => {
                assert!(c.is_closed());
                // sticky
                assert!(c.try_pop(&mut out).is_err());
                break;
            }
        }
    }
}

#[test]
fn fuzz_header_and_directory_bytes() {
    let mut rng = Rng::new(0xA11CE);
    for _ in 0..iters(4000) {
        let bar = HeapMapping::new(1 << 15);
        let _ = Region::create(bar.clone(), 3, Role::GuestAgent, &three_lanes()).unwrap();
        // Corrupt 1..8 random bytes in the header page (directory-biased).
        for _ in 0..1 + rng.below(8) {
            let off = if rng.below(4) == 0 {
                rng.below(HEADER_LEN as u64)
            } else {
                rng.below((DIR_OFFSET + 4 * DIR_ENTRY_LEN) as u64)
            } as usize;
            unsafe {
                match rng.below(3) {
                    0 => poke8(&bar, off, rng.next() as u8),
                    1 => poke8(&bar, off, 0xFF),
                    _ => poke8(&bar, off, 0),
                }
            }
        }
        for side in [Side::Guest, Side::Host] {
            if let Ok(r) = Region::attach(bar.clone(), side) {
                exercise(&r, &mut rng);
            }
        }
    }
}

#[test]
fn fuzz_structured_directory_values() {
    // Plausible-looking but wrong 64-bit values in offset/size fields.
    let mut rng = Rng::new(0xD1CE);
    let interesting = [
        0u64,
        1,
        63,
        64,
        65,
        127,
        128,
        4095,
        4096,
        4160,
        1 << 15,
        (1 << 15) - 64,
        (1 << 15) + 64,
        u64::MAX,
        u64::MAX - 7,
        1 << 63,
        (1 << 63) + 64,
        u32::MAX as u64,
    ];
    for _ in 0..iters(4000) {
        let bar = HeapMapping::new(1 << 15);
        let _ = Region::create(bar.clone(), 0, Role::GuestClient, &three_lanes()).unwrap();
        for _ in 0..1 + rng.below(4) {
            let e = rng.below(4) as usize; // sometimes beyond lane_count
            let field = [8usize, 16][rng.below(2) as usize];
            let v = interesting[rng.below(interesting.len() as u64) as usize];
            unsafe { poke64(&bar, DIR_OFFSET + e * DIR_ENTRY_LEN + field, v) };
        }
        if rng.below(3) == 0 {
            unsafe { poke32(&bar, 0x28, rng.below(70) as u32) };
        }
        if rng.below(3) == 0 {
            unsafe { poke64(&bar, 0x18, interesting[rng.below(interesting.len() as u64) as usize]) };
        }
        if let Ok(r) = Region::attach(bar.clone(), Side::Guest) {
            exercise(&r, &mut rng);
        }
    }
}

#[test]
fn fuzz_pure_random_region() {
    let mut rng = Rng::new(0xBAD);
    for round in 0..iters(2000) {
        let len = 1 << 14;
        let bar = HeapMapping::new(len);
        let mut bytes = vec![0u8; len];
        rng.fill(&mut bytes);
        // Half of the rounds start from a valid magic/version so the parser
        // gets past the first checks.
        if round % 2 == 0 {
            bytes[..8].copy_from_slice(b"WBSHMEM\0");
            bytes[8..12].copy_from_slice(&VERSION.to_le_bytes());
            bytes[12..16].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
            bytes[0x18..0x20].copy_from_slice(&(len as u64).to_le_bytes());
            bytes[0x20..0x24].copy_from_slice(&1u32.to_le_bytes());
            bytes[0x24..0x28].copy_from_slice(&2u32.to_le_bytes());
            bytes[0x28..0x2C].copy_from_slice(&(rng.below(5) as u32).to_le_bytes());
        }
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), bar.base(), len) };
        for side in [Side::Guest, Side::Host] {
            if let Ok(r) = Region::attach(bar.clone(), side) {
                exercise(&r, &mut rng);
            }
        }
    }
}

#[test]
fn fuzz_ring_memory_garbage() {
    let mut rng = Rng::new(0x51DE);
    for _ in 0..iters(6000) {
        let (host, _guest, bar) = heap_pair(1 << 15, &three_lanes());
        let lanes = host.lanes().to_vec();
        for l in &lanes {
            let base = l.offset;
            match rng.below(4) {
                // Fully random header and data.
                0 => {
                    let mut buf = vec![0u8; RING_HEADER_LEN + l.size];
                    rng.fill(&mut buf);
                    unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), bar.base().add(base), buf.len()) };
                }
                // Plausible indices, random record headers.
                1 | 2 => unsafe {
                    let tail = (rng.below(l.size as u64 / 8)) * 8;
                    let head = tail + rng.below(l.size as u64 / 8 + 2) * 8;
                    poke64(&bar, base + HEAD, head);
                    poke64(&bar, base + TAIL, tail);
                    let mut data = vec![0u8; l.size];
                    rng.fill(&mut data);
                    // Make many headers "almost valid": small lengths, no high flag.
                    let mut i = 0;
                    while i + 8 <= data.len() {
                        if rng.below(2) == 0 {
                            let len = rng.below(l.size as u64 / 2) as u32;
                            data[i..i + 4].copy_from_slice(&len.to_le_bytes());
                            data[i + 6..i + 8].copy_from_slice(&0u16.to_le_bytes());
                        }
                        i += 8 * (1 + rng.below(4) as usize);
                    }
                    std::ptr::copy_nonoverlapping(data.as_ptr(), bar.base().add(base + DATA), l.size);
                },
                // Boundary indices.
                _ => unsafe {
                    let vals = [0u64, 8, l.size as u64, l.size as u64 + 8, u64::MAX, u64::MAX - 7, 1 << 40];
                    poke64(&bar, base + HEAD, vals[rng.below(vals.len() as u64) as usize]);
                    poke64(&bar, base + TAIL, vals[rng.below(vals.len() as u64) as usize]);
                },
            }
        }
        // The host takes whatever it can: consumers drain hostile data, producers
        // push against hostile tails.
        exercise(&host, &mut rng);
    }
}

// The payload copy races with the tampering thread by design (that is the threat
// model); Miri would report the race, so it is skipped there.
#[test]
#[cfg_attr(miri, ignore)]
fn fuzz_concurrent_tampering_while_consuming() {
    // A "guest" thread scribbles random values into the ring header and data
    // while the host drains; the host must stay in bounds and never panic.
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let (host, _guest, bar) = heap_pair(1 << 15, &[spec(1, G2H, Priority::High, 1024)]);
    let off = host.lanes()[0].offset;
    let mut rx = host.consumer(1).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let b2 = bar.clone();
    let t = std::thread::spawn(move || {
        let mut rng = Rng::new(7);
        while !s2.load(Ordering::Relaxed) {
            unsafe {
                let w = rng.below(RING_HEADER_LEN as u64 + 1024) as usize & !7;
                let v = match rng.below(3) {
                    0 => rng.below(2048) & !7,
                    1 => rng.next(),
                    _ => rng.below(1 << 20),
                };
                // Atomic stores: the tampering thread itself must not be a data race.
                let p = b2.base().add(off + w) as *const std::sync::atomic::AtomicU64;
                (*p).store(v, Ordering::Relaxed);
            }
            std::thread::yield_now();
        }
    });
    let mut out = Vec::new();
    let n = iters(200_000);
    let mut popped = 0u64;
    for _ in 0..n {
        match rx.try_pop(&mut out) {
            Ok(Some(m)) => {
                assert!(m.len <= rx.max_record());
                popped += 1;
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
    stop.store(true, Ordering::Relaxed);
    t.join().unwrap();
    let _ = popped;
}

#[test]
fn endpoint_creation_rejects_indices_that_could_overflow() {
    for bad in [u64::MAX & !7, (1u64 << 62) + 8] {
        let (host, _guest, bar) = heap_pair(1 << 16, &[spec(1, H2G, Priority::High, 64)]);
        let off = host.lanes()[0].offset;
        // Consumer-owned tail and producer-owned head both start "just below" the limit.
        unsafe {
            poke64(&bar, off + HEAD, bad);
            poke64(&bar, off + TAIL, bad - 8);
        }
        assert!(matches!(
            host.producer(1).err(),
            Some(RegionError::Lane(LaneError::Corrupt(Corruption::IndexOutOfRange)))
        ));
    }
}
