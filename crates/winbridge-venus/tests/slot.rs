// SPDX-License-Identifier: GPL-3.0-or-later

//! The slot channel as the serve loop uses it: records in, close detection, and the wake-up
//! protocol over the socket that stands in for the virtio-serial port.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use winbridge_shm::file::{self, FileMapping};
use winbridge_shm::{Region, Side};
use winbridge_venus::channel::Channel;
use winbridge_venus::slot::{Arrival, ShmChannel, Slot, CLAIMED, CLOSED, FREE, LANE_G2H, LANE_H2G, SLOT_LEN};

struct Rig {
    dir: PathBuf,
    host: ShmChannel,
    guest: Region<FileMapping>,
    guest_sock: UnixStream,
    block: winbridge_venus::slot::SlotBlock,
    _region: Region<FileMapping>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rig(tag: &str) -> Rig {
    let dir = std::env::temp_dir().join(format!("wb-slot-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("slot");
    std::fs::File::create(&path).unwrap().set_len(SLOT_LEN).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let slot = Slot::open(&path, 0).unwrap();
    let region = slot.reset().unwrap();
    let block = slot.block();
    let guest = Region::attach(file::open(&path, SLOT_LEN).unwrap(), Side::Guest).unwrap();
    block.set_state(CLAIMED);
    let (host_sock, guest_sock) = UnixStream::pair().unwrap();
    let host = ShmChannel::new(&region, block.clone(), host_sock).unwrap();
    Rig { dir, host, guest, guest_sock, block, _region: region }
}

#[test]
fn records_arrive_in_order_and_a_closed_slot_ends_the_stream_after_they_are_read() {
    let mut r = rig("close");
    let mut tx = r.guest.producer(LANE_G2H).unwrap();
    assert!(tx.try_push(b"first ").is_ok());
    assert!(tx.try_push(b"second").is_ok());
    let mut buf = Vec::new();
    assert!(r.host.read_available(&mut buf).unwrap());
    assert_eq!(buf, b"first second");
    // The guest closes after its last record: the record is delivered first, then the end.
    assert!(tx.try_push(b" last").is_ok());
    r.block.set_state(CLOSED);
    buf.clear();
    assert!(r.host.read_available(&mut buf).unwrap());
    assert_eq!(buf, b" last");
    assert!(!r.host.read_available(&mut buf).unwrap());
}

#[test]
fn a_guest_that_claims_and_leaves_between_two_looks_is_still_noticed() {
    let r = rig("arrival");
    // Claimed and closed again before the host looked: the slot is CLOSED, never seen CLAIMED.
    r.block.set_state(CLOSED);
    assert_eq!(r.block.wait_for_guest(|| false), Arrival::Left);
    r.block.set_state(FREE);
    assert_eq!(r.block.wait_for_guest(|| true), Arrival::Abandoned);
    let block = r.block.clone();
    let guest = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        block.set_state(CLAIMED);
    });
    assert_eq!(r.block.wait_for_guest(|| false), Arrival::Claimed);
    guest.join().unwrap();
}

#[test]
fn a_sleeping_host_wakes_on_a_kick_and_kicks_a_sleeping_guest() {
    let mut r = rig("kick");
    let mut tx = r.guest.producer(LANE_G2H).unwrap();

    // The host sleeps (the wait is longer than the polling limit): it must publish `sleeping`
    // so that the guest knows to send a wake-up byte, and returns when one arrives.
    let kicker = r.guest_sock.try_clone().unwrap();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        let pushed = tx.try_push(b"x").unwrap();
        assert!(pushed.kick, "the guest must be told that the host sleeps");
        use std::io::Write;
        (&kicker).write_all(&[1]).unwrap();
        tx
    });
    let started = Instant::now();
    r.host.wait(-1, Duration::from_secs(5)).unwrap();
    assert!(started.elapsed() < Duration::from_secs(2), "the wake-up did not end the wait");
    let _tx = t.join().unwrap();
    let mut buf = Vec::new();
    assert!(r.host.read_available(&mut buf).unwrap());
    assert_eq!(buf, b"x");

    // A guest that sleeps gets one byte for a write into an empty lane, and none while it is awake.
    let mut rx = r.guest.consumer(LANE_H2G).unwrap();
    r.host.write_all(b"awake").unwrap();
    r.guest_sock.set_nonblocking(true).unwrap();
    let mut b = [0u8; 8];
    assert!(r.guest_sock.read(&mut b).is_err(), "no kick for an awake guest");
    let mut out = Vec::new();
    assert!(rx.try_pop(&mut out).unwrap().is_some());
    rx.set_sleeping(true);
    r.host.write_all(b"asleep").unwrap();
    assert_eq!(r.guest_sock.read(&mut b).unwrap(), 1, "one wake-up byte");
    assert!(rx.try_pop(&mut out).unwrap().is_some());
    assert_eq!(out, b"asleep");
    assert!(r.guest_sock.as_raw_fd() >= 0);
}

#[test]
fn a_write_larger_than_a_record_is_split_and_reads_back_whole() {
    let mut r = rig("big");
    let data: Vec<u8> = (0..(3 << 20) + 17).map(|i| (i % 251) as u8).collect();
    let mut rx = r.guest.consumer(LANE_H2G).unwrap();
    let reader = std::thread::spawn(move || {
        let mut got = Vec::new();
        let mut rec = Vec::new();
        while got.len() < (3 << 20) + 17 {
            match rx.try_pop(&mut rec).unwrap() {
                Some(_) => got.extend_from_slice(&rec),
                None => std::thread::yield_now(),
            }
        }
        got
    });
    r.host.write_all(&data).unwrap();
    assert_eq!(reader.join().unwrap(), data);
}
