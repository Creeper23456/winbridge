// SPDX-License-Identifier: GPL-3.0-or-later

//! The guest's ring code (`venus/guest/wbvn_ring.h`, built here as `venus/tests/ring_peer.c` with
//! the system `cc`) against the host's (`winbridge-shm`) on a real shared file: both directions
//! at once, 10,000 records of 1 to 70,000 bytes each, which wraps a 16 MiB lane dozens of times
//! and exercises the skip markers. Every byte is checked on both sides.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use winbridge_venus::slot::{Slot, CLAIMED, CLOSED, LANE_G2H, LANE_H2G, SLOT_LEN};
use winbridge_shm::file;
use winbridge_shm::{PushError, Region, Side};

const RECORDS: u32 = 10_000;
const MAX_LEN: u32 = 70_000;

fn rec_len(seed: u32, i: u32) -> usize {
    1 + ((u64::from(i + seed) * 2_654_435_761) % u64::from(MAX_LEN)) as usize
}

fn rec_byte(seed: u32, i: u32, j: usize) -> u8 {
    (i + seed).wrapping_mul(31).wrapping_add(j as u32) as u8
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("wb-ring-interop-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn build_peer(dir: &Path) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../venus/tests/ring_peer.c");
    let exe = dir.join("ring_peer");
    let out = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread", "-o"])
        .arg(&exe)
        .arg(&src)
        .output()
        .expect("this test needs a C compiler named `cc`");
    assert!(out.status.success(), "cc failed:\n{}", String::from_utf8_lossy(&out.stderr));
    exe
}

#[test]
fn the_guest_ring_code_and_the_host_ring_code_agree_in_both_directions() {
    let dir = tmp("both");
    let peer = build_peer(&dir);
    let file = dir.join("slot");
    let f = std::fs::File::create(&file).unwrap();
    f.set_len(SLOT_LEN).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();

    let slot = Slot::open(&file, 0).unwrap();
    let region = slot.reset().unwrap();
    let block = slot.block();
    let mut rx = region.consumer(LANE_G2H).unwrap();
    let mut tx = region.producer(LANE_H2G).unwrap();

    let mut child = Command::new(&peer).arg(&file).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);

    let sender = std::thread::spawn(move || {
        let mut buf = Vec::new();
        for i in 0..RECORDS {
            buf.clear();
            buf.extend((0..rec_len(2, i)).map(|j| rec_byte(2, i, j)));
            loop {
                match tx.try_push(&buf) {
                    Ok(_) => break,
                    Err(PushError::Full) => std::thread::yield_now(),
                    Err(e) => panic!("push {i}: {e}"),
                }
            }
        }
    });

    let mut buf = Vec::new();
    for i in 0..RECORDS {
        loop {
            match rx.try_pop(&mut buf).unwrap() {
                Some(m) => {
                    assert_eq!(m.len, rec_len(1, i), "length of record {i}");
                    assert!(buf.iter().enumerate().all(|(j, b)| *b == rec_byte(1, i, j)), "bytes of record {i}");
                    break;
                }
                None => {
                    assert!(Instant::now() < deadline, "no record {i} from the guest side");
                    std::thread::yield_now();
                }
            }
        }
    }
    sender.join().unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "the guest side reported a mismatch");
    assert_eq!(block.state(), CLOSED);
    assert_eq!(block.nonce(), 0x1234);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_reset_clears_what_the_last_guest_left_and_frees_the_slot() {
    let dir = tmp("reset");
    let file = dir.join("slot");
    let f = std::fs::File::create(&file).unwrap();
    f.set_len(SLOT_LEN).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();

    let slot = Slot::open(&file, 3).unwrap();
    let host = slot.reset().unwrap();
    let block = slot.block();
    assert_eq!(block.state(), 0);
    block.set_state(CLAIMED);
    block.set_nonce(7);
    let guest = Region::attach(file::open(&file, SLOT_LEN).unwrap(), Side::Guest).unwrap();
    let mut tx = guest.producer(LANE_G2H).unwrap();
    assert!(tx.try_push(b"left over").is_ok());
    drop((tx, guest, host));

    let host = slot.reset().unwrap();
    assert_eq!((block.state(), block.nonce(), block.heartbeat()), (0, 0, 0));
    let mut rx = host.consumer(LANE_G2H).unwrap();
    assert!(rx.try_pop(&mut Vec::new()).unwrap().is_none(), "the old record is gone");
    std::fs::remove_dir_all(dir).unwrap();
}
