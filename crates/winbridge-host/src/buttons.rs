// SPDX-License-Identifier: GPL-3.0-or-later

//! The set of mouse buttons the guest holds down because of this viewer. Every press that is
//! forwarded gets exactly one release, whatever order the physical releases come in, and whether
//! or not the pointer is still over the canvas.

/// QEMU button numbers 0..16 as a bit set.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct HeldButtons(u16);

const LEFT: u16 = 1;

impl HeldButtons {
    /// The guest holds the left button (guest drag-and-drop may be running).
    pub fn left_held(self) -> bool {
        self.0 & LEFT != 0
    }

    /// Records a forwarded press. False (and nothing recorded) for an unknown button or one that
    /// is already held: a second press would need a second release.
    pub fn press(&mut self, button: u32) -> bool {
        let Some(bit) = Self::bit(button) else {
            return false;
        };
        if self.0 & bit != 0 {
            return false;
        }
        self.0 |= bit;
        true
    }

    /// Records a release. False if the guest does not hold the button (the click was swallowed,
    /// or the button was already released), in which case nothing is forwarded.
    pub fn release(&mut self, button: u32) -> bool {
        let Some(bit) = Self::bit(button) else {
            return false;
        };
        let held = self.0 & bit != 0;
        self.0 &= !bit;
        held
    }

    /// Takes every held button (except the left one if `keep_left`) to be released now.
    pub fn drain(&mut self, keep_left: bool) -> Vec<u32> {
        let keep = if keep_left { LEFT } else { 0 };
        let take = self.0 & !keep;
        self.0 &= keep;
        (0..u16::BITS).filter(|b| take & (1 << b) != 0).collect()
    }

    fn bit(button: u32) -> Option<u16> {
        (button < u16::BITS).then(|| 1 << button)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every press/release order of two buttons ends with both released exactly once.
    #[test]
    fn all_orders_of_two_buttons() {
        for (first, second) in [(0, 2), (2, 0)] {
            for release_first_first in [true, false] {
                let mut h = HeldButtons::default();
                assert!(h.press(first));
                assert!(h.press(second));
                let order = if release_first_first { [first, second] } else { [second, first] };
                assert!(h.release(order[0]));
                assert!(h.release(order[1]));
                assert!(!h.release(first), "no second release");
                assert!(!h.release(second));
                assert_eq!(h, HeldButtons::default());
            }
        }
    }

    #[test]
    fn duplicate_press_needs_no_second_release() {
        let mut h = HeldButtons::default();
        assert!(h.press(1));
        assert!(!h.press(1));
        assert!(h.release(1));
        assert!(!h.release(1));
    }

    #[test]
    fn swallowed_click_has_no_release() {
        // The recapture click was never pressed in the guest, so its release is dropped.
        let mut h = HeldButtons::default();
        assert!(!h.release(0));
    }

    #[test]
    fn leave_keeps_only_the_left_button() {
        let mut h = HeldButtons::default();
        h.press(0);
        h.press(2);
        h.press(5);
        assert_eq!(h.drain(true), [2, 5]);
        assert!(h.left_held());
        // The left button's physical release still arrives outside the canvas.
        assert!(h.release(0));
        assert!(!h.left_held());
    }

    #[test]
    fn focus_loss_releases_everything_once() {
        let mut h = HeldButtons::default();
        h.press(0);
        h.press(1);
        h.press(2);
        assert_eq!(h.drain(false), [0, 1, 2]);
        assert!(!h.left_held());
        assert!(h.drain(false).is_empty());
        // Physical releases after the focus loss reach nobody.
        assert!(!h.release(0) && !h.release(1) && !h.release(2));
    }

    #[test]
    fn out_of_range_buttons_are_ignored() {
        let mut h = HeldButtons::default();
        assert!(!h.press(16));
        assert!(!h.release(99));
        assert_eq!(h, HeldButtons::default());
    }
}
