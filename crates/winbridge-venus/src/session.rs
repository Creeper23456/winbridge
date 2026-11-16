// SPDX-License-Identifier: GPL-3.0-or-later

//! One guest session on the WBVN channel (see `venus/guest/wbvn_proto.h`).
//!
//! The guest cannot map host memory, so the venus shmems it creates are mirrored: this side owns
//! a memfd per shmem, keeps a snapshot of what the guest has seen, and sends the 8-byte words
//! that the renderer changed. Words the guest changed arrive in `PUSH` messages and are applied
//! last range first. The ring is scanned before the other shmems and sent after them, so a ring
//! head never becomes visible ahead of the reply it covers.

use crate::proto::{self, Hdr, PushBuilder, HDR_LEN};
use crate::stats::Stats;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Shmems up to this size are scanned for renderer writes (rings, reply pools). Larger ones
/// (the command-stream pool) are only ever written by the guest.
pub const SCAN_MAX: usize = 2 << 20;
/// Runs closer than this many words are sent as one.
const RUN_GAP_WORDS: usize = 8;
/// The one context id of a session.
const CTX: u32 = 1;
/// Timelines the context offers (`PROXY_CONTEXT_TIMELINE_COUNT`).
const MAX_TIMELINES: u32 = 64;
/// While anything happened this recently the loop polls at `BUSY_POLL`.
const BUSY_WINDOW: Duration = Duration::from_millis(5);
const BUSY_POLL: Duration = Duration::from_micros(100);
const IDLE_POLL: Duration = Duration::from_millis(2);
/// A full scan runs at least this often even if nothing announced a change.
const SCAN_PERIOD: Duration = Duration::from_millis(5);

/// The Venus renderer: virglrenderer in production, a fake in the tests.
pub trait Renderer {
    /// The venus capset (`struct virgl_renderer_capset_venus`) as raw bytes.
    fn capset(&mut self) -> Result<Vec<u8>, String>;
    fn create_context(&mut self, ctx: u32) -> Result<(), i32>;
    fn destroy_context(&mut self, ctx: u32);
    /// Imports a memfd as shmem resource `res_id` and attaches it to the context. The caller
    /// keeps `fd`; the implementation duplicates it if it needs to keep one.
    fn attach_shmem(&mut self, ctx: u32, res_id: u32, fd: RawFd, size: u64) -> Result<(), i32>;
    fn unref(&mut self, res_id: u32);
    fn submit(&mut self, ctx: u32, cs: &[u8]) -> Result<(), i32>;
    fn create_fence(&mut self, ctx: u32, ring_idx: u32, fence_id: u64) -> Result<(), i32>;
    /// Runs the renderer's completion processing; returns the fences that retired
    /// as `(ring_idx, fence_id)`.
    fn poll(&mut self, ctx: u32) -> Vec<(u32, u64)>;
    /// A descriptor that becomes readable when `poll` has work, or a negative number.
    fn poll_fd(&self, ctx: u32) -> RawFd;
    /// Creates a mappable host3d blob for the device memory with venus object id `blob_id`
    /// as resource `res_id` and attaches it to the context.
    fn create_blob(&mut self, ctx: u32, res_id: u32, blob_id: u64, size: u64) -> Result<(), i32>;
    /// Maps a blob. Returns the mapping and its size.
    fn map_blob(&mut self, res_id: u32) -> Result<(*mut u8, u64), i32>;
    fn unmap_blob(&mut self, res_id: u32);
}

struct Shm {
    /// The memfd behind a shmem; none for device memory the renderer mapped.
    fd: Option<OwnedFd>,
    /// Device memory: the renderer's GPU writes show up here, so it is scanned when a fence
    /// retires instead of on every tick.
    bo: bool,
    map: *mut u8,
    size: usize,
    /// What the guest has seen; only for scanned shmems.
    snap: Option<Vec<u64>>,
    ring: bool,
    /// The id `vkCreateRingMESA` names, for waking the ring.
    ring_id: u64,
}

