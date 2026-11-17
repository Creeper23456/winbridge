// SPDX-License-Identifier: GPL-3.0-or-later

//! One guest session on the WBVN channel (see `venus/guest/wbvn_proto.h`).
//!
//! The guest cannot map host memory, so the venus shmems it creates are mirrored: this side owns
//! a memfd per shmem, keeps a snapshot of what the guest has seen, and sends the 8-byte words
//! that the renderer changed. Words the guest changed arrive in `PUSH` messages and are applied
//! last range first. The ring is scanned before the other shmems and sent after them, so a ring
//! head never becomes visible ahead of the reply it covers.
//!
//! A session serves one guest process, which may run several Vulkan instances. Each instance is a
//! venus context (named in the high half of a message's `kind`, see `proto.rs`) with its own
//! renderer state; shmem, device memory and sync ids are unique across the session, so a push or a
//! signal needs no context.

use crate::channel::Channel;
use crate::proto::{self, Hdr, PushBuilder, HDR_LEN};
use crate::stats::Stats;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::os::fd::RawFd;

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Shmems up to this size are scanned for renderer writes (rings, reply pools). Mesa's reply pool
/// is 1 MiB and grows to the next power of two for a larger reply, so replies up to 4 MiB reach the
/// guest; its command-stream pool is 8 MiB or more and only ever written by the guest.
pub const SCAN_MAX: usize = 4 << 20;
/// Runs closer than this many words are sent as one.
const RUN_GAP_WORDS: usize = 8;
/// Timelines the context offers (`PROXY_CONTEXT_TIMELINE_COUNT`).
const MAX_TIMELINES: u32 = 64;
/// While anything happened this recently the loop polls at `BUSY_POLL`.
const BUSY_WINDOW: Duration = Duration::from_millis(5);
const BUSY_POLL: Duration = Duration::from_micros(100);
const IDLE_POLL: Duration = Duration::from_millis(2);
/// A scan runs at least this often even if nothing announced a change.
const SCAN_PERIOD: Duration = Duration::from_millis(5);
/// Between two full scans, a shmem the renderer writes (the guest's reply pool is filled from
/// its start upwards) is compared only up to this many words past the last word that ever
/// changed. A reply written further out waits for the next full scan.
const HOT_SLACK_WORDS: usize = 64 * 1024 / 8;
/// How often every shmem is compared in full.
const FULL_SCAN_PERIOD: Duration = Duration::from_millis(5);

/// The Venus renderer: virglrenderer in production, a fake in the tests.
pub trait Renderer {
    /// The venus capset (`struct virgl_renderer_capset_venus`) as raw bytes.
    fn capset(&mut self) -> Result<Vec<u8>, String>;
    fn create_context(&mut self, ctx: u32) -> Result<(), i32>;
    fn destroy_context(&mut self, ctx: u32);
    fn unref(&mut self, res_id: u32);
    fn submit(&mut self, ctx: u32, cs: &[u8]) -> Result<(), i32>;
    fn create_fence(&mut self, ctx: u32, ring_idx: u32, fence_id: u64) -> Result<(), i32>;
    /// Runs the renderer's completion processing; returns the fences that retired
    /// as `(ring_idx, fence_id)`.
    fn poll(&mut self, ctx: u32) -> Vec<(u32, u64)>;
    /// A descriptor that becomes readable when `poll` has work, or a negative number.
    fn poll_fd(&self, ctx: u32) -> RawFd;
    /// Creates a mappable host3d blob as resource `res_id` and attaches it to the context: the
    /// device memory with venus object id `blob_id`, or, for blob id 0, a shmem of `size` bytes
    /// that the renderer makes in its own process, so the context knows it when this returns.
    fn create_blob(&mut self, ctx: u32, res_id: u32, blob_id: u64, size: u64) -> Result<(), i32>;
    /// Maps a blob. Returns the mapping and its size.
    fn map_blob(&mut self, res_id: u32) -> Result<(*mut u8, u64), i32>;
    fn unmap_blob(&mut self, res_id: u32);
}

