// SPDX-License-Identifier: GPL-3.0-or-later

//! WinBridge Windows 11 Guest Agent.
//!
//! Bridges guest display resolution, DPI scaling, clipboard events, and file drop
//! reception to the Linux host viewer across a VirtIO-serial communication port.

mod channel;
mod clipboard;
mod display;
mod dnd;

use channel::VirtioSerialStream;
use clipboard::{write_clipboard_text, ClipboardWatcher};
use dnd::DndManager;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
#[allow(unused_imports)]
use tracing::{error, info, warn};
use winbridge_proto::{read_frame, write_frame, ClipboardFormat, Message};

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("WinBridge Windows Guest Agent v{}", env!("CARGO_PKG_VERSION"));
        println!("Usage: winbridge-agent [OPTIONS]");
        println!();
        println!("Options:");
        println!("  --port <DEVICE_PATH>   VirtIO-serial port path (default: \\\\.\\Global\\org.winbridge.0)");
        println!("  --test-ping            Run protocol ping self-test and exit");
        println!("  --version, -V          Print version");
        println!("  --help, -h             Print help");
        return;
    }

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("winbridge-agent v{}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if args.iter().any(|a| a == "--test-ping") {
        let ping = Message::Ping {
            seq: 1,
            timestamp_ms: 1000,
        };
        let frame = winbridge_proto::encode_frame(&ping).expect("encode ping frame");
        let mut cursor = std::io::Cursor::new(frame);
        let decoded = winbridge_proto::read_frame(&mut cursor).expect("read ping frame");
        assert_eq!(ping, decoded);

        let pong = Message::Pong {
            seq: 1,
            timestamp_ms: 1000,
        };
        let pong_frame = winbridge_proto::encode_frame(&pong).expect("encode pong frame");
        let mut pong_cursor = std::io::Cursor::new(pong_frame);
        let pong_decoded = winbridge_proto::read_frame(&mut pong_cursor).expect("read pong frame");
        assert_eq!(pong, pong_decoded);

        println!("PING_TEST_OK: WinBridge agent protocol verified!");
        return;
    }
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("Starting WinBridge Windows Guest Agent v{}", env!("CARGO_PKG_VERSION"));

    let port_arg = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str());
    let suppress_echo = Arc::new(AtomicBool::new(false));

    let stream = VirtioSerialStream::open(port_arg);
    let stream_writer = Arc::new(Mutex::new(stream));
    let writer_for_clipboard = stream_writer.clone();

    // Start clipboard monitoring thread
    let suppress_echo_clone = suppress_echo.clone();
    let _watcher = ClipboardWatcher::start(
        move |text| {
            info!("Local clipboard changed ({} chars), announcing to host", text.chars().count());
            let msg = Message::ClipboardPayload {
                format: ClipboardFormat::Text,
                data: text.into_bytes(),
            };
            if let Ok(mut writer) = writer_for_clipboard.lock() {
                let _ = write_frame(&mut *writer, &msg);
            }
        },
        suppress_echo_clone,
    );

    let mut dnd = DndManager::new();

    // Notify host agent is alive with initial status
    {
        let (w, h, dpi) = display::get_display_info();
        let initial_status = Message::StatusReply {
            current_width: w,
            current_height: h,
            current_dpi: dpi,
            os_description: "Windows 11 x64".to_string(),
        };
        if let Ok(mut writer) = stream_writer.lock() {
            let _ = write_frame(&mut *writer, &initial_status);
        }
    }

    info!("Agent ready, listening for host messages...");

    loop {
        // Read incoming frame from host
        let msg = {
            let mut writer = match stream_writer.lock() {
                Ok(w) => w,
                Err(_) => break,
            };
            read_frame(&mut *writer)
        };

        match msg {
            Ok(Message::Ping { seq, timestamp_ms }) => {
                let reply = Message::Pong { seq, timestamp_ms };
                if let Ok(mut writer) = stream_writer.lock() {
                    let _ = write_frame(&mut *writer, &reply);
                }
            }

            Ok(Message::SetResolution { width, height }) => {
                info!("Received SetResolution: {}x{}", width, height);
                let (success, error) = match display::set_resolution(width, height) {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(e)),
                };
                let reply = Message::ResolutionResult {
                    width,
                    height,
                    success,
                    error,
                };
                if let Ok(mut writer) = stream_writer.lock() {
                    let _ = write_frame(&mut *writer, &reply);
                }
            }

            Ok(Message::SetDpiScale { scale_percent }) => {
                info!("Received SetDpiScale: {}%", scale_percent);
                let (success, error) = match display::set_dpi_scale(scale_percent) {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(e)),
                };
                let reply = Message::DpiScaleResult {
                    scale_percent,
                    success,
                    error,
                };
                if let Ok(mut writer) = stream_writer.lock() {
                    let _ = write_frame(&mut *writer, &reply);
                }
            }

            Ok(Message::QueryStatus) => {
                let (w, h, dpi) = display::get_display_info();
                let reply = Message::StatusReply {
                    current_width: w,
                    current_height: h,
                    current_dpi: dpi,
                    os_description: "Windows 11 x64".to_string(),
                };
                if let Ok(mut writer) = stream_writer.lock() {
                    let _ = write_frame(&mut *writer, &reply);
                }
            }

            Ok(Message::ClipboardPayload { format, data }) => {
                if format == ClipboardFormat::Text {
                    if let Ok(text) = String::from_utf8(data) {
                        info!("Setting guest clipboard from host ({} chars)", text.chars().count());
                        suppress_echo.store(true, Ordering::SeqCst);
                        write_clipboard_text(&text);
                    }
                }
            }

            Ok(Message::DropFileStart { transfer_id, filename, total_bytes }) => {
                let _ = dnd.start_file(transfer_id, &filename, total_bytes);
            }

            Ok(Message::DropFileChunk { transfer_id, offset, data }) => {
                let _ = dnd.write_chunk(transfer_id, offset, &data);
            }

            Ok(Message::DropFileFinish { transfer_id, success, error: _ }) => {
                let (ok, msg) = match dnd.finish_file(transfer_id, success) {
                    Ok(path) => (true, format!("Saved to {}", path)),
                    Err(e) => (false, e),
                };
                let reply = Message::DropResult { success: ok, message: msg };
                if let Ok(mut writer) = stream_writer.lock() {
                    let _ = write_frame(&mut *writer, &reply);
                }
            }

            Ok(Message::DropText { text }) => {
                let (ok, msg) = match dnd.handle_drop_text(&text) {
                    Ok(path) => (true, format!("Text saved to {}", path)),
                    Err(e) => (false, e),
                };
                let reply = Message::DropResult { success: ok, message: msg };
                if let Ok(mut writer) = stream_writer.lock() {
                    let _ = write_frame(&mut *writer, &reply);
                }
            }

            Err(e) => {
                warn!("Stream read error: {}, reconnecting...", e);
                std::thread::sleep(std::time::Duration::from_millis(500));
            }

            _ => {}
        }
    }
}
