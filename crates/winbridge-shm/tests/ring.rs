// SPDX-License-Identifier: GPL-3.0-or-later

mod common;

use common::*;
use winbridge_shm::*;

const H2G: Direction = Direction::HostToGuest;
const G2H: Direction = Direction::GuestToHost;

#[test]
fn roundtrip_tags_padding_and_empty_record() {
    let (host, guest, _) = heap_pair(1 << 16, &[spec(1, H2G, Priority::Control, 4096)]);
    let mut tx = host.producer(1).unwrap();
    let mut rx = guest.consumer(1).unwrap();
    let mut buf = Vec::new();
    assert_eq!(rx.try_pop(&mut buf).unwrap(), None);

    let _ = tx.try_push_tagged(7, 3, b"abc").unwrap();
    let _ = tx.try_push(b"").unwrap();
    let _ = tx.try_push(&[9u8; 17]).unwrap();
    let m = rx.try_pop(&mut buf).unwrap().unwrap();
    assert_eq!((m.len, m.tag, m.flags, &buf[..]), (3, 7, 3, &b"abc"[..]));
    let m = rx.try_pop(&mut buf).unwrap().unwrap();
    assert_eq!((m.len, buf.len()), (0, 0));
    let m = rx.try_pop(&mut buf).unwrap().unwrap();
    assert_eq!((m.len, &buf[..]), (17, &[9u8; 17][..]));
    assert_eq!(rx.try_pop(&mut buf).unwrap(), None);
}

#[test]
fn wrap_with_skip_markers_preserves_content_and_order() {
    // 256-byte ring forces a skip marker very frequently.
    let (host, guest, _) = heap_pair(1 << 16, &[spec(1, H2G, Priority::High, 256)]);
    let mut tx = host.producer(1).unwrap();
    let mut rx = guest.consumer(1).unwrap();
    let mut rng = Rng::new(1);
    let mut buf = Vec::new();
    let mut expect = std::collections::VecDeque::new();
    let mut sent = 0u32;
    let mut got = 0u32;
    while got < 5000 {
        let n = rng.below(120) as usize;
        let mut p = vec![0u8; n];
        rng.fill(&mut p);
        match tx.try_push(&p) {
            Ok(_) => {
                expect.push_back(p);
                sent += 1;
            }
            Err(PushError::Full) => {}
            Err(e) => panic!("{e}"),
        }
        if rng.below(3) != 0 {
            while let Some(m) = rx.try_pop(&mut buf).unwrap() {
                let want = expect.pop_front().unwrap();
                assert_eq!(m.len, want.len());
                assert_eq!(buf, want);
                got += 1;
            }
        }
    }
    assert!(sent >= got);
}

#[test]
fn full_toolarge_badflags_and_pop_into() {
    let (host, guest, _) = heap_pair(1 << 16, &[spec(1, H2G, Priority::High, 128)]);
    let mut tx = host.producer(1).unwrap();
    let mut rx = guest.consumer(1).unwrap();
    assert_eq!(tx.try_push(&[0u8; 121]).unwrap_err(), PushError::TooLarge);
    assert_eq!(
        tx.try_push_tagged(0, FLAG_SKIP, b"x").unwrap_err(),
        PushError::BadFlags
    );
    let _ = tx.try_push(&[1u8; 56]).unwrap(); // 64 bytes
    let _ = tx.try_push(&[2u8; 56]).unwrap(); // 64 bytes
    assert_eq!(tx.try_push(b"x").unwrap_err(), PushError::Full);

    let mut small = [0u8; 8];
    assert_eq!(
        rx.try_pop_into(&mut small).unwrap_err(),
        PopError::BufferTooSmall { needed: 56 }
    );
    let mut big = [0u8; 64];
    let m = rx.try_pop_into(&mut big).unwrap().unwrap();
    assert_eq!((m.len, big[..56] == [1u8; 56]), (56, true));
    let _ = tx.try_push(b"x").unwrap();
}

#[test]
fn endpoint_acquisition_rules() {
    let (host, guest, _) = heap_pair(
        1 << 16,
        &[
            spec(1, H2G, Priority::Control, 4096),
            spec(2, G2H, Priority::Bulk, 4096),
        ],
    );
    assert_eq!(host.consumer(1).err(), Some(RegionError::WrongDirection));
    assert_eq!(guest.producer(1).err(), Some(RegionError::WrongDirection));
    assert_eq!(host.producer(9).err(), Some(RegionError::UnknownLane(9)));
    let p = host.producer(1).unwrap();
    assert_eq!(host.producer(1).err(), Some(RegionError::AlreadyTaken));
    drop(p);
    let _p = host.producer(1).unwrap();
    assert_eq!(guest.lanes().len(), 2);
    assert_eq!(guest.capabilities(), 0x5);
    assert_eq!(guest.guest_role(), Role::GuestAgent);
    assert_eq!(guest.host_role(), Role::Host);
    assert_eq!(guest.side(), Side::Guest);
}

