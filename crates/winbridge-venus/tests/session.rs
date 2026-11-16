// SPDX-License-Identifier: GPL-3.0-or-later

//! Session logic against a fake renderer: shmem mirroring order, echo suppression, fences.

use std::io::{Read, Write};
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use winbridge_venus::channel::StreamChannel;
use winbridge_venus::proto::{self, Hdr, PushBuilder, HDR_LEN};
use winbridge_venus::session::{Renderer, Session};

struct Map(*mut u8, usize);
// SAFETY: a shared mapping used through atomics only.
unsafe impl Send for Map {}

#[derive(Default)]
struct Inner {
    maps: std::collections::HashMap<u32, Map>,
    fences: Vec<(u32, u64)>,
    submits: Vec<Vec<u8>>,
    /// Runs on every submit: the fake renderer's "work".
    on_submit: Option<Box<dyn FnMut(&mut Inner) + Send>>,
    /// The word new blobs are filled with: device memory starts out with whatever it holds.
    blob_fill: u64,
}

impl Inner {
    fn word(&self, res: u32, off: usize) -> &std::sync::atomic::AtomicU64 {
        let m = &self.maps[&res];
        assert!(off + 8 <= m.1);
        // SAFETY: in bounds and aligned.
        unsafe { &*m.0.add(off).cast() }
    }
}

#[derive(Clone, Default)]
struct Mock(Arc<Mutex<Inner>>);

impl Renderer for Mock {
    fn capset(&mut self) -> Result<Vec<u8>, String> {
        Ok(vec![7; 16])
    }
    fn create_context(&mut self, _: u32) -> Result<(), i32> {
        Ok(())
    }
    fn destroy_context(&mut self, _: u32) {}
    fn attach_shmem(&mut self, _: u32, res_id: u32, fd: RawFd, size: u64) -> Result<(), i32> {
        // SAFETY: maps the session's memfd shared.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), size as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
        assert_ne!(p, libc::MAP_FAILED);
        self.0.lock().unwrap().maps.insert(res_id, Map(p.cast(), size as usize));
        Ok(())
    }
    fn unref(&mut self, res_id: u32) {
        self.0.lock().unwrap().maps.remove(&res_id);
    }
    fn submit(&mut self, _: u32, cs: &[u8]) -> Result<(), i32> {
        let mut g = self.0.lock().unwrap();
        g.submits.push(cs.to_vec());
        if let Some(mut f) = g.on_submit.take() {
            f(&mut g);
            g.on_submit = Some(f);
        }
        Ok(())
    }
    fn create_fence(&mut self, _: u32, ring_idx: u32, id: u64) -> Result<(), i32> {
        self.0.lock().unwrap().fences.push((ring_idx, id));
        Ok(())
    }
    fn poll(&mut self, _: u32) -> Vec<(u32, u64)> {
        std::mem::take(&mut self.0.lock().unwrap().fences)
    }
    fn poll_fd(&self, _: u32) -> RawFd {
        -1
    }
    fn create_blob(&mut self, _: u32, res_id: u32, _blob_id: u64, size: u64) -> Result<(), i32> {
        let size = ((size + 4095) & !4095) as usize;
        // SAFETY: an anonymous shared mapping of our own.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED | libc::MAP_ANONYMOUS, -1, 0) };
        assert_ne!(p, libc::MAP_FAILED);
        let mut g = self.0.lock().unwrap();
        let fill = g.blob_fill;
        for i in 0..size / 8 {
            // SAFETY: in bounds and aligned.
            unsafe { p.cast::<u64>().add(i).write(fill) };
        }
        g.maps.insert(res_id, Map(p.cast(), size));
        Ok(())
    }
    fn map_blob(&mut self, res_id: u32) -> Result<(*mut u8, u64), i32> {
        let g = self.0.lock().unwrap();
        let m = &g.maps[&res_id];
        Ok((m.0, m.1 as u64))
    }
    fn unmap_blob(&mut self, _: u32) {}
}

struct Guest(UnixStream);

impl Guest {
    fn send(&mut self, kind: u32, req: u32, arg: u32, payload: &[u8]) {
        let h = Hdr { len: payload.len() as u32, kind, req, arg };
        self.0.write_all(&h.encode()).unwrap();
        self.0.write_all(payload).unwrap();
    }

    fn recv(&mut self, wait: Duration) -> Option<(Hdr, Vec<u8>)> {
        self.0.set_read_timeout(Some(wait)).unwrap();
        let mut h = [0u8; HDR_LEN];
        self.0.read_exact(&mut h).ok()?;
        let hdr = Hdr::decode(&h);
        let mut p = vec![0u8; hdr.len as usize];
        self.0.read_exact(&mut p).ok()?;
        Some((hdr, p))
    }

    fn request(&mut self, kind: u32, arg: u32, payload: &[u8]) -> (Hdr, Vec<u8>) {
        self.send(kind, 1, arg, payload);
        loop {
            let (h, p) = self.recv(Duration::from_secs(5)).expect("reply");
            if h.kind == kind | proto::REPLY {
                return (h, p);
            }
        }
    }
}

