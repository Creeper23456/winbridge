// SPDX-License-Identifier: GPL-3.0-or-later

//! Host-side keyboard shortcut filter that sits in front of the guest keyboard.
//!
//! It recognises the two ways to release the mouse by hand and handles them entirely on the host,
//! so they work with a hung guest, without `--grab-keyboard`, and during a transfer:
//!
//! * **Ctrl+Alt+G** is swallowed (the guest never sees the G) and any Ctrl/Alt already forwarded
//!   is released in the guest first.
//! * **Right Ctrl tap** (pressed and released with no other key in between). Right Ctrl is a
//!   modifier in guest shortcuts, so its press is held back until the next key arrives (then the
//!   held press is forwarded first and nothing is swallowed) or until its release (then it was a
//!   tap: nothing is forwarded).

/// QEMU key numbers of the modifiers involved.
const LCTRL: u32 = 0x1D;
const RCTRL: u32 = 0x1D | 0x80;
const LALT: u32 = 0x38;
const RALT: u32 = 0x38 | 0x80;

/// What the caller must do, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOut {
    /// Forward a key press to the guest.
    Press(u32),
    /// Forward a key release to the guest.
    Release(u32),
    /// A release shortcut was used: toggle the manual mouse release.
    Toggle,
}

#[derive(Default)]
pub struct KeyFilter {
    /// Keys the guest holds down because of us.
    held: Vec<u32>,
    /// Right Ctrl went down and is held back.
    pending_rctrl: bool,
}

impl KeyFilter {
    /// A key went down. `forwarding` says whether the guest receives keys right now; when it
    /// does not (mouse released), shortcuts are still detected but nothing is emitted for the
    /// guest. `ctrl_alt` is the GDK modifier state: Ctrl and Alt are both down.
    pub fn press(&mut self, code: u32, is_g: bool, ctrl_alt: bool, forwarding: bool, out: &mut Vec<KeyOut>) {
        if code == RCTRL {
            // Auto-repeat of a held-back press stays held back; a forwarded one is repeated.
            if self.pending_rctrl {
                return;
            }
            if forwarding && self.held.contains(&RCTRL) {
                out.push(KeyOut::Press(RCTRL));
                return;
            }
            self.pending_rctrl = true;
            return;
        }
        self.flush_pending(forwarding, out);

        if is_g && ctrl_alt {
            for m in [LCTRL, RCTRL, LALT, RALT] {
                if let Some(i) = self.held.iter().position(|k| *k == m) {
                    self.held.swap_remove(i);
                    out.push(KeyOut::Release(m));
                }
            }
            out.push(KeyOut::Toggle);
            return;
        }

        if forwarding {
            if !self.held.contains(&code) {
                self.held.push(code);
            }
            out.push(KeyOut::Press(code));
        }
    }

    /// A key went up.
    pub fn release(&mut self, code: u32, out: &mut Vec<KeyOut>) {
        if code == RCTRL && self.pending_rctrl {
            self.pending_rctrl = false;
            out.push(KeyOut::Toggle);
            return;
        }
        // Only keys the guest holds are released: the G of Ctrl+Alt+G, and keys pressed while
        // nothing was captured, never reached the guest.
        if let Some(i) = self.held.iter().position(|k| *k == code) {
            self.held.swap_remove(i);
            out.push(KeyOut::Release(code));
        }
    }

    /// Another key arrived: Right Ctrl was a modifier, forward its held-back press.
    fn flush_pending(&mut self, forwarding: bool, out: &mut Vec<KeyOut>) {
        if !std::mem::take(&mut self.pending_rctrl) {
            return;
        }
        if forwarding {
            self.held.push(RCTRL);
            out.push(KeyOut::Press(RCTRL));
        }
    }

