// SPDX-License-Identifier: GPL-3.0-or-later

//! Single-producer single-consumer byte ring with variable-length records.
//!
//! Records never wrap: when a record does not fit in the space left before the
//! end of the data area, the producer publishes a *skip marker* covering that
//! space and starts the record at offset 0. Indices are free-running `u64` byte
//! counts (position = index & (size - 1)); every record and every skip is a
//! multiple of 8 bytes, so both indices are always multiples of 8.

use core::ptr;
use core::sync::atomic::{compiler_fence, fence, AtomicU64, Ordering};
use std::fmt;
use std::sync::Arc;

use crate::mapping::Mapping;
use crate::region::{Inner, RingPtr};

/// Size of the record header: `u32` payload length, `u16` tag, `u16` flags.
pub const REC_HEADER_LEN: usize = 8;
/// Record flag reserved for the transport: skip marker. Callers must not set it.
pub const FLAG_SKIP: u16 = 0x8000;
/// Default cap on a single record's payload (the lane size may lower it).
pub const DEFAULT_MAX_RECORD: usize = 4 << 20;

const PROD_CLOSED: u32 = 1;
const CONS_SLEEPING: u32 = 1;
const CONS_CLOSED: u32 = 2;
/// Indices above this are rejected as corrupt: honest peers never get near it
/// (2^62 bytes), and it keeps every `index + record` sum free of overflow.
const MAX_INDEX: u64 = 1 << 62;

/// Call between writing record bytes and storing the new head index.
///
/// On x86-64 this executes `sfence`. Ordinary (write-back) memory under x86
/// TSO does not need it, but the guest mapping of an ivshmem BAR is probably
/// write-combining, where stores may become visible out of order unless
/// fenced **[UNVERIFIED: depends on the driver's mapping type]**. It costs a few
/// cycles, so the producer always calls it. On other architectures it is a
/// release fence.
#[inline(always)]
pub fn publish_fence() {
    compiler_fence(Ordering::SeqCst);
    #[cfg(target_arch = "x86_64")]
    #[allow(unused_unsafe)]
    // SAFETY: SSE is part of the x86-64 baseline; `sfence` has no memory-safety
    // preconditions.
    unsafe {
        core::arch::x86_64::_mm_sfence();
    }
    #[cfg(not(target_arch = "x86_64"))]
    fence(Ordering::Release);
}

/// What the consumer or producer found wrong in the shared ring state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corruption {
    /// The peer's index moved backwards.
    IndexBackwards,
    /// The distance between the indices exceeds the data size.
    IndexOutOfRange,
    /// An index is not a multiple of 8.
    IndexMisaligned,
    /// A record claims more payload than the lane's cap.
    RecordTooLong,
    /// A record claims more bytes than have been published.
    RecordExceedsAvailable,
    /// A record would run past the end of the data area.
    RecordWraps,
    /// A skip marker at an impossible position or longer than published.
    BadSkip,
}

/// Terminal lane errors. After the first one the endpoint stays closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneError {
    /// The peer violated the ring protocol; the lane is now closed.
    Corrupt(Corruption),
    /// The lane is closed (earlier corruption, or closed by the peer/locally).
    Closed,
}

impl fmt::Display for LaneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for LaneError {}

/// Errors from `try_push`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushError {
    /// Not enough free space right now; retry later.
    Full,
    /// The payload exceeds the lane's record cap and can never be sent.
    TooLarge,
    /// A reserved flag bit was set.
    BadFlags,
    /// The lane is closed or corrupt.
    Lane(LaneError),
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for PushError {}

/// Errors from `try_pop_into`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopError {
    /// The destination is too small; the record is left in the ring.
    BufferTooSmall {
        /// Payload length of the pending record.
        needed: usize,
    },
    /// The lane is closed or corrupt.
    Lane(LaneError),
}

impl fmt::Display for PopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for PopError {}

/// Result of a successful push.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct PushOk {
    /// The consumer had published `sleeping` and the ring was empty before
    /// this push: the caller should send a wake-up kick.
    pub kick: bool,
}

