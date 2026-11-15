// SPDX-License-Identifier: GPL-3.0-or-later

//! Lane scheduler: decides which lane to serve next.
//!
//! Policy, evaluated at every scheduling point (a record, or one bulk block):
//!
//! 1. **Starvation guard**: a bulk lane that has pending data and credit, and
//!    has not been served for `bulk_min_interval` (default 50 ms), goes first.
//!    This is what guarantees bulk its minimum even when control and frames
//!    saturate the link; it delays everything else by at most one block.
//! 2. **Control** lanes, strict priority, round-robin among control lanes.
//! 3. **Normal** (commands/frames) lanes, round-robin.
//! 4. **Bulk** lanes with credit, round-robin: bulk takes leftover bandwidth.
//!
//! Bulk is sent in blocks of at most `block_size` (1 MiB) with at most
//! `credit_window` blocks in flight; the consumer returns credit with a small
//! control record ([`encode_credit`] / [`decode_credit`]). Because a block is
//! the unit of non-preemption, a control record waits at most one block time.
//!
//! The scheduler is pure logic driven by a caller-supplied clock value, so it
//! can be simulated with a virtual clock.

use std::collections::VecDeque;
use std::time::Duration;

use crate::region::Priority;

/// Scheduling class of a lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Strict priority.
    Control,
    /// Commands/frames.
    Normal,
    /// Bulk transfers.
    Bulk,
}

impl From<Priority> for Class {
    fn from(p: Priority) -> Self {
        match p {
            Priority::Control => Self::Control,
            Priority::High => Self::Normal,
            Priority::Bulk => Self::Bulk,
        }
    }
}

/// Scheduler tuning.
#[derive(Clone, Copy, Debug)]
pub struct SchedConfig {
    /// Guaranteed minimum service interval for a bulk lane with pending data.
    pub bulk_min_interval: Duration,
    /// Largest bulk block.
    pub block_size: usize,
    /// Bulk blocks that may be in flight (sent, credit not yet returned).
    pub credit_window: u32,
}

impl Default for SchedConfig {
    fn default() -> Self {
        Self {
            bulk_min_interval: Duration::from_millis(50),
            block_size: 1 << 20,
            credit_window: 4,
        }
    }
}

/// The scheduler's decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pick {
    /// Index returned by [`Scheduler::add_lane`].
    pub lane: usize,
    /// Class of the lane.
    pub class: Class,
    /// Largest amount to send now: `block_size` for bulk, unlimited otherwise
    /// (one record).
    pub max_len: usize,
}

/// A peer returned more credit than it had been given: protocol violation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreditOverflow;

struct LaneState {
    class: Class,
    in_flight: u32,
    /// Guard reference time: last served, or last time the lane was idle.
    bulk_ref: Duration,
}

/// See the module documentation.
pub struct Scheduler {
    cfg: SchedConfig,
    lanes: Vec<LaneState>,
    cursor: [usize; 3],
}

fn class_slot(c: Class) -> usize {
    match c {
        Class::Control => 0,
        Class::Normal => 1,
        Class::Bulk => 2,
    }
}

impl Scheduler {
    /// New scheduler without lanes.
    pub fn new(cfg: SchedConfig) -> Self {
        Self {
            cfg,
            lanes: Vec::new(),
            cursor: [0; 3],
        }
    }

    /// Register a lane; returns its index.
    pub fn add_lane(&mut self, class: Class) -> usize {
        self.lanes.push(LaneState {
            class,
            in_flight: 0,
            bulk_ref: Duration::ZERO,
        });
        self.lanes.len() - 1
    }

    /// Bulk blocks sent on `lane` whose credit has not come back.
    pub fn in_flight(&self, lane: usize) -> u32 {
        self.lanes[lane].in_flight
    }

    /// The consumer acknowledged `blocks` bulk blocks on `lane`. Returning more
    /// than is in flight is a protocol violation and changes nothing.
    pub fn return_credit(&mut self, lane: usize, blocks: u32) -> Result<(), CreditOverflow> {
        let l = self.lanes.get_mut(lane).ok_or(CreditOverflow)?;
        if l.class != Class::Bulk || blocks > l.in_flight {
            return Err(CreditOverflow);
        }
        l.in_flight -= blocks;
        Ok(())
    }

    fn can_send(&self, i: usize, pending: &mut impl FnMut(usize) -> bool) -> bool {
        let l = &self.lanes[i];
        pending(i) && (l.class != Class::Bulk || l.in_flight < self.cfg.credit_window)
    }