fn valid_bar() -> (HeapMapping, Vec<LaneInfo>) {
    let bar = HeapMapping::new(1 << 16);
    let host = Region::create(
        bar.clone(),
        1,
        Role::GuestClient,
        &[
            spec(1, H2G, Priority::Control, 4096),
            spec(2, G2H, Priority::High, 8192),
        ],
    )
    .unwrap();
    (bar, host.lanes().to_vec())
}

fn dir(i: usize) -> usize {
    DIR_OFFSET + i * DIR_ENTRY_LEN
}

fn attach_err(bar: &HeapMapping) -> RegionError {
    Region::attach(bar.clone(), Side::Guest).err().unwrap()
}

#[test]
fn attach_rejects_bad_headers() {
    let (bar, lanes) = valid_bar();
    assert!(Region::attach(bar.clone(), Side::Guest).is_ok());
    assert!(Region::attach(bar.clone(), Side::Host).is_ok());

    // Uninitialised and wrong magic.
    assert_eq!(
        attach_err(&HeapMapping::new(1 << 16)),
        RegionError::NotInitialised
    );
    unsafe { poke64(&bar, 0, 0x1234) };
    assert_eq!(attach_err(&bar), RegionError::BadMagic);
    unsafe { poke64(&bar, 0, MAGIC) };

    unsafe { poke32(&bar, 0x08, 2) };
    assert_eq!(attach_err(&bar), RegionError::BadVersion(2));
    unsafe { poke32(&bar, 0x08, VERSION) };

    unsafe { poke32(&bar, 0x0C, 8192) };
    assert_eq!(attach_err(&bar), RegionError::BadHeader);
    unsafe { poke32(&bar, 0x0C, HEADER_LEN as u32) };

    // Declared length larger than the mapping.
    unsafe { poke64(&bar, 0x18, 1 << 20) };
    assert_eq!(attach_err(&bar), RegionError::BadHeader);
    unsafe { poke64(&bar, 0x18, 1 << 16) };

    unsafe { poke32(&bar, 0x20, 2) };
    assert_eq!(attach_err(&bar), RegionError::BadRole);
    unsafe { poke32(&bar, 0x20, 1) };
    unsafe { poke32(&bar, 0x24, 9) };
    assert_eq!(attach_err(&bar), RegionError::BadRole);
    unsafe { poke32(&bar, 0x24, Role::GuestClient as u32) };

    unsafe { poke32(&bar, 0x28, MAX_LANES as u32 + 1) };
    assert_eq!(attach_err(&bar), RegionError::TooManyLanes);
    unsafe { poke32(&bar, 0x28, u32::MAX) };
    assert_eq!(attach_err(&bar), RegionError::TooManyLanes);
    unsafe { poke32(&bar, 0x28, 2) };
    assert!(Region::attach(bar.clone(), Side::Guest).is_ok());

    // Mapping too small for the header page.
    let tiny = HeapMapping::new(1024);
    assert_eq!(attach_err(&tiny), RegionError::TooSmall);
    let _ = lanes;
}

#[test]
fn attach_rejects_bad_directory_entries() {
    let (bar, lanes) = valid_bar();
    let (a, b) = (lanes[0], lanes[1]);
    let fault = |bar: &HeapMapping| match attach_err(bar) {
        RegionError::Directory { fault, .. } => fault,
        e => panic!("unexpected {e:?}"),
    };
    let restore = |bar: &HeapMapping| unsafe {
        poke64(bar, dir(0) + 8, a.offset as u64);
        poke64(bar, dir(0) + 16, a.size as u64);
        poke64(bar, dir(1) + 8, b.offset as u64);
        poke64(bar, dir(1) + 16, b.size as u64);
        poke8(bar, dir(0) + 2, 0);
        poke8(bar, dir(0) + 3, 0);
        poke32(bar, dir(1), 2);
    };

    for bad_size in [0u64, 63, 100, 4097, u64::MAX, 1 << 41, (1 << 16) * 2] {
        unsafe { poke64(&bar, dir(0) + 16, bad_size) };
        let f = fault(&bar);
        assert!(
            matches!(f, DirFault::BadSize | DirFault::OutOfBounds),
            "{bad_size}: {f:?}"
        );
        restore(&bar);
    }
    unsafe { poke64(&bar, dir(0) + 16, 0) };
    assert_eq!(fault(&bar), DirFault::BadSize);
    unsafe { poke64(&bar, dir(0) + 16, 3000) };
    assert_eq!(fault(&bar), DirFault::BadSize);
    restore(&bar);

    // Offsets: misaligned, inside the header page, past the end, wrapping u64.
    for (off, want) in [
        (a.offset as u64 + 8, DirFault::BadOffset),
        (0, DirFault::BadOffset),
        (64, DirFault::BadOffset),
        (1 << 16, DirFault::OutOfBounds),
        (u64::MAX - 63, DirFault::OutOfBounds),
        (u64::MAX & !63, DirFault::OutOfBounds),
    ] {
        unsafe { poke64(&bar, dir(0) + 8, off) };
        assert_eq!(fault(&bar), want, "offset {off:#x}");
        restore(&bar);
    }

    // Overlap: move lane 1 into the middle of lane 0 / onto it.
    unsafe { poke64(&bar, dir(1) + 8, a.offset as u64) };
    assert_eq!(fault(&bar), DirFault::Overlap);
    restore(&bar);
    unsafe { poke64(&bar, dir(1) + 8, a.offset as u64 + 64) };
    assert_eq!(fault(&bar), DirFault::Overlap);
    restore(&bar);

    unsafe { poke32(&bar, dir(1), 1) };
    assert_eq!(fault(&bar), DirFault::DuplicateId);
    restore(&bar);
    unsafe { poke8(&bar, dir(0) + 2, 7) };
    assert_eq!(fault(&bar), DirFault::BadDirection);
    restore(&bar);
    unsafe { poke8(&bar, dir(0) + 3, 9) };
    assert_eq!(fault(&bar), DirFault::BadPriority);
    restore(&bar);
    assert!(Region::attach(bar.clone(), Side::Guest).is_ok());
}

