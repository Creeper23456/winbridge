// SPDX-License-Identifier: GPL-3.0-or-later

//! GTK4 viewer user interface, Cairo canvas rendering, and input routing.

use crate::dbus_display::{DbusDisplayConnection, Framebuffer};
use crate::keymap::gdk_to_qemu_keycode;
use crate::serial_client::SerialClientHandle;
use gtk4::cairo::{Context, Format, ImageSurface};
use gtk4::gdk::{self, DragAction};
use gtk4::gio;
use gtk4::glib::{self, clone};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, DrawingArea, DropTarget, EventControllerKey,
    EventControllerMotion, GestureClick, HeaderBar, Label, Orientation,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, warn};

#[allow(dead_code)]
pub struct ViewerWindow {
    window: ApplicationWindow,
    drawing_area: DrawingArea,
    fb: Arc<Mutex<Framebuffer>>,
    serial: SerialClientHandle,
    dbus_conn: Arc<Mutex<Option<DbusDisplayConnection>>>,
    last_guest_clipboard: Arc<Mutex<String>>,
    transfer_counter: Arc<AtomicU32>,
}

impl ViewerWindow {
    pub fn new(
        app: &Application,
        fb: Arc<Mutex<Framebuffer>>,
        serial: SerialClientHandle,
        dbus_conn: Arc<Mutex<Option<DbusDisplayConnection>>>,
    ) -> Rc<Self> {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("WinBridge - Windows 11 Guest Viewer")
            .default_width(1280)
            .default_height(720)
            .build();

        let header = HeaderBar::new();
        let status_label = Label::new(Some("Connecting to QEMU Display & VirtIO-Serial..."));
        header.set_title_widget(Some(&status_label));
        window.set_titlebar(Some(&header));

        let drawing_area = DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .focusable(true)
            .can_focus(true)
            .build();

        let main_box = GtkBox::new(Orientation::Vertical, 0);
        main_box.append(&drawing_area);
        window.set_child(Some(&main_box));

        let viewer = Rc::new(Self {
            window: window.clone(),
            drawing_area: drawing_area.clone(),
            fb: fb.clone(),
            serial: serial.clone(),
            dbus_conn: dbus_conn.clone(),
            last_guest_clipboard: Arc::new(Mutex::new(String::new())),
            transfer_counter: Arc::new(AtomicU32::new(1)),
        });

        // Set up Cairo drawing routine
        let fb_for_draw = fb.clone();
        drawing_area.set_draw_func(move |_area, cr, width, height| {
            render_framebuffer(&fb_for_draw, cr, width as f64, height as f64);
        });

        // Mouse motion controller
        let motion_ctrl = EventControllerMotion::new();
        let fb_for_motion = fb.clone();
        let dbus_for_motion = dbus_conn.clone();
        let area_for_motion = drawing_area.clone();
        motion_ctrl.connect_motion(move |_ctrl, x, y| {
            let widget_w = area_for_motion.width() as f64;
            let widget_h = area_for_motion.height() as f64;
            if widget_w <= 0.0 || widget_h <= 0.0 {
                return;
            }

            let (guest_w, guest_h) = {
                let guard = fb_for_motion.lock().unwrap();
                (guard.width as f64, guard.height as f64)
            };

            let guest_x = ((x / widget_w) * guest_w).clamp(0.0, guest_w - 1.0) as u32;
            let guest_y = ((y / widget_h) * guest_h).clamp(0.0, guest_h - 1.0) as u32;

            if let Some(conn) = dbus_for_motion.lock().unwrap().as_ref() {
                let mouse = conn.mouse.clone();
                tokio::spawn(async move {
                    let _ = mouse.set_abs_position(guest_x, guest_y).await;
                });
            }
        });
        drawing_area.add_controller(motion_ctrl);

        // Mouse click gesture
        let click_gesture = GestureClick::new();
        let dbus_for_click = dbus_conn.clone();
        click_gesture.connect_pressed(move |gesture, _n_press, _x, _y| {
            let btn = gesture.current_button();
            let qemu_btn = match btn {
                1 => 0, // Left
                2 => 1, // Middle
                3 => 2, // Right
                _ => return,
            };

            if let Some(conn) = dbus_for_click.lock().unwrap().as_ref() {
                let mouse = conn.mouse.clone();
                tokio::spawn(async move {
                    let _ = mouse.press(qemu_btn).await;
                });
            }
        });

        let dbus_for_release = dbus_conn.clone();
        click_gesture.connect_released(move |gesture, _n_press, _x, _y| {
            let btn = gesture.current_button();
            let qemu_btn = match btn {
                1 => 0, // Left
                2 => 1, // Middle
                3 => 2, // Right
                _ => return,
            };

            if let Some(conn) = dbus_for_release.lock().unwrap().as_ref() {
                let mouse = conn.mouse.clone();
                tokio::spawn(async move {
                    let _ = mouse.release(qemu_btn).await;
                });
            }
        });
        drawing_area.add_controller(click_gesture);

        // Keyboard controller
        let key_ctrl = EventControllerKey::new();
        let dbus_for_key_press = dbus_conn.clone();
        key_ctrl.connect_key_pressed(move |_ctrl, keyval, keycode, _state| {
            if let Some(qemu_key) = gdk_to_qemu_keycode(keycode, keyval) {
                if let Some(conn) = dbus_for_key_press.lock().unwrap().as_ref() {
                    let kbd = conn.keyboard.clone();
                    tokio::spawn(async move {
                        let _ = kbd.press(qemu_key).await;
                    });
                }
            }
            glib::Propagation::Proceed
        });

        let dbus_for_key_release = dbus_conn.clone();
        key_ctrl.connect_key_released(move |_ctrl, keyval, keycode, _state| {
            if let Some(qemu_key) = gdk_to_qemu_keycode(keycode, keyval) {
                if let Some(conn) = dbus_for_key_release.lock().unwrap().as_ref() {
                    let kbd = conn.keyboard.clone();
                    tokio::spawn(async move {
                        let _ = kbd.release(qemu_key).await;
                    });
                }
            }
        });
        window.add_controller(key_ctrl);

        // Window resize debounce timer
        let resize_debounce: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        let serial_for_resize = serial.clone();

        drawing_area.connect_resize(clone!(
            #[strong]
            resize_debounce,
            move |_area, width, height| {
                if width <= 100 || height <= 100 {
                    return;
                }

                // Debounce resolution change requests by 150 ms
                if let Some(source_id) = resize_debounce.borrow_mut().take() {
                    source_id.remove();
                }

                let serial_clone = serial_for_resize.clone();
                let w = width as u32;
                let h = height as u32;

                let id = glib::timeout_add_local_once(Duration::from_millis(150), move || {
                    info!("Window resized, requesting guest resolution: {}x{}", w, h);
                    serial_clone.send_resolution(w, h);
                });

                *resize_debounce.borrow_mut() = Some(id);
            }
        ));

        // Window / Monitor DPI scale synchronization
        let serial_for_scale = serial.clone();
        let last_scale = Rc::new(RefCell::new(0u32));
        window.connect_scale_factor_notify(clone!(
            #[strong]
            last_scale,
            move |win| {
                let factor = win.scale_factor();
                let scale_percent = (factor * 100) as u32;

                if *last_scale.borrow() != scale_percent {
                    *last_scale.borrow_mut() = scale_percent;
                    info!("GTK scale factor changed to {}x ({}%), requesting guest DPI scale", factor, scale_percent);
                    serial_for_scale.send_dpi_scale(scale_percent);
                }
            }
        ));

        // Clipboard sharing: monitor host clipboard
        let gdk_display = gdk::Display::default().expect("Failed to get default GDK display");
        let clipboard = gdk_display.clipboard();
        let serial_for_clip = serial.clone();
        let last_guest_clip_clone = viewer.last_guest_clipboard.clone();

        clipboard.connect_changed(move |clip| {
            let last_clip = last_guest_clip_clone.clone();
            let serial_sender = serial_for_clip.clone();

            clip.read_text_async(gio::Cancellable::NONE, move |res| {
                if let Ok(Some(text)) = res {
                    let s = text.to_string();
                    let mut guard = last_clip.lock().unwrap();
                    if *guard != s {
                        *guard = s.clone();
                        info!("Host clipboard changed ({} chars), syncing to guest", s.chars().count());
                        serial_sender.send_clipboard_text(s);
                    }
                }
            });
        });

        // Drag-and-drop: drop target on drawing area
        let drop_target = DropTarget::new(gio::glib::Type::INVALID, DragAction::COPY);
        drop_target.set_types(&[gdk::FileList::static_type(), glib::types::Type::STRING]);

        let serial_for_drop = serial.clone();
        let counter_for_drop = viewer.transfer_counter.clone();

        drop_target.connect_drop(move |_target, value, _x, _y| {
            if let Ok(file_list) = value.get::<gdk::FileList>() {
                for file in file_list.files() {
                    if let Some(path) = file.path() {
                        let transfer_id = counter_for_drop.fetch_add(1, Ordering::SeqCst);
                        let s_clone = serial_for_drop.clone();
                        tokio::spawn(async move {
                            if let Err(e) = s_clone.send_drop_file(path, transfer_id).await {
                                warn!("Drag-and-drop file transfer failed: {}", e);
                            }
                        });
                    }
                }
                return true;
            }

            if let Ok(text) = value.get::<String>() {
                info!("Dropped text onto viewer, streaming to guest");
                serial_for_drop.send_drop_text(text);
                return true;
            }

            false
        });
        drawing_area.add_controller(drop_target);

        // Initial DPI sync trigger
        let factor = window.scale_factor();
        let scale_percent = (factor * 100) as u32;
        *last_scale.borrow_mut() = scale_percent;
        serial.send_dpi_scale(scale_percent);

        window.present();
        viewer
    }