fn start(mock: &Mock) -> (Guest, std::thread::JoinHandle<()>) {
    let (a, b) = UnixStream::pair().unwrap();
    let m = mock.clone();
    let t = std::thread::spawn(move || {
        let mut m = m;
        Session::new(&mut m, StreamChannel::new(b)).run().unwrap();
    });
    let mut g = Guest(a);
    let (h, p) = g.request(proto::HELLO, 0, &proto::VERSION.to_le_bytes());
    assert_eq!(h.arg, 0);
    assert_eq!(proto::u32_at(&p, 0), proto::VERSION);
    (g, t)
}

fn create(g: &mut Guest, res: u32, size: u64) {
    let (h, _) = g.request(proto::SHM_CREATE, res, &size.to_le_bytes());
    assert_eq!(h.arg, 0);
}

#[test]
fn guest_words_reach_the_renderer_and_are_not_echoed() {
    let mock = Mock::default();
    let (mut g, t) = start(&mock);
    create(&mut g, 1, 0x1000);
    g.send(proto::SHM_ROLE, 0, 1, &[]);

    let mut b = PushBuilder::default();
    b.add(1, 64, &9u64.to_le_bytes()); // tail
    b.add(1, 192, &[0x5a; 16]); // commands
    g.send(proto::PUSH, 0, 0, &b.finish());
    // A round trip orders the push before the check.
    create(&mut g, 2, 0x1000);
    {
        let m = mock.0.lock().unwrap();
        assert_eq!(m.word(1, 64).load(std::sync::atomic::Ordering::Relaxed), 9);
        assert_eq!(m.word(1, 192).load(std::sync::atomic::Ordering::Relaxed), 0x5a5a_5a5a_5a5a_5a5a);
    }
    assert!(g.recv(Duration::from_millis(80)).is_none(), "the guest's own words were echoed back");
    drop(g);
    t.join().unwrap();
}

#[test]
fn replies_arrive_before_the_head_and_before_the_signal() {
    let mock = Mock::default();
    let (mut g, t) = start(&mock);
    create(&mut g, 1, 0x1000); // ring
    create(&mut g, 2, 0x1000); // reply pool
    g.send(proto::SHM_ROLE, 0, 1, &[]);
    mock.0.lock().unwrap().on_submit = Some(Box::new(|m| {
        m.word(2, 0).store(0xAA, std::sync::atomic::Ordering::Relaxed);
        m.word(1, 0).store(7, std::sync::atomic::Ordering::Release);
    }));

    let mut sub = Vec::new();
    for v in [3u32, 1, 8, 0] {
        sub.extend_from_slice(&v.to_le_bytes());
    }
    sub.extend_from_slice(&5u32.to_le_bytes());
    sub.extend_from_slice(&0u32.to_le_bytes());
    sub.extend_from_slice(&1u64.to_le_bytes());
    sub.extend_from_slice(&[0; 8]);
    g.send(proto::SUBMIT, 0, 0, &sub);

    let mut order = Vec::new();
    while let Some((h, p)) = g.recv(Duration::from_millis(300)) {
        match h.kind {
            proto::PUSH => order.extend(proto::parse_push(&p).iter().map(|r| ("push", r.res_id))),
            proto::SYNC_SIGNAL => {
                assert_eq!((proto::u32_at(&p, 8), proto::u64_at(&p, 16)), (5, 1));
                order.push(("signal", 0));
            }
            other => panic!("unexpected message {other}"),
        }
        if order.last() == Some(&("signal", 0)) {
            break;
        }
    }
    let at = |what: (&str, u32)| order.iter().position(|o| *o == what).unwrap_or_else(|| panic!("{what:?} missing in {order:?}"));
    assert!(at(("push", 2)) < at(("push", 1)), "reply data must precede the head: {order:?}");
    assert!(at(("push", 1)) < at(("signal", 0)), "{order:?}");
    drop(g);
    t.join().unwrap();
}

#[test]
fn clearing_a_status_bit_reaches_the_renderer_and_the_renderer_can_set_it_again() {
    use std::sync::atomic::Ordering::SeqCst;
    let mock = Mock::default();
    let (mut g, t) = start(&mock);
    create(&mut g, 1, 0x1000);
    g.send(proto::SHM_ROLE, 0, 1, &[]);

    // The renderer marks the ring idle and alive; the guest learns of it.
    mock.0.lock().unwrap().word(1, 128).store(0b101, SeqCst);
    let (_, p) = g.recv(Duration::from_secs(2)).expect("status push");
    let r = &proto::parse_push(&p)[0];
    assert_eq!((r.offset, u64::from_le_bytes(r.data[..8].try_into().unwrap())), (128, 0b101));

    // The guest clears ALIVE, as its watchdog does.
    let mut op = Vec::new();
    op.extend_from_slice(&128u32.to_le_bytes());
    op.extend_from_slice(&0b100u32.to_le_bytes());
    g.send(proto::STATUS_CLEAR, 0, 1, &op);
    create(&mut g, 2, 0x1000);
    assert_eq!(mock.0.lock().unwrap().word(1, 128).load(SeqCst), 0b001);
    assert!(g.recv(Duration::from_millis(80)).is_none(), "the cleared word was echoed back");

    // The monitor sets ALIVE again; that must reach the guest.
    mock.0.lock().unwrap().word(1, 128).fetch_or(0b100, SeqCst);
    let (_, p) = g.recv(Duration::from_secs(2)).expect("alive push");
    let r = &proto::parse_push(&p)[0];
    assert_eq!(u64::from_le_bytes(r.data[..8].try_into().unwrap()), 0b101);
    drop(g);
    t.join().unwrap();
}

