// SPDX-License-Identifier: GPL-3.0-or-later

//! Two-thread stress: one producer pushes mixed-size records (0 B .. 64 KiB) with
//! sequence numbers and checksums, one consumer verifies order, length and
//! content. The mmap variant uses two independent mappings of one /dev/shm file
//! ("two-process-style": separate virtual mappings, separate `Region` handles,
//! nothing shared but the memory).

mod common;

use std::thread;
use std::time::Instant;

use common::*;
use winbridge_shm::*;

const K: u64 = 0x9E37_79B9_7F4A_7C15;

fn checksum(data: &[u8]) -> u64 {
    let mut c = 0xCBF2_9CE4_8422_2325u64;
    let mut it = data.chunks_exact(8);
    for w in &mut it {
        c = (c.rotate_left(5) ^ u64::from_le_bytes(w.try_into().unwrap())).wrapping_mul(K);
    }
    for &b in it.remainder() {
        c = (c.rotate_left(5) ^ b as u64).wrapping_mul(K);
    }
    c
}

fn size_for(rng: &mut Rng) -> usize {
    match rng.below(1000) {
        0..=599 => rng.below(257) as usize,
        600..=899 => 257 + rng.below(3840) as usize,
        900..=989 => 4096 + rng.below(12288) as usize,
        _ => rng.below(65537) as usize,
    }
}

/// Stall guard: a failing peer thread must fail the test, not hang it.
const BUDGET: std::time::Duration = std::time::Duration::from_secs(300);

/// Payloads shorter than 16 bytes are a prefix of this pattern of the sequence number.
fn short(seq: u64) -> [u8; 16] {
    let b = seq.to_le_bytes();
    let mut o = [0u8; 16];
    o[..8].copy_from_slice(&b);
    o[8..].copy_from_slice(&b);
    o
}

/// Payload layout for len >= 16: seq | filler | checksum(seq | filler).
fn fill(buf: &mut Vec<u8>, seq: u64, len: usize) {
    buf.clear();
    buf.resize(len, 0);
    if len < 16 {
        buf.copy_from_slice(&short(seq)[..len]);
        return;
    }
    buf[..8].copy_from_slice(&seq.to_le_bytes());
    let mut w = seq.wrapping_mul(K) | 1;
    for chunk in buf[8..len - 8].chunks_mut(8) {
        w = w.wrapping_mul(K).rotate_left(13) ^ seq;
        let b = w.to_le_bytes();
        chunk.copy_from_slice(&b[..chunk.len()]);
    }
    let c = checksum(&buf[..len - 8]);
    buf[len - 8..].copy_from_slice(&c.to_le_bytes());
}

fn verify(buf: &[u8], seq: u64, len: usize) {
    assert_eq!(buf.len(), len, "record {seq}: wrong length");
    if len < 16 {
        assert_eq!(buf, &short(seq)[..len], "record {seq}");
        return;
    }
    assert_eq!(&buf[..8], &seq.to_le_bytes(), "record {seq}: out of order");
    let c = u64::from_le_bytes(buf[len - 8..].try_into().unwrap());
    assert_eq!(c, checksum(&buf[..len - 8]), "record {seq}: checksum");
}

fn stress<M: Mapping>(host: Region<M>, guest: Region<M>, n: u64, label: &str) {
    let mut tx = host.producer(1).unwrap();
    let mut rx = guest.consumer(1).unwrap();
    let t0 = Instant::now();
    let t_start = t0;
    let producer = thread::spawn(move || {
        let mut rng = Rng::new(99);
        let mut buf = Vec::new();
        let mut bytes = 0u64;
        for seq in 0..n {
            let len = size_for(&mut rng);
            fill(&mut buf, seq, len);
            loop {
                match tx.try_push(&buf) {
                    Ok(_) => break,
                    Err(PushError::Full) => {
                        assert!(t_start.elapsed() < BUDGET, "producer stalled");
                        thread::yield_now();
                    }
                    Err(e) => panic!("push {seq}: {e}"),
                }
            }
            bytes += len as u64;
        }
        bytes
    });
    let consumer = thread::spawn(move || {
        let mut rng = Rng::new(99);
        let mut buf = Vec::new();
        for seq in 0..n {
            let len = size_for(&mut rng);
            let mut spins = 0u32;
            loop {
                match rx.try_pop(&mut buf).unwrap() {
                    Some(m) => {
                        assert_eq!(m.len, len);
                        break;
                    }
                    None => {
                        spins += 1;
                        if spins > 100 {
                            assert!(t_start.elapsed() < BUDGET, "consumer stalled");
                            thread::yield_now();
                        } else {
                            std::hint::spin_loop();
                        }
                    }
                }
            }
            verify(&buf, seq, len);
        }
        assert_eq!(rx.try_pop(&mut buf).unwrap(), None);
    });
    let bytes = producer.join().unwrap();
    consumer.join().unwrap();
    let secs = t0.elapsed().as_secs_f64();
    println!(
        "[{label}] {n} records, {:.1} MiB in {secs:.2} s: {:.0} MiB/s, {:.0} k records/s",
        bytes as f64 / 1048576.0,
        bytes as f64 / 1048576.0 / secs,
        n as f64 / secs / 1000.0
    );
}

const LANE: usize = 1 << 20;

#[cfg(all(unix, not(miri)))]
#[test]
fn stress_one_million_mixed_records_over_mmap() {
    let (a, b) = mm::file_pair(LANE + (1 << 16));
    let host = Region::create(
        a,
        0,
        Role::GuestAgent,
        &[spec(1, Direction::HostToGuest, Priority::High, LANE)],
    )
    .unwrap();
    let guest = Region::attach(b, Side::Guest).unwrap();
    stress(host, guest, 1_000_000, "mmap /dev/shm");
}

#[test]
fn stress_heap_block_two_handles() {
    let (host, guest, _) = heap_pair(
        LANE / 4 + (1 << 16),
        &[spec(1, Direction::HostToGuest, Priority::High, LANE / 4)],
    );
    let n = if cfg!(miri) { 200 } else { 100_000 };
    stress(host, guest, n, "heap");
}

#[test]
fn stress_guest_to_host_direction_small_ring() {
    // Tiny ring: constant wrap, skip markers and back-pressure.
    let (host, guest, _) = heap_pair(
        (1 << 17) + (1 << 16),
        &[spec(1, Direction::GuestToHost, Priority::Control, 1 << 17)],
    );
    // swap roles: guest produces, host consumes
    let n = if cfg!(miri) { 100 } else { 30_000 };
    let t_start = Instant::now();
    let mut tx = guest.producer(1).unwrap();
    let mut rx = host.consumer(1).unwrap();
    let h = thread::spawn(move || {
        let mut rng = Rng::new(5);
        let mut buf = Vec::new();
        for seq in 0..n {
            let len = size_for(&mut rng).min(60_000);
            fill(&mut buf, seq, len);
            while let Err(PushError::Full) = tx.try_push(&buf) {
                assert!(t_start.elapsed() < BUDGET, "producer stalled");
                thread::yield_now();
            }
        }
    });
    let mut rng = Rng::new(5);
    let mut buf = Vec::new();
    for seq in 0..n {
        let len = size_for(&mut rng).min(60_000);
        while rx.try_pop(&mut buf).unwrap().is_none() {
            assert!(t_start.elapsed() < BUDGET, "consumer stalled");
            thread::yield_now();
        }
        verify(&buf, seq, len);
    }
    h.join().unwrap();
}
