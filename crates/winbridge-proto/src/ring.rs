// SPDX-License-Identifier: GPL-3.0-or-later

//! Message transport over the shared-memory lanes of `winbridge-shm`.
//!
//! Both ends (the host viewer and the guest agent) use [`RingLink`]; see `docs/PROTOCOL.md`
//! ("Shared-memory transport") and `docs/IVSHMEM-TRANSPORT.md`.
//!
//! * Every [`Message`] is encoded exactly as on the serial wire ([`encode_frame`]) and becomes
//!   one record on a lane: **control** lanes carry the messages for which
//!   [`Message::is_bulk`] is false, **bulk** lanes the rest. Records have a tag: `0` = message
//!   frame, `1` = bulk credit (control lanes only).
//! * One **writer thread** per direction chooses between the control and the bulk queue with the
//!   `winbridge-shm` [`Scheduler`]: control is strict priority, bulk is sent in blocks of at most
//!   `block_size` bytes with a credit window, and the scheduler's minimum-progress rule lets a
//!   starved bulk lane through. The last record of a bulk block carries [`FLAG_BLOCK_END`]; the
//!   consumer returns one credit per block on the reverse control lane.
//! * One **reader thread** per consumer lane copies a record out, validates it (tag, flags, exact
//!   frame length, lane versus [`Message::is_bulk`]) and hands the decoded message to the
//!   `deliver` callback. The callback may block: that stalls only this lane (back-pressure to
//!   the producer through the ring); it never touches the serial reader.
//! * Wake-up is adaptive polling (bounded spin, then ~1 ms sleep with the `sleeping` flag); a
//!   producer that finds the consumer asleep calls the [`Kick`], which the owner implements by
//!   sending `Message::ShmKick` over the serial channel (see [`KickPump`]); the receiver calls
//!   [`RingLink::wake`].
//! * Any ring corruption, a peer that stopped reading for `stall_limit`, too many invalid
//!   records or a credit protocol violation ends the link: the `on_fault` callback gets the
//!   reason and every message that was queued but not yet written, so the owner can send them
//!   over the serial channel instead.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tracing::{debug, warn};
use winbridge_shm::sched::{decode_credit, encode_credit};
use winbridge_shm::{
    Class, Consumer, Direction, Idle, IdleConfig, Kick, LaneError, LaneSpec, Mapping, Priority,
    Producer, PushError, Region, Role, SchedConfig, Scheduler, Side, Waker,
};

use crate::{decode_frame, encode_frame, Message, ProtoError};

/// Control lane host to guest.
pub const LANE_CTL_H2G: u16 = 1;
/// Control lane guest to host (highest priority).
pub const LANE_CTL_G2H: u16 = 2;
/// Bulk lane host to guest.
pub const LANE_BULK_H2G: u16 = 3;
/// Bulk lane guest to host (lowest priority).
pub const LANE_BULK_G2H: u16 = 4;
/// Data size of a control ring.
pub const CTL_RING_BYTES: usize = 1 << 20;
/// Data size of a bulk ring.
pub const BULK_RING_BYTES: usize = 64 << 20;
/// Length of the region (the whole BAR): header page, four rings, spare.
pub const REGION_LEN: u64 = 256 << 20;
/// Largest control record (a bigger control message goes over the serial channel).
pub const CTL_MAX_RECORD: usize = 256 * 1024;
/// Largest bulk record: a clipboard item of `clip::MAX_ITEM_BYTES` (48 MiB) plus framing.
pub const BULK_MAX_RECORD: usize = 56 << 20;
/// Record tag: one message frame.
pub const TAG_MESSAGE: u16 = 0;
/// Record tag: bulk credit (control lanes).
pub const TAG_CREDIT: u16 = 1;
/// Record flag on a bulk record: this record ends a scheduling block (one credit).
pub const FLAG_BLOCK_END: u16 = 1;
/// Capability bits of the region header (informational; the high half holds the generation).
pub const REGION_CAPS: u64 = 1;

/// The lane table of a region created by the host (`Region::create`).
pub fn lane_specs() -> [LaneSpec; 4] {
    let spec = |id, direction, priority, data_size| LaneSpec {
        id,
        direction,
        priority,
        flags: 0,
        data_size,
    };
    [
        spec(LANE_CTL_H2G, Direction::HostToGuest, Priority::Control, CTL_RING_BYTES),
        spec(LANE_CTL_G2H, Direction::GuestToHost, Priority::Control, CTL_RING_BYTES),
        spec(LANE_BULK_H2G, Direction::HostToGuest, Priority::Bulk, BULK_RING_BYTES),
        spec(LANE_BULK_G2H, Direction::GuestToHost, Priority::Bulk, BULK_RING_BYTES),
    ]
}

/// Creates the region on the host side with the standard lanes. `generation` is stored in the
/// high half of the capability word (a new one per serial session).
pub fn create_region<M: Mapping>(map: M, generation: u32) -> Result<Region<M>, winbridge_shm::RegionError> {
    Region::create(
        map,
        REGION_CAPS | (generation as u64) << 32,
        Role::GuestAgent,
        &lane_specs(),
    )
}

/// Which lane class a message travelled on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// Control lane.
    Control,
    /// Bulk lane.
    Bulk,
}