struct Shm {
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
    /// One past the last word that ever changed on the host side.
    hot: usize,
    /// The context that owns the resource: its commands run there.
    ctx: u32,
}

impl Shm {
    /// Wraps memory the renderer created and mapped for us: device memory (`bo`) or a shmem.
    /// `size` is what the guest can see, a multiple of 8 no larger than the mapping. The guest's
    /// copy starts out as it is on the host, so memory the application has not written is not
    /// sent. A shmem above `SCAN_MAX` is only written by the guest and keeps no snapshot. The
    /// mapping belongs to the renderer and is released through `unmap_blob`.
    fn from_blob(map: *mut u8, size: usize, ctx: u32, bo: bool) -> Shm {
        let mut shm = Shm { map, size, snap: None, ring: false, ring_id: 0, hot: 0, bo, ctx };
        if bo || size <= SCAN_MAX {
            let words = (0..size / 8).map(|i| shm.word(i * 8).load(Ordering::Relaxed)).collect();
            shm.snap = Some(words);
        }
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

    /// Appends the words the renderer changed since the last scan. Unless `full`, only the part
    /// up to `HOT_SLACK_WORDS` past the last change ever seen is compared. Returns how many bytes
    /// it compared.
    fn scan(&mut self, res_id: u32, out: &mut PushBuilder, full: bool) -> usize {
        let Some(mut snap) = self.snap.take() else { return 0 };
        let total = snap.len();
        let words = if full { total } else { total.min(self.hot + HOT_SLACK_WORDS) };
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
            self.hot = self.hot.max(end);
            let bytes: Vec<u8> = cur.iter().flat_map(|w| w.to_le_bytes()).collect();
            out.add(res_id, (start * 8) as u32, &bytes);
            i = end;
        }
        self.snap = Some(snap);
        words * 8
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

struct Fence {
    id: u64,
    ctx: u32,
    ring_idx: u32,
    syncs: Vec<(u32, u64)>,
    submitted: Instant,
}

pub struct Session<R: Renderer, C: Channel> {
    r: R,
    chan: C,
    rbuf: Vec<u8>,
    rpos: usize,
    /// Scratch for a message that goes out as one write.
    wbuf: Vec<u8>,
    /// The venus contexts the guest has created and not yet destroyed.
    ctxs: Vec<u32>,
    /// Reused by the serve loop: the poll descriptors of the contexts.
    fds: Vec<RawFd>,
    shms: HashMap<u32, Shm>,
    fences: VecDeque<Fence>,
    next_fence: u64,
    busy_until: Instant,
    last_scan: Instant,
    /// The last scan that compared every shmem in full.
    last_full: Instant,
    last_state: Instant,
    record: Option<(std::fs::File, Instant)>,
    stats: Stats,
}

/// Messages up to this payload size are sent header and payload in one write.
const COALESCE_MAX: usize = 64 << 10;

impl<R: Renderer, C: Channel> Session<R, C> {
    pub fn new(r: R, chan: C) -> Session<R, C> {
        let now = Instant::now();
        Session { r, chan, rbuf: Vec::new(), rpos: 0, wbuf: Vec::new(), ctxs: Vec::new(), fds: Vec::new(), shms: HashMap::new(), fences: VecDeque::new(), next_fence: 1, busy_until: now, last_scan: now, last_full: now, last_state: now, record: None, stats: Stats::new() }
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
            let mut fds = std::mem::take(&mut self.fds);
            fds.clear();
            fds.extend(self.ctxs.iter().map(|&c| self.r.poll_fd(c)));
            let timeout = if Instant::now() < self.busy_until { BUSY_POLL } else { IDLE_POLL };
            let waited = self.chan.wait(&fds, timeout);
            self.fds = fds;
            waited?;
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
            if !self.ctxs.is_empty() {
                let mut done: Vec<(u32, Vec<(u32, u64)>)> = Vec::new();
                for i in 0..self.ctxs.len() {
                    let ctx = self.ctxs[i];
                    let retired = self.r.poll(ctx);
                    if !retired.is_empty() {
                        done.push((ctx, retired));
                    }
                }
                lap(&mut phases[2]);
                for (ctx, retired) in done {
                    self.retire(ctx, &retired)?;
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
            if self.stats.due() {
                let mut now = [(0u64, 0u64); 3];
                for s in self.shms.values().filter(|s| s.snap.is_some()) {
                    let kind = if s.ring { 0 } else if s.bo { 2 } else { 1 };
                    now[kind].0 += 1;
                    now[kind].1 += s.size as u64;
                }
                self.stats.mirrored(now);
            }
            self.stats.tick();
            if phases.iter().any(|p| *p >= Duration::from_millis(20)) {
                debug!("slow loop iteration: wait {:?}, handle {:?}, poll {:?}, retire {:?}, scan {:?}", phases[0], phases[1], phases[2], phases[3], phases[4]);
            }
        }
    }

    /// Reads whatever the channel holds. Returns false when the guest closed the channel.
    fn read_available(&mut self) -> io::Result<bool> {
        if self.rpos > 0 && self.rpos == self.rbuf.len() {
            self.rbuf.clear();
            self.rpos = 0;
        }
        let before = self.rbuf.len();
        let open = self.chan.read_available(&mut self.rbuf)?;
        if self.rbuf.len() > before {
            if let Some((file, start)) = self.record.as_mut() {
                let got = &self.rbuf[before..];
                let mut rec = Vec::with_capacity(12 + got.len());
                rec.extend_from_slice(&(start.elapsed().as_micros() as u64).to_le_bytes());
                rec.extend_from_slice(&(got.len() as u32).to_le_bytes());
                rec.extend_from_slice(got);
                if let Err(e) = file.write_all(&rec) {
                    warn!("cannot record the guest stream: {e}");
                    self.record = None;
                }
            }
        }
        Ok(open)
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
        let result = if payload.len() <= COALESCE_MAX {
            self.wbuf.clear();
            self.wbuf.extend_from_slice(&hdr.encode());
            self.wbuf.extend_from_slice(payload);
            self.chan.write_all(&self.wbuf)
        } else {
            self.chan.write_all(&hdr.encode()).and_then(|()| self.chan.write_all(payload))
        };
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
        let (ctx, hdr) = (hdr.ctx(), Hdr { kind: hdr.id(), ..hdr });
        debug!("guest message {} (context {ctx}, arg {}, {} bytes)", hdr.kind, hdr.arg, p.len());
        match hdr.kind {
            proto::HELLO => self.on_hello(ctx, hdr, p)?,
            proto::PING => self.send(hdr.kind | proto::REPLY, hdr.req, hdr.arg, p)?,
            proto::CTX_DESTROY => self.on_ctx_destroy(ctx, hdr)?,
            proto::SHM_CREATE => self.on_shm_create(ctx, hdr, p)?,
            proto::SHM_DESTROY => {
                self.release(hdr.arg);
                self.send(hdr.kind | proto::REPLY, hdr.req, 0, &[])?;
            }
            proto::BO_CREATE => return self.on_bo_create(ctx, hdr, p),
            proto::SHM_ROLE => {
                if let Some(s) = self.shms.get_mut(&hdr.arg) {
                    s.ring = true;
                    s.ring_id = if p.len() >= 8 { proto::u64_at(p, 0) } else { 0 };
                }
            }
            proto::PUSH => self.on_push(p),
            proto::STATUS_CLEAR => self.on_status_clear(hdr.arg, p),
            proto::SUBMIT => return self.on_submit(ctx, p),
            // A frame too long to be real: the stream cannot be resynchronised.
            proto::LOST => return Ok(false),
            other => warn!("unknown message {other}"),
        }
        Ok(true)
    }

    fn on_hello(&mut self, ctx: u32, hdr: Hdr, p: &[u8]) -> io::Result<()> {
        let fail = |this: &mut Self, why: &str| {
            error!("{why}");
            this.send(hdr.kind | proto::REPLY, hdr.req, 1, &[])
        };
        if p.len() < 4 || proto::u32_at(p, 0) != proto::VERSION {
            return fail(self, "unsupported WBVN version");
        }
        if ctx == 0 || self.ctxs.contains(&ctx) {
            return fail(self, &format!("context id {ctx} is not usable"));
        }
        let caps = match self.r.capset() {
            Ok(c) => c,
            Err(e) => return fail(self, &e),
        };
        if let Err(e) = self.r.create_context(ctx) {
            return fail(self, &format!("cannot create the venus context: {e}"));
        }
        self.ctxs.push(ctx);
        let mut reply = Vec::with_capacity(8 + caps.len());
        reply.extend_from_slice(&proto::VERSION.to_le_bytes());
        reply.extend_from_slice(&MAX_TIMELINES.to_le_bytes());
        reply.extend_from_slice(&caps);
        info!("guest connected, venus context {ctx} created");
        self.send(hdr.kind | proto::REPLY, hdr.req, 0, &reply)
    }

    /// The guest destroyed a Vulkan instance: its resources and its context go, the others stay.
    fn on_ctx_destroy(&mut self, ctx: u32, hdr: Hdr) -> io::Result<()> {
        let status = if self.ctxs.contains(&ctx) {
            self.drop_ctx(ctx);
            info!("venus context {ctx} destroyed");
            0
        } else {
            libc::EINVAL as u32
        };
        self.send(hdr.kind | proto::REPLY, hdr.req, status, &[])
    }

    fn on_shm_create(&mut self, ctx: u32, hdr: Hdr, p: &[u8]) -> io::Result<()> {
        let res_id = hdr.arg;
        let size = if p.len() >= 8 { proto::u64_at(p, 0) } else { 0 };
        let status = if !self.ctxs.contains(&ctx) || res_id == 0 || size == 0 || size % 8 != 0 || size > (1 << 30) || self.shms.contains_key(&res_id) {
            libc::EINVAL as u32
        } else {
            self.create_resource(ctx, res_id, 0, size, false)
        };
        self.send(hdr.kind | proto::REPLY, hdr.req, status, &[])
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
        if let Err(e) = self.r.submit(s.ctx, &cs) {
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

    fn on_submit(&mut self, ctx: u32, p: &[u8]) -> io::Result<bool> {
        let Some(sub) = proto::parse_submit(p) else {
            warn!("malformed SUBMIT");
            return Ok(false);
        };
        if !self.ctxs.contains(&ctx) {
            // A context the renderer lost or the guest destroyed; its last messages were in flight.
            debug!("SUBMIT for context {ctx}, which is gone");
            return Ok(true);
        }
        self.run_submit(ctx, sub)?;
        Ok(true)
    }

    /// Runs a command stream on `ctx` and creates the fence its syncs ask for. Returns false when
    /// the renderer failed: the guest has been told and the context is gone, the session goes on.
    fn run_submit(&mut self, ctx: u32, sub: proto::Submit<'_>) -> io::Result<bool> {
        let started = Instant::now();
        let submitted = self.r.submit(ctx, sub.cs);
        let took = started.elapsed();
        self.stats.submit(sub.cs.len(), took);
        if took >= Duration::from_millis(5) {
            debug!("submit_cmd of {} byte(s) took {:?}", sub.cs.len(), took);
        }
        if let Err(e) = submitted {
            error!("submit_cmd failed on context {ctx}: {e}");
            self.fail_ctx(ctx)?;
            return Ok(false);
        }
        if !sub.syncs.is_empty() {
            let id = self.next_fence;
            self.next_fence += 1;
            if let Err(e) = self.r.create_fence(ctx, sub.ring_idx, id) {
                error!("create_fence failed on context {ctx}: {e}");
                self.fail_ctx(ctx)?;
                return Ok(false);
            }
            self.fences.push_back(Fence { id, ctx, ring_idx: sub.ring_idx, syncs: sub.syncs, submitted: Instant::now() });
        }
        Ok(true)
    }

    /// The renderer cannot serve `ctx` any more: tell the guest (its instance is lost) and release it.
    fn fail_ctx(&mut self, ctx: u32) -> io::Result<()> {
        self.send(proto::LOST, 0, ctx, &[])?;
        self.drop_ctx(ctx);
        Ok(())
    }

    fn on_bo_create(&mut self, ctx: u32, hdr: Hdr, p: &[u8]) -> io::Result<bool> {
        const HEAD: usize = 24;
        if p.len() < HEAD {
            warn!("malformed BO_CREATE");
            return Ok(false);
        }
        let (size, blob_id, batch_len) = (proto::u64_at(p, 0), proto::u64_at(p, 8), proto::u32_at(p, 20) as usize);
        let res_id = hdr.arg;
        if HEAD + batch_len != p.len() || res_id == 0 || size == 0 || !self.ctxs.contains(&ctx) || self.shms.contains_key(&res_id) {
            warn!("invalid BO_CREATE for resource {res_id} on context {ctx}");
            self.send(hdr.kind | proto::REPLY, hdr.req, libc::EINVAL as u32, &[])?;
            return Ok(true);
        }
        if batch_len > 0 {
            // It makes the renderer's ring reach the allocation before the blob is created.
            let Some(sub) = proto::parse_submit(&p[HEAD..]) else {
                warn!("malformed batch in BO_CREATE");
                return Ok(false);
            };
            if !self.run_submit(ctx, sub)? {
                self.send(hdr.kind | proto::REPLY, hdr.req, libc::EIO as u32, &[])?;
                return Ok(true);
            }
        }
        let status = self.create_resource(ctx, res_id, blob_id, size, true);
        self.send(hdr.kind | proto::REPLY, hdr.req, status, &[])?;
        Ok(true)
    }

    /// Has the renderer create resource `res_id` for `ctx`, and maps it. `blob_id` names the
    /// VkDeviceMemory of a bo; 0 asks for a shmem. The renderer makes a shmem in its own process:
    /// an imported memfd reaches that process by a message of its own, and the ring thread there can
    /// run a command that names the shmem before the message is handled (a stream of the
    /// application's first commands failed with "invalid stream" that way). Returns the reply status.
    fn create_resource(&mut self, ctx: u32, res_id: u32, blob_id: u64, size: u64, bo: bool) -> u32 {
        if let Err(e) = self.r.create_blob(ctx, res_id, blob_id, size) {
            warn!("cannot create resource {res_id} (blob {blob_id}, {size} bytes): {e}");
            return e.unsigned_abs().max(1);
        }
        match self.r.map_blob(res_id) {
            Err(e) => {
                warn!("cannot map resource {res_id} (blob {blob_id}): {e}");
                self.r.unref(res_id);
                e.unsigned_abs().max(1)
            }
            Ok((map, host_size)) => {
                // A bo's copy in the guest is rounded up to whole pages.
                let guest_size = if bo { (size + 4095) & !4095 } else { size };
                self.shms.insert(res_id, Shm::from_blob(map, (guest_size.min(host_size) & !7) as usize, ctx, bo));
                0
            }
        }
    }

    /// Completes the fences the renderer retired. Mirrored memory goes out first, so a waiter
    /// woken by the signal finds the replies it waits for.
    fn retire(&mut self, ctx: u32, retired: &[(u32, u64)]) -> io::Result<()> {
        let mut points = Vec::new();
        for &(ring_idx, id) in retired {
            // Fences on a ring retire in order, and a merged fence retires its predecessors.
            self.fences.retain(|f| {
                let done = f.ctx == ctx && f.ring_idx == ring_idx && f.id <= id;
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
        let now = Instant::now();
        self.last_scan = now;
        let full = now.duration_since(self.last_full) >= FULL_SCAN_PERIOD;
        if full {
            self.last_full = now;
        }
        let mut ring = PushBuilder::default();
        let mut others = PushBuilder::default();
        // Bytes compared: the rings, the other shmems, device memory.
        let mut compared = [0u64; 3];
        // The ring is read first: its head must never be newer than the data behind it.
        let mut ids: Vec<u32> = self.shms.keys().copied().collect();
        ids.sort_unstable();
        for &id in &ids {
            let s = self.shms.get_mut(&id).unwrap();
            if s.ring {
                compared[0] += s.scan(id, &mut ring, true) as u64;
            }
        }
        for &id in &ids {
            let s = self.shms.get_mut(&id).unwrap();
            if !s.ring && (include_bos || !s.bo) {
                compared[if s.bo { 2 } else { 1 }] += s.scan(id, &mut others, full || s.bo) as u64;
            }
        }
        self.stats.compared(compared);
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

    /// Releases a resource: the mapping first, then the resource.
    fn release(&mut self, id: u32) {
        if self.shms.remove(&id).is_some() {
            self.r.unmap_blob(id);
            self.r.unref(id);
        }
    }

    /// Releases a context: its resources first, as the guest did, then the context.
    fn drop_ctx(&mut self, ctx: u32) {
        let ids: Vec<u32> = self.shms.iter().filter(|(_, s)| s.ctx == ctx).map(|(&id, _)| id).collect();
        for id in ids {
            self.release(id);
        }
        self.fences.retain(|f| f.ctx != ctx);
        self.r.destroy_context(ctx);
        self.ctxs.retain(|&c| c != ctx);
    }

    fn shutdown(&mut self) {
        debug!("ending the session");
        for ctx in self.ctxs.clone() {
            self.drop_ctx(ctx);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn shm_of(size: usize) -> Shm {
        // SAFETY: an anonymous mapping of our own; the test keeps it.
        let map = unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED | libc::MAP_ANONYMOUS, -1, 0) };
        assert_ne!(map, libc::MAP_FAILED);
        Shm::from_blob(map.cast(), size, 1, false)
    }

    fn write(shm: &Shm, off: usize, v: u64) {
        shm.word(off).store(v, Ordering::Relaxed);
    }

    fn scanned(shm: &mut Shm, full: bool) -> (usize, u32) {
        let mut out = PushBuilder::default();
        let bytes = shm.scan(1, &mut out, full);
        (bytes, out.len())
    }

    #[test]
    fn a_light_scan_stays_near_the_last_change_and_a_full_scan_finds_the_rest() {
        let mut shm = shm_of(1 << 20);
        // Nothing has changed yet: a light scan looks only at the first slack.
        assert_eq!(scanned(&mut shm, false), (HOT_SLACK_WORDS * 8, 0));

        // A reply near the start is found at once and moves the frontier.
        write(&shm, 4096, 7);
        assert_eq!(scanned(&mut shm, false).1, 1);
        assert_eq!(shm.hot, 4096 / 8 + 1);

        // A word far beyond the frontier is not found by a light scan, but a full scan has it.
        write(&shm, 512 * 1024, 9);
        assert_eq!(scanned(&mut shm, false).1, 0);
        assert_eq!(scanned(&mut shm, true), (1 << 20, 1));
        assert_eq!(shm.hot, 512 * 1024 / 8 + 1);

        // Now that the frontier moved there, a light scan sees what follows it.
        write(&shm, 512 * 1024 + 8192, 11);
        assert_eq!(scanned(&mut shm, false).1, 1);
    }
}