impl Shm {
    fn create(size: usize) -> io::Result<Shm> {
        // SAFETY: plain syscalls; the name is NUL-terminated.
        unsafe {
            let fd = libc::memfd_create(c"winbridge-venus".as_ptr(), libc::MFD_CLOEXEC);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = OwnedFd::from_raw_fd(fd);
            if libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) != 0 {
                return Err(io::Error::last_os_error());
            }
            let map = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0);
            if map == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            let snap = (size <= SCAN_MAX).then(|| vec![0u64; size / 8]);
            Ok(Shm { fd: Some(fd), map: map.cast(), size, snap, ring: false, ring_id: 0, bo: false })
        }
    }

    /// Wraps device memory the renderer mapped for us. `size` is what the guest can see, a
    /// multiple of 8 no larger than the mapping. The guest's copy starts out as it is on the
    /// host, so memory the application has not written is not sent.
    fn from_blob(map: *mut u8, size: usize) -> Shm {
        let mut shm = Shm { fd: None, map, size, snap: None, ring: false, ring_id: 0, bo: true };
        let words = (0..size / 8).map(|i| shm.word(i * 8).load(Ordering::Relaxed)).collect();
        shm.snap = Some(words);
        shm
    }

    fn word(&self, off: usize) -> &AtomicU64 {
        debug_assert!(off % 8 == 0 && off + 8 <= self.size);
        // SAFETY: in bounds, 8-aligned (the mapping is page aligned); the renderer writes the
        // same memory from another process, so every access is atomic.
        unsafe { &*self.map.add(off).cast::<AtomicU64>() }
    }

    fn word32(&self, off: usize) -> &AtomicU32 {
        debug_assert!(off % 4 == 0 && off + 4 <= self.size);
        // SAFETY: as in `word`.
        unsafe { &*self.map.add(off).cast::<AtomicU32>() }
    }

    /// Appends the words the renderer changed since the last scan.
    fn scan(&mut self, res_id: u32, out: &mut PushBuilder) {
        let Some(mut snap) = self.snap.take() else { return };
        let words = snap.len();
        let mut i = 0;
        let mut cur = vec![0u64; 0];
        // Most of a scanned shmem is unchanged between two scans, so equal 4 KiB blocks are
        // skipped with one memcmp, which is much faster than comparing word by word.
        const BLOCK_WORDS: usize = 512;
        while i < words {
            if i % BLOCK_WORDS == 0 {
                let n = BLOCK_WORDS.min(words - i);
                // SAFETY: both ranges are in bounds (`snap` has `words` entries and the mapping is
                // `words * 8` bytes). The renderer may write the mapping while it is compared; a
                // block that differs is read again word by word below, so a torn read here only
                // costs a second look.
                let equal = unsafe { libc::memcmp(self.map.add(i * 8).cast(), snap.as_ptr().add(i).cast(), n * 8) } == 0;
                if equal {
                    i += n;
                    continue;
                }
            }
            if self.word(i * 8).load(Ordering::Relaxed) == snap[i] {
                i += 1;
                continue;
            }
            let (start, mut end, mut gap) = (i, i + 1, 0);
            cur.clear();
            cur.push(self.word(i * 8).load(Ordering::Relaxed));
            let mut j = i + 1;
            while j < words && gap < RUN_GAP_WORDS {
                let v = self.word(j * 8).load(Ordering::Relaxed);
                cur.push(v);
                if v != snap[j] {
                    end = j + 1;
                    gap = 0;
                } else {
                    gap += 1;
                }
                j += 1;
            }
            cur.truncate(end - start);
            snap[start..end].copy_from_slice(&cur);
            let bytes: Vec<u8> = cur.iter().flat_map(|w| w.to_le_bytes()).collect();
            out.add(res_id, (start * 8) as u32, &bytes);
            i = end;
        }
        self.snap = Some(snap);
    }

    /// Writes words pushed by the guest, and records them as seen so they are not echoed.
    fn apply(&mut self, offset: usize, data: &[u8]) {
        for (k, w) in data.chunks_exact(8).enumerate() {
            let v = u64::from_le_bytes(w.try_into().unwrap());
            let at = offset + k * 8;
            if let Some(s) = self.snap.as_mut() {
                s[at / 8] = v;
            }
            self.word(at).store(v, Ordering::Relaxed);
        }
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // A blob's mapping belongs to the renderer and is released through `unmap_blob`.
        if self.fd.is_some() {
            // SAFETY: the mapping was created in `create` and is unmapped once.
            unsafe { libc::munmap(self.map.cast(), self.size) };
        }
    }
}

