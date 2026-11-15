// SPDX-License-Identifier: GPL-3.0-or-later

//! WinBridge Host Viewer Application.
//!
//! Linux GTK4 viewer and bridge host connecting to QEMU's D-Bus display and
//! VirtIO-serial communication channel for Windows 11 guest synchronization.

mod capture;
mod clipboard;
mod config;
mod dragout;
mod dbus_display;
mod devices;
mod input;
mod kbgrab;
mod keyfilter;
mod keymap;
mod launch;
mod pointer;
mod qmp;
mod router;
mod serial_client;
mod tray;
mod transfer;
mod ui;

use clap::Parser;
use dbus_display::{DbusDisplayConnection, Framebuffer};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::Application;
use serial_client::{SerialClient, SerialClientHandle};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tracing::{info, warn};
use ui::ViewerWindow;
use winbridge_proto::Message;

thread_local! {
    static ACTIVE_VIEWER: RefCell<Option<Rc<ViewerWindow>>> = const { RefCell::new(None) };
}

fn with_viewer<F: FnOnce(&ViewerWindow)>(f: F) {
    ACTIVE_VIEWER.with(|cell| {
        if let Some(viewer) = cell.borrow().as_ref() {
            f(viewer);
        }
    });
}

/// Asks the agent for its state; the `StatusReply` refreshes the title text.
fn refresh_guest_status(slot: &OnceLock<SerialClientHandle>) {
    if let Some(serial) = slot.get() {
        serial.send_message(Message::QueryStatus);
    }
}

#[derive(Parser, Debug)]
#[command(name = "winbridge-host")]
#[command(author = "Creeper23456 <Creeper23456@outlook.com>")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "Linux GTK4 viewer and bridge host for Windows 11 QEMU/KVM guest")]
struct Cli {
    /// Path to the QEMU VirtIO-serial chardev Unix stream socket [default: /tmp/winbridge.sock]
    #[arg(short, long)]
    socket: Option<PathBuf>,

    /// D-Bus address for QEMU Display1 (e.g. unix:path=/tmp/qemu-dbus.sock); defaults to the session bus
    #[arg(long)]
    dbus_addr: Option<String>,

    /// QEMU Machine Protocol socket for CD/DVD media and USB passthrough [default: /tmp/winbridge-qmp.sock]
    #[arg(long)]
    qmp_socket: Option<PathBuf>,

    /// QEMU USB controller id for passed-through devices; omit to let QEMU use its default bus
    #[arg(long)]
    usb_bus: Option<String>,

    /// Actively grab the keyboard while the pointer is over the VM canvas (off by default: a
    /// grab takes keys from the rest of the session, including other apps' input methods)
    #[arg(long)]
    grab_keyboard: bool,

    /// Script the viewer runs once at startup to launch the VM; its arguments come from config.toml
    #[arg(long, value_name = "PATH")]
    launch_script: Option<PathBuf>,

    /// Run in headless self-test mode without opening GTK window
    #[arg(long)]
    headless_test: bool,
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    info!("Starting WinBridge Host Viewer v{}", env!("CARGO_PKG_VERSION"));

    if cli.headless_test {
        info!("Running in headless self-test mode: OK");
        return glib::ExitCode::SUCCESS;
    }

