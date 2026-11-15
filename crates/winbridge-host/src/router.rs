// SPDX-License-Identifier: GPL-3.0-or-later

//! Routes GTK pointer and keyboard events to the guest through the ordered input pipeline, and
//! owns everything that depends on the capture state: the Linux/guest cursor, held buttons and
//! keys, the keyboard grab, the release shortcuts and relative (game) mode.
//!
//! Capture is scoped by the pointer (see [`CaptureState`]). Every transition is applied in one
//! place, [`InputRouter::apply`], on the GTK main thread; the events it produces go through the
//! same ordered queue as everything else, so a release can never overtake an earlier press.

use crate::capture::{Capture, CaptureState, RelativeTracker};
use crate::dbus_display::{DbusDisplayConnection, Framebuffer};
use crate::input::InputOp;
use crate::kbgrab::KeyboardGrab;
use crate::keyfilter::{KeyFilter, KeyOut};
use crate::keymap::gdk_to_qemu_keycode;
use crate::pointer;
use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{ApplicationWindow, DrawingArea};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use crate::buttons::HeldButtons;
use tracing::info;

use crate::input::abs_wire_value;
/// Widget coordinates come out of GTK's single-precision transforms and can sit a hair below the
/// integer pixel they stand for (97.99999 for 98); this much is added before flooring.
const EDGE_EPSILON: f64 = 0.01;

/// Guest pixel for a widget coordinate: the widget spans `0..widget` and the guest `0..guest`
/// pixels, so the last widget pixel maps to `guest - 1` and anything outside is clamped.
fn map_axis(pos: f64, widget: f64, guest: f64) -> u32 {
    (pos * guest / widget + EDGE_EPSILON).floor().clamp(0.0, (guest - 1.0).max(0.0)) as u32
}

#[cfg(test)]
mod tests {
    use super::map_axis;

    #[test]
    fn every_pixel_maps_to_itself_at_scale_one_even_a_hair_below() {
        // Regression: y = 98 on a 677 px canvas went out as 97 (and 11, 22, 31 ... with plain
        // double arithmetic), landing the guest cursor one pixel high.
        for extent in [677u32, 1280, 1340, 1920, 2560, 3840] {
            for p in 0..extent {
                let exact = p as f64;
                let below = f32::from_bits((p as f32).to_bits().saturating_sub(1)) as f64;
                let e = extent as f64;
                assert_eq!(map_axis(exact, e, e), p, "extent {extent} pixel {p}");
                assert_eq!(map_axis(below, e, e), p, "extent {extent} pixel {p} (f32 just below)");
                assert_eq!(map_axis(exact + 0.5, e, e), p, "pixel centre");
            }
        }
        assert_eq!(map_axis(98.0, 677.0, 677.0), 98);
        assert_eq!(map_axis(94.0, 677.0, 677.0), 94);
        assert_eq!(map_axis(249.0, 677.0, 677.0), 249);
        assert_eq!(map_axis(499.0, 677.0, 677.0), 499);
    }

    #[test]
    fn last_pixel_is_reachable_at_scale_one() {
        let (w, g) = (1280.0, 1280.0);
        assert_eq!(map_axis(0.0, w, g), 0);
        assert_eq!(map_axis(1.0, w, g), 1);
        assert_eq!(map_axis(w - 2.0, w, g), 1278);
        assert_eq!(map_axis(w - 1.0, w, g), 1279);
        assert_eq!(map_axis(w - 0.5, w, g), 1279);
        assert_eq!(map_axis(w, w, g), 1279, "one outside is clamped");
        assert_eq!(map_axis(-1.0, w, g), 0);
    }

    #[test]
    fn fractional_scales_reach_the_last_guest_pixel() {
        // Guest = canvas * scale (rounded), as the resize sync sets it.
        for (w, scale) in [(1280.0, 1.25), (1280.0, 1.5), (677.0, 1.5), (677.0, 1.25)] {
            let g = (w * scale as f64).round();
            assert_eq!(map_axis(0.0, w, g), 0);
            assert_eq!(map_axis(w - 1.0, w, g), (g * (w - 1.0) / w).floor() as u32);
            assert_eq!(map_axis(w - 0.01, w, g), g as u32 - 1);
            assert_eq!(map_axis(w, w, g), g as u32 - 1);
            // Monotonic: no guest pixel is skipped backwards.
            let mut last = 0;
            for i in 0..w as u32 {
                let m = map_axis(i as f64, w, g);
                assert!(m >= last);
                last = m;
            }
        }
    }
}