/// Metadata of a popped record (the payload was copied out).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordMeta {
    /// Payload length.
    pub len: usize,
    /// Record tag.
    pub tag: u16,
    /// Record flags (never contains [`FLAG_SKIP`]).
    pub flags: u16,
}

#[inline]
fn round8(n: usize) -> usize {
    (n + 7) & !7
}

/// Writer end of a lane.
pub struct Producer<M: Mapping> {
    inner: Arc<Inner<M>>,
    idx: usize,
    ring: RingPtr,
    head: u64,
    tail_seen: u64,
    max_record: usize,
    closed: bool,
}

// SAFETY: the raw pointers point into `inner.map`, kept alive by the `Arc`;
// `Mapping: Send + Sync`. The endpoint is the unique writer of its index.
unsafe impl<M: Mapping> Send for Producer<M> {}

impl<M: Mapping> Producer<M> {
    pub(crate) fn new(inner: Arc<Inner<M>>, idx: usize) -> Result<Self, LaneError> {
        let info = inner.lanes[idx];
        // SAFETY: `info` was validated against this mapping.
        let ring = unsafe { RingPtr::new(inner.map.base(), &info) };
        let head = ring.head().load(Ordering::Acquire);
        let tail = ring.tail().load(Ordering::Acquire);
        let size = info.size as u64;
        if tail > head {
            return Err(LaneError::Corrupt(Corruption::IndexBackwards));
        }
        if head > MAX_INDEX || head - tail > size {
            return Err(LaneError::Corrupt(Corruption::IndexOutOfRange));
        }
        if head % 8 != 0 || tail % 8 != 0 {
            return Err(LaneError::Corrupt(Corruption::IndexMisaligned));
        }
        ring.prod_flags().store(0, Ordering::Release);
        Ok(Self {
            max_record: DEFAULT_MAX_RECORD.min(info.size - REC_HEADER_LEN),
            inner,
            idx,
            ring,
            head,
            tail_seen: tail,
            closed: false,
        })
    }

    /// Lane id.
    pub fn lane_id(&self) -> u16 {
        self.inner.lanes[self.idx].id
    }

    /// Largest payload this producer will send.
    pub fn max_record(&self) -> usize {
        self.max_record
    }

    /// Lower the record cap (it cannot exceed what the ring can hold).
    pub fn set_max_record(&mut self, cap: usize) {
        self.max_record = cap.min(self.ring.size - REC_HEADER_LEN);
    }

    /// True once the lane is closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Mark the lane closed for the consumer (graceful end of stream).
    pub fn close(&mut self) {
        self.closed = true;
        self.ring.prod_flags().fetch_or(PROD_CLOSED, Ordering::Release);
    }

    /// Whether the consumer has published its `sleeping` flag.
    pub fn consumer_sleeping(&self) -> bool {
        self.ring.cons_flags().load(Ordering::SeqCst) & CONS_SLEEPING != 0
    }

    /// Re-read the consumer's tail and validate it against what we know.
    fn refresh_tail(&mut self) -> Result<(), PushError> {
        let tail = self.ring.tail().load(Ordering::Acquire);
        let bad = if tail < self.tail_seen {
            Some(Corruption::IndexBackwards)
        } else if tail > self.head {
            Some(Corruption::IndexOutOfRange)
        } else if tail % 8 != 0 {
            Some(Corruption::IndexMisaligned)
        } else {
            None
        };
        if let Some(c) = bad {
            self.closed = true;
            self.ring.prod_flags().fetch_or(PROD_CLOSED, Ordering::Release);
            return Err(PushError::Lane(LaneError::Corrupt(c)));
        }
        self.tail_seen = tail;
        Ok(())
    }

    /// True if `need` bytes are free (refreshing the cached tail if necessary).
    fn has_room(&mut self, need: usize) -> Result<bool, PushError> {
        let size = self.ring.size as u64;
        if size - (self.head - self.tail_seen) >= need as u64 {
            return Ok(true);
        }
        self.refresh_tail()?;
        Ok(size - (self.head - self.tail_seen) >= need as u64)
    }

    fn publish(&mut self, new_head: u64) {
        publish_fence();
        self.ring.head().store(new_head, Ordering::Release);
        self.head = new_head;
    }