    let overrides = config::CliOverrides {
        launch_script: cli.launch_script,
        socket: cli.socket,
        dbus_addr: cli.dbus_addr,
        qmp_socket: cli.qmp_socket,
        usb_bus: cli.usb_bus,
        grab_keyboard: cli.grab_keyboard,
    };
    let config = match config::load(overrides) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("winbridge-host: {}", e);
            return glib::ExitCode::FAILURE;
        }
    };
    info!("VirtIO-serial socket: {:?}", config.socket);

    // Initialize Tokio multithread runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to initialize Tokio runtime");

    clipboard::purge_staging();
    let _guard = rt.enter();

    let app = Application::builder()
        .application_id("org.winbridge.viewer")
        .build();


    app.connect_activate(move |app| {
        // A second activation (the desktop re-activating the app) only raises the open viewer.
        if ACTIVE_VIEWER.with(|cell| cell.borrow().is_some()) {
            with_viewer(|v| v.present());
            return;
        }
        let fb = Arc::new(Mutex::new(Framebuffer::default()));
        let dbus_conn = Arc::new(Mutex::new(None));

        let serial_slot: Arc<OnceLock<SerialClientHandle>> = Arc::new(OnceLock::new());
        let serial_for_requests = serial_slot.clone();
        let serial = SerialClient::start(config.socket.clone(), move |msg| match msg {
            Message::ResolutionResult {
                width,
                height,
                success,
                error,
            } => {
                if success {
                    info!("Guest accepted resolution: {}x{}", width, height);
                    // The title shows the guest size: ask for the new state.
                    refresh_guest_status(&serial_for_requests);
                } else {
                    warn!("Guest rejected resolution {}x{}: {:?}", width, height, error);
                }
            }
            Message::DpiScaleResult {
                scale_percent,
                success,
                error,
            } => {
                if success {
                    info!("Guest applied DPI scale: {}%", scale_percent);
                    refresh_guest_status(&serial_for_requests);
                } else {
                    warn!("Guest failed to apply DPI scale {}%: {:?}", scale_percent, error);
                }
            }
            msg @ (Message::ClipboardOffer { .. }
            | Message::ClipboardItem { .. }
            | Message::ClipFilesManifest { .. }) => {
                if let Some((clip_id, bundle)) = clipboard::CLIP_IN.handle(&msg) {
                    glib::idle_add_once(move || {
                        with_viewer(|v| v.on_guest_clipboard(clip_id, bundle));
                    });
                }
            }
            msg @ (Message::FilesBegin { .. }
            | Message::FileEntry { .. }
            | Message::FileChunk { .. }
            | Message::FilesEnd { .. }) => {
                // File streams answer either a guest drag fetch or a clipboard paste.
                if !dragout::handle(&msg) {
                    clipboard::CLIP_IN.handle_files(&msg);
                }
            }
            Message::GuestDragNone => {
                info!("Guest reports no drag in progress (GuestDragNone)");
                glib::idle_add_once(|| {
                    with_viewer(|v| v.on_guest_drag_none());
                });
            }
            Message::GuestDragOffer { drag_id, names, total_bytes, text } => {
                info!(
                    "GuestDragOffer received: id={} {} file(s), {} bytes, text={}",
                    drag_id,
                    names.len(),
                    total_bytes,
                    text.is_some()
                );
                glib::idle_add_once(move || {
                    with_viewer(|v| v.on_guest_drag_offer(drag_id, names, total_bytes, text));
                });
            }
            Message::DropResult { success, message } => {
                info!("Guest drop result: success={}, msg={}", success, message);
                glib::idle_add_once(move || {
                    with_viewer(|v| v.on_drop_result(success, message));
                });
            }
            Message::ClipFilesRequest { clip_id } => match serial_for_requests.get() {
                Some(serial) => clipboard::on_files_request(serial, clip_id),
                None => warn!("ClipFilesRequest {} arrived before the serial client was ready", clip_id),
            },
            Message::StatusReply {
                current_width,
                current_height,
                current_dpi,
                os_description,
            } => {
                info!(
                    "Guest status: {}x{} @ {}% DPI ({})",
                    current_width, current_height, current_dpi, os_description
                );
                glib::idle_add_once(move || {
                    with_viewer(|v| v.on_guest_status(current_width, current_height, current_dpi));
                });
            }
            _ => {}
        });

        let _ = serial_slot.set(serial.clone());
        let viewer = ViewerWindow::new(app, fb.clone(), serial, dbus_conn.clone(), &config);
        ACTIVE_VIEWER.with(|cell| {
            *cell.borrow_mut() = Some(viewer);
        });
        tray::start();

        // Launch mode: the viewer runs the configured script once, as its child.
        if let Some(spec) = &config.launch {
            if let Err(e) = launch::start(spec, &config) {
                with_viewer(|v| v.on_launch_exit(launch::LaunchOutcome::Failed(e)));
            }
        }

        // Callback triggered whenever a new D-Bus frame or dirty rectangle is received
        let on_update = Arc::new(|| {
            glib::idle_add_once(|| {
                with_viewer(|v| v.queue_draw());
            });
        });

        // Connects to QEMU's D-Bus display, retrying once a second until QEMU is up.
        let dbus_conn_clone = dbus_conn.clone();
        let dbus_addr = config.dbus_addr.clone();
        tokio::spawn(async move {
            let mut warned = false;
            loop {
                match DbusDisplayConnection::connect(dbus_addr.as_deref(), fb.clone(), on_update.clone()).await {
                    Ok(conn) => {
                        info!("Connected to QEMU D-Bus Display");
                        *dbus_conn_clone.lock().unwrap() = Some(conn);
                        glib::idle_add_once(|| {
                            with_viewer(|v| v.on_display_connected());
                        });
                        break;
                    }
                    Err(e) => {
                        if !warned {
                            warn!("QEMU D-Bus display not available yet ({}), retrying every second", e);
                            warned = true;
                        }
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });
    });

    // Arguments are already parsed by clap; do not let GApplication reject them.
    let exit = app.run_with_args::<&str>(&[]);
    // Blocking tasks must not hold the process open after the window closes.
    drop(_guard);
    rt.shutdown_timeout(Duration::from_secs(2));
    exit
}
