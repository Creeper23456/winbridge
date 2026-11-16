// SPDX-License-Identifier: GPL-3.0-or-later

//! GTK4 viewer user interface, Cairo canvas rendering, and input routing.

use crate::capture::Capture;
use crate::dbus_display::{DbusDisplayConnection, Framebuffer};
use crate::router::InputRouter;
use crate::serial_client::SerialClientHandle;
use crate::transfer::{self, Direction, Transfer};
use crate::config::{launch_log_path, Config};
use crate::launch::LaunchOutcome;
use crate::qmp::QmpClient;
use crate::pointer;
use gtk4::cairo::{Context, Format, ImageSurface};
use gtk4::gdk::{self, DragAction};
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, Button, DrawingArea, DropTarget, EventControllerKey,
    EventControllerMotion, GestureClick, HeaderBar, Label, Orientation, Overlay, ProgressBar, ToggleButton,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use serde_json::json;
use winbridge_proto::Message;

/// What the periodic drag probe should do now.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum ProbeAction {
    /// Send `GuestDragProbe` and nudge the guest pointer.
    Probe,
    /// Nothing to do.
    Idle,
    /// No guest drag showed up in [`PROBE_WINDOW`]: stop probing this press.
    GiveUp,
}

const PROBE_INTERVAL: Duration = Duration::from_millis(300);
const PROBE_WINDOW: Duration = Duration::from_secs(5);
/// The agent counts as unresponsive when it sent nothing (no answer to the keepalive ping) for
/// this long, or when the serial channel is down while the VM runs.
const AGENT_SILENCE_LIMIT: Duration = Duration::from_secs(5);

/// When to probe the guest for a drag: while the left button is held and the pointer is outside
/// the canvas, every [`PROBE_INTERVAL`], for at most [`PROBE_WINDOW`].
#[derive(Default)]
struct ProbeState {
    outside_since: Option<Instant>,
    last_probe: Option<Instant>,
    gave_up: bool,
}

impl ProbeState {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn update(&mut self, now: Instant, left_down: bool, outside: bool, host_drag: bool) -> ProbeAction {
        if !left_down || !outside || host_drag {
            self.reset();
            return ProbeAction::Idle;
        }
        if self.gave_up {
            return ProbeAction::Idle;
        }
        let since = *self.outside_since.get_or_insert(now);
        if now.duration_since(since) >= PROBE_WINDOW {
            self.gave_up = true;
            return ProbeAction::GiveUp;
        }
        match self.last_probe {
            Some(last) if now.duration_since(last) < PROBE_INTERVAL => ProbeAction::Idle,
            _ => {
                self.last_probe = Some(now);
                ProbeAction::Probe
            }
        }
    }
}

/// Tracks the left button and pointer so a guest drag-and-drop that leaves the canvas is noticed.
#[derive(Default)]
struct DragTrack {
    left_down: Cell<bool>,
    /// The pointer is outside the canvas.
    outside: Cell<bool>,
    probe: RefCell<ProbeState>,
    /// Last pointer position in guest pixels, clamped to the screen.
    last_pos: Cell<(u32, u32)>,
    /// A GTK drag for a guest drag is running.
    active: Cell<bool>,
}

#[allow(dead_code)]
pub struct ViewerWindow {
    window: ApplicationWindow,
    drawing_area: DrawingArea,
    fb: Arc<Mutex<Framebuffer>>,
    serial: SerialClientHandle,
    dbus_conn: Arc<Mutex<Option<DbusDisplayConnection>>>,
    transfer_counter: Arc<AtomicU32>,
    status_label: Label,
    /// (QEMU display connected, latest guest agent status text)
    status: RefCell<(bool, Option<String>)>,
    /// QMP client for guest power control and USB hotplug.
    qmp: QmpClient,
    /// True while the launch script (if any) is still running.
    launch_running: Cell<bool>,
    /// Routes pointer and keyboard events to the guest and owns the capture state.
    input: Rc<InputRouter>,
    /// Header toggle for relative (game) mouse mode.
    relative_button: ToggleButton,
    /// A one-off header message, e.g. why game mode switched itself off.
    notice: RefCell<Option<String>>,
    /// Clears [`Self::notice`] after a few seconds.
    notice_timer: RefCell<Option<glib::SourceId>>,
    /// The agent answered the last keepalive check; a silence followed by an answer re-syncs.
    agent_heard: Cell<bool>,
    /// The refresh rate (millihertz) last sent to the guest; `None` until sent, and again after
    /// the agent was silent, so a restarted agent gets it once more.
    refresh_sent: Cell<Option<u32>>,
    /// Re-sends the canvas resolution and DPI to the guest (through the resize debounce).
    resync: RefCell<Option<Rc<dyn Fn()>>>,
    /// `Mouse.IsAbsolute` as read from QEMU when game mode was last switched on.
    is_absolute: Cell<Option<bool>>,
    /// The agent does not answer the keepalive (or the channel is down): the header says how to
    /// release the mouse.
    agent_unresponsive: Cell<bool>,
    /// Keepalive pings sent so far.
    ping_seq: Cell<u64>,
    /// Set when guest shutdown starts: input stops and status updates are held.
    shutting_down: Cell<bool>,
    /// A modal dialog about the guest's power state is open: nothing is captured.
    dialog_open: Cell<bool>,
    /// Set when the viewer is quitting, so the close handler lets the window close.
    quitting: Cell<bool>,
    /// StatusNotifierItem handle, set once the tray registered with a watcher.
    tray: RefCell<Option<ksni::Handle<crate::tray::VmTray>>>,
    /// Keeps the app alive while the window is hidden.
    hold: RefCell<Option<gio::ApplicationHoldGuard>>,
    tray_notice_shown: Cell<bool>,
    /// The guest is off (shut down from inside, or QEMU exited): nothing is left to shut down.
    vm_stopped: Cell<bool>,
    /// Pointer and button state for guest-to-host drags.
    drag: Rc<DragTrack>,
    /// The GTK drag that carries a guest drag, while it runs.
    current_drag: Rc<RefCell<Option<gdk::Drag>>>,
    /// The canvas click gesture (its implicit grab is released before a guest drag starts).
    click_gesture: GestureClick,
    /// Set while a file transfer runs: guest input is released and blocked.
    input_locked: Cell<bool>,
    /// Transfer bar over the canvas: container, bar and text.
    transfer_bar: GtkBox,
    transfer_progress: ProgressBar,
    transfer_label: Label,
    /// The running host-to-guest drop and how many more `DropResult`s (one per file) it awaits.
    drop_pending: Cell<Option<(Transfer, u32)>>,
}