    /// Forgets everything and returns the keys the guest holds, to be released.
    pub fn drain_held(&mut self) -> Vec<u32> {
        self.pending_rctrl = false;
        std::mem::take(&mut self.held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: u32 = 0x1E;
    const G: u32 = 0x22;

    fn press(f: &mut KeyFilter, code: u32, is_g: bool, ctrl_alt: bool, fwd: bool) -> Vec<KeyOut> {
        let mut out = Vec::new();
        f.press(code, is_g, ctrl_alt, fwd, &mut out);
        out
    }

    fn release(f: &mut KeyFilter, code: u32) -> Vec<KeyOut> {
        let mut out = Vec::new();
        f.release(code, &mut out);
        out
    }

    #[test]
    fn plain_keys_pass_through() {
        let mut f = KeyFilter::default();
        assert_eq!(press(&mut f, A, false, false, true), [KeyOut::Press(A)]);
        assert_eq!(release(&mut f, A), [KeyOut::Release(A)]);
        assert!(release(&mut f, A).is_empty(), "no stray release");
    }

    #[test]
    fn ctrl_alt_g_is_swallowed_and_releases_modifiers() {
        let mut f = KeyFilter::default();
        assert_eq!(press(&mut f, LCTRL, false, false, true), [KeyOut::Press(LCTRL)]);
        assert_eq!(press(&mut f, LALT, false, true, true), [KeyOut::Press(LALT)]);
        let out = press(&mut f, G, true, true, true);
        assert!(!out.contains(&KeyOut::Press(G)), "the guest never sees G");
        assert!(out.contains(&KeyOut::Release(LCTRL)));
        assert!(out.contains(&KeyOut::Release(LALT)));
        assert_eq!(out.last(), Some(&KeyOut::Toggle));
        // The physical releases that follow reach nobody.
        assert!(release(&mut f, G).is_empty());
        assert!(release(&mut f, LALT).is_empty());
        assert!(release(&mut f, LCTRL).is_empty());
        assert!(f.drain_held().is_empty());
    }

    #[test]
    fn ctrl_alt_g_works_when_released() {
        let mut f = KeyFilter::default();
        assert!(press(&mut f, LCTRL, false, false, false).is_empty());
        assert!(press(&mut f, LALT, false, true, false).is_empty());
        assert_eq!(press(&mut f, G, true, true, false), [KeyOut::Toggle]);
    }

    #[test]
    fn g_without_ctrl_alt_is_a_normal_key() {
        let mut f = KeyFilter::default();
        assert_eq!(press(&mut f, G, true, false, true), [KeyOut::Press(G)]);
    }

    #[test]
    fn right_ctrl_tap_toggles_and_forwards_nothing() {
        let mut f = KeyFilter::default();
        assert!(press(&mut f, RCTRL, false, false, true).is_empty());
        assert_eq!(release(&mut f, RCTRL), [KeyOut::Toggle]);
        assert!(f.drain_held().is_empty());
    }

    #[test]
    fn right_ctrl_as_modifier_is_forwarded_before_the_next_key() {
        let mut f = KeyFilter::default();
        assert!(press(&mut f, RCTRL, false, false, true).is_empty());
        assert_eq!(press(&mut f, A, false, true, true), [KeyOut::Press(RCTRL), KeyOut::Press(A)]);
        assert_eq!(release(&mut f, A), [KeyOut::Release(A)]);
        assert_eq!(release(&mut f, RCTRL), [KeyOut::Release(RCTRL)], "not a tap");
    }

    #[test]
    fn right_ctrl_repeat_stays_held_back() {
        let mut f = KeyFilter::default();
        press(&mut f, RCTRL, false, false, true);
        assert!(press(&mut f, RCTRL, false, false, true).is_empty());
        assert_eq!(release(&mut f, RCTRL), [KeyOut::Toggle]);
    }

    #[test]
    fn right_ctrl_tap_works_when_released() {
        let mut f = KeyFilter::default();
        assert!(press(&mut f, RCTRL, false, false, false).is_empty());
        assert_eq!(release(&mut f, RCTRL), [KeyOut::Toggle]);
        // Used as a modifier while released: nothing reaches the guest, and no tap follows.
        assert!(press(&mut f, RCTRL, false, false, false).is_empty());
        assert!(press(&mut f, A, false, true, false).is_empty());
        assert!(release(&mut f, RCTRL).is_empty());
    }

    #[test]
    fn right_ctrl_alt_g_releases_the_forwarded_right_ctrl() {
        let mut f = KeyFilter::default();
        press(&mut f, RCTRL, false, false, true);
        press(&mut f, LALT, false, true, true);
        let out = press(&mut f, G, true, true, true);
        assert!(out.contains(&KeyOut::Release(RCTRL)));
        assert!(out.contains(&KeyOut::Release(LALT)));
        assert_eq!(out.last(), Some(&KeyOut::Toggle));
    }

    #[test]
    fn drain_returns_held_keys_and_forgets_a_pending_tap() {
        let mut f = KeyFilter::default();
        press(&mut f, A, false, false, true);
        press(&mut f, RCTRL, false, false, true);
        assert_eq!(f.drain_held(), [A]);
        assert!(release(&mut f, RCTRL).is_empty(), "the tap is forgotten");
    }
}
