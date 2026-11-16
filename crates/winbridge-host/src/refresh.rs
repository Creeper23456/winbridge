// SPDX-License-Identifier: GPL-3.0-or-later

//! Refresh rate of the virtual screen: the lowest rate among the monitors connected to the host,
//! at most 60 Hz. The viewer re-reads it every keepalive tick, so a monitor that is plugged in,
//! removed or switched to another mode changes the guest rate without a restart.

use gtk4::gdk;
use gtk4::prelude::*;

/// The guest never runs faster than this, however fast the host monitors are.
pub const MAX_MILLIHERTZ: u32 = 60_000;

/// The lowest known rate (GDK reports 0 for a monitor whose rate it cannot read), capped at
/// [`MAX_MILLIHERTZ`]. `None` when no monitor reports a rate: the guest keeps its own then.
pub fn pick(rates: impl IntoIterator<Item = i32>) -> Option<u32> {
    rates
        .into_iter()
        .filter(|rate| *rate > 0)
        .map(|rate| rate as u32)
        .min()
        .map(|rate| rate.min(MAX_MILLIHERTZ))
}

/// The rate for the monitors connected right now, in millihertz.
pub fn target_millihertz() -> Option<u32> {
    let monitors = gdk::Display::default()?.monitors();
    pick(
        (0..monitors.n_items())
            .filter_map(|i| monitors.item(i).and_downcast::<gdk::Monitor>())
            .map(|monitor| monitor.refresh_rate()),
    )
}

#[cfg(test)]
mod tests {
    use super::pick;

    #[test]
    fn lowest_monitor_wins() {
        assert_eq!(pick([144_000, 59_951, 75_000]), Some(59_951));
        assert_eq!(pick([30_000, 144_000]), Some(30_000));
    }

    #[test]
    fn capped_at_60_hz() {
        assert_eq!(pick([144_000]), Some(60_000));
        assert_eq!(pick([240_000, 165_000]), Some(60_000));
    }

    #[test]
    fn unknown_rates_are_ignored() {
        assert_eq!(pick([0, 120_000]), Some(60_000));
        assert_eq!(pick([0, -1]), None);
        assert_eq!(pick([]), None);
    }
}