struct Fence {
    id: u64,
    ring_idx: u32,
    syncs: Vec<(u32, u64)>,
    submitted: Instant,
}

pub struct Session<R: Renderer> {
    r: R,
    stream: UnixStream,
    rbuf: Vec<u8>,
    rpos: usize,
    hello: bool,
    shms: HashMap<u32, Shm>,
    fences: VecDeque<Fence>,
    next_fence: u64,
    busy_until: Instant,
    last_scan: Instant,
    last_state: Instant,
    record: Option<(std::fs::File, Instant)>,
    stats: Stats,
}

impl<R: Renderer> Session<R> {
    pub fn new(r: R, stream: UnixStream) -> Session<R> {
        let now = Instant::now();
        Session { r, stream, rbuf: Vec::new(), rpos: 0, hello: false, shms: HashMap::new(), fences: VecDeque::new(), next_fence: 1, busy_until: now, last_scan: now, last_state: now, record: None, stats: Stats::new() }
    }

    /// Appends everything the guest sends to `file` as `(microseconds, length, bytes)` records,
    /// for `replay` in `main.rs`.
    pub fn record_to(&mut self, file: std::fs::File) {
        self.record = Some((file, Instant::now()));
    }

    /// Reports the serve loop's counters once a second (see `stats.rs`).
    pub fn enable_stats(&mut self) {
        self.stats.enable();
    }

    /// Serves the guest until it closes the channel or the renderer fails.
    pub fn run(&mut self) -> io::Result<()> {
        let result = self.serve();
        self.shutdown();
        result
    }

    fn serve(&mut self) -> io::Result<()> {
        loop {
            let mut phases = [Duration::ZERO; 5];
            let mut mark = Instant::now();
            let mut lap = |slot: &mut Duration| {
                *slot = mark.elapsed();
                mark = Instant::now();
            };
            let ctx_fd = if self.hello { self.r.poll_fd(CTX) } else { -1 };
            let timeout = if Instant::now() < self.busy_until { BUSY_POLL } else { IDLE_POLL };
            self.wait_readable(ctx_fd, timeout)?;
            lap(&mut phases[0]);
            if !self.read_available()? {
                return Ok(());
            }
            while let Some((hdr, payload)) = self.next_frame() {
                self.busy_until = Instant::now() + BUSY_WINDOW;
                if !self.handle(hdr, &payload)? {
                    return Ok(());
                }
            }
            lap(&mut phases[1]);
            if self.hello {
                let retired = self.r.poll(CTX);
                lap(&mut phases[2]);
                if !retired.is_empty() {
                    self.retire(&retired)?;
                }
                lap(&mut phases[3]);
                if self.ring_changed() || self.last_scan.elapsed() >= SCAN_PERIOD {
                    self.scan_and_push(false)?;
                }
                lap(&mut phases[4]);
                if self.last_state.elapsed() >= Duration::from_secs(1) {
                    self.last_state = Instant::now();
                    self.log_rings();
                }
            }
            self.stats.loop_phases(&phases);
            self.stats.tick();
            if phases.iter().any(|p| *p >= Duration::from_millis(20)) {
                debug!("slow loop iteration: wait {:?}, handle {:?}, poll {:?}, retire {:?}, scan {:?}", phases[0], phases[1], phases[2], phases[3], phases[4]);
            }
        }
    }