#[test]
fn create_rejects_bad_specs() {
    let bar = || HeapMapping::new(1 << 14);
    let mk = |specs: &[LaneSpec]| Region::create(bar(), 0, Role::GuestAgent, specs).err();
    assert!(matches!(
        mk(&[spec(1, H2G, Priority::High, 0)]),
        Some(RegionError::Directory { fault: DirFault::BadSize, .. })
    ));
    assert!(matches!(
        mk(&[spec(1, H2G, Priority::High, 1000)]),
        Some(RegionError::Directory { fault: DirFault::BadSize, .. })
    ));
    assert!(matches!(
        mk(&[spec(1, H2G, Priority::High, 1 << 20)]),
        Some(RegionError::Directory { fault: DirFault::OutOfBounds, .. })
    ));
    assert!(matches!(
        mk(&[spec(1, H2G, Priority::High, 64), spec(1, G2H, Priority::High, 64)]),
        Some(RegionError::Directory { fault: DirFault::DuplicateId, .. })
    ));
    assert_eq!(
        Region::create(bar(), 0, Role::Host, &[]).err(),
        Some(RegionError::BadRole)
    );
}

// ---- wake-up flag protocol -------------------------------------------------

mod wake_tests {
    use super::*;
    use std::sync::mpsc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn kick_only_when_sleeping_and_ring_was_empty() {
        let (host, guest, _) = heap_pair(1 << 16, &[spec(1, H2G, Priority::Control, 4096)]);
        let mut tx = host.producer(1).unwrap();
        let mut rx = guest.consumer(1).unwrap();
        let mut buf = Vec::new();

        // Consumer awake: never kick.
        assert!(!tx.try_push(b"a").unwrap().kick);
        assert!(rx.try_pop(&mut buf).unwrap().is_some());

        // Sleeping and empty: kick once; second push sees a non-empty ring.
        rx.set_sleeping(true);
        assert!(tx.consumer_sleeping());
        assert!(tx.try_push(b"b").unwrap().kick);
        assert!(!tx.try_push(b"c").unwrap().kick);
        rx.set_sleeping(false);
        assert!(!tx.consumer_sleeping());
        assert!(rx.try_pop(&mut buf).unwrap().is_some());
        assert!(rx.try_pop(&mut buf).unwrap().is_some());

        // Sleeping but the ring still holds an old record: no kick.
        assert!(!tx.try_push(b"d").unwrap().kick);
        rx.set_sleeping(true);
        assert!(!tx.try_push(b"e").unwrap().kick);
    }

    struct ChanKick(mpsc::Sender<()>);
    impl Kick for ChanKick {
        fn kick(&self) {
            let _ = self.0.send(());
        }
    }