/// Runs `f` on the held-button set kept in a `Cell`.
fn held<R>(cell: &Cell<HeldButtons>, f: impl FnOnce(&mut HeldButtons) -> R) -> R {
    let mut buttons = cell.get();
    let result = f(&mut buttons);
    cell.set(buttons);
    result
}

pub struct InputRouter {
    dbus: Arc<Mutex<Option<DbusDisplayConnection>>>,
    window: ApplicationWindow,
    area: DrawingArea,
    fb: Arc<Mutex<Framebuffer>>,
    kbgrab: KeyboardGrab,
    state: RefCell<CaptureState>,
    /// What the guest side currently reflects; changes only in [`Self::apply`].
    applied: Cell<Capture>,
    /// The manual release shown to the user, tracked to notice changes that keep the mode.
    released: Cell<bool>,
    keys: RefCell<KeyFilter>,
    key_out: RefCell<Vec<KeyOut>>,
    /// QEMU buttons the guest holds down because of us.
    buttons: Cell<HeldButtons>,
    rel: RefCell<RelativeTracker>,
    /// Last pointer position over the canvas, in widget coordinates.
    pos: Cell<Option<(f64, f64)>>,
    /// Cursor last applied to the canvas: capture mode and guest cursor generation.
    cursor_key: Cell<Option<(Capture, u64)>>,
    scroll_acc: Cell<(f64, f64)>,
    /// Called (outside any borrow) after the capture mode changed.
    on_change: RefCell<Option<Rc<dyn Fn()>>>,
    relative_ok: bool,
}

impl InputRouter {
    pub fn new(
        window: &ApplicationWindow,
        area: &DrawingArea,
        fb: Arc<Mutex<Framebuffer>>,
        dbus: Arc<Mutex<Option<DbusDisplayConnection>>>,
        grab_keyboard: bool,
    ) -> Rc<Self> {
        Rc::new(Self {
            dbus,
            window: window.clone(),
            area: area.clone(),
            fb,
            kbgrab: KeyboardGrab::new(grab_keyboard),
            state: RefCell::new(CaptureState::default()),
            applied: Cell::new(Capture::Off),
            released: Cell::new(false),
            keys: RefCell::new(KeyFilter::default()),
            key_out: RefCell::new(Vec::new()),
            buttons: Cell::new(HeldButtons::default()),
            rel: RefCell::new(RelativeTracker::default()),
            pos: Cell::new(None),
            cursor_key: Cell::new(None),
            scroll_acc: Cell::new((0.0, 0.0)),
            on_change: RefCell::new(None),
            relative_ok: pointer::relative_supported(),
        })
    }

    pub fn set_on_change(&self, f: Rc<dyn Fn()>) {
        *self.on_change.borrow_mut() = Some(f);
    }

    pub fn relative_supported(&self) -> bool {
        self.relative_ok
    }

    pub fn mode(&self) -> Capture {
        self.applied.get()
    }

    /// The user released the mouse by hand and has not taken it back.
    pub fn manually_released(&self) -> bool {
        self.released.get()
    }

    /// Queues events for the guest, in order.
    fn send(&self, ops: &[InputOp]) {
        if let Some(conn) = self.dbus.lock().unwrap().as_ref() {
            conn.input.push(ops);
        }
    }

    /// Guest pixel to the value `SetAbsPosition` takes, see [`abs_wire_value`]. The only place
    /// positions are converted for the wire.
    fn wire_pos(&self, px: u32, py: u32) -> (u32, u32) {
        let fb = self.fb.lock().unwrap();
        (abs_wire_value(px, fb.width), abs_wire_value(py, fb.height))
    }

    /// Guest pixel under a widget position, clamped to the screen.
    pub fn guest_pos(&self, x: f64, y: f64) -> Option<(u32, u32)> {
        let (w, h) = (self.area.width() as f64, self.area.height() as f64);
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        let (gw, gh) = {
            let fb = self.fb.lock().unwrap();
            (fb.width as f64, fb.height as f64)
        };
        Some((
            map_axis(x, w, gw),
            map_axis(y, h, gh),
        ))
    }

    // ---- capture transitions -------------------------------------------------------------

