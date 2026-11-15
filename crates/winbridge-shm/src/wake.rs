// SPDX-License-Identifier: GPL-3.0-or-later

//! Wake-up: adaptive polling plus an out-of-band kick.
//!
//! Flag protocol (per ring, no lost wake-up):
//!
//! 1. The consumer finds the ring empty, spins for a bounded time, then calls
//!    `set_sleeping(true)` (SeqCst store + fence), **re-checks** the ring with
//!    `poll_ready()`, and only if it is still empty waits on its [`Waker`]
//!    with a timeout (about 1 ms). After waking it clears the flag.
//! 2. The producer publishes the new head (release), executes a SeqCst fence,
//!    then loads the flag. If `sleeping` is set and the consumer had drained
//!    the ring before this push ([`PushOk::kick`](crate::PushOk)) it calls
//!    [`Kick::kick`], which the transport implements by sending one byte over
//!    virtio-serial; the receiving side calls [`Waker::wake`].
//!
//! The two SeqCst fences make it impossible for the consumer to miss the new
//! head *and* the producer to miss the flag (store-buffering/Dekker pattern).
//! The timed wait is a safety net for a kick that is lost or coalesced on the
//! other channel, so a missing kick costs at most one sleep interval.
//! A busy ring never sleeps, so it never pays for a kick.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::mapping::Mapping;
use crate::ring::{Consumer, LaneError, RecordMeta};

/// Implemented by a transport to wake a sleeping peer over another channel
/// (one byte over virtio-serial in production).
pub trait Kick {
    /// Send the wake-up. Must not block for long and must not fail loudly: a
    /// lost kick is covered by the consumer's timed sleep.
    fn kick(&self);
}

/// Receiver-side wake-up primitive: a token plus a condvar, so a `wake()`
/// that arrives just before `wait_timeout()` is not lost.
#[derive(Default)]
pub struct Waker {
    token: Mutex<bool>,
    cv: Condvar,
}

impl Waker {
    /// New waker without a pending token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Wake the sleeping consumer (or make its next wait return immediately).
    pub fn wake(&self) {
        let mut t = self.token.lock().unwrap_or_else(PoisonError::into_inner);
        *t = true;
        self.cv.notify_one();
    }

    /// Wait until woken or `timeout` elapses. Returns true if woken.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let mut t = self.token.lock().unwrap_or_else(PoisonError::into_inner);
        if !*t {
            let (guard, _) = self
                .cv
                .wait_timeout_while(t, timeout, |t| !*t)
                .unwrap_or_else(PoisonError::into_inner);
            t = guard;
        }
        std::mem::take(&mut *t)
    }
}

/// Polling policy.
#[derive(Clone, Copy, Debug)]
pub struct IdleConfig {
    /// Empty polls before switching from spinning to timed sleeping.
    pub spin_polls: u32,
    /// Sleep interval once spinning gave up.
    pub sleep: Duration,
}

impl Default for IdleConfig {
    fn default() -> Self {
        Self {
            spin_polls: 2000,
            sleep: Duration::from_millis(1),
        }
    }
}

/// Adaptive idle state: bounded spin after traffic, then timed sleep.
#[derive(Clone, Copy, Debug)]
pub struct Idle {
    cfg: IdleConfig,
    polls: u32,
}

impl Idle {
    /// New idle state in the spinning phase.
    pub fn new(cfg: IdleConfig) -> Self {
        Self { cfg, polls: 0 }
    }

    /// Traffic seen: go back to spinning.
    pub fn reset(&mut self) {
        self.polls = 0;
    }

    /// True while the spin budget is not exhausted.
    pub fn spinning(&self) -> bool {
        self.polls < self.cfg.spin_polls
    }

    /// One empty poll: burn a spin iteration.
    pub fn spin(&mut self) {
        self.polls = self.polls.saturating_add(1);
        std::hint::spin_loop();
    }

    /// The configured sleep interval.
    pub fn sleep_interval(&self) -> Duration {
        self.cfg.sleep
    }
}

impl Default for Idle {
    fn default() -> Self {
        Self::new(IdleConfig::default())
    }
}

impl<M: Mapping> Consumer<M> {
    /// Pop the next record, waiting (spin, then flag + timed sleep on `waker`)
    /// until one arrives or `timeout` elapses (`None` waits forever).
    /// `Ok(None)` means timeout.
    pub fn pop_wait(
        &mut self,
        out: &mut Vec<u8>,
        idle: &mut Idle,
        waker: &Waker,
        timeout: Option<Duration>,
    ) -> Result<Option<RecordMeta>, LaneError> {
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            if let Some(m) = self.try_pop(out)? {
                idle.reset();
                return Ok(Some(m));
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(None);
            }
            if idle.spinning() {
                idle.spin();
                continue;
            }
            self.set_sleeping(true);
            match self.poll_ready() {
                Ok(false) => {
                    let mut nap = idle.sleep_interval();
                    if let Some(d) = deadline {
                        nap = nap.min(d.saturating_duration_since(Instant::now()));
                    }
                    waker.wait_timeout(nap);
                    self.set_sleeping(false);
                }
                Ok(true) => self.set_sleeping(false),
                Err(e) => {
                    self.set_sleeping(false);
                    return Err(e);
                }
            }
        }
    }
}
