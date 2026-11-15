// SPDX-License-Identifier: GPL-3.0-or-later

//! Progress hub for file transfers (drops, clipboard pastes, guest drags).
//!
//! While at least one transfer is active the viewer shows a progress bar over the canvas and locks
//! guest input. The hub is usable from any thread: state sits behind a mutex and every change
//! schedules one coalesced refresh of the GTK-side hook ([`set_ui_hook`]) on the main loop.
//!
//! Safety nets: a transfer that reports no progress for [`STALL_TIMEOUT`] is ended by a watchdog
//! task, and the user can cancel everything. Ending by cancel or timeout runs the transfer's
//! abort hook (outside the hub lock) so its owner can stop streaming and free its waiters.
//! Hooks must not be called while the caller holds a lock the hook itself takes.

use gtk4::glib;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// A transfer without progress for this long is ended and input is restored.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(30);
const WATCHDOG_TICK: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    ToGuest,
    FromGuest,
}

/// Why a transfer was ended from the outside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Abort {
    Cancelled,
    TimedOut,
}

/// Runs once when the transfer is cancelled or times out (never on `finish`/`fail`).
pub type AbortHook = Box<dyn FnOnce(Abort) + Send>;

struct Entry {
    id: u64,
    label: String,
    direction: Direction,
    done: u64,
    total: u64,
    last_progress: Instant,
    on_abort: Option<AbortHook>,
}

/// What the progress bar shows: all active transfers added up.
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub label: String,
    pub direction: Direction,
    pub done: u64,
    pub total: u64,
}

impl View {
    /// `None` while the total is unknown (the bar pulses).
    pub fn fraction(&self) -> Option<f64> {
        (self.total > 0).then(|| self.done.min(self.total) as f64 / self.total as f64)
    }

    /// Bar text such as `Sending 3 files to Windows - 42%`.
    pub fn text(&self) -> String {
        match self.fraction() {
            Some(f) => format!("{} - {}%", self.label, (f * 100.0).floor() as u32),
            None => self.label.clone(),
        }
    }
}

/// The state machine. Pure: time is passed in, so it is unit-testable.
#[derive(Default)]
struct Core {
    entries: Vec<Entry>,
    notices: Vec<String>,
    next_id: u64,
}

impl Core {
    fn begin(
        &mut self,
        now: Instant,
        label: String,
        direction: Direction,
        total: u64,
        on_abort: Option<AbortHook>,
    ) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.entries.push(Entry { id, label, direction, done: 0, total, last_progress: now, on_abort });
        id
    }

    fn entry(&mut self, id: u64) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    fn is_active(&self, id: u64) -> bool {
        self.entries.iter().any(|e| e.id == id)
    }

    /// Records progress; false when the transfer already ended.
    fn progress(&mut self, now: Instant, id: u64, done: u64, total: u64) -> bool {
        match self.entry(id) {
            Some(e) => {
                e.done = done;
                e.total = total;
                e.last_progress = now;
                true
            }
            None => false,
        }
    }

    fn set_label(&mut self, now: Instant, id: u64, label: String) -> bool {
        match self.entry(id) {
            Some(e) => {
                e.label = label;
                e.last_progress = now;
                true
            }
            None => false,
        }
    }

    /// Ends a transfer normally. Its abort hook is dropped unused.
    fn finish(&mut self, id: u64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        self.entries.len() != before
    }

    /// Ends a transfer with an error the user must see.
    fn fail(&mut self, id: u64, message: &str) -> bool {
        match self.entries.iter().position(|e| e.id == id) {
            Some(pos) => {
                let e = self.entries.remove(pos);
                self.notices.push(format!("{}: {}", e.label, message));
                true
            }
            None => false,
        }
    }

    /// The user cancelled: everything ends silently; the abort hooks are returned to run.
    fn cancel_all(&mut self) -> Vec<(AbortHook, Abort)> {
        self.entries
            .drain(..)
            .filter_map(|e| e.on_abort.map(|h| (h, Abort::Cancelled)))
            .collect()
    }

    /// Ends every transfer without progress for [`STALL_TIMEOUT`], with a notice each.
    fn expire(&mut self, now: Instant) -> Vec<(AbortHook, Abort)> {
        let mut hooks = Vec::new();
        let mut keep = Vec::with_capacity(self.entries.len());
        for e in self.entries.drain(..) {
            if now.saturating_duration_since(e.last_progress) >= STALL_TIMEOUT {
                self.notices.push(format!(
                    "{}: no progress for {} s, transfer stopped",
                    e.label,
                    STALL_TIMEOUT.as_secs()
                ));
                if let Some(h) = e.on_abort {
                    hooks.push((h, Abort::TimedOut));
                }
            } else {
                keep.push(e);
            }
        }
        self.entries = keep;
        hooks
    }

    fn view(&self) -> Option<View> {
        let newest = self.entries.last()?;
        let label = if self.entries.len() == 1 {
            newest.label.clone()
        } else {
            format!("{} file transfers", self.entries.len())
        };
        Some(View {
            label,
            direction: newest.direction,
            done: self.entries.iter().map(|e| e.done).sum(),
            total: self.entries.iter().map(|e| e.total).sum(),
        })
    }
}

