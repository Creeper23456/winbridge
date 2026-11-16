// SPDX-License-Identifier: GPL-3.0-or-later

//! QEMU D-Bus Display (`org.qemu.Display1`) interface integration.

use crate::input::InputPipeline;
use crate::stats;
use std::fs::File;
use std::os::fd::OwnedFd as StdOwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use tracing::{error, info};
use zbus::zvariant::OwnedFd;
use zbus::{connection, proxy};

/// Guest mouse cursor image pushed by QEMU (premultiplied BGRA, little-endian ARGB32).
#[derive(Clone)]
pub struct CursorSprite {
    pub width: i32,
    pub height: i32,
    pub hot_x: i32,
    pub hot_y: i32,
    pub data: Vec<u8>,
}

/// Shared framebuffer storing the current guest display scanout.
#[derive(Clone)]
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// Pixels received over D-Bus; empty while `mapped` is in use.
    pub data: Vec<u8>,
    /// The scanout as shared memory, when QEMU shares it.
    pub mapped: Option<Arc<MappedScanout>>,
    /// Latest guest cursor image, if the guest uses a hardware cursor.
    pub cursor: Option<CursorSprite>,
    /// Whether the guest currently shows its cursor.
    pub cursor_on: bool,
    /// Bumped whenever `cursor` or `cursor_on` changes.
    pub cursor_gen: u64,
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
            mapped: None,
            cursor: None,
            cursor_on: true,
            cursor_gen: 0,
        }
    }
}

/// A guest scanout in memory QEMU shares (`org.qemu.Display1.Listener.Unix.Map`). The pixels are
/// read where QEMU draws them: nothing is sent over D-Bus and nothing is copied before drawing.
/// The guest changes the memory at any time, so a frame can show a half-finished update.
pub struct MappedScanout {
    map: memmap2::MmapRaw,
}

impl MappedScanout {
    /// Maps `height` rows of `stride` bytes at `offset` in `file`, which must be that long.
    pub fn open(file: &File, offset: u64, width: u32, height: u32, stride: u32) -> Result<Self, String> {
        if width == 0 || height == 0 || (stride as u64) < width as u64 * 4 {
            return Err(format!("a {}x{} scanout with stride {} makes no sense", width, height, stride));
        }
        let len = stride as u64 * height as u64;
        let size = file.metadata().map_err(|e| e.to_string())?.len();
        // Reading past the end of the file would end the process with SIGBUS.
        if offset.checked_add(len).is_none_or(|end| end > size) {
            return Err(format!("the scanout needs {} bytes at {}, the memory has {}", len, offset, size));
        }
        let map = memmap2::MmapOptions::new()
            .offset(offset)
            .len(len as usize)
            .map_raw_read_only(file)
            .map_err(|e| e.to_string())?;
        Ok(Self { map })
    }

    /// First pixel. Valid for as long as `self` lives; the memory is read only.
    pub fn as_ptr(&self) -> *const u8 {
        self.map.as_ptr()
    }
}

/// What to draw the canvas from.
pub enum Pixels {
    /// Pixels sent over D-Bus (a copy).
    Owned(Vec<u8>),
    /// QEMU's own memory.
    Mapped(Arc<MappedScanout>),
}

// Proxies for QEMU D-Bus Display interfaces on /org/qemu/Display1/Console_0

#[proxy(
    interface = "org.qemu.Display1.Console",
    default_service = "org.qemu",
    default_path = "/org/qemu/Display1/Console_0"
)]
pub trait Console {
    fn register_listener(&self, listener: OwnedFd) -> zbus::Result<()>;
    #[zbus(name = "SetUIInfo")]
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
    /// True while QEMU's active pointing device is absolute (usb-tablet); `RelMotion` is then
    /// refused, and `SetAbsPosition` is refused when it is false.
    #[zbus(property)]
    fn is_absolute(&self) -> zbus::Result<bool>;
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

const LISTENER_PATH: &str = "/org/qemu/Display1/Listener";
/// The interface that lets QEMU share the scanout as memory instead of sending the pixels.
const MAP_INTERFACE: &str = "org.qemu.Display1.Listener.Unix.Map";

/// Listener server receiving peer-to-peer display frames from QEMU.
pub struct DisplayListener {
    pub fb: Arc<Mutex<Framebuffer>>,
    pub on_update: Arc<dyn Fn() + Send + Sync + 'static>,
}

#[zbus::interface(name = "org.qemu.Display1.Listener")]
impl DisplayListener {
    /// What else the listener implements. QEMU shares the scanout as memory when it finds
    /// [`MAP_INTERFACE`] here and the guest's surface can be shared (virtio-gpu 2D can); otherwise
    /// it sends the pixels with `Scanout` and `Update`.
    #[zbus(property)]
    fn interfaces(&self) -> Vec<String> {
        vec![MAP_INTERFACE.to_string()]
    }

    fn scanout(&self, width: u32, height: u32, stride: u32, _pixman_format: u32, data: &[u8]) {
        let t = stats::begin();
        if let Ok(mut fb) = self.fb.lock() {
            fb.width = width;
            fb.height = height;
            fb.stride = stride;
            fb.data = data.to_vec();
            fb.mapped = None;
        }
        stats::scanout(t);
        (self.on_update)();
    }

