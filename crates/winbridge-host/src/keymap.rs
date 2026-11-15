// SPDX-License-Identifier: GPL-3.0-or-later

//! Translates Linux / GDK key events to QEMU key numbers (XT scancode set 1).

use gtk4::gdk;

/// Translates GDK keycode and keyval to a QEMU key number.
pub fn gdk_to_qemu_keycode(hardware_keycode: u32, keyval: gdk::Key) -> Option<u32> {
    // Under X11, hardware_keycode is evdev scancode + 8.
    // Under Wayland, hardware_keycode is direct evdev scancode or offset.
    let evdev = if hardware_keycode >= 8 {
        hardware_keycode - 8
    } else {
        hardware_keycode
    };

    if let Some(code) = evdev_to_qemu(evdev) {
        return Some(code);
    }

    // Fallback: match by GDK keyval
    keyval_to_qemu(keyval)
}

fn evdev_to_qemu(evdev: u32) -> Option<u32> {
    match evdev {
        // Standard alphanumeric and function keys (Set 1 make codes 1..83)
        1..=83 => Some(evdev),
        87 => Some(0x57), // F11
        88 => Some(0x58), // F12

        // Extended keys (QEMU encodes extended scancodes with high bit 0x80)
        96 => Some(0x1C | 0x80),  // Keypad Enter
        97 => Some(0x1D | 0x80),  // Right Ctrl
        98 => Some(0x35 | 0x80),  // Keypad /
        100 => Some(0x38 | 0x80), // Right Alt / AltGr
        102 => Some(0x47 | 0x80), // Home
        103 => Some(0x48 | 0x80), // Up
        104 => Some(0x49 | 0x80), // PageUp
        105 => Some(0x4B | 0x80), // Left
        106 => Some(0x4D | 0x80), // Right
        107 => Some(0x4F | 0x80), // End
        108 => Some(0x50 | 0x80), // Down
        109 => Some(0x51 | 0x80), // PageDown
        110 => Some(0x52 | 0x80), // Insert
        111 => Some(0x53 | 0x80), // Delete
        125 => Some(0x5B | 0x80), // Left Meta / Windows
        126 => Some(0x5C | 0x80), // Right Meta
        127 => Some(0x5D | 0x80), // Menu
        _ => None,
    }
}

fn keyval_to_qemu(keyval: gdk::Key) -> Option<u32> {
    match keyval {
        gdk::Key::Escape => Some(0x01),
        gdk::Key::Return => Some(0x1C),
        gdk::Key::BackSpace => Some(0x0E),
        gdk::Key::Tab => Some(0x0F),
        gdk::Key::space => Some(0x39),
        gdk::Key::Control_L => Some(0x1D),
        gdk::Key::Control_R => Some(0x1D | 0x80),
        gdk::Key::Shift_L => Some(0x2A),
        gdk::Key::Shift_R => Some(0x36),
        gdk::Key::Alt_L => Some(0x38),
        gdk::Key::Alt_R => Some(0x38 | 0x80),
        gdk::Key::Up => Some(0x48 | 0x80),
        gdk::Key::Down => Some(0x50 | 0x80),
        gdk::Key::Left => Some(0x4B | 0x80),
        gdk::Key::Right => Some(0x4D | 0x80),
        gdk::Key::Home => Some(0x47 | 0x80),
        gdk::Key::End => Some(0x4F | 0x80),
        gdk::Key::Page_Up => Some(0x49 | 0x80),
        gdk::Key::Page_Down => Some(0x51 | 0x80),
        gdk::Key::Insert => Some(0x52 | 0x80),
        gdk::Key::Delete => Some(0x53 | 0x80),
        gdk::Key::Super_L => Some(0x5B | 0x80),
        gdk::Key::Super_R => Some(0x5C | 0x80),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_standard_keycodes() {
        assert_eq!(evdev_to_qemu(1), Some(0x01)); // Esc
        assert_eq!(evdev_to_qemu(28), Some(0x1C)); // Enter
        assert_eq!(evdev_to_qemu(57), Some(0x39)); // Space
    }

    #[test]
    fn test_extended_keycodes() {
        assert_eq!(evdev_to_qemu(103), Some(0xC8)); // Up arrow
        assert_eq!(evdev_to_qemu(125), Some(0xDB)); // Super / Windows
    }
}