impl ViewerWindow {
    pub fn new(
        app: &Application,
        fb: Arc<Mutex<Framebuffer>>,
        serial: SerialClientHandle,
        dbus_conn: Arc<Mutex<Option<DbusDisplayConnection>>>,
        config: &Config,
    ) -> Rc<Self> {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("WinBridge - Windows 11 Guest Viewer")
            .default_width(1280)
            .default_height(720)
            .build();

        let header = HeaderBar::new();
        let status_label = Label::new(Some("Connecting to QEMU Display & VirtIO-Serial..."));
        // Header text must never widen the window (and with it the guest resolution).
        status_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        status_label.set_max_width_chars(90);
        status_label.set_single_line_mode(true);
        header.set_title_widget(Some(&status_label));
        header.pack_end(&crate::devices::devices_button(&window, config.qmp_socket.clone(), config.usb_bus.clone()));
        let relative_button = ToggleButton::with_label("Game mode");
        header.pack_end(&relative_button);
        window.set_titlebar(Some(&header));

        let drawing_area = DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .focusable(true)
            .can_focus(true)
            .build();

        // Progress bar over the top of the canvas; visible only while a transfer runs.
        let transfer_label = Label::builder()
            .halign(gtk4::Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        let transfer_progress = ProgressBar::builder()
            .hexpand(true)
            .valign(gtk4::Align::Center)
            .build();
        let cancel_button = Button::with_label("Cancel");
        cancel_button.connect_clicked(|_| transfer::cancel_all());
        let transfer_bar = GtkBox::builder()
            .orientation(Orientation::Horizontal)
            .spacing(12)
            .halign(gtk4::Align::Fill)
            .valign(gtk4::Align::Start)
            .margin_top(6)
            .margin_start(6)
            .margin_end(6)
            .css_classes(["osd"])
            .visible(false)
            .build();
        transfer_bar.append(&transfer_label);
        transfer_bar.append(&transfer_progress);
        transfer_bar.append(&cancel_button);
        let overlay = Overlay::builder().hexpand(true).vexpand(true).build();
        overlay.set_child(Some(&drawing_area));
        overlay.add_overlay(&transfer_bar);
        window.set_child(Some(&overlay));

        let track = Rc::new(DragTrack::default());
        let click_gesture = GestureClick::new();
        let input = InputRouter::new(&window, &drawing_area, fb.clone(), dbus_conn.clone(), config.grab_keyboard);
        let viewer = Rc::new(Self {
            window: window.clone(),
            drawing_area: drawing_area.clone(),
            fb: fb.clone(),
            serial: serial.clone(),
            dbus_conn: dbus_conn.clone(),
            transfer_counter: Arc::new(AtomicU32::new(1)),
            status_label: status_label.clone(),
            status: RefCell::new((false, None)),
            qmp: QmpClient::new(config.qmp_socket.clone()),
            launch_running: Cell::new(config.launch.is_some()),
            input: input.clone(),
            relative_button: relative_button.clone(),
            notice: RefCell::new(None),
            notice_timer: RefCell::new(None),
            agent_heard: Cell::new(true),
            refresh_sent: Cell::new(None),
            resync: RefCell::new(None),
            is_absolute: Cell::new(None),
            agent_unresponsive: Cell::new(false),
            ping_seq: Cell::new(0),
            shutting_down: Cell::new(false),
            dialog_open: Cell::new(false),
            quitting: Cell::new(false),
            tray: RefCell::new(None),
            hold: RefCell::new(None),
            tray_notice_shown: Cell::new(false),
            vm_stopped: Cell::new(false),
            drag: track.clone(),
            current_drag: Rc::new(RefCell::new(None)),
            click_gesture: click_gesture.clone(),
            input_locked: Cell::new(false),
            transfer_bar: transfer_bar.clone(),
            transfer_progress: transfer_progress.clone(),
            transfer_label: transfer_label.clone(),
            drop_pending: Cell::new(None),
        });

        // The header follows capture changes (released / game mode).
        {
            let weak = Rc::downgrade(&viewer);
            input.set_on_change(Rc::new(move || {
                if let Some(viewer) = weak.upgrade() {
                    viewer.refresh_status();
                }
            }));
        }

        // Set up Cairo drawing routine. The cursor follows the capture mode: the Linux pointer
        // while nothing is captured, only the guest's own cursor while captured.
        let fb_for_draw = fb.clone();
        let input_for_draw = input.clone();
        drawing_area.set_draw_func(move |_area, cr, width, height| {
            render_framebuffer(&fb_for_draw, cr, width as f64, height as f64);
            input_for_draw.refresh_cursor();
        });

        // Pointer enter / leave / motion. Capture is decided by the router from these.
        let motion_ctrl = EventControllerMotion::new();
        {
            let input = input.clone();
            let track = track.clone();
            motion_ctrl.connect_enter(move |_ctrl, x, y| {
                track.outside.set(false);
                input.enter(x, y);
            });
        }
        {
            let input = input.clone();
            let track = track.clone();
            motion_ctrl.connect_leave(move |_ctrl| {
                track.outside.set(true);
                input.leave();
            });
        }
        {
            let input = input.clone();
            let track = track.clone();
            let area = drawing_area.clone();
            motion_ctrl.connect_motion(move |_ctrl, x, y| {
                // A held left button leaving the canvas may be a guest drag-and-drop; the timer
                // below probes the guest while the pointer stays outside.
                track.outside.set(x < 0.0 || y < 0.0 || x >= area.width() as f64 || y >= area.height() as f64);
                if let Some(pos) = input.motion(x, y) {
                    track.last_pos.set(pos);
                }
            });
        }
        drawing_area.add_controller(motion_ctrl);

        // Raw button events, tracked by the router's own held-button set. GTK gestures follow one
        // button at a time: pressing a second button ended the first button's gesture and its
        // release was never reported, leaving the guest button stuck down.
        let button_ctrl = gtk4::EventControllerLegacy::new();
        {
            let input = input.clone();
            let track = track.clone();
            let area = drawing_area.clone();
            button_ctrl.connect_event(move |_ctrl, event| {
                let down = match event.event_type() {
                    gdk::EventType::ButtonPress => true,
                    gdk::EventType::ButtonRelease => false,
                    _ => return glib::Propagation::Proceed,
                };
                let Some(button_event) = event.downcast_ref::<gdk::ButtonEvent>() else {
                    return glib::Propagation::Proceed;
                };
                let qemu_button = match button_event.button() {
                    1 => 0, // Left
                    2 => 1, // Middle
                    3 => 2, // Right
                    8 => 5, // Side (back)
                    9 => 6, // Extra (forward)
                    _ => return glib::Propagation::Proceed,
                };
                // Event positions are surface coordinates; convert to canvas coordinates.
                let Some((ex, ey)) = event.position() else {
                    return glib::Propagation::Proceed;
                };
                let Some(native) = area.native() else {
                    return glib::Propagation::Proceed;
                };
                let (tx, ty) = native.surface_transform();
                let Some(p) = native
                    .upcast_ref::<gtk4::Widget>()
                    .compute_point(&area, &gtk4::graphene::Point::new((ex - tx) as f32, (ey - ty) as f32))
                else {
                    return glib::Propagation::Proceed;
                };
                let forwarded = input.button(qemu_button, down, p.x() as f64, p.y() as f64);
                if qemu_button == 0 && (forwarded || !down) {
                    track.left_down.set(down);
                    track.probe.borrow_mut().reset();
                }
                glib::Propagation::Proceed
            });
        }
        drawing_area.add_controller(button_ctrl);
        // The left-button gesture only exists so a guest drag can deny its sequence before
        // `Drag::begin` (see `on_guest_drag_offer`); it forwards nothing.
        click_gesture.set_button(1);
        drawing_area.add_controller(click_gesture.clone());

        // Scroll wheel / touchpad.
        let scroll_ctrl = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
        {
            let input = input.clone();
            scroll_ctrl.connect_scroll(move |_ctrl, dx, dy| {
                if input.scroll(dx, dy) {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
        }
        drawing_area.add_controller(scroll_ctrl);

        // Keyboard: forwarded only while captured; outside the canvas, ordinary GTK and desktop
        // shortcuts keep working. The release shortcuts are handled by the router.
        let key_ctrl = EventControllerKey::new();
        {
            let input = input.clone();
            key_ctrl.connect_key_pressed(move |_ctrl, keyval, keycode, state| input.key_pressed(keyval, keycode, state));
        }
        {
            let input = input.clone();
            key_ctrl.connect_key_released(move |_ctrl, keyval, keycode, _state| input.key_released(keyval, keycode));
        }
        key_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
        window.add_controller(key_ctrl);

        {
            let input = input.clone();
            window.connect_map(move |w| input.set_active(w.is_active()));
        }
        {
            let input = input.clone();
            window.connect_unmap(move |_| input.set_active(false));
        }
        {
            let input = input.clone();
            window.connect_is_active_notify(move |w| input.set_active(w.is_active()));
        }

        // Game mode (relative mouse): off by default, and only where the pointer can be warped.
        if input.relative_supported() {
            relative_button.set_tooltip_text(Some(
                "Game mode: relative mouse for games. Needs a relative pointing device in the guest (for example -device usb-mouse). Release with Ctrl+Alt+G or Right Ctrl.",
            ));
            let weak = Rc::downgrade(&viewer);
            relative_button.connect_toggled(move |button| {
                if let Some(viewer) = weak.upgrade() {
                    viewer.on_relative_toggled(button.is_active());
                }
            });
        } else {
            relative_button.set_sensitive(false);
            relative_button.set_tooltip_text(Some(pointer::UNSUPPORTED_REASON));
        }

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
        *viewer.resync.borrow_mut() = Some(Rc::new({
            let request_sync = request_sync.clone();
            let area = drawing_area.clone();
            let window = window.clone();
            move || request_sync(area.width(), area.height(), window.scale_factor())
        }));

        window.connect_scale_factor_notify({
            let request_sync = request_sync.clone();
            let area = drawing_area.clone();
            move |win| {
                info!("GTK scale factor changed to {}x", win.scale_factor());
                request_sync(area.width(), area.height(), win.scale_factor())
            }
        });

        // Clipboard sharing: text, HTML, RTF, images and files in both directions.
        let gdk_display = gdk::Display::default().expect("Failed to get default GDK display");
        crate::clipboard::watch(&gdk_display.clipboard(), serial.clone());

        // Drag-and-drop: drop target on drawing area
        let drop_target = DropTarget::new(gio::glib::Type::INVALID, DragAction::COPY);
        drop_target.set_types(&[gdk::FileList::static_type(), glib::types::Type::STRING]);

        let serial_for_drop = serial.clone();
        let counter_for_drop = viewer.transfer_counter.clone();
        let fb_for_drop = fb.clone();
        let area_for_drop = drawing_area.clone();
        let viewer_for_drop = Rc::downgrade(&viewer);

        drop_target.connect_drop(move |_target, value, x, y| {
            let widget_w = area_for_drop.width() as f64;
            let widget_h = area_for_drop.height() as f64;
            if widget_w <= 0.0 || widget_h <= 0.0 {
                return false;
            }
            let (guest_w, guest_h) = {
                let guard = fb_for_drop.lock().unwrap();
                (guard.width as f64, guard.height as f64)
            };
            let guest_x = ((x / widget_w) * guest_w).clamp(0.0, guest_w - 1.0) as u32;
            let guest_y = ((y / widget_h) * guest_h).clamp(0.0, guest_h - 1.0) as u32;

            if let Ok(file_list) = value.get::<gdk::FileList>() {
                let paths = file_list
                    .files()
                    .iter()
                    .filter_map(|file| file.path())
                    .collect::<Vec<_>>();
                if paths.is_empty() {
                    return false;
                }
                let total: u64 = paths.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
                let label = match paths.len() {
                    1 => "Sending 1 file to Windows".to_string(),
                    n => format!("Sending {} files to Windows", n),
                };
                let xfer = transfer::begin(label, Direction::ToGuest, total, None);
                if let Some(viewer) = viewer_for_drop.upgrade() {
                    viewer.drop_pending.set(Some((xfer, paths.len() as u32)));
                }
                let transfer_id = counter_for_drop.fetch_add(paths.len() as u32, Ordering::SeqCst);
                let s_clone = serial_for_drop.clone();
                tokio::spawn(async move {
                    match s_clone.send_drop_files(paths, transfer_id, guest_x, guest_y, xfer, total).await {
                        // The bar stays until the guest's DropResult arrives (or the watchdog fires).
                        Ok(()) => xfer.set_label("Windows is saving the dropped files"),
                        Err(e) => {
                            warn!("Drag-and-drop file transfer failed: {}", e);
                            xfer.fail(&e);
                        }
                    }
                });
                return true;
            }

            if let Ok(text) = value.get::<String>() {
                info!("Dropped text onto viewer at guest ({}, {})", guest_x, guest_y);
                serial_for_drop.send_drop_text(text, guest_x, guest_y);
                return true;
            }

            false
        });
        // Our own guest drag must never be dropped back onto the guest canvas.
        let track_for_accept = track.clone();
        drop_target.connect_accept(move |_, _| !track_for_accept.active.get());
        drawing_area.add_controller(drop_target);

        // Close button: starts the shutdown flow instead of closing at once.
        let viewer_for_close = Rc::downgrade(&viewer);
        window.connect_close_request(move |_| {
            match viewer_for_close.upgrade() {
                Some(viewer) if !viewer.quitting.get() => viewer.on_close_request(),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        let viewer_for_transfers = Rc::downgrade(&viewer);
        transfer::set_ui_hook(Rc::new(move || {
            if let Some(viewer) = viewer_for_transfers.upgrade() {
                viewer.on_transfer_update();
            }
        }));
        // Guest drag probing: while the left button is held outside the canvas the guest gets
        // no motion from us (positions are clamped), so probe periodically and nudge its pointer.
        {
            let track = track.clone();
            let serial = serial.clone();
            let input = input.clone();
            let viewer_weak = Rc::downgrade(&viewer);
            glib::timeout_add_local(Duration::from_millis(100), move || {
                let action = track.probe.borrow_mut().update(
                    Instant::now(),
                    track.left_down.get(),
                    track.outside.get(),
                    track.active.get(),
                );
                match action {
                    ProbeAction::Idle => {}
                    ProbeAction::GiveUp => {
                        info!("Guest drag probing stopped: no drag offered within {} s", PROBE_WINDOW.as_secs());
                    }
                    ProbeAction::Probe => {
                        let (x, y) = track.last_pos.get();
                        info!("Sending GuestDragProbe at guest ({}, {}) (left_down=true, pointer outside canvas)", x, y);
                        serial.send_message(Message::GuestDragProbe { x, y });
                        // One pixel inward and back: a real move event, ending exactly at (x, y).
                        let nudge = if x > 0 { (x - 1, y) } else { (x + 1, y) };
                        input.jiggle(nudge, (x, y));
                    }
                }
                match viewer_weak.upgrade() {
                    Some(viewer) => {
                        viewer.check_relative_rejected();
                        glib::ControlFlow::Continue
                    }
                    None => glib::ControlFlow::Break,
                }
            });
        }
        // Keepalive: ping the agent once a second; the header explains how to release the mouse
        // while it does not answer.
        {
            let viewer_weak = Rc::downgrade(&viewer);
            glib::timeout_add_local(Duration::from_secs(1), move || match viewer_weak.upgrade() {
                Some(viewer) => {
                    viewer.keepalive_tick();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            });
        }
        glib::spawn_future_local(viewer.clone().monitor_vm());
        window.present();
        viewer
    }

    /// Redraws the viewer canvas when a new scanout/update arrives.
    pub fn queue_draw(&self) {
        self.drawing_area.queue_draw();
    }

    /// Puts a snapshot offered by the guest on the host clipboard (offered files stay lazy).
    pub fn on_guest_clipboard(&self, clip_id: u32, bundle: winbridge_proto::clip::ClipBundle) {
        let gdk_display = gdk::Display::default().expect("Default display");
        crate::clipboard::apply(&gdk_display.clipboard(), clip_id, bundle, &self.serial);
    }

    /// The guest answered a dropped file or text. Each dropped file is answered once; the drop
    /// ends with the last answer or the first failure, and failures are shown to the user.
    pub fn on_drop_result(&self, success: bool, message: String) {
        let Some((xfer, remaining)) = self.drop_pending.take() else {
            if !success {
                warn!("Guest drop failed: {}", message);
                self.show_error("Dropping into Windows failed", &message);
            }
            return;
        };
        if !xfer.is_active() {
            // Cancelled, timed out or failed on the host side already: a late answer.
            return;
        }
        if !success {
            xfer.fail(&message);
        } else if remaining <= 1 {
            xfer.finish();
        } else {
            self.drop_pending.set(Some((xfer, remaining - 1)));
        }
    }

    /// Refreshes the transfer bar and the input lock from the transfer hub. Main thread only.
    fn on_transfer_update(&self) {
        let (view, notices) = transfer::take_view();
        match view {
            Some(view) => {
                self.transfer_label.set_text(&view.text());
                match view.fraction() {
                    Some(fraction) => self.transfer_progress.set_fraction(fraction),
                    None => self.transfer_progress.pulse(),
                }
                self.transfer_bar.set_visible(true);
                self.set_input_locked(true);
            }
            None => {
                self.transfer_bar.set_visible(false);
                self.set_input_locked(false);
            }
        }
        if !notices.is_empty() {
            self.show_error("File transfer failed", &notices.join("\n"));
        }
    }

    /// While a transfer runs the guest gets no input: the keyboard is handed back, held keys and
    /// the left button are released, and the canvas goes insensitive (it keeps redrawing).
    fn set_input_locked(&self, locked: bool) {
        if self.input_locked.replace(locked) == locked {
            return;
        }
        if locked {
            info!("File transfer running: guest input blocked");
            if self.drag.left_down.replace(false) {
                self.drag.probe.borrow_mut().reset();
            }
        } else {
            info!("File transfer ended: guest input restored");
        }
        // Blocking also hands the keyboard back and releases every held guest key and button.
        self.sync_sensitivity();
    }

    /// The canvas takes input only while the guest runs, is not shutting down and no transfer
    /// holds the lock. The input router captures nothing in those states, nor while a power
    /// dialog is open.
    fn sync_sensitivity(&self) {
        let locked = self.shutting_down.get() || self.vm_stopped.get() || self.input_locked.get();
        self.drawing_area.set_sensitive(!locked);
        self.input.set_blocked(locked || self.dialog_open.get());
    }

    /// The guest saw no drag at the probed position. Probing goes on until the window ends.
    pub fn on_guest_drag_none(&self) {
        info!(
            "GuestDragNone handled (left_down={}, outside={}, active={})",
            self.drag.left_down.get(),
            self.drag.outside.get(),
            self.drag.active.get()
        );
    }

    /// The guest started a drag-and-drop that the pointer carried out of the canvas. If the left
    /// button is still held, a GTK drag begins: the Linux application under the pointer decides
    /// where the dragged files go, and they are fetched from the guest only when it asks.
    pub fn on_guest_drag_offer(&self, drag_id: u32, names: Vec<String>, total_bytes: u64, text: Option<String>) {
        let end = |serial: &SerialClientHandle| serial.send_message(Message::GuestDragEnd { drag_id });
        if !self.drag.left_down.get() || self.drag.active.get() {
            info!(
                "Guest drag offer {} rejected: left_down={} host drag active={}",
                drag_id,
                self.drag.left_down.get(),
                self.drag.active.get()
            );
            end(&self.serial);
            return;
        }
        self.drag.probe.borrow_mut().reset();
        let Some(content) = crate::dragout::provider(drag_id, names.clone(), text, self.serial.clone()) else {
            end(&self.serial);
            return;
        };
        let Some(surface) = self.window.surface() else {
            end(&self.serial);
            return;
        };
        let Some(device) = gdk::Display::default()
            .and_then(|d| d.default_seat())
            .and_then(|s| s.pointer())
        else {
            end(&self.serial);
            return;
        };
        // X11 keeps an implicit grab for our click gesture; give up its claim before the drag.
        self.click_gesture.set_state(gtk4::EventSequenceState::Denied);
        let Some(drag) = gdk::Drag::begin(&surface, &device, &content, DragAction::COPY, 0.0, 0.0) else {
            warn!("gdk::Drag::begin returned None (the desktop refused the drag); ending the guest drag");
            end(&self.serial);
            return;
        };
        info!("gdk::Drag::begin ok: {} file(s), {} bytes; host drag started", names.len(), total_bytes);
        self.drag.active.set(true);

        // GTK owns the pointer during the drag, so the click handler never sees the release:
        // release the guest's button here, then let the guest end its drag.
        let finished = Rc::new(Cell::new(false));
        let finish: Rc<dyn Fn()> = {
            let track = self.drag.clone();
            let serial = self.serial.clone();
            let input = self.input.clone();
            let slot = self.current_drag.clone();
            Rc::new(move || {
                if finished.replace(true) {
                    return;
                }
                input.force_release_button(0);
                serial.send_message(Message::GuestDragEnd { drag_id });
                track.left_down.set(false);
                track.probe.borrow_mut().reset();
                track.active.set(false);
                slot.borrow_mut().take();
            })
        };
        let on_drop = finish.clone();
        drag.connect_drop_performed(move |_| on_drop());
        let on_cancel = finish.clone();
        drag.connect_cancel(move |_, _| on_cancel());
        drag.connect_dnd_finished(move |_| finish());
        *self.current_drag.borrow_mut() = Some(drag);
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

    /// Brings the viewer window forward.
    pub fn present(&self) {
        self.window.present();
    }

    /// Handles the launch script's exit. A clean exit after QEMU was reachable means the VM has
    /// gone, so the viewer quits. Anything else keeps the window open with the error.
    pub fn on_launch_exit(&self, outcome: LaunchOutcome) {
        self.launch_running.set(false);
        match outcome {
            LaunchOutcome::VmGone => {
                info!("Guest powered off; closing the viewer");
                self.quit();
            }
            LaunchOutcome::Failed(reason) => {
                let log = launch_log_path();
                self.status_label
                    .set_text(&format!("VM launch failed - log: {}", log.display()));
                self.show_error("The VM did not start", &reason);
            }
        }
    }

    fn quit(&self) {
        self.quitting.set(true);
        if let Some(app) = self.window.application() {
            app.quit();
        }
    }

    fn show_error(&self, message: &str, detail: &str) {
        gtk4::AlertDialog::builder()
            .modal(true)
            .message(message)
            .detail(detail)
            .buttons(["OK"])
            .build()
            .choose(Some(&self.window), None::<&gio::Cancellable>, |_| {});
    }

    /// Close button: offer shutdown, minimize, or cancel. The window stays open until the VM is gone.
    fn on_close_request(self: Rc<Self>) {
        if self.vm_stopped.get() {
            // Nothing left to shut down: closing the window just ends the viewer.
            self.quit();
            return;
        }
        if self.shutting_down.get() {
            return;
        }
        glib::spawn_future_local(async move {
            // No QEMU behind the window (never started, or already exited): nothing to ask about.
            if crate::qmp::qemu_confirmed_gone(self.qmp.path()).await {
                self.quit();
                return;
            }
            self.close_dialog();
        });
    }

    fn close_dialog(self: Rc<Self>) {
        self.dialog_open.set(true);
        self.sync_sensitivity();
        let viewer = self.clone();
        gtk4::AlertDialog::builder()
            .modal(true)
            .message("Shut down the Windows guest?")
            .detail("The guest is powered off gracefully. This viewer closes once the VM is gone.")
            .buttons(["Shut down VM", "Minimize to tray", "Cancel"])
            .default_button(0)
            .cancel_button(2)
            .build()
            .choose(Some(&self.window), None::<&gio::Cancellable>, move |result| {
                viewer.dialog_open.set(false);
                viewer.sync_sensitivity();
                match result {
                    Ok(0) => viewer.begin_shutdown(),
                    Ok(1) => viewer.minimize_to_tray(),
                    _ => {}
                }
            });
    }

    /// Sends ACPI powerdown and tracks the guest until it is off. Input stops here.
    fn begin_shutdown(self: Rc<Self>) {
        self.shutting_down.set(true);
        self.sync_sensitivity();
        self.status_label.set_text("Shutting down guest…");
        self.set_tray_status("VM shutting down");
        glib::spawn_future_local(async move { self.run_shutdown().await });
    }

    async fn run_shutdown(self: Rc<Self>) {
        match self.qmp.execute("system_powerdown", json!({})).await {
            Ok(_) => info!("ACPI powerdown sent to QEMU"),
            Err(e) => {
                warn!("system_powerdown failed: {}", e);
                self.powerdown_failed(e.to_string()).await;
                return;
            }
        }
        self.wait_for_power_off().await;
    }

    /// QMP refused or was unreachable. "Quit anyway" is offered only when QEMU is confirmed gone.
    async fn powerdown_failed(self: Rc<Self>, reason: String) {
        let gone = crate::qmp::qemu_confirmed_gone(self.qmp.path()).await;
        // The guest is still running after a failed power-off request, so input comes back.
        self.shutting_down.set(false);
        self.sync_sensitivity();
        self.set_tray_status("VM running");
        self.refresh_status();
        let buttons: &[&str] = if gone {
            &["Retry", "Minimize to tray", "Quit anyway"]
        } else {
            &["Retry", "Minimize to tray"]
        };
        let viewer = self.clone();
        gtk4::AlertDialog::builder()
            .modal(true)
            .message("Could not shut down the guest")
            .detail(reason.as_str())
            .buttons(buttons)
            .default_button(0)
            .cancel_button(1)
            .build()
            .choose(Some(&self.window), None::<&gio::Cancellable>, move |result| match result {
                Ok(0) => viewer.begin_shutdown(),
                Ok(2) if gone => viewer.quit(),
                _ => viewer.minimize_to_tray(),
            });
    }

    /// Polls query-status once a second. Power-off shows as status `shutdown`, or as QMP being
    /// unreachable twice in a row. Nothing is killed.
    async fn wait_for_power_off(self: Rc<Self>) {
        let started = Instant::now();
        let mut unreachable = 0u32;
        let mut asked_to_wait = false;
        loop {
            glib::timeout_future(Duration::from_secs(1)).await;
            match self.qmp.execute("query-status", json!({})).await {
                Ok(reply) => {
                    unreachable = 0;
                    if reply["status"] == "shutdown" {
                        self.vm_off();
                        return;
                    }
                }
                Err(e) if e.is_disconnected() => {
                    unreachable += 1;
                    if unreachable >= 2 {
                        self.vm_off();
                        return;
                    }
                }
                Err(e) => {
                    unreachable = 0;
                    warn!("query-status failed: {}", e);
                }
            }
            if !asked_to_wait && started.elapsed() >= Duration::from_secs(120) {
                asked_to_wait = true;
                self.clone().not_powered_off_dialog();
            }
        }
    }

    /// The guest is off. Quit now unless the launch script is still running; its exit quits then.
    fn vm_off(&self) {
        info!("Guest powered off");
        if !self.launch_running.get() {
            self.quit();
        }
    }

    /// Asked once after 120 s without power-off. "Keep waiting" leaves the poll running.
    fn not_powered_off_dialog(self: Rc<Self>) {
        let viewer = self.clone();
        gtk4::AlertDialog::builder()
            .modal(true)
            .message("Guest has not powered off")
            .detail("The guest is still running. Keep waiting, or minimize the viewer.")
            .buttons(["Keep waiting", "Minimize to tray"])
            .default_button(0)
            .cancel_button(0)
            .build()
            .choose(Some(&self.window), None::<&gio::Cancellable>, move |result| {
                if let Ok(1) = result {
                    viewer.minimize_to_tray();
                }
            });
    }

    /// Hides the window while the app keeps running, if a tray icon can bring it back. Without a
    /// StatusNotifierWatcher the window would be unreachable, so it is only minimized and a
    /// one-time notice explains what the desktop is missing.
    fn minimize_to_tray(self: Rc<Self>) {
        if self.tray.borrow().is_some() {
            if self.hold.borrow().is_none() {
                if let Some(app) = self.window.application() {
                    *self.hold.borrow_mut() = Some(app.hold());
                }
            }
            self.window.set_visible(false);
            return;
        }
        if self.tray_notice_shown.replace(true) {
            self.window.minimize();
            return;
        }
        let viewer = self.clone();
        gtk4::AlertDialog::builder()
            .modal(true)
            .message("No system tray found")
            .detail("The window is minimized instead of hidden. Showing a tray icon needs a StatusNotifierItem host (on GNOME, the AppIndicator/KStatusNotifierItem extension).")
            .buttons(["OK"])
            .build()
            .choose(Some(&self.window), None::<&gio::Cancellable>, move |_| viewer.window.minimize());
    }

    /// Stores the tray handle once the StatusNotifierItem is registered.
    pub fn set_tray(&self, handle: ksni::Handle<crate::tray::VmTray>) {
        *self.tray.borrow_mut() = Some(handle);
    }

    /// Tray left click: hide the window if shown, show it otherwise.
    pub fn tray_toggle(&self) {
        if self.window.is_visible() {
            self.window.set_visible(false);
            if self.hold.borrow().is_none() {
                if let Some(app) = self.window.application() {
                    *self.hold.borrow_mut() = Some(app.hold());
                }
            }
        } else {
            self.tray_show();
        }
    }

    /// Shows and raises the window; the hold taken while hidden is released.
    pub fn tray_show(&self) {
        self.window.set_visible(true);
        self.window.present();
        self.hold.borrow_mut().take();
    }

    /// Tray "Shut down VM": the same dialog and flow as the close button.
    pub fn tray_shutdown(&self) {
        self.tray_show();
        self.window.close();
    }

    fn set_tray_status(&self, text: &'static str) {
        let handle = self.tray.borrow().clone();
        if let Some(handle) = handle {
            tokio::spawn(async move {
                handle.update(|t| t.status = text).await;
            });
        }
    }

    /// Game mode toggled in the header.
    fn on_relative_toggled(self: &Rc<Self>, on: bool) {
        self.clear_notice();
        self.input.set_relative_enabled(on);
        if on {
            // Informational: QEMU refuses RelMotion while its active pointing device is absolute.
            let mouse = self.dbus_conn.lock().unwrap().as_ref().map(|c| c.mouse.clone());
            if let Some(mouse) = mouse {
                let reply = tokio::spawn(async move { mouse.is_absolute().await });
                let viewer = self.clone();
                glib::spawn_future_local(async move {
                    match reply.await {
                        Ok(Ok(absolute)) => {
                            info!("QEMU Mouse.IsAbsolute = {} (game mode needs false)", absolute);
                            viewer.is_absolute.set(Some(absolute));
                        }
                        Ok(Err(e)) => warn!("Reading QEMU Mouse.IsAbsolute failed: {}", e),
                        Err(e) => warn!("Reading QEMU Mouse.IsAbsolute failed: {}", e),
                    }
                });
            }
        }
        self.refresh_status();
    }

    /// QEMU refused relative motion: the guest has no relative pointing device (or its active
    /// pointer is the absolute tablet). Game mode switches itself off and says why.
    fn check_relative_rejected(self: &Rc<Self>) {
        let rejected = self
            .dbus_conn
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|c| c.input.take_rel_rejected());
        if !rejected || !self.relative_button.is_active() {
            return;
        }
        let absolute = self.is_absolute.get().map_or("unknown".to_string(), |a| a.to_string());
        warn!("QEMU refused RelMotion (IsAbsolute = {}): switching game mode off", absolute);
        // Toggling clears the notice, so set it afterwards.
        self.relative_button.set_active(false);
        self.set_notice(format!(
            "Game mode off: QEMU refused relative motion (IsAbsolute = {}); the guest needs a relative pointing device such as -device usb-mouse",
            absolute
        ));
        self.refresh_status();
    }

    /// Shows a short header message (the full text is in the tooltip) for a few seconds.
    fn set_notice(self: &Rc<Self>, text: String) {
        self.clear_notice();
        *self.notice.borrow_mut() = Some(text);
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_secs(8), move || {
            if let Some(viewer) = weak.upgrade() {
                viewer.notice_timer.borrow_mut().take();
                viewer.notice.borrow_mut().take();
                viewer.refresh_status();
            }
        });
        *self.notice_timer.borrow_mut() = Some(id);
    }

    fn clear_notice(&self) {
        if let Some(id) = self.notice_timer.borrow_mut().take() {
            id.remove();
        }
        self.notice.borrow_mut().take();
    }

    /// Pings the agent and records whether it still answers.
    fn keepalive_tick(&self) {
        if self.serial.is_connected() {
            let seq = self.ping_seq.get() + 1;
            self.ping_seq.set(seq);
            let timestamp_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            self.serial.send_message(Message::Ping { seq, timestamp_ms });
        }
        // A running transfer keeps the channel busy, so a late answer then proves nothing.
        let unresponsive = self.status.borrow().0
            && !self.vm_stopped.get()
            && !self.shutting_down.get()
            && !self.input_locked.get()
            && (!self.serial.is_connected() || self.serial.silent_for() > AGENT_SILENCE_LIMIT);
        if self.agent_unresponsive.replace(unresponsive) != unresponsive {
            info!(
                "Guest agent {}",
                if unresponsive { "unresponsive: showing the mouse release hint" } else { "answers again" }
            );
            self.refresh_status();
        }
        // The agent was silent and answers again (typically it started after the viewer sent its
        // resolution): send the canvas resolution and DPI once more and refresh the title.
        let heard = self.serial.is_connected() && self.serial.silent_for() <= AGENT_SILENCE_LIMIT;
        if heard {
            if !self.agent_heard.replace(true) {
                info!("Guest agent is answering again: re-syncing resolution and DPI");
                let resync = self.resync.borrow().clone();
                if let Some(resync) = resync {
                    resync();
                }
                self.serial.send_message(Message::QueryStatus);
            }
            self.sync_refresh_rate();
        } else {
            self.agent_heard.set(false);
            self.refresh_sent.set(None);
        }
    }

    /// Sends the refresh rate for the monitors connected now when it differs from the last one
    /// sent (hot-plug, a monitor switched to another mode).
    fn sync_refresh_rate(&self) {
        let Some(millihertz) = crate::refresh::target_millihertz() else {
            return;
        };
        if self.refresh_sent.replace(Some(millihertz)) != Some(millihertz) {
            info!("Syncing guest refresh rate: {} mHz", millihertz);
            self.serial.send_message(Message::SetRefreshRate { millihertz });
        }
    }

    /// Watches QEMU in the background so a guest that was shut down from the inside (or a QEMU
    /// that exited) is noticed. Only a QEMU that was reached once can later count as gone, and
    /// "gone" needs the socket to refuse connections, not just one failed query.
    async fn monitor_vm(self: Rc<Self>) {
        let mut seen_running = false;
        let mut misses = 0u32;
        loop {
            glib::timeout_future(Duration::from_secs(2)).await;
            if self.quitting.get() {
                return;
            }
            if self.shutting_down.get() {
                continue;
            }
            match self.qmp.execute("query-status", json!({})).await {
                Ok(reply) => {
                    seen_running = true;
                    misses = 0;
                    // `shutdown` is a guest that powered off while QEMU stays alive (-no-shutdown).
                    self.set_vm_stopped(reply["status"] == "shutdown");
                }
                Err(e) if e.is_disconnected() => {
                    misses += 1;
                    // A QEMU that never showed up gets 30 s (a launch script gets as long as it runs).
                    let patient = !seen_running && (self.launch_running.get() || misses < 15);
                    if misses >= 2
                        && !patient
                        && crate::qmp::qemu_confirmed_gone(self.qmp.path()).await
                    {
                        info!("QEMU is gone; closing the viewer");
                        self.quit();
                        return;
                    }
                }
                Err(e) => warn!("query-status failed: {}", e),
            }
        }
    }

    /// Records that the guest is off (or running again) and updates header, input and tray.
    fn set_vm_stopped(&self, stopped: bool) {
        if self.vm_stopped.replace(stopped) == stopped {
            return;
        }
        if stopped {
            info!("Guest is off");
            self.sync_sensitivity();
            self.set_tray_status("VM stopped");
        } else {
            info!("Guest is running again");
            self.sync_sensitivity();
            self.set_tray_status("VM running");
        }
        self.refresh_status();
    }

    fn refresh_status(&self) {
        if self.shutting_down.get() {
            return;
        }
        if self.vm_stopped.get() {
            self.status_label.set_text("Guest is off - close the window to exit");
            return;
        }
        let (display, guest) = self.status.borrow().clone();
        let mut text = match (display, guest) {
            (true, Some(g)) => format!("Display connected - Agent connected - {}", g),
            (true, None) => "Display connected - waiting for guest agent...".to_string(),
            (false, Some(g)) => format!("Waiting for QEMU display - Agent connected - {}", g),
            (false, None) => "Connecting to QEMU Display & VirtIO-Serial...".to_string(),
        };
        let notice = self.notice.borrow().clone();
        if notice.is_some() {
            text.push_str(" - Game mode off (hover for details)");
        }
        // The release shortcuts are only mentioned when they are needed: in game mode (the pointer
        // is confined) and while the agent does not answer (the guest may be hung).
        if self.input.manually_released() {
            text.push_str(" - Mouse released");
        } else if self.input.mode() == Capture::Relative {
            text.push_str(" - Game mode: Ctrl+Alt+G or Right Ctrl releases the mouse");
        } else if self.agent_unresponsive.get() {
            text.push_str(" - Ctrl+Alt+G or Right Ctrl: release mouse");
        }
        self.status_label.set_text(&text);
        match notice {
            Some(notice) => self.status_label.set_tooltip_text(Some(&format!("{}\n{}", text, notice))),
            None => self.status_label.set_tooltip_text(Some(&text)),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_every_interval_while_outside_and_held() {
        let t0 = Instant::now();
        let mut p = ProbeState::default();
        assert_eq!(p.update(t0, true, true, false), ProbeAction::Probe);
        assert_eq!(p.update(t0 + Duration::from_millis(100), true, true, false), ProbeAction::Idle);
        assert_eq!(p.update(t0 + Duration::from_millis(299), true, true, false), ProbeAction::Idle);
        assert_eq!(p.update(t0 + Duration::from_millis(300), true, true, false), ProbeAction::Probe);
    }

    #[test]
    fn gives_up_after_window_once() {
        let t0 = Instant::now();
        let mut p = ProbeState::default();
        assert_eq!(p.update(t0, true, true, false), ProbeAction::Probe);
        assert_eq!(p.update(t0 + PROBE_WINDOW, true, true, false), ProbeAction::GiveUp);
        assert_eq!(p.update(t0 + PROBE_WINDOW + Duration::from_secs(1), true, true, false), ProbeAction::Idle);
    }

    #[test]
    fn stops_on_release_reentry_or_host_drag_and_restarts() {
        let t0 = Instant::now();
        for (down, outside, host) in [(false, true, false), (true, false, false), (true, true, true)] {
            let mut p = ProbeState::default();
            assert_eq!(p.update(t0, true, true, false), ProbeAction::Probe);
            assert_eq!(p.update(t0 + Duration::from_millis(50), down, outside, host), ProbeAction::Idle);
            // A new excursion probes at once.
            assert_eq!(p.update(t0 + Duration::from_millis(60), true, true, false), ProbeAction::Probe);
        }
        // After giving up, leaving and coming back re-arms.
        let mut p = ProbeState::default();
        p.update(t0, true, true, false);
        assert_eq!(p.update(t0 + PROBE_WINDOW, true, true, false), ProbeAction::GiveUp);
        p.update(t0 + PROBE_WINDOW, true, false, false);
        assert_eq!(p.update(t0 + PROBE_WINDOW, true, true, false), ProbeAction::Probe);
    }
}
