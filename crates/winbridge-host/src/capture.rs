// SPDX-License-Identifier: GPL-3.0-or-later

//! Pure input-capture logic: which kind of capture applies, and how relative mouse motion is
//! derived from a pointer that is warped back to the canvas centre.

/// How the viewer currently drives the guest's pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    /// Nothing is captured: the Linux pointer is shown and no events are forwarded.
    Off,
    /// The pointer is over the canvas: absolute positions, Linux pointer hidden.
    Absolute,
    /// Game mode: relative deltas, pointer hidden and kept at the canvas centre.
    Relative,
}

/// Decides whether input is captured. Capture is scoped by the pointer (inside the canvas) and
/// by window focus; the user can override it with a manual release that lasts until the pointer
/// leaves the canvas, the release is toggled back, or the canvas is clicked.
#[derive(Debug, Default)]
pub struct CaptureState {
    active: bool,
    inside: bool,
    /// Shutdown, stopped guest or a running transfer.
    blocked: bool,
    /// The user released the mouse by hand.
    released: bool,
    relative_enabled: bool,
}

impl CaptureState {
    pub fn mode(&self) -> Capture {
        if !self.active || !self.inside || self.blocked || self.released {
            Capture::Off
        } else if self.relative_enabled {
            Capture::Relative
        } else {
            Capture::Absolute
        }
    }

    /// The user's manual release is in effect.
    pub fn released(&self) -> bool {
        self.released
    }

    pub fn inside(&self) -> bool {
        self.inside
    }

    pub fn enter(&mut self) {
        self.inside = true;
        // A new visit starts captured, whatever the last one ended with.
        self.released = false;
    }

    pub fn leave(&mut self) {
        self.inside = false;
        self.released = false;
    }

    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    pub fn set_blocked(&mut self, blocked: bool) {
        self.blocked = blocked;
    }

    pub fn set_relative_enabled(&mut self, enabled: bool) {
        self.relative_enabled = enabled;
    }

    pub fn relative_enabled(&self) -> bool {
        self.relative_enabled
    }

    /// Ctrl+Alt+G or a Right Ctrl tap: releases the mouse, or takes it back if it was released.
    /// Only meaningful with the pointer over the canvas of the active window; returns whether
    /// the shortcut was acted on.
    pub fn toggle_release(&mut self) -> bool {
        if !self.active || !self.inside {
            return false;
        }
        self.released = !self.released;
        true
    }

    /// A click on the canvas while released takes the mouse back. Returns true if it did; the
    /// click must then not reach the guest.
    pub fn click_recapture(&mut self) -> bool {
        if self.inside && self.released {
            self.released = false;
            true
        } else {
            false
        }
    }
}

/// Turns pointer positions into relative deltas while the pointer is warped back to the canvas
/// centre whenever it strays. Positions are widget coordinates; deltas are guest pixels.
#[derive(Debug, Default)]
pub struct RelativeTracker {
    last: Option<(f64, f64)>,
    /// A warp to the centre was requested and its motion event has not been seen yet.
    expect_echo: bool,
    /// Sub-pixel remainder carried to the next event.
    rem: (f64, f64),
}

#[derive(Debug, PartialEq)]
pub struct RelStep {
    /// Motion to send to the guest.
    pub delta: Option<(i32, i32)>,
    /// Warp the pointer back to the centre now.
    pub warp: bool,
}

/// Events this close to the centre right after a warp are the warp's own motion event.
const ECHO_RADIUS: f64 = 1.5;

impl RelativeTracker {
    /// Starts over; the caller warps the pointer to the centre.
    pub fn reset(&mut self) {
        *self = Self { last: None, expect_echo: true, rem: (0.0, 0.0) };
    }