    /// Push a record with tag 0 and no flags.
    pub fn try_push(&mut self, payload: &[u8]) -> Result<PushOk, PushError> {
        self.try_push_tagged(0, 0, payload)
    }

    /// Push a record with the given tag and flags. Copies `payload` into the
    /// ring (the only copy on the producer side), zeroes the padding, executes
    /// [`publish_fence`], then publishes the head with release ordering.
    pub fn try_push_tagged(
        &mut self,
        tag: u16,
        flags: u16,
        payload: &[u8],
    ) -> Result<PushOk, PushError> {
        if self.closed {
            return Err(PushError::Lane(LaneError::Closed));
        }
        if flags & FLAG_SKIP != 0 {
            return Err(PushError::BadFlags);
        }
        if payload.len() > self.max_record {
            return Err(PushError::TooLarge);
        }
        if self.ring.cons_flags().load(Ordering::Acquire) & CONS_CLOSED != 0 {
            self.closed = true;
            return Err(PushError::Lane(LaneError::Closed));
        }
        let size = self.ring.size;
        let total = REC_HEADER_LEN + round8(payload.len());
        let start_head = self.head;

        let pos = (self.head & (size as u64 - 1)) as usize;
        if total > size - pos {
            // Not contiguous: cover the tail of the buffer with a skip marker
            // (published on its own, so a later `Full` leaves a consistent ring).
            let skip = size - pos;
            if !self.has_room(skip)? {
                return Err(PushError::Full);
            }
            // SAFETY: pos + 8 <= size (both multiples of 8, pos < size).
            unsafe {
                ptr::write(
                    self.ring.data.add(pos) as *mut u64,
                    ((FLAG_SKIP as u64) << 48).to_le(),
                );
            }
            self.publish(self.head + skip as u64);
        }
        if !self.has_room(total)? {
            return Err(PushError::Full);
        }
        let pos = (self.head & (size as u64 - 1)) as usize;
        debug_assert!(pos + total <= size);
        let hdr = payload.len() as u64 | (tag as u64) << 32 | (flags as u64) << 48;
        // SAFETY: [pos, pos + total) lies inside the data area (checked above)
        // and is free (has_room); the consumer will not read it before the
        // head store below.
        unsafe {
            let dst = self.ring.data.add(pos);
            ptr::write(dst as *mut u64, hdr.to_le());
            ptr::copy_nonoverlapping(payload.as_ptr(), dst.add(REC_HEADER_LEN), payload.len());
            let pad = total - REC_HEADER_LEN - payload.len();
            ptr::write_bytes(dst.add(REC_HEADER_LEN + payload.len()), 0, pad);
        }
        self.publish(self.head + total as u64);

        // Dekker with the consumer's `set_sleeping(true)`: either it sees our
        // head in its re-check, or we see its flag here.
        fence(Ordering::SeqCst);
        let mut kick = false;
        if self.ring.cons_flags().load(Ordering::SeqCst) & CONS_SLEEPING != 0 {
            // "Ring was empty": the consumer had consumed everything up to the
            // head we started from. The raw tail is only used as a hint here.
            kick = self.ring.tail().load(Ordering::Acquire) == start_head;
        }
        Ok(PushOk { kick })
    }

    /// Push and, if the push asks for it, call `kick.kick()`.
    pub fn push_and_kick<K: crate::wake::Kick + ?Sized>(
        &mut self,
        payload: &[u8],
        kick: &K,
    ) -> Result<(), PushError> {
        if self.try_push(payload)?.kick {
            kick.kick();
        }
        Ok(())
    }
}

impl<M: Mapping> Drop for Producer<M> {
    fn drop(&mut self) {
        self.inner.taken[self.idx].store(false, Ordering::Release);
    }
}

/// Reader end of a lane. Records are copied out and only then handed to the
/// caller; there is deliberately no API that exposes references into shared
/// memory.
pub struct Consumer<M: Mapping> {
    inner: Arc<Inner<M>>,
    idx: usize,
    ring: RingPtr,
    tail: u64,
    head_seen: u64,
    max_record: usize,
    flags: u32,
    closed: bool,
    reason: Option<Corruption>,
}

