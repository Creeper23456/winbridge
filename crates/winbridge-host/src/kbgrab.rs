// SPDX-License-Identifier: GPL-3.0-or-later

//! Keyboard grab so the guest receives keys the desktop would otherwise keep for itself
//! (Super/Windows key, Alt+Tab, ...).
//!
//! On X11 the window manager owns those keys through passive grabs on the root window. A client
//! that holds an active keyboard grab gets every key first, which is how remote-desktop viewers
//! pass the Windows key through. On Wayland the toplevel's "inhibit system shortcuts" request is
//! the corresponding mechanism.

use gtk4::gdk;
use gtk4::prelude::*;
use gtk4::ApplicationWindow;
use std::cell::Cell;
use tracing::{info, warn};

/// Applies keyboard capture while remembering the last requested backend state. Capture is
/// opt-in (`--grab-keyboard` / `grab_keyboard = true`): an active X11 keyboard grab takes every
/// key away from the rest of the session, including input methods used by other applications
/// such as Flatpak apps, so the default only forwards keys that reach the focused canvas.
pub struct KeyboardGrab {
    enabled: bool,
    on: Cell<bool>,
}

impl KeyboardGrab {
    pub fn new(enabled: bool) -> Self {
        Self { enabled, on: Cell::new(false) }
    }

    /// Grabs (`on`) or releases the keyboard for `window`.
    pub fn set(&self, window: &ApplicationWindow, on: bool) {
        if !self.enabled || self.on.get() == on {
            return;
        }
        let Some(surface) = window.surface() else {
            return;
        };
        self.on.set(on);

        if let Some(x11_surface) = surface.downcast_ref::<gdk4_x11::X11Surface>() {
            let display = surface.display();
            let Some(x11_display) = display.downcast_ref::<gdk4_x11::X11Display>() else {
                return;
            };

            let Ok(xlib) = x11_dl::xlib::Xlib::open() else {
                return;
            };
            use x11_dl::xlib::{CurrentTime, GrabModeAsync, GrabSuccess, True};

            // SAFETY: the display pointer stays valid while GDK's display is alive, and the calls
            // are plain Xlib requests made from the GTK main thread.
            unsafe {
                let xdisplay = x11_display.xdisplay();
                if on {
                    let status = (xlib.XGrabKeyboard)(
                        xdisplay,
                        x11_surface.xid(),
                        True,
                        GrabModeAsync,
                        GrabModeAsync,
                        CurrentTime,
                    );
                    if status == GrabSuccess {
                        info!("Keyboard grabbed: Super/Alt+Tab go to the guest");
                    } else {
                        warn!("Keyboard grab failed with status {}", status);
                    }
                } else {
                    (xlib.XUngrabKeyboard)(xdisplay, CurrentTime);
                    info!("Keyboard released");
                }
                (xlib.XFlush)(xdisplay);
            }
            return;
        }

        if let Some(toplevel) = surface.downcast_ref::<gdk::Toplevel>() {
            if on {
                toplevel.inhibit_system_shortcuts(None::<&gdk::Event>);
                info!("Wayland system shortcuts inhibited");
            } else {
                toplevel.restore_system_shortcuts();
                info!("Wayland system shortcuts restored");
            }
        }
    }
}