    pub fn motion(
        &mut self,
        pos: (f64, f64),
        center: (f64, f64),
        scale: (f64, f64),
        warp_radius: f64,
    ) -> RelStep {
        let dist = |a: (f64, f64), b: (f64, f64)| ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
        if self.expect_echo && dist(pos, center) <= ECHO_RADIUS {
            self.expect_echo = false;
            self.last = Some(center);
            return RelStep { delta: None, warp: false };
        }
        let delta = match self.last {
            Some(last) => {
                let fx = (pos.0 - last.0) * scale.0 + self.rem.0;
                let fy = (pos.1 - last.1) * scale.1 + self.rem.1;
                let (dx, dy) = (fx.trunc(), fy.trunc());
                self.rem = (fx - dx, fy - dy);
                (dx != 0.0 || dy != 0.0).then_some((dx as i32, dy as i32))
            }
            None => None,
        };
        self.last = Some(pos);
        let warp = dist(pos, center) > warp_radius;
        if warp {
            self.expect_echo = true;
        }
        RelStep { delta, warp }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> CaptureState {
        let mut s = CaptureState::default();
        s.set_active(true);
        s
    }

    #[test]
    fn captures_only_active_and_inside() {
        let mut s = CaptureState::default();
        assert_eq!(s.mode(), Capture::Off);
        s.enter();
        assert_eq!(s.mode(), Capture::Off, "inactive window");
        s.set_active(true);
        assert_eq!(s.mode(), Capture::Absolute);
        s.set_active(false);
        assert_eq!(s.mode(), Capture::Off);
        s.set_active(true);
        s.leave();
        assert_eq!(s.mode(), Capture::Off, "title bar or outside the canvas");
    }

    #[test]
    fn blocked_wins() {
        let mut s = state();
        s.enter();
        s.set_blocked(true);
        assert_eq!(s.mode(), Capture::Off);
        s.set_blocked(false);
        assert_eq!(s.mode(), Capture::Absolute);
    }

    #[test]
    fn manual_release_lasts_until_leave_and_enter() {
        let mut s = state();
        s.enter();
        assert!(s.toggle_release());
        assert!(s.released());
        assert_eq!(s.mode(), Capture::Off);
        // Moving inside does not recapture.
        s.set_active(false);
        s.set_active(true);
        assert_eq!(s.mode(), Capture::Off);
        s.leave();
        assert!(!s.released());
        s.enter();
        assert_eq!(s.mode(), Capture::Absolute);
    }

    #[test]
    fn shortcut_toggles_back() {
        let mut s = state();
        s.enter();
        assert!(s.toggle_release());
        assert!(s.toggle_release());
        assert_eq!(s.mode(), Capture::Absolute);
    }

    #[test]
    fn shortcut_needs_pointer_in_active_window() {
        let mut s = state();
        assert!(!s.toggle_release(), "pointer outside the canvas");
        s.enter();
        s.set_active(false);
        assert!(!s.toggle_release(), "window not active");
        assert!(!s.released());
    }

    #[test]
    fn shortcut_works_while_blocked() {
        let mut s = state();
        s.enter();
        s.set_blocked(true);
        assert!(s.toggle_release());
        s.set_blocked(false);
        assert_eq!(s.mode(), Capture::Off, "the release survives the transfer");
    }

    #[test]
    fn click_recaptures_only_when_released() {
        let mut s = state();
        s.enter();
        assert!(!s.click_recapture(), "a normal click is forwarded");
        s.toggle_release();
        assert!(s.click_recapture());
        assert_eq!(s.mode(), Capture::Absolute);
        assert!(!s.click_recapture());
    }

    #[test]
    fn relative_mode_follows_the_same_scoping() {
        let mut s = state();
        s.set_relative_enabled(true);
        assert_eq!(s.mode(), Capture::Off);
        s.enter();
        assert_eq!(s.mode(), Capture::Relative);
        s.toggle_release();
        assert_eq!(s.mode(), Capture::Off);
        assert!(s.click_recapture());
        assert_eq!(s.mode(), Capture::Relative);
        s.leave();
        assert_eq!(s.mode(), Capture::Off);
        s.set_relative_enabled(false);
        s.enter();
        assert_eq!(s.mode(), Capture::Absolute);
    }

    const C: (f64, f64) = (100.0, 100.0);

    #[test]
    fn relative_deltas_follow_the_pointer() {
        let mut t = RelativeTracker::default();
        t.reset();
        // Echo of the initial warp.
        assert_eq!(t.motion((100.0, 100.0), C, (1.0, 1.0), 50.0), RelStep { delta: None, warp: false });
        assert_eq!(
            t.motion((110.0, 95.0), C, (1.0, 1.0), 50.0),
            RelStep { delta: Some((10, -5)), warp: false }
        );
        assert_eq!(
            t.motion((111.0, 95.0), C, (1.0, 1.0), 50.0),
            RelStep { delta: Some((1, 0)), warp: false }
        );
    }

    #[test]
    fn relative_warps_when_far_and_ignores_the_echo() {
        let mut t = RelativeTracker::default();
        t.reset();
        t.motion(C, C, (1.0, 1.0), 50.0);
        let far = t.motion((170.0, 100.0), C, (1.0, 1.0), 50.0);
        assert_eq!(far, RelStep { delta: Some((70, 0)), warp: true });
        // The warp's own event must not move the guest back.
        assert_eq!(t.motion(C, C, (1.0, 1.0), 50.0), RelStep { delta: None, warp: false });
        assert_eq!(
            t.motion((103.0, 100.0), C, (1.0, 1.0), 50.0),
            RelStep { delta: Some((3, 0)), warp: false }
        );
    }

    #[test]
    fn relative_keeps_subpixel_remainders() {
        let mut t = RelativeTracker::default();
        t.reset();
        t.motion(C, C, (1.5, 1.5), 50.0);
        let mut sum = 0;
        for i in 1..=4 {
            if let Some((dx, _)) = t.motion((100.0 + i as f64, 100.0), C, (1.5, 1.5), 50.0).delta {
                sum += dx;
            }
        }
        assert_eq!(sum, 6, "4 widget pixels at 1.5x");
    }
}