    /// Re-evaluates the capture mode and brings the guest, the cursor and the keyboard grab in
    /// line with it. `keep_left`: a held left button survives (the pointer left the canvas
    /// while dragging, which may be a guest drag-and-drop; its release still arrives).
    fn apply(&self, keep_left: bool) {
        let (new, released) = {
            let state = self.state.borrow();
            (state.mode(), state.released())
        };
        let old = self.applied.replace(new);
        let released_changed = self.released.replace(released) != released;
        if old == new {
            // E.g. the pointer left while released: the mode stays Off but the header changes.
            if released_changed {
                self.notify();
            }
            return;
        }
        info!("Input capture: {:?} -> {:?}", old, new);
        match new {
            Capture::Off => {
                self.release_held(keep_left, true);
                self.kbgrab.set(&self.window, false);
                self.rel.borrow_mut().reset();
            }
            Capture::Absolute | Capture::Relative => {
                if old == Capture::Off {
                    self.kbgrab.set(&self.window, true);
                } else {
                    self.release_held(false, false);
                }
                if new == Capture::Relative {
                    self.rel.borrow_mut().reset();
                    pointer::warp_to_center(&self.window, &self.area);
                } else if let Some((x, y)) = self.pos.get() {
                    // Put the guest pointer under the Linux pointer before any click.
                    if let Some((gx, gy)) = self.guest_pos(x, y) {
                        let (wx, wy) = self.wire_pos(gx, gy);
                        self.send(&[InputOp::Abs(wx, wy)]);
                    }
                }
            }
        }
        // A new capture starts from the current guest cursor, never from a stale shape.
        self.cursor_key.set(None);
        self.refresh_cursor();
        self.notify();
    }

    fn notify(&self) {
        let on_change = self.on_change.borrow().clone();
        if let Some(f) = on_change {
            f();
        }
    }

    /// Releases buttons (and optionally keys) the guest holds because of us.
    fn release_held(&self, keep_left: bool, keys: bool) {
        let mut ops: Vec<InputOp> = held(&self.buttons, |b| b.drain(keep_left))
            .into_iter()
            .map(|button| InputOp::Button { button, down: false })
            .collect();
        if keys {
            ops.extend(
                self.keys
                    .borrow_mut()
                    .drain_held()
                    .into_iter()
                    .map(|code| InputOp::Key { code, down: false }),
            );
        }
        if !ops.is_empty() {
            info!("Releasing {} held guest button(s)/key(s)", ops.len());
            self.send(&ops);
        }
    }

    pub fn enter(&self, x: f64, y: f64) {
        self.pos.set(Some((x, y)));
        self.state.borrow_mut().enter();
        self.apply(false);
    }

    pub fn leave(&self) {
        self.state.borrow_mut().leave();
        self.apply(true);
    }

    pub fn set_active(&self, active: bool) {
        self.state.borrow_mut().set_active(active);
        self.apply(false);
    }

    /// Shutdown, a stopped guest, a transfer or a dialog: nothing is forwarded.
    pub fn set_blocked(&self, blocked: bool) {
        self.state.borrow_mut().set_blocked(blocked);
        self.apply(false);
    }

    /// Switches relative (game) mode. Ignored where it is not supported.
    pub fn set_relative_enabled(&self, enabled: bool) {
        let enabled = enabled && self.relative_ok;
        info!("Relative (game) mouse mode {}", if enabled { "enabled" } else { "disabled" });
        self.state.borrow_mut().set_relative_enabled(enabled);
        self.apply(false);
    }

    fn toggle_release(&self) {
        if !self.state.borrow_mut().toggle_release() {
            return;
        }
        let released = self.state.borrow().released();
        info!(
            "Release shortcut: mouse {}",
            if released { "released (override until re-entry, click or shortcut)" } else { "captured again" }
        );
        self.apply(false);
    }

    // ---- pointer events ---------------------------------------------------------------------

    /// Pointer motion over the canvas (widget coordinates). Returns the guest position when it
    /// was absolute, for the drag probe.
    pub fn motion(&self, x: f64, y: f64) -> Option<(u32, u32)> {
        self.pos.set(Some((x, y)));
        match self.applied.get() {
            Capture::Relative => {
                self.relative_motion(x, y);
                None
            }
            // A held left button keeps the guest informed outside the canvas as well (clamped).
            Capture::Absolute => self.send_abs(x, y),
            Capture::Off if self.buttons.get().left_held() => self.send_abs(x, y),
            Capture::Off => None,
        }
    }

