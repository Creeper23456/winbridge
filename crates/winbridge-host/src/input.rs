// SPDX-License-Identifier: GPL-3.0-or-later

//! The ordered input pipeline: every pointer and keyboard event for the guest goes through one
//! queue that one task per D-Bus connection drains, so QEMU sees the events in the order the
//! user produced them.
//!
//! Before this existed each event was sent by its own `tokio::spawn`. Tasks on the multi-threaded
//! runtime are not ordered, and QEMU's button events carry no position of their own, so a click
//! could be processed before the pointer move that preceded it and land where the pointer had
//! been a moment earlier.

use crate::dbus_display::{KeyboardProxy, MouseProxy};
use crate::stats;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tracing::warn;

/// One event for the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOp {
    /// Absolute pointer position in guest pixels. Consecutive ones collapse into the last.
    Abs(u32, u32),
    /// Absolute pointer position that is never merged or dropped (the drag probe's nudge).
    AbsExact(u32, u32),
    /// Relative pointer motion in guest pixels. Consecutive ones add up.
    Rel(i32, i32),
    /// QEMU button number (0 left, 1 middle, 2 right, 3/4 wheel up/down, 5/6 side, 7/8 wheel left/right).
    Button { button: u32, down: bool },
    /// QEMU key number.
    Key { code: u32, down: bool },
}

/// Value to send in `SetAbsPosition` so the guest cursor lands on guest pixel `px` of an axis
/// `extent` pixels wide. Inferred (not read from QEMU source), then measured on the real VM:
/// QEMU maps the position into the tablet's 0..0x7fff range with `px * 0x7fff / extent` and the
/// guest's tablet driver floors when scaling back, so a pixel reads one lower than sent. Sending
/// `px + 1` compensates. QEMU rejects `px >= extent` ("Invalid mouse position"), so the value is
/// capped at `extent - 1`: pixels `0..=extent-2` are exact, and the last pixel (`extent - 1`)
/// reads `extent - 2`. The last column and row are unreachable through QEMU's absolute pointer.
pub fn abs_wire_value(px: u32, extent: u32) -> u32 {
    (px + 1).min(extent.saturating_sub(1))
}

/// Pending events above this count trigger dropping of superseded motion events.
const BACKLOG_LIMIT: usize = 1024;

/// FIFO of pending events. Only motion is ever merged or dropped: buttons and keys always
/// survive, in order.
#[derive(Default)]
pub struct InputQueue {
    ops: VecDeque<InputOp>,
}

impl InputQueue {
    pub fn push(&mut self, op: InputOp) {
        match (self.ops.back_mut(), op) {
            (Some(InputOp::Abs(x, y)), InputOp::Abs(nx, ny)) => {
                (*x, *y) = (nx, ny);
                return;
            }
            (Some(InputOp::Rel(dx, dy)), InputOp::Rel(ndx, ndy)) => {
                *dx = dx.saturating_add(ndx);
                *dy = dy.saturating_add(ndy);
                return;
            }
            _ => {}
        }
        self.ops.push_back(op);
        if self.ops.len() > BACKLOG_LIMIT {
            self.drop_stale_motion();
        }
    }

    pub fn pop(&mut self) -> Option<InputOp> {
        self.ops.pop_front()
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Backed up: drop absolute positions that a later event supersedes. A position stays when a
    /// button follows it directly (that is where the click must land) or when it is the newest
    /// event. Relative deltas are summed, never dropped.
    fn drop_stale_motion(&mut self) {
        let old = std::mem::take(&mut self.ops);
        let last = old.len().saturating_sub(1);
        let mut keep = VecDeque::with_capacity(old.len());
        for (i, op) in old.iter().enumerate() {
            let stale = matches!(op, InputOp::Abs(..))
                && i != last
                && !matches!(old[i + 1], InputOp::Button { .. });
            if !stale {
                keep.push_back(*op);
            }
        }
        self.ops = keep;
    }
}

struct Shared {
    queue: Mutex<InputQueue>,
    wake: Notify,
    /// QEMU refused a `RelMotion` (the guest has no relative pointing device, or reports an
    /// absolute one).
    rel_rejected: AtomicBool,
    warned: [AtomicBool; 4],
}

/// Handle to the ordered input pipeline of one D-Bus connection.
#[derive(Clone)]
pub struct InputPipeline {
    shared: Arc<Shared>,
}

/// D-Bus calls normally return in well under a millisecond; this only keeps one wedged call from
/// stalling every later event.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

impl InputPipeline {
    /// Spawns the draining task. Must run inside the Tokio runtime.
    pub fn spawn(mouse: MouseProxy<'static>, keyboard: KeyboardProxy<'static>) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(InputQueue::default()),
            wake: Notify::new(),
            rel_rejected: AtomicBool::new(false),
            warned: Default::default(),
        });
        let task_shared = shared.clone();
        tokio::spawn(async move {
            loop {
                task_shared.wake.notified().await;
                loop {
                    let (op, backlog) = {
                        let mut queue = task_shared.queue.lock().unwrap();
                        (queue.pop(), queue.len())
                    };
                    let Some(op) = op else { break };
                    send(&task_shared, &mouse, &keyboard, op, backlog).await;
                }
            }
        });
        Self { shared }
    }

    /// Queues events in order. Never blocks on the guest.
    pub fn push(&self, ops: &[InputOp]) {
        if ops.is_empty() {
            return;
        }
        {
            let mut queue = self.shared.queue.lock().unwrap();
            for op in ops {
                queue.push(*op);
            }
        }
        self.shared.wake.notify_one();
    }

    /// True once if QEMU refused relative motion since the last call.
    pub fn take_rel_rejected(&self) -> bool {
        self.shared.rel_rejected.swap(false, Ordering::SeqCst)
    }
}

