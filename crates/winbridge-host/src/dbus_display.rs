// SPDX-License-Identifier: GPL-3.0-or-later

//! QEMU D-Bus Display (`org.qemu.Display1`) interface integration.

use std::os::fd::OwnedFd as StdOwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use tracing::{error, info};
use zbus::zvariant::OwnedFd;
use zbus::{connection, proxy};

/// Shared framebuffer storing the current guest display scanout.
#[derive(Clone)]
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
}

impl Default for Framebuffer {
    fn default() -> Self {
        // Initial fallback 1280x720 blank frame
        let width = 1280;
        let height = 720;
        let stride = width * 4;
        let data = vec![0x20; (stride * height) as usize]; // dark gray background
        Self {
            width,
            height,
            stride,
            data,
        }
    }
}

// Proxies for QEMU D-Bus Display interfaces on /org/qemu/Display1/Console_0

#[proxy(
    interface = "org.qemu.Display1.Console",
    default_service = "org.qemu",
    default_path = "/org/qemu/Display1/Console_0"
)]
pub trait Console {
    fn register_listener(&self, listener: OwnedFd) -> zbus::Result<()>;
    fn set_ui_info(
        &self,
        width_mm: u16,
        height_mm: u16,
        xoff: i32,
        yoff: i32,
        width: u32,
        height: u32,
    ) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.qemu.Display1.Mouse",
    default_service = "org.qemu",
    default_path = "/org/qemu/Display1/Console_0"
)]
pub trait Mouse {
    fn press(&self, button: u32) -> zbus::Result<()>;
    fn release(&self, button: u32) -> zbus::Result<()>;
    fn set_abs_position(&self, x: u32, y: u32) -> zbus::Result<()>;
    fn rel_motion(&self, dx: i32, dy: i32) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.qemu.Display1.Keyboard",
    default_service = "org.qemu",
    default_path = "/org/qemu/Display1/Console_0"
)]
pub trait Keyboard {
    fn press(&self, keycode: u32) -> zbus::Result<()>;
    fn release(&self, keycode: u32) -> zbus::Result<()>;
}

/// Listener server receiving peer-to-peer display frames from QEMU.
pub struct DisplayListener {
    pub fb: Arc<Mutex<Framebuffer>>,
    pub on_update: Arc<dyn Fn() + Send + Sync + 'static>,
}

#[zbus::interface(name = "org.qemu.Display1.Listener")]
impl DisplayListener {
    fn scanout(&self, width: u32, height: u32, stride: u32, _pixman_format: u32, data: Vec<u8>) {
        if let Ok(mut fb) = self.fb.lock() {
            fb.width = width;
            fb.height = height;
            fb.stride = stride;
            fb.data = data;
        }
        (self.on_update)();
    }

    fn update(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stride: u32,
        _pixman_format: u32,
        data: Vec<u8>,
    ) {
        if let Ok(mut fb) = self.fb.lock() {
            let fb_stride = fb.stride as usize;
            let src_stride = stride as usize;
            let w_bytes = (width as usize) * 4;

            for row in 0..(height as usize) {
                let dst_y = (y as usize) + row;
                let dst_offset = dst_y * fb_stride + (x as usize) * 4;
                let src_offset = row * src_stride;

                if dst_offset + w_bytes <= fb.data.len() && src_offset + w_bytes <= data.len() {
                    fb.data[dst_offset..dst_offset + w_bytes]
                        .copy_from_slice(&data[src_offset..src_offset + w_bytes]);
                }
            }
        }
        (self.on_update)();
    }

    fn mouse_set(&self, _x: i32, _y: i32, _on: i32) {}
    fn cursor_define(
        &self,
        _width: i32,
        _height: i32,
        _hot_x: i32,
        _hot_y: i32,
        _data: Vec<u8>,
    ) {
    }
    fn disable(&self) {}
}

/// Handle to active D-Bus display connection.
#[allow(dead_code)]
pub struct DbusDisplayConnection {
    pub mouse: MouseProxy<'static>,
    pub keyboard: KeyboardProxy<'static>,
    pub console: ConsoleProxy<'static>,
}

impl DbusDisplayConnection {
    /// Establishes connection to QEMU's D-Bus display, registering the listener.
    pub async fn connect(
        dbus_addr: Option<&str>,
        fb: Arc<Mutex<Framebuffer>>,
        on_update: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let conn = if let Some(addr) = dbus_addr {
            info!("Connecting to QEMU D-Bus address: {}", addr);
            connection::Builder::address(addr)?.build().await?
        } else {
            info!("Connecting to session D-Bus bus");
            connection::Connection::session().await?
        };

        let console = ConsoleProxy::new(&conn).await?;
        let mouse = MouseProxy::new(&conn).await?;
        let keyboard = KeyboardProxy::new(&conn).await?;

        // Create socketpair for peer-to-peer listener connection
        let (host_stream, qemu_stream) = UnixStream::pair()?;
        let qemu_fd = OwnedFd::from(StdOwnedFd::from(qemu_stream));
        let host_tokio_stream = tokio::net::UnixStream::from_std(host_stream)?;

        // Register listener with QEMU Console
        console.register_listener(qemu_fd).await?;
        info!("Registered P2P listener with QEMU Console_0");

        // Serve Listener interface on our end of the socketpair
        let listener = DisplayListener { fb, on_update };
        tokio::spawn(async move {
            let p2p_res = match connection::Builder::unix_stream(host_tokio_stream)
                .p2p()
                .serve_at("/org/qemu/Display1/Listener", listener)
            {
                Ok(builder) => builder.build().await,
                Err(e) => Err(e),
            };

            match p2p_res {
                Ok(p2p_conn) => {
                    info!("P2P D-Bus listener active");
                    // Keep connection alive
                    let _ = std::future::pending::<()>().await;
                    drop(p2p_conn);
                }
                Err(e) => {
                    error!("Failed to establish P2P listener connection: {}", e);
                }
            }
        });

        Ok(Self {
            mouse,
            keyboard,
            console,
        })
    }
}