    /// Redraws the viewer canvas when a new scanout/update arrives.
    pub fn queue_draw(&self) {
        self.drawing_area.queue_draw();
    }

    /// Updates the host clipboard when content is received from the guest agent.
    pub fn on_guest_clipboard(&self, text: String) {
        if let Ok(mut guard) = self.last_guest_clipboard.lock() {
            *guard = text.clone();
        }
        let gdk_display = gdk::Display::default().expect("Default display");
        let clipboard = gdk_display.clipboard();
        clipboard.set_text(&text);
        info!("Updated host clipboard from guest ({} chars)", text.chars().count());
    }
}

/// Cairo rendering routine for the shared framebuffer scanout.
fn render_framebuffer(
    fb_arc: &Arc<Mutex<Framebuffer>>,
    cr: &Context,
    target_width: f64,
    target_height: f64,
) {
    let (width, height, stride, data) = {
        let guard = fb_arc.lock().unwrap();
        (guard.width, guard.height, guard.stride, guard.data.clone())
    };

    if width == 0 || height == 0 || data.is_empty() {
        return;
    }

    // PIXMAN_x8r8g8b8 pixel data matches Cairo Format::Rgb24
    if let Ok(surface) = ImageSurface::create_for_data(
        data,
        Format::Rgb24,
        width as i32,
        height as i32,
        stride as i32,
    ) {
        let scale_x = target_width / (width as f64);
        let scale_y = target_height / (height as f64);

        cr.save().unwrap();
        cr.scale(scale_x, scale_y);
        let _ = cr.set_source_surface(&surface, 0.0, 0.0);
        let _ = cr.paint();
        cr.restore().unwrap();
    }
}