// SAFETY: see `Producer`.
unsafe impl<M: Mapping> Send for Consumer<M> {}

struct Loc {
    pos: usize,
    len: usize,
    tag: u16,
    flags: u16,
    total: u64,
}

impl<M: Mapping> Consumer<M> {
    pub(crate) fn new(inner: Arc<Inner<M>>, idx: usize) -> Result<Self, LaneError> {
        let info = inner.lanes[idx];
        // SAFETY: `info` was validated against this mapping.
        let ring = unsafe { RingPtr::new(inner.map.base(), &info) };
        let head = ring.head().load(Ordering::Acquire);
        let tail = ring.tail().load(Ordering::Acquire);
        let size = info.size as u64;
        if tail > head {
            return Err(LaneError::Corrupt(Corruption::IndexBackwards));
        }
        if head > MAX_INDEX || head - tail > size {
            return Err(LaneError::Corrupt(Corruption::IndexOutOfRange));
        }
        if head % 8 != 0 || tail % 8 != 0 {
            return Err(LaneError::Corrupt(Corruption::IndexMisaligned));
        }
        ring.cons_flags().store(0, Ordering::Release);
        Ok(Self {
            max_record: DEFAULT_MAX_RECORD.min(info.size - REC_HEADER_LEN),
            inner,
            idx,
            ring,
            tail,
            head_seen: head,
            flags: 0,
            closed: false,
            reason: None,
        })
    }

    /// Lane id.
    pub fn lane_id(&self) -> u16 {
        self.inner.lanes[self.idx].id
    }

    /// Per-lane cap on a record's payload; larger records close the lane.
    pub fn max_record(&self) -> usize {
        self.max_record
    }

    /// Change the record cap (clamped to what the ring can hold).
    pub fn set_max_record(&mut self, cap: usize) {
        self.max_record = cap.min(self.ring.size - REC_HEADER_LEN);
    }

    /// True once the lane is closed (sticky).
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Why the lane was closed, if because of corruption.
    pub fn corruption(&self) -> Option<Corruption> {
        self.reason
    }

    fn set_flag(&mut self, f: u32, on: bool) {
        if on {
            self.flags |= f;
        } else {
            self.flags &= !f;
        }
        self.ring.cons_flags().store(self.flags, Ordering::SeqCst);
    }

    fn close_with(&mut self, c: Corruption) -> LaneError {
        self.closed = true;
        self.reason = Some(c);
        self.set_flag(CONS_CLOSED, true);
        LaneError::Corrupt(c)
    }

    /// Load and validate the producer's head.
    fn load_head(&mut self) -> Result<u64, LaneError> {
        let head = self.ring.head().load(Ordering::Acquire);
        let c = if head < self.head_seen {
            Some(Corruption::IndexBackwards)
        } else if head > MAX_INDEX || head - self.tail > self.ring.size as u64 {
            Some(Corruption::IndexOutOfRange)
        } else if head % 8 != 0 {
            Some(Corruption::IndexMisaligned)
        } else {
            None
        };
        if let Some(c) = c {
            return Err(self.close_with(c));
        }
        self.head_seen = head;
        Ok(head)
    }

    /// Whether unread bytes are published. Validates the index like a pop.
    pub fn poll_ready(&mut self) -> Result<bool, LaneError> {
        if self.closed {
            return Err(LaneError::Closed);
        }
        Ok(self.load_head()? != self.tail)
    }