struct Hub {
    core: Mutex<Core>,
    refresh_pending: AtomicBool,
    watchdog_running: AtomicBool,
}

static HUB: LazyLock<Hub> = LazyLock::new(|| Hub {
    core: Mutex::new(Core::default()),
    refresh_pending: AtomicBool::new(false),
    watchdog_running: AtomicBool::new(false),
});

thread_local! {
    /// Set once by the viewer window on the GTK main thread.
    static UI_HOOK: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
}

/// Registers what refreshes the progress bar. Main thread only; the hook calls [`take_view`].
pub fn set_ui_hook(hook: Rc<dyn Fn()>) {
    UI_HOOK.with(|h| *h.borrow_mut() = Some(hook));
}

impl Hub {
    /// Schedules one refresh on the GTK main loop (changes in between share it).
    fn notify(&self) {
        if self.refresh_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        glib::idle_add_once(|| {
            HUB.refresh_pending.store(false, Ordering::SeqCst);
            let hook = UI_HOOK.with(|h| h.borrow().clone());
            if let Some(hook) = hook {
                hook();
            }
        });
    }

    fn ensure_watchdog(&self) {
        if self.watchdog_running.swap(true, Ordering::SeqCst) {
            return;
        }
        tokio::spawn(async {
            loop {
                tokio::time::sleep(WATCHDOG_TICK).await;
                let (hooks, idle) = {
                    let mut core = HUB.core.lock().unwrap();
                    let hooks = core.expire(Instant::now());
                    let idle = core.entries.is_empty();
                    if idle {
                        // Under the lock, so a concurrent `begin` either sees the task alive
                        // or starts a new one.
                        HUB.watchdog_running.store(false, Ordering::SeqCst);
                    }
                    (hooks, idle)
                };
                if !hooks.is_empty() || idle {
                    HUB.notify();
                }
                run_hooks(hooks);
                if idle {
                    break;
                }
            }
        });
    }
}

fn run_hooks(hooks: Vec<(AbortHook, Abort)>) {
    for (hook, why) in hooks {
        hook(why);
    }
}

/// Handle of one running transfer. Cheap to copy; the owner must end it with
/// [`Transfer::finish`] or [`Transfer::fail`] (the watchdog is only the safety net).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transfer(u64);

/// Starts a transfer: the progress bar appears and guest input locks. `total` may be 0 when
/// unknown yet. Needs a Tokio runtime context (the watchdog task).
pub fn begin(label: impl Into<String>, direction: Direction, total: u64, on_abort: Option<AbortHook>) -> Transfer {
    let label = label.into();
    let id = HUB.core.lock().unwrap().begin(Instant::now(), label.clone(), direction, total, on_abort);
    info!("Transfer {} started: {}", id, label);
    HUB.ensure_watchdog();
    HUB.notify();
    Transfer(id)
}

impl Transfer {
    /// Reports bytes done and the total. False when the transfer already ended (cancelled,
    /// timed out or finished): the producer should stop.
    pub fn progress(self, done: u64, total: u64) -> bool {
        let active = HUB.core.lock().unwrap().progress(Instant::now(), self.0, done, total);
        if active {
            HUB.notify();
        }
        active
    }

    /// Changes the label (counts as progress).
    pub fn set_label(self, label: impl Into<String>) {
        if HUB.core.lock().unwrap().set_label(Instant::now(), self.0, label.into()) {
            HUB.notify();
        }
    }

    /// False once the transfer ended by any means.
    pub fn is_active(self) -> bool {
        HUB.core.lock().unwrap().is_active(self.0)
    }

    /// Ends the transfer successfully.
    pub fn finish(self) {
        if HUB.core.lock().unwrap().finish(self.0) {
            info!("Transfer {} finished", self.0);
            HUB.notify();
        }
    }

    /// Ends the transfer with an error that is shown to the user. No-op when it already ended.
    pub fn fail(self, message: &str) {
        if HUB.core.lock().unwrap().fail(self.0, message) {
            warn!("Transfer {} failed: {}", self.0, message);
            HUB.notify();
        }
    }
}

/// The Cancel button: ends every transfer and runs their abort hooks.
pub fn cancel_all() {
    let hooks = HUB.core.lock().unwrap().cancel_all();
    info!("Transfers cancelled by the user ({} with abort hook)", hooks.len());
    HUB.notify();
    run_hooks(hooks);
}