#[test]
fn a_pushed_tail_wakes_an_idle_ring_and_only_an_idle_one() {
    use std::sync::atomic::Ordering::SeqCst;
    let mock = Mock::default();
    let (mut g, t) = start(&mock);
    create(&mut g, 1, 0x1000);
    g.send(proto::SHM_ROLE, 0, 1, &0xabcd_u64.to_le_bytes());

    let push_tail = |g: &mut Guest, tail: u64| {
        let mut b = PushBuilder::default();
        b.add(1, 64, &tail.to_le_bytes());
        g.send(proto::PUSH, 0, 0, &b.finish());
        create(g, 100 + tail as u32, 0x1000); // round trip: the push is handled first
    };

    // Running ring: the renderer sees the tail by itself, no wake.
    push_tail(&mut g, 1);
    assert!(mock.0.lock().unwrap().submits.is_empty());

    // Idle ring: the guest's status read may predate the idle, so the host wakes it.
    mock.0.lock().unwrap().word(1, 128).store(1, SeqCst);
    let _ = g.recv(Duration::from_secs(2)).expect("status push");
    push_tail(&mut g, 2);
    let submits = mock.0.lock().unwrap().submits.clone();
    assert_eq!(submits, vec![proto::notify_ring_cs(0xabcd, 2).to_vec()]);
    drop(g);
    t.join().unwrap();
}

fn submit_payload(ring_idx: u32, syncs: &[(u32, u64)], cs: &[u8]) -> Vec<u8> {
    let mut p = Vec::new();
    for v in [ring_idx, syncs.len() as u32, cs.len() as u32, 0] {
        p.extend_from_slice(&v.to_le_bytes());
    }
    for (id, v) in syncs {
        p.extend_from_slice(&id.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes());
        p.extend_from_slice(&v.to_le_bytes());
    }
    p.extend_from_slice(cs);
    p
}

#[test]
fn device_memory_goes_down_on_push_and_comes_back_when_a_fence_retires() {
    use std::sync::atomic::Ordering::SeqCst;
    let mock = Mock::default();
    mock.0.lock().unwrap().blob_fill = 0x5555;
    let (mut g, t) = start(&mock);

    // The batch runs before the blob exists; the blob is then created and mapped.
    let batch = submit_payload(0, &[], &[9; 8]);
    let mut p = Vec::new();
    p.extend_from_slice(&0x2000u64.to_le_bytes());
    p.extend_from_slice(&77u64.to_le_bytes());
    p.extend_from_slice(&1u32.to_le_bytes());
    p.extend_from_slice(&(batch.len() as u32).to_le_bytes());
    p.extend_from_slice(&batch);
    let (h, _) = g.request(proto::BO_CREATE, 3, &p);
    assert_eq!(h.arg, 0);
    assert_eq!(mock.0.lock().unwrap().submits, vec![vec![9u8; 8]]);

    // The guest writes memory the GPU will read.
    let mut b = PushBuilder::default();
    b.add(3, 16, &0x1111_u64.to_le_bytes());
    g.send(proto::PUSH, 0, 0, &b.finish());
    create(&mut g, 2, 0x1000); // round trip: the push is handled first
    assert_eq!(mock.0.lock().unwrap().word(3, 16).load(SeqCst), 0x1111);

    // The GPU writes a word. Nothing is sent until a fence retires, and what the memory held
    // when it was created is never sent.
    mock.0.lock().unwrap().word(3, 64).store(0xBEEF, SeqCst);
    assert!(g.recv(Duration::from_millis(80)).is_none(), "device memory was scanned without a fence");

    g.send(proto::SUBMIT, 0, 0, &submit_payload(1, &[(5, 1)], &[0; 8]));
    let mut pushes = Vec::new();
    loop {
        let (h, p) = g.recv(Duration::from_secs(2)).expect("push then signal");
        if h.kind == proto::SYNC_SIGNAL {
            break;
        }
        assert_eq!(h.kind, proto::PUSH);
        pushes.extend(proto::parse_push(&p).iter().map(|r| (r.res_id, r.offset, u64::from_le_bytes(r.data[..8].try_into().unwrap()), r.data.len())));
    }
    assert_eq!(pushes, vec![(3, 64, 0xBEEF, 8)], "only the GPU's write comes back, before the signal");

    let (h, _) = g.request(proto::SHM_DESTROY, 3, &[]);
    assert_eq!(h.arg, 0);
    drop(g);
    t.join().unwrap();
}
