// SPDX-License-Identifier: GPL-3.0-or-later

//! WinBridge Host Viewer Application.
//!
//! Linux GTK4 viewer and bridge host connecting to QEMU's D-Bus display and
//! VirtIO-serial communication channel for Windows 11 guest synchronization.

mod dbus_display;
mod keymap;
mod serial_client;
mod ui;

use clap::Parser;
use dbus_display::{DbusDisplayConnection, Framebuffer};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::Application;
use serial_client::SerialClient;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};
use ui::ViewerWindow;
use winbridge_proto::{ClipboardFormat, Message};

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

#[derive(Parser, Debug)]
#[command(name = "winbridge-host")]
#[command(author = "Creeper23456 <Creeper23456@outlook.com>")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "Linux GTK4 viewer and bridge host for Windows 11 QEMU/KVM guest")]
struct Cli {
    /// Path to QEMU VirtIO-serial chardev Unix stream socket
    #[arg(short, long, default_value = "/tmp/winbridge.sock")]
    socket: PathBuf,

    /// D-Bus address for QEMU Display1 (e.g. unix:path=/tmp/qemu-dbus.sock, defaults to session bus)
    #[arg(long)]
    dbus_addr: Option<String>,

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
    info!("VirtIO-serial socket: {:?}", cli.socket);

    if cli.headless_test {
        info!("Running in headless self-test mode: OK");
        return glib::ExitCode::SUCCESS;
    }

    // Initialize Tokio multithread runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to initialize Tokio runtime");

    let _guard = rt.enter();

    let app = Application::builder()
        .application_id("org.winbridge.viewer")
        .build();

    let socket_path = cli.socket;
    let dbus_addr = cli.dbus_addr;

    app.connect_activate(move |app| {
        let fb = Arc::new(Mutex::new(Framebuffer::default()));
        let dbus_conn = Arc::new(Mutex::new(None));

        let serial = SerialClient::start(socket_path.clone(), move |msg| match msg {
            Message::ResolutionResult {
                width,
                height,
                success,
                error,
            } => {
                if success {
                    info!("Guest accepted resolution: {}x{}", width, height);
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
                } else {
                    warn!("Guest failed to apply DPI scale {}%: {:?}", scale_percent, error);
                }
            }
            Message::ClipboardPayload { format, data } => {
                if format == ClipboardFormat::Text {
                    if let Ok(text) = String::from_utf8(data) {
                        glib::idle_add_once(move || {
                            with_viewer(|v| v.on_guest_clipboard(text));
                        });
                    }
                }
            }
            Message::DropResult { success, message } => {
                info!("Guest drop result: success={}, msg={}", success, message);
            }
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
            }
            _ => {}
        });

        let viewer = ViewerWindow::new(app, fb.clone(), serial, dbus_conn.clone());
        ACTIVE_VIEWER.with(|cell| {
            *cell.borrow_mut() = Some(viewer);
        });

        // Callback triggered whenever a new D-Bus frame or dirty rectangle is received
        let on_update = Arc::new(|| {
            glib::idle_add_once(|| {
                with_viewer(|v| v.queue_draw());
            });
        });

        // Spawn D-Bus connection worker
        let dbus_conn_clone = dbus_conn.clone();
        let dbus_addr_clone = dbus_addr.clone();
        tokio::spawn(async move {
            match DbusDisplayConnection::connect(
                dbus_addr_clone.as_deref(),
                fb,
                on_update,
            )
            .await
            {
                Ok(conn) => {
                    info!("Connected to QEMU D-Bus Display");
                    *dbus_conn_clone.lock().unwrap() = Some(conn);
                }
                Err(e) => {
                    warn!(
                        "QEMU D-Bus display connection failed ({}), waiting for QEMU launch...",
                        e
                    );
                }
            }
        });
    });

    app.run()
}
