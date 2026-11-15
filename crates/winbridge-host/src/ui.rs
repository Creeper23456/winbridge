// SPDX-License-Identifier: GPL-3.0-or-later

//! GTK4 viewer user interface, Cairo canvas rendering, and input routing.

use crate::dbus_display::{DbusDisplayConnection, Framebuffer};
use crate::keymap::gdk_to_qemu_keycode;
use crate::serial_client::SerialClientHandle;
use gtk4::cairo::{Context, Format, ImageSurface};
use gtk4::gdk::{self, DragAction};
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, DrawingArea, DropTarget, EventControllerKey,
    EventControllerMotion, GestureClick, HeaderBar, Label, Orientation,
};
use std::cell::{Cell, RefCell};
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
    status_label: Label,
    /// (QEMU display connected, latest guest agent status text)
    status: RefCell<(bool, Option<String>)>,
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
            status_label: status_label.clone(),
            status: RefCell::new((false, None)),
        });

        // Set up Cairo drawing routine
        let fb_for_draw = fb.clone();
        // The Linux pointer is hidden over the canvas; the guest cursor takes its place.
        drawing_area.set_cursor_from_name(Some("none"));
        let last_cursor_gen = Cell::new(u64::MAX);
        drawing_area.set_draw_func(move |area, cr, width, height| {
            render_framebuffer(&fb_for_draw, cr, width as f64, height as f64);
            apply_guest_cursor(area, &fb_for_draw, &last_cursor_gen);
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
        // 0 = listen to every button (default is the primary button only).
        click_gesture.set_button(0);
        let dbus_for_click = dbus_conn.clone();
        click_gesture.connect_pressed(move |gesture, _n_press, _x, _y| {
            let btn = gesture.current_button();
            let qemu_btn = match btn {
                1 => 0, // Left
                2 => 1, // Middle
                3 => 2, // Right
                8 => 5, // Side (back)
                9 => 6, // Extra (forward)
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
                8 => 5,
                9 => 6,
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

        // Scroll wheel / touchpad: QEMU models the wheel as button 3 (up) / 4 (down) and
        // 7 / 8 (left / right) presses, one press+release per notch.
        let scroll_ctrl = gtk4::EventControllerScroll::new(
            gtk4::EventControllerScrollFlags::BOTH_AXES,
        );
        let dbus_for_scroll = dbus_conn.clone();
        let scroll_acc = Rc::new(Cell::new((0.0f64, 0.0f64)));
        scroll_ctrl.connect_scroll(move |_ctrl, dx, dy| {
            let (mut ax, mut ay) = scroll_acc.get();
            ax += dx;
            ay += dy;
            let mut buttons: Vec<u32> = Vec::new();
            while ay <= -1.0 {
                buttons.push(3);
                ay += 1.0;
            }
            while ay >= 1.0 {
                buttons.push(4);
                ay -= 1.0;
            }
            while ax <= -1.0 {
                buttons.push(7);
                ax += 1.0;
            }
            while ax >= 1.0 {
                buttons.push(8);
                ax -= 1.0;
            }
            scroll_acc.set((ax, ay));
            if !buttons.is_empty() {
                if let Some(conn) = dbus_for_scroll.lock().unwrap().as_ref() {
                    let mouse = conn.mouse.clone();
                    tokio::spawn(async move {
                        for b in buttons {
                            let _ = mouse.press(b).await;
                            let _ = mouse.release(b).await;
                        }
                    });
                }
            }
            glib::Propagation::Stop
        });
        drawing_area.add_controller(scroll_ctrl);

        // Keyboard controller
        let key_ctrl = EventControllerKey::new();
        let grab_wanted = Rc::new(Cell::new(true));
        let dbus_for_key_press = dbus_conn.clone();
        let window_for_grab = window.clone();
        let grab_wanted_for_key = grab_wanted.clone();
        key_ctrl.connect_key_pressed(move |_ctrl, keyval, keycode, state| {
            // Ctrl+Alt+G: hand the keyboard back to the desktop (or take it again).
            let ctrl_alt = state.contains(gdk::ModifierType::CONTROL_MASK)
                && state.contains(gdk::ModifierType::ALT_MASK);
            if ctrl_alt && (keyval == gdk::Key::g || keyval == gdk::Key::G) {
                let on = !grab_wanted_for_key.get();
                grab_wanted_for_key.set(on);
                crate::kbgrab::set(&window_for_grab, on && window_for_grab.is_active());
                return glib::Propagation::Stop;
            }
            if let Some(qemu_key) = gdk_to_qemu_keycode(keycode, keyval) {
                if let Some(conn) = dbus_for_key_press.lock().unwrap().as_ref() {
                    let kbd = conn.keyboard.clone();
                    tokio::spawn(async move {
                        let _ = kbd.press(qemu_key).await;
                    });
                }
            }
            glib::Propagation::Stop
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
        // Capture phase: the guest gets every key before any GTK widget can consume it.
        key_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
        window.add_controller(key_ctrl);

        // Keep the desktop's own shortcuts (Super, Alt+Tab, ...) away from the viewer while it has
        // focus so they reach Windows. Wayland: ask the compositor to inhibit them (GNOME keeps
        // Super+Esc as the escape hatch). X11: hold an active keyboard grab while focused.
        window.connect_map(|w| {
            if let Some(toplevel) = w
                .surface()
                .and_then(|s| s.downcast::<gdk::Toplevel>().ok())
            {
                toplevel.inhibit_system_shortcuts(None::<&gdk::Event>);
            }
        });
        window.connect_is_active_notify(move |w| {
            crate::kbgrab::set(w, w.is_active() && grab_wanted.get());
        });

        // Guest sync (size + DPI). Window size and scale changes are debounced into one request.
        // The guest runs at the canvas' physical pixel size with Windows scaling set to the Linux
        // factor, so UI elements end up the same size as on the Linux side.
        let resize_debounce: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        let request_sync: Rc<dyn Fn(i32, i32, i32)> = Rc::new({
            let resize_debounce = resize_debounce.clone();
            let serial = serial.clone();
            let dbus_conn = dbus_conn.clone();
            move |logical_w: i32, logical_h: i32, factor: i32| {
                if logical_w <= 100 || logical_h <= 100 {
                    return;
                }

                if let Some(source_id) = resize_debounce.borrow_mut().take() {
                    source_id.remove();
                }

                let scale = factor.max(1) as u32;
                let w = logical_w as u32 * scale;
                let h = logical_h as u32 * scale;
                let scale_percent = scale * 100;
                let mm_w = (logical_w as f64 * 25.4 / 96.0).round() as u16;
                let mm_h = (logical_h as f64 * 25.4 / 96.0).round() as u16;

                let serial_clone = serial.clone();
                let dbus_inner = dbus_conn.clone();
                let debounce_slot = resize_debounce.clone();

                let id = glib::timeout_add_local_once(Duration::from_millis(150), move || {
                    // The source is gone once it fires: drop the stored id so a later
                    // change does not try to remove it again.
                    debounce_slot.borrow_mut().take();
                    info!("Syncing guest: {}x{} px at {}% DPI", w, h, scale_percent);
                    // Tell QEMU first: the guest GPU driver derives its custom mode from the
                    // host display info, and only a mode it advertises can be applied.
                    let console = dbus_inner
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|c| c.console.clone());
                    tokio::spawn(async move {
                        if let Some(console) = console {
                            if let Err(e) = console.set_ui_info(mm_w, mm_h, 0, 0, w, h).await {
                                warn!("QEMU SetUIInfo failed: {}", e);
                            }
                            tokio::time::sleep(Duration::from_millis(400)).await;
                        }
                        serial_clone.send_resolution(w, h);
                        // Windows caps the scale by the resolution, so apply it after the resolution.
                        serial_clone.send_dpi_scale(scale_percent);
                    });
                });

                *resize_debounce.borrow_mut() = Some(id);
            }
        });

        drawing_area.connect_resize({
            let request_sync = request_sync.clone();
            move |area, width, height| request_sync(width, height, area.scale_factor())
        });

        window.connect_scale_factor_notify({
            let request_sync = request_sync.clone();
            let area = drawing_area.clone();
            move |win| {
                info!("GTK scale factor changed to {}x", win.scale_factor());
                request_sync(area.width(), area.height(), win.scale_factor())
            }
        });

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

    /// Marks the QEMU D-Bus display as connected.
    pub fn on_display_connected(&self) {
        self.status.borrow_mut().0 = true;
        self.refresh_status();
    }

    /// Records the guest agent's reported state (resolution, DPI).
    pub fn on_guest_status(&self, width: u32, height: u32, dpi: u32) {
        self.status.borrow_mut().1 = Some(format!("Guest {}x{} @ {}%", width, height, dpi));
        self.refresh_status();
    }

    fn refresh_status(&self) {
        let (display, guest) = self.status.borrow().clone();
        let text = match (display, guest) {
            (true, Some(g)) => format!("Display connected - Agent connected - {}", g),
            (true, None) => "Display connected - waiting for guest agent...".to_string(),
            (false, Some(g)) => format!("Waiting for QEMU display - Agent connected - {}", g),
            (false, None) => "Connecting to QEMU Display & VirtIO-Serial...".to_string(),
        };
        self.status_label.set_text(&text);
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

/// Shows the guest cursor (hardware cursor sprite from QEMU) instead of the Linux pointer.
/// With no sprite, or while the guest hides its cursor, the canvas shows no host pointer at all
/// (a software cursor drawn into the guest frame stays visible in the frame itself).
fn apply_guest_cursor(area: &DrawingArea, fb: &Arc<Mutex<Framebuffer>>, last_gen: &Cell<u64>) {
    let (generation, on, sprite) = {
        let fb = fb.lock().unwrap();
        if last_gen.get() == fb.cursor_gen {
            return;
        }
        (fb.cursor_gen, fb.cursor_on, fb.cursor.clone())
    };
    last_gen.set(generation);

    let none = gdk::Cursor::from_name("none", None);
    let cursor = match sprite {
        Some(s)
            if on && s.width > 0 && s.height > 0 && s.data.len() >= (s.width * s.height * 4) as usize =>
        {
            let stride = (s.width * 4) as usize;
            let bytes = glib::Bytes::from_owned(s.data);
            let texture = gdk::MemoryTexture::new(
                s.width,
                s.height,
                gdk::MemoryFormat::B8g8r8a8Premultiplied,
                &bytes,
                stride,
            );
            Some(gdk::Cursor::from_texture(
                &texture,
                s.hot_x,
                s.hot_y,
                none.as_ref(),
            ))
        }
        _ => none,
    };
    area.set_cursor(cursor.as_ref());
}