/// Tuning of a [`RingLink`].
#[derive(Clone, Copy, Debug)]
pub struct LinkConfig {
    /// Lane scheduler (bulk block size, credit window, minimum progress interval).
    pub sched: SchedConfig,
    /// Reader polling: bounded spin, then sleep.
    pub idle: IdleConfig,
    /// Messages the control queue holds before `send` blocks.
    pub ctl_queue_msgs: usize,
    /// Bytes the bulk queue holds before `send` blocks (at least one message).
    pub bulk_queue_bytes: usize,
    /// A lane that stays full, or credit that stays outstanding, for this long ends the link.
    pub stall_limit: Duration,
    /// Invalid records tolerated before the link is ended.
    pub bad_record_limit: u32,
    /// Pause before the writer retries a full ring.
    pub full_retry: Duration,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            sched: SchedConfig {
                credit_window: 16,
                ..SchedConfig::default()
            },
            idle: IdleConfig::default(),
            ctl_queue_msgs: 4096,
            bulk_queue_bytes: 8 << 20,
            stall_limit: Duration::from_secs(10),
            bad_record_limit: 8,
            full_retry: Duration::from_micros(100),
        }
    }
}

/// Why and with what leftovers a link ended on its own.
#[derive(Debug)]
pub struct Fault {
    /// Human-readable cause.
    pub reason: String,
    /// Messages that were queued but not written to a lane, in send order (control first).
    pub unsent: Vec<Message>,
}

/// Receives each decoded message on the reader thread of its lane. May block (back-pressure).
pub type DeliverFn = Arc<dyn Fn(Lane, Message) + Send + Sync>;
/// Called once, from the thread that detected the end of the link.
pub type FaultFn = Box<dyn FnOnce(Fault) + Send>;

/// Why a message was not queued; the message comes back so the caller can use serial.
#[derive(Debug)]
pub enum SendError {
    /// The link has ended.
    Closed(Message),
    /// This message cannot use a lane (negotiation message, or larger than the lane's record cap).
    Unsuitable(Message),
    /// Non-blocking send only: the queue is full.
    Full(Message),
    /// The message cannot be encoded at all.
    Encode(ProtoError),
}

/// The link could not be started.
#[derive(Debug)]
pub struct StartError(pub String);

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StartError {}

/// Counters of a link.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Messages written to the control lane.
    pub tx_control: u64,
    /// Messages written to the bulk lane.
    pub tx_bulk: u64,
    /// Messages delivered from the control lane.
    pub rx_control: u64,
    /// Messages delivered from the bulk lane.
    pub rx_bulk: u64,
    /// Kicks requested.
    pub kicks: u64,
    /// Invalid records seen.
    pub bad_records: u32,
}

#[derive(Default)]
struct Out {
    ctl: VecDeque<Vec<u8>>,
    bulk: VecDeque<Vec<u8>>,
    bulk_bytes: usize,
}

#[derive(Default)]
struct Counters {
    tx_control: AtomicU64,
    tx_bulk: AtomicU64,
    rx_control: AtomicU64,
    rx_bulk: AtomicU64,
    kicks: AtomicU64,
}

