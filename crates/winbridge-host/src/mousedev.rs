// SPDX-License-Identifier: GPL-3.0-or-later

//! Which of the guest's pointing devices QEMU delivers input to.
//!
//! QEMU hands every pointer event to one *current* pointing device: the head of its handler list
//! (`ui/input.c`, `qemu_input_find_handler`). `org.qemu.Display1.Mouse.IsAbsolute` says whether
//! that device is absolute, `SetAbsPosition` is refused while it is relative and `RelMotion`
//! while it is absolute. A device becomes current when the guest first polls it, so with a tablet
//! and a mouse (the machine's PS/2 mouse is always there) which one answers is not for the viewer
//! to guess. The monitor command `mouse_set N` chooses; `info mice` lists the devices.

use crate::qmp::QmpClient;
use serde_json::{json, Value};

/// One line of `info mice`.
#[derive(Debug, PartialEq, Eq)]
pub struct Mouse {
    pub index: u32,
    pub name: String,
    pub absolute: bool,
    /// QEMU delivers input to this device now.
    pub current: bool,
}

/// Reads the output of `info mice`: `* Mouse #3: QEMU HID Tablet (absolute)`, a star marking the
/// current device.
pub fn parse_mice(text: &str) -> Vec<Mouse> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_end();
            let current = line.starts_with('*');
            let rest = line.trim_start_matches(['*', ' ']).strip_prefix("Mouse #")?;
            let (index, name) = rest.split_once(": ")?;
            let index = index.parse().ok()?;
            let (name, absolute) = match name.strip_suffix(" (absolute)") {
                Some(name) => (name, true),
                None => (name, false),
            };
            Some(Mouse { index, name: name.to_string(), absolute, current })
        })
        .collect()
}

/// The device to use for absolute (`absolute`) or relative pointing: the current one when it
/// already is of that kind, else the first of that kind.
pub fn choice(mice: &[Mouse], absolute: bool) -> Option<&Mouse> {
    mice.iter()
        .find(|m| m.current && m.absolute == absolute)
        .or_else(|| mice.iter().find(|m| m.absolute == absolute))
}

async fn monitor(qmp: &QmpClient, command: &str) -> Result<String, String> {
    match qmp.execute("human-monitor-command", json!({ "command-line": command })).await {
        Ok(Value::String(text)) => Ok(text),
        Ok(other) => Err(format!("`{}` answered {}", command, other)),
        Err(e) => Err(format!("`{}`: {}", command, e)),
    }
}

/// Makes QEMU deliver pointer input to an absolute (`absolute`) or a relative device. Says which
/// device it ended up on, or why none could be chosen.
pub async fn select(qmp: &QmpClient, absolute: bool) -> Result<String, String> {
    let kind = if absolute { "absolute" } else { "relative" };
    let list = monitor(qmp, "info mice").await?;
    let mice = parse_mice(&list);
    let Some(target) = choice(&mice, absolute) else {
        return Err(format!("the guest has no {} pointing device (`info mice`: {:?})", kind, list.trim()));
    };
    if target.current {
        return Ok(format!("{} ({}) already receives the input", target.name, kind));
    }
    let reply = monitor(qmp, &format!("mouse_set {}", target.index)).await?;
    // The monitor prints nothing when it worked.
    if !reply.trim().is_empty() {
        return Err(format!("`mouse_set {}`: {}", target.index, reply.trim()));
    }
    Ok(format!("{} ({}) now receives the input", target.name, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = "  Mouse #2: QEMU PS/2 Mouse\r\n* Mouse #3: QEMU HID Tablet (absolute)\r\n";

    #[test]
    fn the_listing_of_a_q35_guest_with_a_tablet_is_read() {
        assert_eq!(
            parse_mice(LIVE),
            vec![
                Mouse { index: 2, name: "QEMU PS/2 Mouse".into(), absolute: false, current: false },
                Mouse { index: 3, name: "QEMU HID Tablet".into(), absolute: true, current: true },
            ]
        );
    }

    #[test]
    fn the_current_device_is_kept_when_it_already_fits_and_a_device_of_the_kind_is_found_otherwise() {
        let mice = parse_mice(LIVE);
        assert_eq!(choice(&mice, true).map(|m| m.index), Some(3));
        assert_eq!(choice(&mice, false).map(|m| m.index), Some(2));
        // Two relative devices, the second one current.
        let two = parse_mice("  Mouse #1: QEMU PS/2 Mouse\n* Mouse #4: QEMU HID Mouse\n");
        assert_eq!(choice(&two, false).map(|m| m.index), Some(4));
        // Nothing of the kind.
        assert_eq!(choice(&mice[1..], false), None);
        assert_eq!(choice(&[], true), None);
    }

    #[test]
    fn text_that_is_not_a_listing_gives_no_devices() {
        assert!(parse_mice("").is_empty());
        assert!(parse_mice("Mouse #x: broken\nsomething else\n").is_empty());
    }
}