    /// Locate the next record, consuming skip markers. Reads the record header
    /// once and validates the local copy.
    fn locate(&mut self) -> Result<Option<Loc>, LaneError> {
        if self.closed {
            return Err(LaneError::Closed);
        }
        let size = self.ring.size;
        loop {
            let head = self.load_head()?;
            let avail = head - self.tail;
            if avail == 0 {
                if self.ring.prod_flags().load(Ordering::Acquire) & PROD_CLOSED != 0 {
                    self.closed = true;
                    return Err(LaneError::Closed);
                }
                return Ok(None);
            }
            // avail is a non-zero multiple of 8 and size >= 64, so the 8-byte
            // header at `pos` is inside the published bytes and the data area.
            let pos = (self.tail & (size as u64 - 1)) as usize;
            // SAFETY: pos + 8 <= size (multiples of 8, pos < size) and the data
            // area is 8-aligned. A relaxed atomic load makes this one fetch of the
            // (possibly concurrently rewritten) header; all checks use the local.
            let w = u64::from_le(unsafe {
                (*(self.ring.data.add(pos) as *const AtomicU64)).load(Ordering::Relaxed)
            });
            let len = (w & 0xFFFF_FFFF) as usize;
            let tag = (w >> 32) as u16;
            let flags = (w >> 48) as u16;
            if flags & FLAG_SKIP != 0 {
                let skip = (size - pos) as u64;
                if pos == 0 || skip > avail {
                    return Err(self.close_with(Corruption::BadSkip));
                }
                self.tail += skip;
                self.ring.tail().store(self.tail, Ordering::Release);
                continue;
            }
            if len > self.max_record {
                return Err(self.close_with(Corruption::RecordTooLong));
            }
            let total = (REC_HEADER_LEN + round8(len)) as u64;
            if total > avail {
                return Err(self.close_with(Corruption::RecordExceedsAvailable));
            }
            if pos as u64 + total > size as u64 {
                return Err(self.close_with(Corruption::RecordWraps));
            }
            return Ok(Some(Loc {
                pos,
                len,
                tag,
                flags,
                total,
            }));
        }
    }

    fn finish(&mut self, loc: &Loc) -> RecordMeta {
        self.tail += loc.total;
        self.ring.tail().store(self.tail, Ordering::Release);
        RecordMeta {
            len: loc.len,
            tag: loc.tag,
            flags: loc.flags,
        }
    }

    /// Pop the next record into `out` (cleared first, grown to the payload
    /// length only after the length passed validation). `Ok(None)` if empty.
    pub fn try_pop(&mut self, out: &mut Vec<u8>) -> Result<Option<RecordMeta>, LaneError> {
        let Some(loc) = self.locate()? else {
            return Ok(None);
        };
        out.clear();
        out.reserve(loc.len);
        // SAFETY: source range [pos + 8, pos + 8 + len) is inside the data area
        // (`locate`); the destination has capacity for `len` bytes.
        unsafe {
            let src = self.ring.data.add(loc.pos + REC_HEADER_LEN);
            ptr::copy_nonoverlapping(src, out.as_mut_ptr(), loc.len);
            out.set_len(loc.len);
        }
        Ok(Some(self.finish(&loc)))
    }

    /// Pop the next record into a caller-provided slice. If it is too small the
    /// record stays in the ring and the needed length is reported.
    pub fn try_pop_into(&mut self, out: &mut [u8]) -> Result<Option<RecordMeta>, PopError> {
        let Some(loc) = self.locate().map_err(PopError::Lane)? else {
            return Ok(None);
        };
        if out.len() < loc.len {
            return Err(PopError::BufferTooSmall { needed: loc.len });
        }
        // SAFETY: as in `try_pop`; `out` has at least `len` bytes.
        unsafe {
            let src = self.ring.data.add(loc.pos + REC_HEADER_LEN);
            ptr::copy_nonoverlapping(src, out.as_mut_ptr(), loc.len);
        }
        Ok(Some(self.finish(&loc)))
    }

    /// Publish or clear the `sleeping` flag (sequentially consistent, followed
    /// by a full fence). Protocol: set, re-check with [`poll_ready`](Self::poll_ready),
    /// sleep only if still empty, clear on wake.
    pub fn set_sleeping(&mut self, on: bool) {
        self.set_flag(CONS_SLEEPING, on);
        fence(Ordering::SeqCst);
    }

    /// Close the lane locally and tell the producer.
    pub fn close(&mut self) {
        self.closed = true;
        self.set_flag(CONS_CLOSED, true);
    }
}

impl<M: Mapping> Drop for Consumer<M> {
    fn drop(&mut self) {
        self.inner.taken[self.idx].store(false, Ordering::Release);
    }
}