    fn round_robin(
        &mut self,
        class: Class,
        pending: &mut impl FnMut(usize) -> bool,
    ) -> Option<usize> {
        let n = self.lanes.len();
        let slot = class_slot(class);
        for k in 0..n {
            let i = (self.cursor[slot] + k) % n;
            if self.lanes[i].class == class && self.can_send(i, pending) {
                self.cursor[slot] = (i + 1) % n;
                return Some(i);
            }
        }
        None
    }

    fn make_pick(&mut self, lane: usize, now: Duration) -> Pick {
        let class = self.lanes[lane].class;
        let max_len = if class == Class::Bulk {
            let l = &mut self.lanes[lane];
            l.in_flight += 1;
            l.bulk_ref = now;
            self.cfg.block_size
        } else {
            usize::MAX
        };
        Pick {
            lane,
            class,
            max_len,
        }
    }

    /// Choose the lane to serve at virtual/real time `now`. `pending(i)` says
    /// whether lane `i` has something to send. Picking a bulk lane consumes one
    /// credit; the caller must send at most `max_len` bytes.
    pub fn next(
        &mut self,
        now: Duration,
        mut pending: impl FnMut(usize) -> bool,
    ) -> Option<Pick> {
        // Bulk lanes with nothing to send accrue no starvation debt.
        let mut guard: Option<usize> = None;
        for i in 0..self.lanes.len() {
            if self.lanes[i].class != Class::Bulk {
                continue;
            }
            if !pending(i) {
                self.lanes[i].bulk_ref = now;
                continue;
            }
            let due = now.saturating_sub(self.lanes[i].bulk_ref) >= self.cfg.bulk_min_interval;
            if due
                && self.lanes[i].in_flight < self.cfg.credit_window
                && guard.map_or(true, |g| self.lanes[i].bulk_ref < self.lanes[g].bulk_ref)
            {
                guard = Some(i);
            }
        }
        if let Some(i) = guard {
            return Some(self.make_pick(i, now));
        }
        for class in [Class::Control, Class::Normal, Class::Bulk] {
            if let Some(i) = self.round_robin(class, &mut pending) {
                return Some(self.make_pick(i, now));
            }
        }
        None
    }
}

/// Length of a credit record.
pub const CREDIT_RECORD_LEN: usize = 8;
const CREDIT_KIND: u8 = 0xC1;

/// Encode "I consumed `blocks` bulk blocks of lane `lane`" for the control lane.
pub fn encode_credit(lane: u16, blocks: u32) -> [u8; CREDIT_RECORD_LEN] {
    let mut r = [0u8; CREDIT_RECORD_LEN];
    r[0] = CREDIT_KIND;
    r[2..4].copy_from_slice(&lane.to_le_bytes());
    r[4..8].copy_from_slice(&blocks.to_le_bytes());
    r
}

/// Decode and validate a credit record: exact length, kind byte, zero padding
/// byte, non-zero block count.
pub fn decode_credit(rec: &[u8]) -> Option<(u16, u32)> {
    let r: &[u8; CREDIT_RECORD_LEN] = rec.try_into().ok()?;
    if r[0] != CREDIT_KIND || r[1] != 0 {
        return None;
    }
    let blocks = u32::from_le_bytes([r[4], r[5], r[6], r[7]]);
    if blocks == 0 {
        return None;
    }
    Some((u16::from_le_bytes([r[2], r[3]]), blocks))
}

/// Bounded queue where the newest item wins: pushing into a full queue drops
/// the oldest (stale) item. Used for the frames lane.
pub struct NewestWins<T> {
    cap: usize,
    q: VecDeque<T>,
    dropped: u64,
}

impl<T> NewestWins<T> {
    /// Queue holding at most `cap` (at least 1) items.
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            cap,
            q: VecDeque::with_capacity(cap),
            dropped: 0,
        }
    }

    /// Enqueue `item`; returns the stale item that was dropped to make room.
    pub fn push(&mut self, item: T) -> Option<T> {
        let stale = if self.q.len() == self.cap {
            self.dropped += 1;
            self.q.pop_front()
        } else {
            None
        };
        self.q.push_back(item);
        stale
    }

    /// Oldest remaining item.
    pub fn pop(&mut self) -> Option<T> {
        self.q.pop_front()
    }

    /// Number of queued items.
    pub fn len(&self) -> usize {
        self.q.len()
    }

    /// True if empty.
    pub fn is_empty(&self) -> bool {
        self.q.is_empty()
    }

    /// Total items dropped as stale.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}
