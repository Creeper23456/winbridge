// SPDX-License-Identifier: GPL-3.0-or-later

//! Pointer warping for relative (game) mode. GTK4 has no pointer-lock API, so this only works on
//! X11, where the pointer is moved back to the canvas centre after every strayed motion event.
//! There is no pointer grab and no keyboard grab: other applications and input methods keep
//! working.

use gtk4::prelude::*;
use gtk4::{graphene, ApplicationWindow, DrawingArea};
use std::cell::RefCell;
use x11_dl::xlib::Xlib;

thread_local! {
    /// Loaded once per GTK main thread; `None` if libX11 cannot be opened.
    static XLIB: RefCell<Option<Xlib>> = RefCell::new(Xlib::open().ok());
}

/// Relative mode needs pointer warping, which only the X11 backend offers.
pub fn relative_supported() -> bool {
    gtk4::gdk::Display::default().is_some_and(|d| d.downcast_ref::<gdk4_x11::X11Display>().is_some())
        && XLIB.with(|x| x.borrow().is_some())
}

/// Why relative mode cannot be offered, for the UI.
pub const UNSUPPORTED_REASON: &str =
    "Relative mouse mode is unavailable here: it needs an X11 session (GTK4 has no pointer lock on Wayland)";

/// Moves the pointer to the centre of `area`. Returns false if that is not possible.
pub fn warp_to_center(window: &ApplicationWindow, area: &DrawingArea) -> bool {
    let (w, h) = (area.width() as f64, area.height() as f64);
    if w <= 0.0 || h <= 0.0 {
        return false;
    }
    let Some(centre) = area.compute_point(window, &graphene::Point::new((w / 2.0) as f32, (h / 2.0) as f32)) else {
        return false;
    };
    let (tx, ty) = window.surface_transform();
    let scale = window.scale_factor() as f64;
    let x = ((centre.x() as f64 + tx) * scale).round() as i32;
    let y = ((centre.y() as f64 + ty) * scale).round() as i32;

    let Some(surface) = window.surface() else {
        return false;
    };
    let (Some(x11_surface), Some(x11_display)) = (
        surface.downcast_ref::<gdk4_x11::X11Surface>(),
        surface.display().downcast::<gdk4_x11::X11Display>().ok(),
    ) else {
        return false;
    };
    XLIB.with(|xlib| {
        let xlib = xlib.borrow();
        let Some(xlib) = xlib.as_ref() else {
            return false;
        };
        // SAFETY: the display pointer stays valid while GDK's display is alive, and this is a
        // plain Xlib request made from the GTK main thread.
        unsafe {
            let xdisplay = x11_display.xdisplay();
            (xlib.XWarpPointer)(xdisplay, 0, x11_surface.xid(), 0, 0, 0, 0, x, y);
            (xlib.XFlush)(xdisplay);
        }
        true
    })
}