struct Shared {
    cfg: LinkConfig,
    out: Mutex<Out>,
    /// The writer waits here for work.
    out_cv: Condvar,
    /// Senders wait here for queue space.
    space_cv: Condvar,
    /// The writer must stop.
    tx_stop: AtomicBool,
    /// The link accepts messages.
    up: AtomicBool,
    /// Readers stop once the link is down and this instant has passed (drain window).
    rx_deadline: Mutex<Option<Instant>>,
    credit_in: AtomicU64,
    credit_out: AtomicU32,
    wakers: [Waker; 2],
    bad: AtomicU32,
    live: AtomicU32,
    /// Readers that drained a lane the peer closed.
    peer_closed: AtomicU32,
    on_fault: Mutex<Option<FaultFn>>,
    deliver: DeliverFn,
    kick: Arc<dyn Kick + Send + Sync>,
    counters: Counters,
    tx_ctl_cap: usize,
    tx_bulk_cap: usize,
    tx_bulk_id: u16,
    rx_bulk_id: u16,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn lock_out(&self) -> MutexGuard<'_, Out> {
        lock(&self.out)
    }

    fn take_unsent(&self) -> Vec<Message> {
        let mut o = self.lock_out();
        o.bulk_bytes = 0;
        let ctl = std::mem::take(&mut o.ctl);
        let bulk = std::mem::take(&mut o.bulk);
        drop(o);
        ctl.into_iter()
            .chain(bulk)
            .filter_map(|f| decode_frame(&f).ok())
            .collect()
    }

    /// Ends the link; returns the unsent messages, or `None` if it had already ended.
    fn end(&self, drain: Duration) -> Option<Vec<Message>> {
        if !self.up.swap(false, Ordering::AcqRel) {
            return None;
        }
        *lock(&self.rx_deadline) = Some(Instant::now() + drain);
        self.tx_stop.store(true, Ordering::Release);
        let unsent = self.take_unsent();
        {
            let _g = self.lock_out();
            self.out_cv.notify_all();
            self.space_cv.notify_all();
        }
        Some(unsent)
    }

    fn fault(&self, reason: String) {
        if let Some(unsent) = self.end(Duration::ZERO) {
            warn!("shared-memory link ended: {reason}");
            let f = lock(&self.on_fault).take();
            if let Some(f) = f {
                f(Fault { reason, unsent });
            }
        }
    }

    fn rx_should_stop(&self) -> bool {
        if self.up.load(Ordering::Acquire) {
            return false;
        }
        lock(&self.rx_deadline).map_or(true, |d| Instant::now() >= d)
    }

    fn wake_writer(&self) {
        let _g = self.lock_out();
        self.out_cv.notify_one();
    }

    fn bad_record(&self, why: String) -> bool {
        let n = self.bad.fetch_add(1, Ordering::AcqRel) + 1;
        warn!("invalid shared-memory record ({n}): {why}");
        if n > self.cfg.bad_record_limit {
            self.fault(format!("too many invalid records (last: {why})"));
            return false;
        }
        true
    }

    fn kick(&self) {
        self.counters.kicks.fetch_add(1, Ordering::Relaxed);
        self.kick.kick();
    }

    fn requeue_front(&self, bulk: bool, frame: Vec<u8>) {
        let mut o = self.lock_out();
        if bulk {
            o.bulk_bytes += frame.len();
            o.bulk.push_front(frame);
        } else {
            o.ctl.push_front(frame);
        }
    }

    fn enqueue(&self, msg: Message, block: bool) -> Result<(), SendError> {
        if msg.is_shm_control() {
            return Err(SendError::Unsuitable(msg));
        }
        let bulk = msg.is_bulk();
        let frame = encode_frame(&msg).map_err(SendError::Encode)?;
        if frame.len() > if bulk { self.tx_bulk_cap } else { self.tx_ctl_cap } {
            return Err(SendError::Unsuitable(msg));
        }
        let mut o = self.lock_out();
        loop {
            if !self.up.load(Ordering::Acquire) {
                return Err(SendError::Closed(msg));
            }
            let full = if bulk {
                !o.bulk.is_empty() && o.bulk_bytes + frame.len() > self.cfg.bulk_queue_bytes
            } else {
                o.ctl.len() >= self.cfg.ctl_queue_msgs
            };
            if !full {
                if bulk {
                    o.bulk_bytes += frame.len();
                    o.bulk.push_back(frame);
                } else {
                    o.ctl.push_back(frame);
                }
                drop(o);
                self.out_cv.notify_one();
                return Ok(());
            }
            if !block {
                return Err(SendError::Full(msg));
            }
            o = self
                .space_cv
                .wait_timeout(o, Duration::from_millis(50))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Validates and delivers one record; false ends the reader.
    fn handle_record(&self, lane: Lane, tag: u16, flags: u16, payload: &[u8]) -> bool {
        match tag {
            TAG_MESSAGE => {
                let allowed = if lane == Lane::Bulk { FLAG_BLOCK_END } else { 0 };
                let mut ok = true;
                if flags & !allowed != 0 {
                    ok = self.bad_record(format!("unknown record flags {flags:#x}"));
                } else {
                    match decode_frame(payload) {
                        Ok(msg) if msg.is_shm_control() || msg.is_bulk() != (lane == Lane::Bulk) => {
                            ok = self.bad_record(format!("{msg:?} does not belong on the {lane:?} lane"));
                        }
                        Ok(msg) => {
                            let c = if lane == Lane::Bulk {
                                &self.counters.rx_bulk
                            } else {
                                &self.counters.rx_control
                            };
                            c.fetch_add(1, Ordering::Relaxed);
                            (self.deliver)(lane, msg);
                        }
                        Err(e) => ok = self.bad_record(format!("undecodable frame: {e}")),
                    }
                }
                // The block is consumed either way: hand its credit back.
                if lane == Lane::Bulk && flags & FLAG_BLOCK_END != 0 {
                    self.credit_out.fetch_add(1, Ordering::AcqRel);
                    self.wake_writer();
                }
                ok
            }
            TAG_CREDIT if lane == Lane::Control && flags == 0 => match decode_credit(payload) {
                Some((l, n)) if l == self.tx_bulk_id => {
                    self.credit_in.fetch_add(n as u64, Ordering::AcqRel);
                    self.wake_writer();
                    true
                }
                _ => self.bad_record("malformed credit record".to_string()),
            },
            _ => self.bad_record(format!("unknown record tag {tag}")),
        }
    }
}

struct LiveGuard(Arc<Shared>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A running shared-memory link: one writer thread and two reader threads.
pub struct RingLink {
    shared: Arc<Shared>,
}

impl RingLink {
    /// Starts the link on `region` (already created by the host, or attached by the guest). The
    /// lane table must be the standard one ([`lane_specs`]); anything else is refused.
    pub fn start<M: Mapping>(
        region: &Region<M>,
        cfg: LinkConfig,
        kick: Arc<dyn Kick + Send + Sync>,
        deliver: DeliverFn,
        on_fault: FaultFn,
    ) -> Result<Self, StartError> {
        let (tx_ctl_id, tx_bulk_id, rx_ctl_id, rx_bulk_id) = match region.side() {
            Side::Host => (LANE_CTL_H2G, LANE_BULK_H2G, LANE_CTL_G2H, LANE_BULK_G2H),
            Side::Guest => (LANE_CTL_G2H, LANE_BULK_G2H, LANE_CTL_H2G, LANE_BULK_H2G),
        };
        for want in lane_specs() {
            let ok = region.lanes().iter().any(|l| {
                l.id == want.id
                    && l.direction == want.direction
                    && l.priority == want.priority
                    && l.size >= 4096
            });
            if !ok {
                return Err(StartError(format!("region has no usable lane {}", want.id)));
            }
        }
        let e = |what: &str, err: winbridge_shm::RegionError| StartError(format!("{what}: {err}"));
        let mut tx_ctl = region.producer(tx_ctl_id).map_err(|x| e("control producer", x))?;
        let mut tx_bulk = region.producer(tx_bulk_id).map_err(|x| e("bulk producer", x))?;
        let mut rx_ctl = region.consumer(rx_ctl_id).map_err(|x| e("control consumer", x))?;
        let mut rx_bulk = region.consumer(rx_bulk_id).map_err(|x| e("bulk consumer", x))?;
        tx_ctl.set_max_record(CTL_MAX_RECORD);
        tx_bulk.set_max_record(BULK_MAX_RECORD);
        rx_ctl.set_max_record(CTL_MAX_RECORD);
        rx_bulk.set_max_record(BULK_MAX_RECORD);

        let shared = Arc::new(Shared {
            cfg,
            out: Mutex::new(Out::default()),
            out_cv: Condvar::new(),
            space_cv: Condvar::new(),
            tx_stop: AtomicBool::new(false),
            up: AtomicBool::new(true),
            rx_deadline: Mutex::new(None),
            credit_in: AtomicU64::new(0),
            credit_out: AtomicU32::new(0),
            wakers: [Waker::new(), Waker::new()],
            bad: AtomicU32::new(0),
            live: AtomicU32::new(3),
            peer_closed: AtomicU32::new(0),
            on_fault: Mutex::new(Some(on_fault)),
            deliver,
            kick,
            counters: Counters::default(),
            tx_ctl_cap: tx_ctl.max_record(),
            tx_bulk_cap: tx_bulk.max_record(),
            tx_bulk_id,
            rx_bulk_id,
        });
        let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| {
            std::thread::Builder::new().name(name.into()).spawn(f)
        };
        let s = shared.clone();
        let r1 = spawn("wb-ring-tx", Box::new(move || writer_main(s, tx_ctl, tx_bulk)));
        let s = shared.clone();
        let r2 = spawn("wb-ring-rx-ctl", Box::new(move || reader_main(s, rx_ctl, Lane::Control)));
        let s = shared.clone();
        let r3 = spawn("wb-ring-rx-bulk", Box::new(move || reader_main(s, rx_bulk, Lane::Bulk)));
        if let (Ok(_), Ok(_), Ok(_)) = (&r1, &r2, &r3) {
            return Ok(Self { shared });
        }
        // A thread could not start: stop the ones that did. The unstarted closures were
        // dropped, so account for them.
        let started = [&r1, &r2, &r3].iter().filter(|r| r.is_ok()).count() as u32;
        shared.live.fetch_sub(3 - started, Ordering::AcqRel);
        shared.end(Duration::ZERO);
        Err(StartError("cannot start the ring threads".to_string()))
    }

    /// Queues `msg` for the lane its class selects; blocks while that queue is full
    /// (back-pressure). On error the message comes back (or cannot be encoded at all).
    pub fn send(&self, msg: Message) -> Result<(), SendError> {
        self.shared.enqueue(msg, true)
    }

    /// Like [`RingLink::send`] but returns [`SendError::Full`] instead of blocking.
    pub fn try_send(&self, msg: Message) -> Result<(), SendError> {
        self.shared.enqueue(msg, false)
    }

    /// The peer's `ShmKick` arrived over serial: wake the sleeping readers.
    pub fn wake(&self) {
        for w in &self.shared.wakers {
            w.wake();
        }
    }

    /// False once the link has ended (fault or [`RingLink::close`]).
    pub fn is_up(&self) -> bool {
        self.shared.up.load(Ordering::Acquire)
    }

    /// Ends the link on purpose. The writer stops and marks its lanes closed; the readers keep
    /// draining for up to `drain` so records the peer already wrote are still delivered. Returns
    /// the messages that were queued but never written (send them over serial). `on_fault` is
    /// not called.
    pub fn close(&self, drain: Duration) -> Vec<Message> {
        lock(&self.shared.on_fault).take();
        self.shared.end(drain).unwrap_or_default()
    }

    /// Ends the link as a fault (for example the peer announced a fallback on serial and the
    /// owner wants the unified path); calls `on_fault` once.
    pub fn fail(&self, reason: &str) {
        self.shared.fault(reason.to_string());
    }

    /// Waits until all three threads exited (after the link ended). False on timeout, for
    /// example a `deliver` callback that never returns.
    pub fn wait_stopped(&self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while self.shared.live.load(Ordering::Acquire) != 0 {
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }

    /// Counters.
    pub fn stats(&self) -> Stats {
        let c = &self.shared.counters;
        Stats {
            tx_control: c.tx_control.load(Ordering::Relaxed),
            tx_bulk: c.tx_bulk.load(Ordering::Relaxed),
            rx_control: c.rx_control.load(Ordering::Relaxed),
            rx_bulk: c.rx_bulk.load(Ordering::Relaxed),
            kicks: c.kicks.load(Ordering::Relaxed),
            bad_records: self.shared.bad.load(Ordering::Relaxed),
        }
    }
}

impl Drop for RingLink {
    fn drop(&mut self) {
        self.close(Duration::ZERO);
    }
}

fn writer_main<M: Mapping>(sh: Arc<Shared>, mut ctl: Producer<M>, mut bulk: Producer<M>) {
    let _live = LiveGuard(sh.clone());
    let mut sched = Scheduler::new(sh.cfg.sched);
    let ci = sched.add_lane(Class::Control);
    let bi = sched.add_lane(Class::Bulk);
    let epoch = Instant::now();
    let window = sh.cfg.sched.credit_window;
    // Per lane (0 control, 1 bulk): retry pause after a full ring, and since when it is full.
    let mut blocked_until: [Option<Instant>; 2] = [None; 2];
    let mut full_since: [Option<Instant>; 2] = [None; 2];
    let mut credit_wait_since: Option<Instant> = None;

    'main: loop {
        if sh.tx_stop.load(Ordering::Acquire) {
            break;
        }
        let now = Instant::now();

        let granted = sh.credit_in.swap(0, Ordering::AcqRel);
        if granted > 0 {
            let blocks = u32::try_from(granted).unwrap_or(u32::MAX);
            if sched.return_credit(bi, blocks).is_err() {
                sh.fault("peer returned more bulk credit than was outstanding".to_string());
                break;
            }
            credit_wait_since = None;
        }
        let owed = sh.credit_out.swap(0, Ordering::AcqRel);
        if owed > 0 {
            match ctl.try_push_tagged(TAG_CREDIT, 0, &encode_credit(sh.rx_bulk_id, owed)) {
                Ok(ok) => {
                    if ok.kick {
                        sh.kick();
                    }
                }
                Err(PushError::Full) => {
                    sh.credit_out.fetch_add(owed, Ordering::AcqRel);
                }
                Err(e) => {
                    sh.fault(format!("control lane: {e}"));
                    break;
                }
            }
        }

        let (ctl_pending, bulk_pending) = {
            let o = sh.lock_out();
            (!o.ctl.is_empty(), !o.bulk.is_empty())
        };
        let ready = |i: usize, pending: bool| {
            pending && blocked_until[i].map_or(true, |t| now >= t)
        };
        let (ctl_ok, bulk_ok) = (ready(0, ctl_pending), ready(1, bulk_pending));
        if bulk_ok && sched.in_flight(bi) >= window {
            let since = *credit_wait_since.get_or_insert(now);
            if now.duration_since(since) > sh.cfg.stall_limit {
                sh.fault(format!(
                    "bulk credit not returned for {} s",
                    sh.cfg.stall_limit.as_secs()
                ));
                break;
            }
        } else {
            credit_wait_since = None;
        }

        let pick = sched.next(epoch.elapsed(), |i| if i == ci { ctl_ok } else { bulk_ok });
        match pick {
            Some(p) if p.lane == ci => {
                let Some(frame) = sh.lock_out().ctl.pop_front() else { continue };
                match ctl.try_push_tagged(TAG_MESSAGE, 0, &frame) {
                    Ok(ok) => {
                        full_since[0] = None;
                        blocked_until[0] = None;
                        sh.counters.tx_control.fetch_add(1, Ordering::Relaxed);
                        if ok.kick {
                            sh.kick();
                        }
                        sh.space_cv.notify_all();
                    }
                    Err(PushError::Full) => {
                        sh.requeue_front(false, frame);
                        blocked_until[0] = Some(now + sh.cfg.full_retry);
                        full_since[0].get_or_insert(now);
                    }
                    Err(PushError::TooLarge | PushError::BadFlags) => {
                        warn!("control record of {} bytes rejected by the lane; dropped", frame.len());
                    }
                    Err(e) => {
                        sh.requeue_front(false, frame);
                        sh.fault(format!("control lane: {e}"));
                        break 'main;
                    }
                }
            }
            Some(p) => {
                let mut sent = 0usize;
                let mut ended = false;
                let mut any = false;
                loop {
                    let (frame, next_len) = {
                        let mut o = sh.lock_out();
                        let Some(f) = o.bulk.pop_front() else { break };
                        o.bulk_bytes -= f.len();
                        let nl = o.bulk.front().map(Vec::len);
                        (f, nl)
                    };
                    let end_now = next_len.map_or(true, |nl| sent + frame.len() + nl > p.max_len);
                    let flags = if end_now { FLAG_BLOCK_END } else { 0 };
                    match bulk.try_push_tagged(TAG_MESSAGE, flags, &frame) {
                        Ok(ok) => {
                            any = true;
                            sent += frame.len();
                            sh.counters.tx_bulk.fetch_add(1, Ordering::Relaxed);
                            if ok.kick {
                                sh.kick();
                            }
                            if end_now {
                                ended = true;
                                break;
                            }
                        }
                        Err(PushError::Full) => {
                            sh.requeue_front(true, frame);
                            blocked_until[1] = Some(now + sh.cfg.full_retry);
                            full_since[1].get_or_insert(now);
                            break;
                        }
                        Err(PushError::TooLarge | PushError::BadFlags) => {
                            warn!("bulk record of {} bytes rejected by the lane; dropped", frame.len());
                            if end_now {
                                // The block's end marker went with it.
                                ended = false;
                                break;
                            }
                        }
                        Err(e) => {
                            sh.requeue_front(true, frame);
                            sh.fault(format!("bulk lane: {e}"));
                            break 'main;
                        }
                    }
                }
                if !ended {
                    // A block that did not complete does not hold a credit; the next pick
                    // that finishes it takes one.
                    let _ = sched.return_credit(bi, 1);
                }
                if any {
                    full_since[1] = None;
                    blocked_until[1] = None;
                }
                sh.space_cv.notify_all();
            }
            None => {
                let o = sh.lock_out();
                let blocked = |i: usize| blocked_until[i].is_some_and(|t| Instant::now() < t);
                let has_ctl = !o.ctl.is_empty() && !blocked(0);
                let has_bulk = !o.bulk.is_empty() && !blocked(1) && sched.in_flight(bi) < window;
                if !has_ctl
                    && !has_bulk
                    && sh.credit_in.load(Ordering::Acquire) == 0
                    && sh.credit_out.load(Ordering::Acquire) == 0
                    && !sh.tx_stop.load(Ordering::Acquire)
                {
                    let mut wait = Duration::from_millis(50);
                    for t in blocked_until.iter().flatten() {
                        wait = wait.min(t.saturating_duration_since(Instant::now()));
                    }
                    let _ = sh.out_cv.wait_timeout(o, wait.max(Duration::from_micros(20)));
                }
            }
        }

        for (i, since) in full_since.iter().enumerate() {
            if since.is_some_and(|t| now.duration_since(t) > sh.cfg.stall_limit) {
                let lane = if i == 0 { "control" } else { "bulk" };
                sh.fault(format!(
                    "{lane} lane full for {} s: the peer is not reading",
                    sh.cfg.stall_limit.as_secs()
                ));
                break 'main;
            }
        }
    }
    // Tell the peer's consumers that no more records follow.
    ctl.close();
    bulk.close();
    debug!("ring writer stopped");
}

fn reader_main<M: Mapping>(sh: Arc<Shared>, mut cons: Consumer<M>, lane: Lane) {
    let _live = LiveGuard(sh.clone());
    let waker = &sh.wakers[(lane == Lane::Bulk) as usize];
    let mut idle = Idle::new(sh.cfg.idle);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if sh.rx_should_stop() {
            break;
        }
        match cons.pop_wait(&mut buf, &mut idle, waker, Some(Duration::from_millis(20))) {
            Ok(Some(meta)) => {
                if meta.len != buf.len() || !sh.handle_record(lane, meta.tag, meta.flags, &buf) {
                    if meta.len != buf.len() {
                        sh.fault("record length mismatch".to_string());
                    }
                    break;
                }
                if buf.capacity() > (4 << 20) {
                    buf = Vec::new();
                }
            }
            Ok(None) => {}
            Err(LaneError::Closed) => {
                // The peer closes both lanes; the link ends once both were drained.
                if sh.peer_closed.fetch_add(1, Ordering::AcqRel) + 1 >= 2 {
                    sh.fault("the peer closed both lanes".to_string());
                }
                break;
            }
            Err(e) => {
                sh.fault(format!("{lane:?} lane: {e}"));
                break;
            }
        }
    }
    debug!("ring reader {lane:?} stopped");
}

/// Coalescing kick sender: [`Kick::kick`] only sets a flag, a small thread calls `send`
/// (typically: queue `Message::ShmKick` on the serial channel). The caller (a ring writer
/// thread) therefore never waits for the serial channel.
pub struct KickPump {
    state: Arc<(Mutex<(bool, bool)>, Condvar)>,
}

impl KickPump {
    /// Starts the pump thread.
    pub fn new(send: impl Fn() + Send + 'static) -> Arc<Self> {
        let state = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let st = state.clone();
        let spawned = std::thread::Builder::new().name("wb-ring-kick".into()).spawn(move || loop {
            {
                let mut g = lock(&st.0);
                while !g.0 && !g.1 {
                    g = st.1.wait(g).unwrap_or_else(PoisonError::into_inner);
                }
                if g.1 {
                    return;
                }
                g.0 = false;
            }
            send();
        });
        if let Err(e) = spawned {
            warn!("cannot start the kick thread: {e}");
        }
        Arc::new(Self { state })
    }
}

impl Kick for KickPump {
    fn kick(&self) {
        lock(&self.state.0).0 = true;
        self.state.1.notify_one();
    }
}

impl Drop for KickPump {
    fn drop(&mut self) {
        lock(&self.state.0).1 = true;
        self.state.1.notify_one();
    }
}

/// Where an inbox message came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The serial channel (kept in order as one queue, exactly like the serial-only path).
    Serial,
    /// The control lane.
    Control,
    /// The bulk lane.
    Bulk,
}