    /// Consumer sleeps with a 5 s timer, so only the kick can wake it in time.
    #[test]
    fn no_lost_wakeup_two_threads() {
        let (host, guest, _) = heap_pair(1 << 16, &[spec(1, H2G, Priority::Control, 4096)]);
        let mut tx = host.producer(1).unwrap();
        let mut rx = guest.consumer(1).unwrap();
        let waker = Arc::new(Waker::new());
        let (ktx, krx) = mpsc::channel::<()>();
        let kick = ChanKick(ktx);
        let w2 = waker.clone();
        let _kick_rx_thread = thread::spawn(move || {
            while krx.recv().is_ok() {
                w2.wake();
            }
        });

        let rounds: u32 = if cfg!(miri) { 3 } else { 200 };
        let cfg = IdleConfig {
            spin_polls: 10,
            sleep: Duration::from_secs(5),
        };
        let done = Arc::new(AtomicU32::new(0));
        let done_c = done.clone();
        let consumer = thread::spawn(move || {
            let mut idle = Idle::new(cfg);
            let mut buf = Vec::new();
            for i in 0..rounds {
                let m = rx
                    .pop_wait(&mut buf, &mut idle, &waker, Some(Duration::from_secs(10)))
                    .unwrap()
                    .expect("timed out: lost wake-up");
                assert_eq!(m.len, 4);
                assert_eq!(buf, i.to_le_bytes());
                let m2 = rx
                    .pop_wait(&mut buf, &mut idle, &waker, Some(Duration::from_secs(10)))
                    .unwrap()
                    .expect("timed out: lost wake-up");
                assert_eq!(m2.len, 8);
                done_c.store(i + 1, Ordering::Release);
            }
        });

        // Producer: wait until the consumer has published `sleeping`, then push
        // two records (the second must not need its own kick).
        for i in 0..rounds {
            let t0 = Instant::now();
            while !tx.consumer_sleeping() {
                assert!(t0.elapsed() < Duration::from_secs(10), "consumer never slept");
                thread::yield_now();
            }
            tx.push_and_kick(&i.to_le_bytes(), &kick).unwrap();
            tx.push_and_kick(&0u64.to_le_bytes(), &kick).unwrap();
            // The consumer must consume both well within the 5 s timer.
            let t1 = Instant::now();
            while done.load(Ordering::Acquire) <= i && t1.elapsed() < Duration::from_millis(500) {
                thread::yield_now();
            }
            assert!(
                done.load(Ordering::Acquire) > i,
                "round {i}: consumer did not wake within 500 ms"
            );
        }
        consumer.join().unwrap();
    }

    /// Free-running producer with random gaps: every record must arrive, and the
    /// whole run must finish far sooner than one sleep interval per record.
    #[test]
    fn no_lost_wakeup_random_gaps() {
        let (host, guest, _) = heap_pair(1 << 16, &[spec(1, H2G, Priority::Control, 4096)]);
        let mut tx = host.producer(1).unwrap();
        let mut rx = guest.consumer(1).unwrap();
        let waker = Arc::new(Waker::new());
        let (ktx, krx) = mpsc::channel::<()>();
        let kick = ChanKick(ktx);
        let w2 = waker.clone();
        thread::spawn(move || {
            while krx.recv().is_ok() {
                w2.wake();
            }
        });
        let n: u32 = if cfg!(miri) { 20 } else { 2000 };
        let consumer = thread::spawn(move || {
            let cfg = IdleConfig {
                spin_polls: 50,
                sleep: Duration::from_secs(2),
            };
            let mut idle = Idle::new(cfg);
            let mut buf = Vec::new();
            for i in 0..n {
                rx.pop_wait(&mut buf, &mut idle, &waker, Some(Duration::from_secs(20)))
                    .unwrap()
                    .expect("lost wake-up");
                assert_eq!(buf, i.to_le_bytes());
            }
        });
        let mut rng = Rng::new(42);
        let t0 = Instant::now();
        for i in 0..n {
            if rng.below(4) == 0 {
                thread::sleep(Duration::from_micros(rng.below(300)));
            }
            tx.push_and_kick(&i.to_le_bytes(), &kick).unwrap();
        }
        consumer.join().unwrap();
        // With a 2 s sleep interval a single lost kick would add >= 2 s.
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    }

    #[test]
    fn waker_token_survives_early_wake_and_times_out() {
        let w = Waker::new();
        w.wake();
        assert!(w.wait_timeout(Duration::from_secs(1)));
        assert!(!w.wait_timeout(Duration::from_millis(5)));
    }

    #[test]
    fn pop_wait_times_out_when_empty() {
        let (host, guest, _) = heap_pair(1 << 16, &[spec(1, H2G, Priority::Control, 4096)]);
        let _tx = host.producer(1).unwrap();
        let mut rx = guest.consumer(1).unwrap();
        let mut idle = Idle::new(IdleConfig {
            spin_polls: 5,
            sleep: Duration::from_millis(1),
        });
        let mut buf = Vec::new();
        let w = Waker::new();
        let t0 = Instant::now();
        let r = rx
            .pop_wait(&mut buf, &mut idle, &w, Some(Duration::from_millis(20)))
            .unwrap();
        assert_eq!(r, None);
        assert!(t0.elapsed() < Duration::from_millis(500));
        assert!(!_tx.consumer_sleeping());
    }
}