    fn send_abs(&self, x: f64, y: f64) -> Option<(u32, u32)> {
        let (gx, gy) = self.guest_pos(x, y)?;
        let (wx, wy) = self.wire_pos(gx, gy);
        self.send(&[InputOp::Abs(wx, wy)]);
        Some((gx, gy))
    }

    fn relative_motion(&self, x: f64, y: f64) {
        let (w, h) = (self.area.width() as f64, self.area.height() as f64);
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let (gw, gh) = {
            let fb = self.fb.lock().unwrap();
            (fb.width as f64, fb.height as f64)
        };
        let radius = (w.min(h) / 8.0).max(8.0);
        let step = self.rel.borrow_mut().motion((x, y), (w / 2.0, h / 2.0), (gw / w, gh / h), radius);
        if let Some((dx, dy)) = step.delta {
            self.send(&[InputOp::Rel(dx, dy)]);
        }
        if step.warp {
            pointer::warp_to_center(&self.window, &self.area);
        }
    }

    /// A mouse button changed (QEMU button number). Returns whether it was forwarded to the
    /// guest. The press that takes back a manual release is swallowed, and so is its release.
    pub fn button(&self, button: u32, down: bool, x: f64, y: f64) -> bool {
        if button >= 16 {
            return false;
        }
        let relative = self.applied.get() == Capture::Relative;
        if down {
            // A click that gives the window focus may arrive before the focus notification.
            if self.applied.get() == Capture::Off && !self.state.borrow().released() {
                self.state.borrow_mut().set_active(self.window.is_active());
                self.apply(false);
            }
            let recaptured = self.state.borrow_mut().click_recapture();
            if recaptured {
                info!("Canvas click: mouse captured again (the click is not forwarded)");
                self.apply(false);
                return false;
            }
            if self.applied.get() == Capture::Off {
                return false;
            }
            if !held(&self.buttons, |b| b.press(button)) {
                return false;
            }
        } else {
            if !held(&self.buttons, |b| b.release(button)) {
                return false;
            }
        }
        // The button carries no position: send the exact one of this GTK event ahead of it.
        let pos = if relative { None } else { self.guest_pos(x, y) };
        match pos {
            Some((gx, gy)) => {
                let (wx, wy) = self.wire_pos(gx, gy);
                self.send(&[InputOp::Abs(wx, wy), InputOp::Button { button, down }])
            }
            None => self.send(&[InputOp::Button { button, down }]),
        }
        true
    }

    /// Releases a button the guest holds although GTK will not report its release (a GTK drag
    /// took the pointer).
    pub fn force_release_button(&self, button: u32) {
        if held(&self.buttons, |b| b.release(button)) {
            self.send(&[InputOp::Button { button, down: false }]);
        }
    }

    /// Scroll wheel / touchpad. QEMU models the wheel as button 3 (up) / 4 (down) and 7 / 8
    /// (left / right) presses, one press and release per notch. Returns whether it was handled.
    pub fn scroll(&self, dx: f64, dy: f64) -> bool {
        if self.applied.get() == Capture::Off {
            return false;
        }
        let (mut ax, mut ay) = self.scroll_acc.get();
        ax += dx;
        ay += dy;
        let mut ops: Vec<InputOp> = Vec::new();
        let mut notch = |button: u32| {
            ops.push(InputOp::Button { button, down: true });
            ops.push(InputOp::Button { button, down: false });
        };
        while ay <= -1.0 {
            notch(3);
            ay += 1.0;
        }
        while ay >= 1.0 {
            notch(4);
            ay -= 1.0;
        }
        while ax <= -1.0 {
            notch(7);
            ax += 1.0;
        }
        while ax >= 1.0 {
            notch(8);
            ax -= 1.0;
        }
        self.scroll_acc.set((ax, ay));
        self.send(&ops);
        true
    }

    /// The guest-drag probe's one-pixel nudge: a real move that ends exactly at `to`.
    pub fn jiggle(&self, nudge: (u32, u32), to: (u32, u32)) {
        if self.state.borrow().relative_enabled() {
            return;
        }
        let (n, t) = (self.wire_pos(nudge.0, nudge.1), self.wire_pos(to.0, to.1));
        self.send(&[InputOp::AbsExact(n.0, n.1), InputOp::AbsExact(t.0, t.1)]);
    }

    // ---- keyboard ---------------------------------------------------------------------------