impl From<Lane> for Source {
    fn from(l: Lane) -> Self {
        match l {
            Lane::Control => Source::Control,
            Lane::Bulk => Source::Bulk,
        }
    }
}

/// Result of [`Inbox::pop`].
#[derive(Debug)]
pub enum Pop {
    /// The next message.
    Msg(Message),
    /// Nothing arrived within the timeout.
    Timeout,
    /// The inbox was closed and is empty.
    Closed,
}

struct InboxState {
    q: [VecDeque<Message>; 3],
    closed: bool,
    hold_serial: bool,
    streak: u32,
}

/// Bounded dispatch queue with one queue per [`Source`]. A full queue blocks only its own
/// producer, so a slow consumer of bulk data back-pressures the bulk lane and never the serial
/// reader or the control lane. [`Inbox::pop`] serves control first, then serial, then bulk (a
/// bulk message is served after every 32 others so bulk cannot starve).
pub struct Inbox {
    st: Mutex<InboxState>,
    item_cv: Condvar,
    space_cv: Condvar,
    caps: [usize; 3],
}

impl Inbox {
    /// New inbox; the capacities are in messages per source (at least 1).
    pub fn new(serial_cap: usize, control_cap: usize, bulk_cap: usize) -> Arc<Self> {
        Arc::new(Self {
            st: Mutex::new(InboxState {
                q: Default::default(),
                closed: false,
                hold_serial: false,
                streak: 0,
            }),
            item_cv: Condvar::new(),
            space_cv: Condvar::new(),
            caps: [serial_cap.max(1), control_cap.max(1), bulk_cap.max(1)],
        })
    }