/// What the bar should show now (`None`: hide it, restore input) and the errors to report.
pub fn take_view() -> (Option<View>, Vec<String>) {
    let mut core = HUB.core.lock().unwrap();
    (core.view(), std::mem::take(&mut core.notices))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;

    fn begin(core: &mut Core, now: Instant, total: u64) -> u64 {
        core.begin(now, "Sending 2 files to Windows".into(), Direction::ToGuest, total, None)
    }

    #[test]
    fn view_reports_fraction_and_text() {
        let t0 = Instant::now();
        let mut core = Core::default();
        assert_eq!(core.view(), None);
        let id = begin(&mut core, t0, 200);
        assert_eq!(core.view().unwrap().fraction(), Some(0.0));
        assert!(core.progress(t0, id, 84, 200));
        let view = core.view().unwrap();
        assert_eq!(view.text(), "Sending 2 files to Windows - 42%");
        assert!(core.progress(t0, id, 300, 200));
        assert_eq!(core.view().unwrap().fraction(), Some(1.0));
    }

    #[test]
    fn unknown_total_pulses() {
        let mut core = Core::default();
        let id = begin(&mut core, Instant::now(), 0);
        let view = core.view().unwrap();
        assert_eq!(view.fraction(), None);
        assert_eq!(view.text(), "Sending 2 files to Windows");
        assert!(core.finish(id));
    }

    #[test]
    fn finish_ends_and_is_idempotent() {
        let mut core = Core::default();
        let id = begin(&mut core, Instant::now(), 10);
        assert!(core.is_active(id));
        assert!(core.finish(id));
        assert!(!core.is_active(id));
        assert!(!core.finish(id));
        assert!(!core.progress(Instant::now(), id, 5, 10), "late progress reports the end");
        assert_eq!(core.view(), None);
        assert!(core.notices.is_empty());
    }

    #[test]
    fn fail_records_a_notice() {
        let mut core = Core::default();
        let id = begin(&mut core, Instant::now(), 10);
        assert!(core.fail(id, "disk full"));
        assert!(!core.fail(id, "again"));
        assert_eq!(core.notices, vec!["Sending 2 files to Windows: disk full".to_string()]);
        assert_eq!(core.view(), None);
    }

    #[test]
    fn watchdog_expires_only_after_thirty_quiet_seconds() {
        let t0 = Instant::now();
        let mut core = Core::default();
        let aborted = Arc::new(AtomicU32::new(0));
        let flag = aborted.clone();
        let hook: AbortHook = Box::new(move |why| {
            assert_eq!(why, Abort::TimedOut);
            flag.fetch_add(1, Ordering::SeqCst);
        });
        let id = core.begin(t0, "Receiving files from Windows".into(), Direction::FromGuest, 100, Some(hook));

        assert!(core.expire(t0 + Duration::from_secs(29)).is_empty());
        assert!(core.is_active(id));

        // Progress restarts the clock.
        core.progress(t0 + Duration::from_secs(29), id, 10, 100);
        assert!(core.expire(t0 + Duration::from_secs(58)).is_empty());

        let hooks = core.expire(t0 + Duration::from_secs(59));
        assert_eq!(hooks.len(), 1);
        for (hook, why) in hooks {
            hook(why);
        }
        assert_eq!(aborted.load(Ordering::SeqCst), 1);
        assert!(!core.is_active(id));
        assert_eq!(core.view(), None);
        assert_eq!(core.notices.len(), 1);
        assert!(core.notices[0].contains("no progress for 30 s"));
        assert!(core.expire(t0 + Duration::from_secs(120)).is_empty());
    }

    #[test]
    fn relabel_counts_as_progress() {
        let t0 = Instant::now();
        let mut core = Core::default();
        let id = begin(&mut core, t0, 10);
        assert!(core.set_label(t0 + Duration::from_secs(25), id, "Windows is saving the files".into()));
        assert!(core.expire(t0 + Duration::from_secs(40)).is_empty());
        assert_eq!(core.view().unwrap().label, "Windows is saving the files");
    }

    #[test]
    fn cancel_runs_hooks_silently() {
        let mut core = Core::default();
        let aborted = Arc::new(AtomicU32::new(0));
        let flag = aborted.clone();
        let hook: AbortHook = Box::new(move |why| {
            assert_eq!(why, Abort::Cancelled);
            flag.fetch_add(1, Ordering::SeqCst);
        });
        let with_hook = core.begin(Instant::now(), "a".into(), Direction::FromGuest, 5, Some(hook));
        let without = begin(&mut core, Instant::now(), 5);

        let hooks = core.cancel_all();
        assert_eq!(hooks.len(), 1);
        for (hook, why) in hooks {
            hook(why);
        }
        assert_eq!(aborted.load(Ordering::SeqCst), 1);
        assert!(!core.is_active(with_hook) && !core.is_active(without));
        assert!(core.notices.is_empty());
        assert_eq!(core.view(), None);
    }

    #[test]
    fn concurrent_transfers_add_up() {
        let t0 = Instant::now();
        let mut core = Core::default();
        let a = begin(&mut core, t0, 100);
        let b = core.begin(t0, "Receiving files from Windows".into(), Direction::FromGuest, 300, None);
        core.progress(t0, a, 100, 100);
        core.progress(t0, b, 100, 300);
        let view = core.view().unwrap();
        assert_eq!((view.done, view.total), (200, 400));
        assert_eq!(view.label, "2 file transfers");
        assert_eq!(view.direction, Direction::FromGuest);
        core.finish(b);
        assert_eq!(core.view().unwrap().label, "Sending 2 files to Windows");
    }
}