    /// Key press on the window. Keys go to the guest only while captured; the release shortcuts
    /// work whenever the window is active and the pointer is over the canvas.
    pub fn key_pressed(&self, keyval: gdk::Key, keycode: u32, modifiers: gdk::ModifierType) -> glib::Propagation {
        if !self.keys_in_scope() {
            return glib::Propagation::Proceed;
        }
        let captured = self.applied.get() != Capture::Off;
        let Some(code) = gdk_to_qemu_keycode(keycode, keyval) else {
            return propagation(captured);
        };
        let is_g = matches!(keyval, gdk::Key::g | gdk::Key::G);
        let ctrl_alt = modifiers.contains(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK);
        let mut out = std::mem::take(&mut *self.key_out.borrow_mut());
        out.clear();
        self.keys.borrow_mut().press(code, is_g, ctrl_alt, captured, &mut out);
        let toggled = self.run_key_outputs(&mut out);
        *self.key_out.borrow_mut() = out;
        propagation(captured || toggled)
    }

    pub fn key_released(&self, keyval: gdk::Key, keycode: u32) {
        if !self.keys_in_scope() {
            return;
        }
        let Some(code) = gdk_to_qemu_keycode(keycode, keyval) else {
            return;
        };
        let mut out = std::mem::take(&mut *self.key_out.borrow_mut());
        out.clear();
        self.keys.borrow_mut().release(code, &mut out);
        self.run_key_outputs(&mut out);
        *self.key_out.borrow_mut() = out;
    }

    /// Only the active window with the pointer over the canvas sees keys at all.
    fn keys_in_scope(&self) -> bool {
        let state = self.state.borrow();
        state.inside() && self.window.is_active()
    }

    /// Executes filter outputs in order; returns whether a release shortcut fired.
    fn run_key_outputs(&self, out: &mut Vec<KeyOut>) -> bool {
        let mut toggled = false;
        for op in out.drain(..) {
            match op {
                KeyOut::Press(code) => self.send(&[InputOp::Key { code, down: true }]),
                KeyOut::Release(code) => self.send(&[InputOp::Key { code, down: false }]),
                KeyOut::Toggle => {
                    info!("Release shortcut pressed (Ctrl+Alt+G or Right Ctrl tap)");
                    toggled = true;
                    self.toggle_release();
                }
            }
        }
        toggled
    }

    // ---- cursor -----------------------------------------------------------------------------

    /// Shows the right cursor for the capture mode: the normal Linux pointer while nothing is
    /// captured, no pointer at all in relative mode, and only the guest's own cursor (its
    /// sprite and hotspot) while captured in absolute mode. Cheap; called on every draw.
    pub fn refresh_cursor(&self) {
        let mode = self.applied.get();
        let generation = match mode {
            Capture::Absolute => self.fb.lock().unwrap().cursor_gen,
            _ => 0,
        };
        if self.cursor_key.get() == Some((mode, generation)) {
            return;
        }
        self.cursor_key.set(Some((mode, generation)));
        match mode {
            Capture::Off => self.area.set_cursor(None),
            Capture::Relative => self.area.set_cursor_from_name(Some("none")),
            Capture::Absolute => self.area.set_cursor(self.guest_cursor().as_ref()),
        }
    }

    /// The guest cursor (hardware cursor sprite from QEMU). With no sprite, or while the guest
    /// hides its cursor, no host pointer is shown (a software cursor drawn into the guest frame
    /// stays visible in the frame itself).
    fn guest_cursor(&self) -> Option<gdk::Cursor> {
        let (on, sprite) = {
            let fb = self.fb.lock().unwrap();
            (fb.cursor_on, fb.cursor.clone())
        };
        let none = gdk::Cursor::from_name("none", None);
        match sprite {
            Some(s) if on && s.width > 0 && s.height > 0 && s.data.len() >= (s.width * s.height * 4) as usize => {
                let stride = (s.width * 4) as usize;
                let bytes = glib::Bytes::from_owned(s.data);
                let texture = gdk::MemoryTexture::new(
                    s.width,
                    s.height,
                    gdk::MemoryFormat::B8g8r8a8Premultiplied,
                    &bytes,
                    stride,
                );
                Some(gdk::Cursor::from_texture(&texture, s.hot_x, s.hot_y, none.as_ref()))
            }
            _ => none,
        }
    }
}

fn propagation(stop: bool) -> glib::Propagation {
    if stop {
        glib::Propagation::Stop
    } else {
        glib::Propagation::Proceed
    }
}