    fn wait_readable(&self, ctx_fd: RawFd, timeout: Duration) -> io::Result<()> {
        let mut fds = [
            libc::pollfd { fd: self.stream.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: ctx_fd, events: libc::POLLIN, revents: 0 },
        ];
        let ts = libc::timespec { tv_sec: timeout.as_secs() as _, tv_nsec: timeout.subsec_nanos() as _ };
        // SAFETY: `fds` and `ts` are valid for the call; a negative fd is ignored by poll.
        let r = unsafe { libc::ppoll(fds.as_mut_ptr(), 2, &ts, std::ptr::null()) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        Ok(())
    }

    /// Reads whatever the socket holds. Returns false when the guest closed the channel.
    fn read_available(&mut self) -> io::Result<bool> {
        if self.rpos > 0 && self.rpos == self.rbuf.len() {
            self.rbuf.clear();
            self.rpos = 0;
        }
        let mut chunk = vec![0u8; 256 << 10];
        loop {
            // SAFETY: `chunk` is valid for its length.
            let n = unsafe { libc::recv(self.stream.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len(), libc::MSG_DONTWAIT) };
            if n == 0 {
                return Ok(false);
            }
            if n < 0 {
                let e = io::Error::last_os_error();
                return match e.kind() {
                    io::ErrorKind::WouldBlock => Ok(true),
                    io::ErrorKind::Interrupted => continue,
                    _ => Err(e),
                };
            }
            self.rbuf.extend_from_slice(&chunk[..n as usize]);
            if let Some((file, start)) = self.record.as_mut() {
                let mut rec = Vec::with_capacity(12 + n as usize);
                rec.extend_from_slice(&(start.elapsed().as_micros() as u64).to_le_bytes());
                rec.extend_from_slice(&(n as u32).to_le_bytes());
                rec.extend_from_slice(&chunk[..n as usize]);
                if let Err(e) = file.write_all(&rec) {
                    warn!("cannot record the guest stream: {e}");
                    self.record = None;
                }
            }
        }
    }

    fn next_frame(&mut self) -> Option<(Hdr, Vec<u8>)> {
        let avail = &self.rbuf[self.rpos..];
        if avail.len() < HDR_LEN {
            return None;
        }
        let hdr = Hdr::decode(avail);
        if hdr.len > proto::MAX_PAYLOAD {
            // A broken stream cannot be resynchronised; drop it and let the session end.
            self.rbuf.clear();
            self.rpos = 0;
            return Some((Hdr { kind: proto::LOST, ..hdr }, Vec::new()));
        }
        let total = HDR_LEN + hdr.len as usize;
        if avail.len() < total {
            return None;
        }
        let payload = avail[HDR_LEN..total].to_vec();
        self.rpos += total;
        Some((hdr, payload))
    }

    fn send(&mut self, kind: u32, req: u32, arg: u32, payload: &[u8]) -> io::Result<()> {
        let started = Instant::now();
        let hdr = Hdr { len: payload.len() as u32, kind, req, arg };
        let mut s = &self.stream;
        let result = s.write_all(&hdr.encode()).and_then(|()| s.write_all(payload));
        self.stats.sent(HDR_LEN + payload.len(), started.elapsed());
        result
    }

    /// Handles one message. Returns false when the session is over.
    fn handle(&mut self, hdr: Hdr, p: &[u8]) -> io::Result<bool> {
        let started = Instant::now();
        let result = self.dispatch(hdr, p);
        self.stats.message(hdr.kind, p.len(), started.elapsed());
        result
    }

    fn dispatch(&mut self, hdr: Hdr, p: &[u8]) -> io::Result<bool> {
        debug!("guest message {} (arg {}, {} bytes)", hdr.kind, hdr.arg, p.len());
        match hdr.kind {
            proto::HELLO => self.on_hello(hdr, p)?,
            proto::PING => self.send(hdr.kind | proto::REPLY, hdr.req, hdr.arg, p)?,
            _ if !self.hello => {
                warn!("message {} before HELLO", hdr.kind);
                return Ok(false);
            }
            proto::SHM_CREATE => self.on_shm_create(hdr, p)?,
            proto::SHM_DESTROY => {
                self.release(hdr.arg);
                self.send(hdr.kind | proto::REPLY, hdr.req, 0, &[])?;
            }
            proto::BO_CREATE => return self.on_bo_create(hdr, p),
            proto::SHM_ROLE => {
                if let Some(s) = self.shms.get_mut(&hdr.arg) {
                    s.ring = true;
                    s.ring_id = if p.len() >= 8 { proto::u64_at(p, 0) } else { 0 };
                }
            }
            proto::PUSH => self.on_push(p),
            proto::STATUS_CLEAR => self.on_status_clear(hdr.arg, p),
            proto::SUBMIT => return self.on_submit(p),
            other => warn!("unknown message {other}"),
        }
        Ok(true)
    }

    fn on_hello(&mut self, hdr: Hdr, p: &[u8]) -> io::Result<()> {
        let fail = |this: &mut Self, why: &str| {
            error!("{why}");
            this.send(hdr.kind | proto::REPLY, hdr.req, 1, &[])
        };
        if p.len() < 4 || proto::u32_at(p, 0) != proto::VERSION {
            return fail(self, "unsupported WBVN version");
        }
        let caps = match self.r.capset() {
            Ok(c) => c,
            Err(e) => return fail(self, &e),
        };
        if let Err(e) = self.r.create_context(CTX) {
            return fail(self, &format!("cannot create the venus context: {e}"));
        }
        self.hello = true;
        let mut reply = Vec::with_capacity(8 + caps.len());
        reply.extend_from_slice(&proto::VERSION.to_le_bytes());
        reply.extend_from_slice(&MAX_TIMELINES.to_le_bytes());
        reply.extend_from_slice(&caps);
        info!("guest connected, venus context created");
        self.send(hdr.kind | proto::REPLY, hdr.req, 0, &reply)
    }

    fn on_shm_create(&mut self, hdr: Hdr, p: &[u8]) -> io::Result<()> {
        let res_id = hdr.arg;
        let size = if p.len() >= 8 { proto::u64_at(p, 0) } else { 0 };
        let status = if res_id == 0 || size == 0 || size % 8 != 0 || size > (1 << 30) || self.shms.contains_key(&res_id) {
            libc::EINVAL
        } else {
            match Shm::create(size as usize) {
                Err(e) => e.raw_os_error().unwrap_or(libc::EIO),
                Ok(shm) => match self.r.attach_shmem(CTX, res_id, shm.fd.as_ref().map_or(-1, AsRawFd::as_raw_fd), size) {
                    Ok(()) => {
                        self.shms.insert(res_id, shm);
                        0
                    }
                    Err(e) => {
                        warn!("cannot attach shmem {res_id} ({size} bytes): {e}");
                        e.abs().max(1)
                    }
                },
            }
        };
        self.send(hdr.kind | proto::REPLY, hdr.req, status as u32, &[])
    }

    fn on_push(&mut self, p: &[u8]) {
        let ranges = proto::parse_push(p);
        let mut moved = Vec::new();
        // Last range first: the tail comes first in a push and must land after the commands it covers.
        for r in ranges.iter().rev() {
            match self.shms.get_mut(&r.res_id) {
                Some(s) if r.offset as usize + r.data.len() <= s.size => {
                    std::sync::atomic::fence(Ordering::Release);
                    s.apply(r.offset as usize, r.data);
                    let end = r.offset as usize + r.data.len();
                    if s.ring && r.offset <= proto::RING_TAIL && end > proto::RING_TAIL as usize {
                        moved.push(r.res_id);
                    }
                }
                _ => warn!("push for unknown shmem {} or out of range", r.res_id),
            }
        }
        for id in moved {
            self.wake_ring(id);
        }
    }

    /// The guest stores the tail and then reads the status; the renderer's ring sets IDLE and then
    /// reads the tail. Memory that is only mirrored breaks that handshake, because the ring can go
    /// idle after the guest looked and before the new tail arrived. So when a pushed tail finds the
    /// ring idle, wake it the way the guest would have.
    fn wake_ring(&mut self, res_id: u32) {
        let Some(s) = self.shms.get(&res_id) else { return };
        std::sync::atomic::fence(Ordering::SeqCst);
        if s.word32(proto::RING_STATUS as usize).load(Ordering::SeqCst) & proto::RING_STATUS_IDLE == 0 {
            return;
        }
        let tail = s.word32(proto::RING_TAIL as usize).load(Ordering::SeqCst);
        let cs = proto::notify_ring_cs(s.ring_id, tail);
        debug!("waking ring {res_id}: tail {tail}");
        self.stats.wake();
        if let Err(e) = self.r.submit(CTX, &cs) {
            warn!("cannot wake ring {res_id}: {e}");
        }
    }

    fn on_status_clear(&mut self, res_id: u32, p: &[u8]) {
        if p.len() < 8 {
            return;
        }
        let (off, mask) = (proto::u32_at(p, 0) as usize, proto::u32_at(p, 4));
        let Some(s) = self.shms.get_mut(&res_id) else { return };
        if off % 4 != 0 || off + 4 > s.size {
            return;
        }
        s.word32(off).fetch_and(!mask, Ordering::SeqCst);
        // The guest already holds the cleared word; do not echo it back.
        if let Some(snap) = s.snap.as_mut() {
            let shift = (off % 8) * 8;
            snap[off / 8] &= !((mask as u64) << shift);
        }
    }

    fn on_submit(&mut self, p: &[u8]) -> io::Result<bool> {
        let Some(sub) = proto::parse_submit(p) else {
            warn!("malformed SUBMIT");
            return Ok(false);
        };
        self.run_submit(sub)
    }

    fn run_submit(&mut self, sub: proto::Submit<'_>) -> io::Result<bool> {
        let started = Instant::now();
        let submitted = self.r.submit(CTX, sub.cs);
        let took = started.elapsed();
        self.stats.submit(sub.cs.len(), took);
        if took >= Duration::from_millis(5) {
            debug!("submit_cmd of {} byte(s) took {:?}", sub.cs.len(), took);
        }
        if let Err(e) = submitted {
            error!("submit_cmd failed: {e}");
            self.send(proto::LOST, 0, 0, &[])?;
            return Ok(false);
        }
        if !sub.syncs.is_empty() {
            let id = self.next_fence;
            self.next_fence += 1;
            if let Err(e) = self.r.create_fence(CTX, sub.ring_idx, id) {
                error!("create_fence failed: {e}");
                self.send(proto::LOST, 0, 0, &[])?;
                return Ok(false);
            }
            self.fences.push_back(Fence { id, ring_idx: sub.ring_idx, syncs: sub.syncs, submitted: Instant::now() });
        }
        Ok(true)
    }

    fn on_bo_create(&mut self, hdr: Hdr, p: &[u8]) -> io::Result<bool> {
        const HEAD: usize = 24;
        if p.len() < HEAD {
            warn!("malformed BO_CREATE");
            return Ok(false);
        }
        let (size, blob_id, batch_len) = (proto::u64_at(p, 0), proto::u64_at(p, 8), proto::u32_at(p, 20) as usize);
        let res_id = hdr.arg;
        if HEAD + batch_len != p.len() || res_id == 0 || size == 0 || self.shms.contains_key(&res_id) {
            warn!("invalid BO_CREATE for resource {res_id}");
            self.send(hdr.kind | proto::REPLY, hdr.req, libc::EINVAL as u32, &[])?;
            return Ok(true);
        }
        if batch_len > 0 {
            // It makes the renderer's ring reach the allocation before the blob is created.
            let Some(sub) = proto::parse_submit(&p[HEAD..]) else {
                warn!("malformed batch in BO_CREATE");
                return Ok(false);
            };
            if !self.run_submit(sub)? {
                return Ok(false);
            }
        }
        let status = self.create_bo(res_id, blob_id, size);
        self.send(hdr.kind | proto::REPLY, hdr.req, status, &[])?;
        Ok(true)
    }

    /// Creates the blob for a VkDeviceMemory and maps it. Returns the reply status.
    fn create_bo(&mut self, res_id: u32, blob_id: u64, size: u64) -> u32 {
        if let Err(e) = self.r.create_blob(CTX, res_id, blob_id, size) {
            warn!("cannot create the blob for memory {blob_id} ({size} bytes): {e}");
            return e.unsigned_abs().max(1);
        }
        match self.r.map_blob(res_id) {
            Err(e) => {
                warn!("cannot map the blob for memory {blob_id}: {e}");
                self.r.unref(res_id);
                e.unsigned_abs().max(1)
            }
            Ok((map, host_size)) => {
                // The guest's copy is rounded up to whole pages.
                let guest_size = (size + 4095) & !4095;
                self.shms.insert(res_id, Shm::from_blob(map, (guest_size.min(host_size) & !7) as usize));
                0
            }
        }
    }

    /// Completes the fences the renderer retired. Mirrored memory goes out first, so a waiter
    /// woken by the signal finds the replies it waits for.
    fn retire(&mut self, retired: &[(u32, u64)]) -> io::Result<()> {
        let mut points = Vec::new();
        for &(ring_idx, id) in retired {
            // Fences on a ring retire in order, and a merged fence retires its predecessors.
            self.fences.retain(|f| {
                let done = f.ring_idx == ring_idx && f.id <= id;
                if done {
                    self.stats.fence(f.submitted.elapsed());
                    debug!("fence {} on ring {} retired {:?} after its submit", f.id, f.ring_idx, f.submitted.elapsed());
                    points.extend_from_slice(&f.syncs);
                }
                !done
            });
        }
        if points.is_empty() {
            return Ok(());
        }
        self.scan_and_push(true)?;
        self.send(proto::SYNC_SIGNAL, 0, 0, &proto::sync_signal_payload(&points))
    }

    /// Logs the head, tail and status of each ring, as the host memory has them, and the type of
    /// the command the ring is at.
    fn log_rings(&self) {
        for (id, s) in self.shms.iter().filter(|(_, s)| s.ring) {
            let head = s.word32(proto::RING_HEAD as usize).load(Ordering::Relaxed);
            // The buffer follows the control words (192 bytes) and is followed by 4 bytes of extra.
            let buf = s.size.saturating_sub(196).max(4);
            let at = (192 + (head as usize % buf)) & !3;
            let cmd = if at + 4 <= s.size { s.word32(at).load(Ordering::Relaxed) } else { 0 };
            debug!(
                "ring {id}: head {head} tail {} status {:#x} command type {cmd}, {} fence(s) pending",
                s.word32(proto::RING_TAIL as usize).load(Ordering::Relaxed),
                s.word32(proto::RING_STATUS as usize).load(Ordering::Relaxed),
                self.fences.len()
            );
        }
    }

    /// True when a ring's head or status word changed since the guest last saw it.
    fn ring_changed(&self) -> bool {
        self.shms.values().filter(|s| s.ring).any(|s| {
            let Some(snap) = s.snap.as_ref() else { return false };
            let head = proto::RING_HEAD as usize / 8;
            let status = proto::RING_STATUS as usize / 8;
            s.word(head * 8).load(Ordering::Relaxed) != snap[head] || s.word(status * 8).load(Ordering::Relaxed) != snap[status]
        })
    }

    /// Sends every word the renderer changed: the other shmems first, the ring last. Device
    /// memory is only looked at when `include_bos` is set, which is when a fence retired.
    fn scan_and_push(&mut self, include_bos: bool) -> io::Result<()> {
        self.last_scan = Instant::now();
        let mut ring = PushBuilder::default();
        let mut others = PushBuilder::default();
        // The ring is read first: its head must never be newer than the data behind it.
        let mut ids: Vec<u32> = self.shms.keys().copied().collect();
        ids.sort_unstable();
        for &id in &ids {
            let s = self.shms.get_mut(&id).unwrap();
            if s.ring {
                s.scan(id, &mut ring);
            }
        }
        for &id in &ids {
            let s = self.shms.get_mut(&id).unwrap();
            if !s.ring && (include_bos || !s.bo) {
                s.scan(id, &mut others);
            }
        }
        if !ring.is_empty() || !others.is_empty() {
            debug!("push: {} other range(s), then {} ring range(s)", others.len(), ring.len());
        }
        let mut out = (0u64, 0u64);
        for push in [others, ring] {
            if !push.is_empty() {
                let body = push.finish();
                out = (out.0 + 1, out.1 + body.len() as u64);
                self.send(proto::PUSH, 0, 0, &body)?;
            }
        }
        if out.0 > 0 {
            self.stats.scan(out.0, out.1);
        }
        Ok(())
    }

    /// Releases a resource: the mapping first if the renderer made it, then the resource.
    fn release(&mut self, id: u32) {
        if let Some(s) = self.shms.remove(&id) {
            if s.bo {
                self.r.unmap_blob(id);
            }
            self.r.unref(id);
            drop(s);
        }
    }

    fn shutdown(&mut self) {
        debug!("ending the session");
        if self.hello {
            let ids: Vec<u32> = self.shms.keys().copied().collect();
            // Resources are released before the context goes, as the guest did.
            for id in ids {
                self.release(id);
            }
            self.r.destroy_context(CTX);
            self.hello = false;
        }
    }
}

impl<T: Renderer + ?Sized> Renderer for &mut T {
    fn capset(&mut self) -> Result<Vec<u8>, String> {
        (**self).capset()
    }
    fn create_context(&mut self, ctx: u32) -> Result<(), i32> {
        (**self).create_context(ctx)
    }
    fn destroy_context(&mut self, ctx: u32) {
        (**self).destroy_context(ctx)
    }
    fn attach_shmem(&mut self, ctx: u32, res_id: u32, fd: RawFd, size: u64) -> Result<(), i32> {
        (**self).attach_shmem(ctx, res_id, fd, size)
    }
    fn unref(&mut self, res_id: u32) {
        (**self).unref(res_id)
    }
    fn submit(&mut self, ctx: u32, cs: &[u8]) -> Result<(), i32> {
        (**self).submit(ctx, cs)
    }
    fn create_fence(&mut self, ctx: u32, ring_idx: u32, fence_id: u64) -> Result<(), i32> {
        (**self).create_fence(ctx, ring_idx, fence_id)
    }
    fn poll(&mut self, ctx: u32) -> Vec<(u32, u64)> {
        (**self).poll(ctx)
    }
    fn poll_fd(&self, ctx: u32) -> RawFd {
        (**self).poll_fd(ctx)
    }
    fn create_blob(&mut self, ctx: u32, res_id: u32, blob_id: u64, size: u64) -> Result<(), i32> {
        (**self).create_blob(ctx, res_id, blob_id, size)
    }
    fn map_blob(&mut self, res_id: u32) -> Result<(*mut u8, u64), i32> {
        (**self).map_blob(res_id)
    }
    fn unmap_blob(&mut self, res_id: u32) {
        (**self).unmap_blob(res_id)
    }
}