    fn slot(s: Source) -> usize {
        match s {
            Source::Serial => 0,
            Source::Control => 1,
            Source::Bulk => 2,
        }
    }

    /// Queues `msg`, blocking while that source's queue is full. False if the inbox is closed.
    pub fn push(&self, src: Source, msg: Message) -> bool {
        let i = Self::slot(src);
        let mut st = lock(&self.st);
        while st.q[i].len() >= self.caps[i] && !st.closed {
            st = self.space_cv.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
        if st.closed {
            return false;
        }
        st.q[i].push_back(msg);
        drop(st);
        self.item_cv.notify_one();
        true
    }

    /// Queues `msg` without blocking; the message comes back when the queue is full or closed.
    pub fn try_push(&self, src: Source, msg: Message) -> Result<(), Message> {
        let i = Self::slot(src);
        let mut st = lock(&self.st);
        if st.closed || st.q[i].len() >= self.caps[i] {
            return Err(msg);
        }
        st.q[i].push_back(msg);
        drop(st);
        self.item_cv.notify_one();
        Ok(())
    }

    /// While held, serial messages stay queued and the lanes are served first. Used while a
    /// fallback drains the rings, so records written before the fallback are dispatched before
    /// anything the peer sent over serial after it.
    pub fn set_serial_hold(&self, hold: bool) {
        lock(&self.st).hold_serial = hold;
        self.item_cv.notify_all();
    }

    /// Next message by priority, waiting up to `timeout`.
    pub fn pop(&self, timeout: Duration) -> Pop {
        let end = Instant::now() + timeout;
        let mut st = lock(&self.st);
        loop {
            let order: [usize; 3] = if st.streak >= 32 && !st.q[2].is_empty() {
                [2, 1, 0]
            } else {
                [1, 0, 2]
            };
            for i in order {
                if i == 0 && st.hold_serial {
                    continue;
                }
                if let Some(m) = st.q[i].pop_front() {
                    st.streak = if i == 2 { 0 } else { st.streak.saturating_add(1) };
                    drop(st);
                    self.space_cv.notify_all();
                    return Pop::Msg(m);
                }
            }
            if st.closed {
                return Pop::Closed;
            }
            let now = Instant::now();
            if now >= end {
                return Pop::Timeout;
            }
            st = self
                .item_cv
                .wait_timeout(st, end - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Closes the inbox: pushes fail, blocked producers return, `pop` drains then reports closed.
    pub fn close(&self) {
        lock(&self.st).closed = true;
        self.item_cv.notify_all();
        self.space_cv.notify_all();
    }
}

/// Sends one message over the serial channel. May block (the channel is flow-controlled); it is
/// never called with a lock a lane thread needs, except while a fallback resends the leftovers.
pub type SerialSend = Arc<dyn Fn(Message) + Send + Sync>;

/// Where [`Endpoint::route`] put a message.
#[derive(Debug)]
pub enum Routed {
    /// Queued on a lane.
    Sent,
    /// The caller must send it over the serial channel itself (no link, or it cannot use a lane).
    Serial(Message),
}

/// How long the readers keep draining the rings after a fallback.
pub const DRAIN_WINDOW: Duration = Duration::from_secs(2);

/// See [`Endpoint::set_fast_path`].
pub type FastPath = Arc<dyn Fn(&Message) -> bool + Send + Sync>;

/// One end of the control channel: the serial path plus an optional ring link. The owner (host
/// serial client, agent) calls [`Endpoint::route`] for every outgoing message and
/// [`Endpoint::on_serial`] for every message read from the serial channel.
///
/// Fallback rules: when the link ends on its own (corruption, stalled peer, peer closed it) the
/// endpoint sends `ShmFallback` and then the messages that were queued but not written over the
/// serial channel, and routes everything over serial from then on. When the peer's `ShmFallback`
/// arrives, the endpoint closes its writer, keeps draining the rings for [`DRAIN_WINDOW`] and
/// holds back serial messages in the [`Inbox`] until the drain is over, so messages written
/// before the fallback are dispatched before the ones sent after it.
pub struct Endpoint {
    serial: SerialSend,
    inbox: Arc<Inbox>,
    cfg: LinkConfig,
    slot: std::sync::RwLock<Option<Arc<RingLink>>>,
    fast: std::sync::RwLock<Option<FastPath>>,
}

impl Endpoint {
    /// New endpoint on the serial path only.
    pub fn new(serial: SerialSend, inbox: Arc<Inbox>, cfg: LinkConfig) -> Arc<Self> {
        Arc::new(Self {
            serial,
            inbox,
            cfg,
            slot: std::sync::RwLock::new(None),
            fast: std::sync::RwLock::new(None),
        })
    }

    /// The dispatch queue messages from the serial channel and from the lanes arrive in.
    pub fn inbox(&self) -> &Arc<Inbox> {
        &self.inbox
    }

    /// Registers a handler that sees every message arriving on the **control lane** on the lane's
    /// reader thread, before it is queued. Returning true means "handled, do not queue". For
    /// answers that are cheap and must not wait for a busy dispatcher (`Ping` -> `Pong`); it must
    /// never block for long. Messages from the serial channel are not offered to it.
    pub fn set_fast_path(&self, f: FastPath) {
        *self.fast.write().unwrap_or_else(PoisonError::into_inner) = Some(f);
    }

    fn fast_path(&self, msg: &Message) -> bool {
        let f = self.fast.read().unwrap_or_else(PoisonError::into_inner).clone();
        f.is_some_and(|f| f(msg))
    }

    /// Sends a message over the serial channel (negotiation messages, kicks).
    pub fn send_serial(&self, msg: Message) {
        (self.serial)(msg);
    }

    fn read_slot(&self) -> std::sync::RwLockReadGuard<'_, Option<Arc<RingLink>>> {
        self.slot.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// True while a link is up.
    pub fn is_active(&self) -> bool {
        self.read_slot().as_ref().is_some_and(|l| l.is_up())
    }

    /// Counters of the current link.
    pub fn stats(&self) -> Option<Stats> {
        self.read_slot().as_ref().map(|l| l.stats())
    }

    /// Chooses the path of an outgoing message. Blocks while the chosen lane's queue is full.
    pub fn route(&self, msg: Message) -> Routed {
        let slot = self.read_slot();
        let Some(link) = slot.as_ref() else {
            return Routed::Serial(msg);
        };
        match link.send(msg) {
            Ok(()) => Routed::Sent,
            Err(SendError::Closed(m) | SendError::Unsuitable(m) | SendError::Full(m)) => Routed::Serial(m),
            Err(SendError::Encode(e)) => {
                warn!("message cannot be encoded: {e}");
                Routed::Sent
            }
        }
    }

    /// Like [`Endpoint::route`] but returns the message when the lane queue is full or a
    /// fallback is in progress; the caller retries later.
    pub fn try_route(&self, msg: Message) -> Result<Routed, Message> {
        let slot = match self.slot.try_read() {
            Ok(g) => g,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Err(msg),
        };
        let Some(link) = slot.as_ref() else {
            return Ok(Routed::Serial(msg));
        };
        match link.try_send(msg) {
            Ok(()) => Ok(Routed::Sent),
            Err(SendError::Full(m)) => Err(m),
            Err(SendError::Closed(m) | SendError::Unsuitable(m)) => Ok(Routed::Serial(m)),
            Err(SendError::Encode(e)) => {
                warn!("message cannot be encoded: {e}");
                Ok(Routed::Sent)
            }
        }
    }

    /// Starts the ring link on `region` and routes through it from now on. The caller has
    /// agreed on the switch with the peer (`ShmOffer` / `ShmReady`).
    pub fn activate<M: Mapping>(self: &Arc<Self>, region: &Region<M>) -> Result<(), StartError> {
        let mut slot = self.slot.write().unwrap_or_else(PoisonError::into_inner);
        if slot.as_ref().is_some_and(|l| l.is_up()) {
            return Err(StartError("a link is already active".to_string()));
        }
        let serial = self.serial.clone();
        let kick = KickPump::new(move || serial(Message::ShmKick));
        let inbox = self.inbox.clone();
        let weak = Arc::downgrade(self);
        let deliver: DeliverFn = Arc::new(move |lane, msg| {
            if lane == Lane::Control && weak.upgrade().is_some_and(|ep| ep.fast_path(&msg)) {
                return;
            }
            inbox.push(lane.into(), msg);
        });
        let me = Arc::downgrade(self);
        let on_fault: FaultFn = Box::new(move |fault| {
            if let Some(ep) = me.upgrade() {
                ep.fell_back(fault);
            }
        });
        *slot = Some(Arc::new(RingLink::start(region, self.cfg, kick, deliver, on_fault)?));
        Ok(())
    }

    /// The link ended by itself: tell the peer, resend the leftovers over serial, go serial.
    fn fell_back(&self, fault: Fault) {
        let mut slot = self.slot.write().unwrap_or_else(PoisonError::into_inner);
        (self.serial)(Message::ShmFallback { reason: fault.reason });
        for m in fault.unsent {
            (self.serial)(m);
        }
        *slot = None;
    }

    /// Returns to the serial path on purpose (serial reconnect, agent restart, shutdown): sends
    /// `ShmFallback` and the unsent messages, and closes the link. No-op without a link.
    pub fn leave(&self, reason: &str) {
        let mut slot = self.slot.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(link) = slot.take() {
            let unsent = link.close(Duration::ZERO);
            (self.serial)(Message::ShmFallback { reason: reason.to_string() });
            for m in unsent {
                (self.serial)(m);
            }
        }
    }

    /// Waits until the threads of the last link exited (so its mapping can be released).
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let link = self.read_slot().clone();
        link.map_or(true, |l| l.wait_stopped(timeout))
    }

    /// Handles one message read from the serial channel. `ShmKick` wakes the readers and
    /// `ShmFallback` runs the peer-fallback sequence; both return `None`. `ShmOffer` and
    /// `ShmReady` are returned for the owner's negotiation. Every other message goes into the
    /// inbox (blocking while that queue is full), in order.
    pub fn on_serial(&self, msg: Message) -> Option<Message> {
        match msg {
            Message::ShmKick => {
                if let Some(link) = self.read_slot().as_ref() {
                    link.wake();
                }
                None
            }
            Message::ShmFallback { reason } => {
                self.peer_fallback(&reason);
                None
            }
            m @ (Message::ShmOffer { .. } | Message::ShmReady { .. }) => Some(m),
            m => {
                self.inbox.push(Source::Serial, m);
                None
            }
        }
    }

    /// Like [`Endpoint::on_serial`] for the plain case; returns the message if the serial queue
    /// is full so an async caller can retry on a blocking thread.
    pub fn try_on_serial(&self, msg: Message) -> Result<Option<Message>, Message> {
        match msg {
            Message::ShmKick | Message::ShmFallback { .. } | Message::ShmOffer { .. } | Message::ShmReady { .. } => {
                Ok(self.on_serial(msg))
            }
            m => self.inbox.try_push(Source::Serial, m).map(|()| None),
        }
    }

    fn peer_fallback(&self, reason: &str) {
        let mut slot = self.slot.write().unwrap_or_else(PoisonError::into_inner);
        let Some(link) = slot.take() else {
            debug!("peer fallback ({reason}) without a link");
            return;
        };
        warn!("the peer left shared memory: {reason}");
        self.inbox.set_serial_hold(true);
        let unsent = link.close(DRAIN_WINDOW);
        for m in unsent {
            (self.serial)(m);
        }
        drop(slot);
        let inbox = self.inbox.clone();
        let spawned = std::thread::Builder::new().name("wb-ring-drain".into()).spawn({
            let link = link.clone();
            move || {
                link.wait_stopped(DRAIN_WINDOW + Duration::from_secs(1));
                inbox.set_serial_hold(false);
            }
        });
        if spawned.is_err() {
            self.inbox.set_serial_hold(false);
        }
    }
}

/// Agent side of the negotiation: maps the region the host offered (`map` receives its length
/// and returns the guest mapping), attaches, starts the link and returns the `ShmReady` reply.
/// The caller sends the reply over serial. Any previous link of this endpoint is left first.
/// Runs on its own thread: mapping the device can take a while and must never delay the
/// serial reader.
pub fn answer_offer<M: Mapping>(
    ep: &Arc<Endpoint>,
    shm_version: u32,
    region_len: u64,
    map: impl FnOnce(u64) -> Result<M, String>,
) -> Message {
    let refuse = |reason: String| {
        warn!("shared memory offer refused: {reason}");
        Message::ShmReady { ok: false, reason }
    };
    if shm_version != winbridge_shm::VERSION {
        return refuse(format!(
            "region layout version {shm_version} not supported (have {})",
            winbridge_shm::VERSION
        ));
    }
    ep.leave("new offer");
    if !ep.wait_idle(Duration::from_secs(3)) {
        return refuse("the previous link did not stop".to_string());
    }
    let bar = match map(region_len) {
        Ok(b) => b,
        Err(e) => return refuse(e),
    };
    let region = match Region::attach(bar, Side::Guest) {
        Ok(r) => r,
        Err(e) => return refuse(format!("region header rejected: {e}")),
    };
    match ep.activate(&region) {
        Ok(()) => Message::ShmReady {
            ok: true,
            reason: String::new(),
        },
        Err(e) => refuse(e.to_string()),
    }
}