    // `data` is borrowed from the message: a `Vec<u8>` would be decoded byte by byte (about
    // 10 ms per MiB), which cost a third of a core at the rates a game produces.
    fn update(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stride: u32,
        _pixman_format: u32,
        data: &[u8],
    ) {
        let t = stats::begin();
        let (pixels, bytes) = (width.max(0) as u64 * height.max(0) as u64, data.len() as u64);
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
        stats::update(t, pixels, bytes);
        (self.on_update)();
    }

    fn mouse_set(&self, _x: i32, _y: i32, on: i32) {
        let changed = match self.fb.lock() {
            Ok(mut fb) => {
                let on = on != 0;
                if fb.cursor_on != on {
                    fb.cursor_on = on;
                    fb.cursor_gen += 1;
                    true
                } else {
                    false
                }
            }
            Err(_) => false,
        };
        if changed {
            (self.on_update)();
        }
    }

    fn cursor_define(&self, width: i32, height: i32, hot_x: i32, hot_y: i32, data: Vec<u8>) {
        if let Ok(mut fb) = self.fb.lock() {
            fb.cursor = Some(CursorSprite {
                width,
                height,
                hot_x,
                hot_y,
                data,
            });
            fb.cursor_gen += 1;
        }
        (self.on_update)();
    }
    fn disable(&self) {}
}

/// The shared-memory half of the Listener: QEMU hands over the memory its scanout lives in once,
/// and then only says which rectangle changed.
pub struct DisplayListenerMap {
    pub fb: Arc<Mutex<Framebuffer>>,
    pub on_update: Arc<dyn Fn() + Send + Sync + 'static>,
}

#[zbus::interface(name = "org.qemu.Display1.Listener.Unix.Map")]
impl DisplayListenerMap {
    fn scanout_map(
        &self,
        handle: OwnedFd,
        offset: u32,
        width: u32,
        height: u32,
        stride: u32,
        _pixman_format: u32,
    ) -> zbus::fdo::Result<()> {
        let t = stats::begin();
        let file = File::from(StdOwnedFd::from(handle));
        let map = MappedScanout::open(&file, offset as u64, width, height, stride).map_err(|e| {
            error!("Cannot map QEMU's scanout: {}", e);
            zbus::fdo::Error::Failed(e)
        })?;
        info!("Guest scanout {}x{} is shared memory (stride {}, offset {})", width, height, stride, offset);
        if let Ok(mut fb) = self.fb.lock() {
            fb.width = width;
            fb.height = height;
            fb.stride = stride;
            fb.data = Vec::new();
            fb.mapped = Some(Arc::new(map));
        }
        stats::scanout(t);
        (self.on_update)();
        Ok(())
    }

    fn update_map(&self, _x: i32, _y: i32, width: i32, height: i32) {
        let t = stats::begin();
        stats::update(t, width.max(0) as u64 * height.max(0) as u64, 0);
        (self.on_update)();
    }
}

/// Handle to active D-Bus display connection.
#[allow(dead_code)]
pub struct DbusDisplayConnection {
    pub mouse: MouseProxy<'static>,
    pub console: ConsoleProxy<'static>,
    /// Every pointer and key event for the guest goes through this one ordered queue.
    pub input: InputPipeline,
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

        // Serve both Listener interfaces on our end of the socketpair
        let listener = DisplayListener { fb: fb.clone(), on_update: on_update.clone() };
        let shared = DisplayListenerMap { fb, on_update };
        tokio::spawn(async move {
            let p2p_res = match connection::Builder::unix_stream(host_tokio_stream)
                .p2p()
                .serve_at(LISTENER_PATH, listener)
                .and_then(|builder| builder.serve_at(LISTENER_PATH, shared))
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

        let input = InputPipeline::spawn(mouse.clone(), keyboard);

        Ok(Self {
            mouse,
            console,
            input,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn memory(bytes: usize) -> File {
        let path = std::env::temp_dir().join(format!("wb-scanout-{}-{}", std::process::id(), bytes));
        let mut file = File::options().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let data: Vec<u8> = (0..bytes).map(|i| (i % 251) as u8).collect();
        file.write_all(&data).unwrap();
        file
    }

    #[test]
    fn a_scanout_is_read_where_it_lies_in_the_memory() {
        // An offset that is no multiple of the page size, a stride wider than the row.
        let file = memory(64 * 1024);
        let map = MappedScanout::open(&file, 5000, 10, 4, 48).unwrap();
        let first = unsafe { std::slice::from_raw_parts(map.as_ptr(), 48 * 4) };
        let expected: Vec<u8> = (5000..5000 + 48 * 4).map(|i| (i % 251) as u8).collect();
        assert_eq!(first, &expected[..]);
    }

    #[test]
    fn a_scanout_that_does_not_fit_the_memory_is_refused() {
        let file = memory(4096);
        assert!(MappedScanout::open(&file, 0, 32, 32, 128).is_ok(), "needs 4096 bytes: fits exactly");
        assert!(MappedScanout::open(&file, 1, 32, 32, 128).is_err(), "one byte past the end");
        assert!(MappedScanout::open(&file, u64::MAX, 1, 1, 4).is_err(), "offset overflow");
        assert!(MappedScanout::open(&file, 0, 100, 4, 100).is_err(), "stride narrower than a row");
        assert!(MappedScanout::open(&file, 0, 0, 4, 16).is_err(), "no width");
    }
}
