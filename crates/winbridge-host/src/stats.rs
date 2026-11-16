// SPDX-License-Identifier: GPL-3.0-or-later

//! What the display path costs, once a second (`--stats`): the updates QEMU sends over D-Bus
//! (how many, how large, how long the handler takes) and the canvas redraws.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::info;

static ENABLED: AtomicBool = AtomicBool::new(false);

static UPDATES: AtomicU64 = AtomicU64::new(0);
static UPDATE_PIXELS: AtomicU64 = AtomicU64::new(0);
static UPDATE_BYTES: AtomicU64 = AtomicU64::new(0);
static UPDATE_NS: AtomicU64 = AtomicU64::new(0);
static SCANOUTS: AtomicU64 = AtomicU64::new(0);
static DRAWS: AtomicU64 = AtomicU64::new(0);
static DRAW_NS: AtomicU64 = AtomicU64::new(0);
static COPY_NS: AtomicU64 = AtomicU64::new(0);
static LAST_UPDATE: Mutex<Option<Instant>> = Mutex::new(None);
/// How far apart updates arrive: < 8 ms, < 15 ms, < 20 ms, < 28 ms, < 40 ms, more.
const GAP_LIMITS: [Duration; 5] = [
    Duration::from_millis(8),
    Duration::from_millis(15),
    Duration::from_millis(20),
    Duration::from_millis(28),
    Duration::from_millis(40),
];
static GAPS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

pub fn enable() {
    ENABLED.store(true, Relaxed);
}

/// The clock for a measurement: `None` (and no cost) when the counters are off.
pub fn begin() -> Option<Instant> {
    ENABLED.load(Relaxed).then(Instant::now)
}

fn nanos(t: Instant) -> u64 {
    t.elapsed().as_nanos() as u64
}

/// One `Update` signal of `pixels` pixels carrying `bytes` bytes, handled since `t`.
pub fn update(t: Option<Instant>, pixels: u64, bytes: u64) {
    if let Some(t) = t {
        UPDATES.fetch_add(1, Relaxed);
        UPDATE_PIXELS.fetch_add(pixels, Relaxed);
        UPDATE_BYTES.fetch_add(bytes, Relaxed);
        UPDATE_NS.fetch_add(nanos(t), Relaxed);
        let mut last = LAST_UPDATE.lock().unwrap();
        if let Some(prev) = last.replace(t) {
            let gap = t.saturating_duration_since(prev);
            let slot = GAP_LIMITS.iter().position(|l| gap < *l).unwrap_or(GAP_LIMITS.len());
            GAPS[slot].fetch_add(1, Relaxed);
        }
    }
}

pub fn scanout(t: Option<Instant>) {
    if t.is_some() {
        SCANOUTS.fetch_add(1, Relaxed);
    }
}

/// One canvas redraw since `t`, of which `copy_ns` went into copying the framebuffer.
pub fn draw(t: Option<Instant>, copy_ns: u64) {
    if let Some(t) = t {
        DRAWS.fetch_add(1, Relaxed);
        DRAW_NS.fetch_add(nanos(t), Relaxed);
        COPY_NS.fetch_add(copy_ns, Relaxed);
    }
}

/// Logs the counters every second while they are on. Needs a Tokio runtime.
pub fn start() {
    if !ENABLED.load(Relaxed) {
        return;
    }
    tokio::spawn(async {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let updates = UPDATES.swap(0, Relaxed);
            let pixels = UPDATE_PIXELS.swap(0, Relaxed);
            let bytes = UPDATE_BYTES.swap(0, Relaxed);
            let update_ns = UPDATE_NS.swap(0, Relaxed);
            let scanouts = SCANOUTS.swap(0, Relaxed);
            let draws = DRAWS.swap(0, Relaxed);
            let draw_ns = DRAW_NS.swap(0, Relaxed);
            let copy_ns = COPY_NS.swap(0, Relaxed);
            if updates + scanouts + draws == 0 {
                continue;
            }
            let gaps: Vec<u64> = GAPS.iter().map(|g| g.swap(0, Relaxed)).collect();
            info!(
                "display: {} updates ({} px on average, {} KiB/s, {:.1} ms in the handler), {} scanouts; {} draws ({:.1} ms, {:.1} ms of it copying the framebuffer); gaps between updates <8/<15/<20/<28/<40/more ms: {:?}",
                updates,
                pixels.checked_div(updates).unwrap_or(0),
                bytes / 1024,
                update_ns as f64 / 1e6,
                scanouts,
                draws,
                draw_ns as f64 / 1e6,
                copy_ns as f64 / 1e6,
                gaps,
            );
        }
    });
}