async fn send(shared: &Shared, mouse: &MouseProxy<'static>, keyboard: &KeyboardProxy<'static>, op: InputOp, backlog: usize) {
    let started = stats::begin();
    let (kind, what, result) = match op {
        InputOp::Abs(x, y) | InputOp::AbsExact(x, y) => {
            (0, "SetAbsPosition", tokio::time::timeout(CALL_TIMEOUT, mouse.set_abs_position(x, y)).await)
        }
        InputOp::Rel(dx, dy) => (1, "RelMotion", tokio::time::timeout(CALL_TIMEOUT, mouse.rel_motion(dx, dy)).await),
        InputOp::Button { button, down: true } => {
            (2, "Mouse.Press", tokio::time::timeout(CALL_TIMEOUT, mouse.press(button)).await)
        }
        InputOp::Button { button, down: false } => {
            (2, "Mouse.Release", tokio::time::timeout(CALL_TIMEOUT, mouse.release(button)).await)
        }
        InputOp::Key { code, down: true } => {
            (3, "Keyboard.Press", tokio::time::timeout(CALL_TIMEOUT, keyboard.press(code)).await)
        }
        InputOp::Key { code, down: false } => {
            (3, "Keyboard.Release", tokio::time::timeout(CALL_TIMEOUT, keyboard.release(code)).await)
        }
    };
    stats::input(started, backlog);
    let error = match result {
        Ok(Ok(())) => return,
        Ok(Err(e)) => e.to_string(),
        Err(_) => format!("no answer within {} s", CALL_TIMEOUT.as_secs()),
    };
    if kind == 1 {
        shared.rel_rejected.store(true, Ordering::SeqCst);
    }
    // One warning per kind of event: a failing pointer would otherwise flood the log.
    if !shared.warned[kind].swap(true, Ordering::SeqCst) {
        warn!("QEMU {} failed: {} (further failures of this kind are not logged)", what, error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inferred model of QEMU's tablet scaling plus the guest's floor when reading it back.
    fn guest_reads(sent: u64, extent: u64) -> u64 {
        (sent * 32767 / extent) * extent / 32768
    }

    #[test]
    fn model_matches_the_measured_points() {
        // (extent, sent, measured guest position) from the GetCursorPos measurements.
        for (extent, sent, read) in [(1340, 1338, 1337), (1340, 1339, 1338), (1340, 1339, 1338), (677, 676, 675)] {
            assert_eq!(guest_reads(sent, extent), read, "extent {extent} sent {sent}");
        }
        for (sent, read) in [(0, 0), (1, 0), (2, 1), (3, 2)] {
            assert_eq!(guest_reads(sent, 1340), read);
        }
    }

    #[test]
    fn wire_value_is_exact_except_for_the_last_pixel() {
        for extent in [677u32, 1280, 1340, 1920, 2560, 3840] {
            for p in 0..extent - 1 {
                let sent = abs_wire_value(p, extent);
                assert!(sent < extent, "QEMU rejects values >= extent");
                assert_eq!(guest_reads(sent as u64, extent as u64), p as u64, "extent {extent} pixel {p}");
            }
            let last = abs_wire_value(extent - 1, extent);
            assert_eq!(last, extent - 1);
            assert_eq!(guest_reads(last as u64, extent as u64), extent as u64 - 2, "last pixel reads one lower");
            assert_eq!(abs_wire_value(0, extent), 1);
            assert_eq!(abs_wire_value(extent + 5, extent), extent - 1, "out of range is clamped");
        }
    }

    fn drain(q: &mut InputQueue) -> Vec<InputOp> {
        std::iter::from_fn(|| q.pop()).collect()
    }

    const DOWN: InputOp = InputOp::Button { button: 2, down: true };
    const UP: InputOp = InputOp::Button { button: 2, down: false };

    #[test]
    fn keeps_order() {
        let mut q = InputQueue::default();
        let ops = [
            InputOp::Abs(1, 1),
            DOWN,
            InputOp::Key { code: 0x1E, down: true },
            InputOp::Abs(2, 2),
            UP,
            InputOp::Key { code: 0x1E, down: false },
        ];
        for op in ops {
            q.push(op);
        }
        assert_eq!(drain(&mut q), ops);
    }

    #[test]
    fn coalesces_adjacent_motion_only() {
        let mut q = InputQueue::default();
        for i in 0..10 {
            q.push(InputOp::Abs(i, i));
        }
        q.push(InputOp::Rel(1, 2));
        q.push(InputOp::Rel(3, -5));
        q.push(InputOp::Key { code: 1, down: true });
        q.push(InputOp::Rel(1, 1));
        assert_eq!(
            drain(&mut q),
            [
                InputOp::Abs(9, 9),
                InputOp::Rel(4, -3),
                InputOp::Key { code: 1, down: true },
                InputOp::Rel(1, 1)
            ]
        );
    }

    #[test]
    fn button_keeps_the_position_before_it() {
        // Move, press at (5,5), move away, release at (9,9): the position ahead of each button
        // must not be merged into the next one.
        let mut q = InputQueue::default();
        q.push(InputOp::Abs(1, 1));
        q.push(InputOp::Abs(5, 5));
        q.push(DOWN);
        q.push(InputOp::Abs(7, 7));
        q.push(InputOp::Abs(9, 9));
        q.push(UP);
        assert_eq!(
            drain(&mut q),
            [InputOp::Abs(5, 5), DOWN, InputOp::Abs(9, 9), UP]
        );
    }

    #[test]
    fn exact_positions_are_never_merged() {
        let mut q = InputQueue::default();
        q.push(InputOp::AbsExact(4, 4));
        q.push(InputOp::AbsExact(5, 4));
        q.push(InputOp::Abs(5, 4));
        q.push(InputOp::Abs(6, 4));
        assert_eq!(
            drain(&mut q),
            [InputOp::AbsExact(4, 4), InputOp::AbsExact(5, 4), InputOp::Abs(6, 4)]
        );
    }

    #[test]
    fn backlog_drops_only_superseded_motion() {
        let mut q = InputQueue::default();
        // Alternating key and position events cannot coalesce, so the queue backs up.
        for i in 0..BACKLOG_LIMIT as u32 {
            q.push(InputOp::Abs(i, 0));
            q.push(InputOp::Key { code: 7, down: i % 2 == 0 });
        }
        q.push(InputOp::Abs(1, 1));
        q.push(DOWN);
        q.push(InputOp::Abs(2, 2));
        let ops = drain(&mut q);
        let keys = ops.iter().filter(|o| matches!(o, InputOp::Key { .. })).count();
        assert_eq!(keys, BACKLOG_LIMIT);
        // The click and the position ahead of it survive, and the newest position is last.
        let i = ops.iter().position(|o| *o == DOWN).unwrap();
        assert_eq!(ops[i - 1], InputOp::Abs(1, 1));
        assert_eq!(*ops.last().unwrap(), InputOp::Abs(2, 2));
        assert!(ops.len() < 2 * BACKLOG_LIMIT);
    }

    #[test]
    fn buttons_survive_any_backlog() {
        let mut q = InputQueue::default();
        let mut downs = 0;
        for i in 0..5000u32 {
            q.push(InputOp::Abs(i, i));
            q.push(InputOp::Key { code: 1, down: true });
            if i % 7 == 0 {
                q.push(InputOp::Button { button: 0, down: i % 2 == 0 });
                downs += 1;
            }
        }
        let buttons = drain(&mut q).iter().filter(|o| matches!(o, InputOp::Button { .. })).count();
        assert_eq!(buttons, downs);
        assert_eq!(q.len(), 0);
    }
}
