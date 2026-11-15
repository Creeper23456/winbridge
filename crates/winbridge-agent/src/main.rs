// SPDX-License-Identifier: GPL-3.0-or-later

// No console window when started at logon; see `logging`.
#![cfg_attr(windows, windows_subsystem = "windows")]

//! WinBridge Windows 11 Guest Agent.
//!
//! Bridges guest display resolution, DPI scaling, clipboard events, and file drop
//! reception to the Linux host viewer across a VirtIO-serial communication port.

mod channel;
mod clipboard;
mod display;
mod dnd;
mod dragipc;
mod dragout;
mod dragprobe;
mod gfx_inject;
mod hostfiles;
mod logging;
mod medium;
mod owner;

use channel::VirtioSerialStream;
use clipboard::{write_bundle, ClipboardWatcher, GuestOffer};
use hostfiles::HOST_FILES;
use dnd::DndManager;
use dragout::DragOut;
use std::sync::{Arc, Mutex};
#[allow(unused_imports)]
use tracing::{error, info, warn};
use winbridge_proto::clip::{self, Assembler, ClipBundle, FilesEvent};
use winbridge_proto::{read_frame, write_frame, Message};

fn main() {
    let has_console = logging::attach_console();
    // IFEO starts this agent as `<agent> --ifeo-debug <target command line>`. Dispatch before flag
    // parsing, so the target's own arguments are never read as agent flags.
    // The drag probe helper (started by the agent at medium integrity) has its own log and
    // takes nothing else from this program.
    let raw: Vec<String> = std::env::args().collect();
    if let Some(i) = raw.iter().position(|a| a == "--drag-probe") {
        logging::init_as(false, "drag-probe.log");
        let pipe = raw.get(i + 1).cloned().unwrap_or_default();
        let agent_pid = raw.get(i + 2).and_then(|p| p.parse().ok()).unwrap_or(0);
        std::process::exit(dragprobe::run(&pipe, agent_pid));
    }
    logging::init(has_console);
    if let Some(target_cmd) = gfx_inject::ifeo_debug_request() {
        std::process::exit(gfx_inject::run_debugger(&target_cmd) as i32);
    }
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("WinBridge Windows Guest Agent v{}", env!("CARGO_PKG_VERSION"));
        println!("Usage: winbridge-agent [OPTIONS]");
        println!();
        println!("Options:");
        println!("  --port <DEVICE_PATH>   VirtIO-serial port path (default: \\\\.\\Global\\org.winbridge.0)");
        println!("  --test-ping            Run protocol ping self-test and exit");
        println!("  --version, -V          Print version");
        println!("  --enable-gfx           Turn on the experimental GPU injection (writes IFEO entries; off by default)");
        println!("  --uninstall-gfx        Remove every IFEO entry the injection created, then exit");
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
    #[cfg(windows)]
    if let Some(i) = args.iter().position(|a| a == "--self-test-drag" || a == "--self-test-drag-hold") {
        let out = args.get(i + 1).cloned().unwrap_or_else(|| "drag-selftest.txt".into());
        std::process::exit(dragout::self_test(&out, args[i] == "--self-test-drag-hold"));
    }
    if args.iter().any(|a| a == "--uninstall-gfx") {
        gfx_inject::uninstall();
        return;
    }
    // The IFEO injection is experimental and has never run on a real guest: it stays off unless
    // the agent is started with --enable-gfx.
    if args.iter().any(|a| a == "--enable-gfx") {
        gfx_inject::start_watcher();
    }
    // Before the first file or folder is created: the user, not Administrators, owns them.
    owner::adopt_user_ownership();
    display::init_dpi_awareness();

    info!("Starting WinBridge Windows Guest Agent v{}", env!("CARGO_PKG_VERSION"));

    let port_arg = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str());
    let clip_staging = std::env::temp_dir().join("WinBridge").join("clip");
    let _ = std::fs::remove_dir_all(&clip_staging);

    let mut reader = VirtioSerialStream::open(port_arg);
    let stream_writer = Arc::new(Mutex::new(reader.clone()));

    // Clipboard changes are sent by a worker so a big copy never blocks the message loop.
    let (clip_tx, clip_rx) = std::sync::mpsc::channel::<ClipBundle>();
    let writer_for_clipboard = stream_writer.clone();
    let guest_offer = Arc::new(GuestOffer::default());
    let offer_for_clipboard = guest_offer.clone();
    std::thread::spawn(move || {
        for bundle in clip_rx {
            let clip_id = clipboard::next_id();
            info!(
                "Local clipboard changed (text={} html={} rtf={} image={} files={}), sending to host",
                bundle.text.is_some(),
                bundle.html.is_some(),
                bundle.rtf.is_some(),
                bundle.png.is_some() || bundle.bmp.is_some(),
                bundle.files.len()
            );
            // Remembered before the offer goes out: the host's request can follow immediately.
            offer_for_clipboard.remember(clip_id, &bundle.files);
            let result = bundle.send(clip_id, &mut |msg| {
                let mut writer = writer_for_clipboard.lock().map_err(|e| e.to_string())?;
                write_frame(&mut *writer, &msg).map_err(|e| e.to_string())
            });
            match result {
                Ok(warnings) => warnings.iter().for_each(|w| warn!("Clipboard: {}", w)),
                Err(e) => warn!("Clipboard send failed: {}", e),
            }
        }
    });
    // A guest application reads the content of host files: ask the host for them (once).
    let writer_for_requests = stream_writer.clone();
    hostfiles::set_requester(move |clip_id| {
        info!("Paste of host files (clip {}): asking the host for them", clip_id);
        let mut writer = writer_for_requests.lock().map_err(|e| e.to_string())?;
        write_frame(&mut *writer, &Message::ClipFilesRequest { clip_id }).map_err(|e| e.to_string())
    });
    let _watcher = ClipboardWatcher::start(move |bundle| {
        let _ = clip_tx.send(bundle);
    });

    let writer_for_drag = stream_writer.clone();
    let dragout = DragOut::start(Arc::new(move |msg| {
        let mut writer = writer_for_drag.lock().map_err(|e| e.to_string())?;
        write_frame(&mut *writer, &msg).map_err(|e| e.to_string())
    }));
    let mut clips = Assembler::new(clip_staging.clone());
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

    let mut file_milestone = 25u64;
    loop {
        // Read incoming frame from host
        // Reads use their own clone of the port handle: they must never hold the write lock.
        let msg = read_frame(&mut reader);

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

            Ok(msg @ (Message::ClipboardOffer { clip_id, .. }
            | Message::ClipboardItem { clip_id, .. }
            | Message::ClipFilesManifest { clip_id, .. })) => {
                if matches!(msg, Message::ClipboardOffer { .. }) {
                    // The old snapshot (and its staged files) is being replaced.
                    HOST_FILES.invalidate();
                }
                match clips.handle(&msg) {
                    Ok(Some(bundle)) => {
                        info!(
                            "Setting guest clipboard from host (text={} html={} rtf={} image={} files_offered={} items={})",
                            bundle.text.is_some(),
                            bundle.html.is_some(),
                            bundle.rtf.is_some(),
                            bundle.png.is_some() || bundle.bmp.is_some(),
                            bundle.files_offered,
                            bundle.files_manifest.len()
                        );
                        if bundle.files_offered {
                            HOST_FILES.offer(clip_id, &bundle.files_manifest);
                        }
                        if !write_bundle(bundle, clip_id) {
                            warn!("The guest clipboard could not be set");
                        }
                    }
                    Ok(None) => {}
                    Err(e) => warn!("Host clipboard transfer failed: {}", e),
                }
            }

            Ok(msg @ (Message::FilesBegin { stream_id, .. }
            | Message::FileEntry { stream_id, .. }
            | Message::FileChunk { stream_id, .. }
            | Message::FilesEnd { stream_id, .. })) => match clips.handle_files(&msg) {
                Ok(FilesEvent::Progress { done, total }) => {
                    if done == 0 {
                        info!("Receiving host files of clip {}: {} bytes", stream_id, total);
                        file_milestone = 25;
                    } else if total > 0 && done * 100 / total >= file_milestone {
                        info!("Clip {}: {} of {} bytes received ({}%)", stream_id, done, total, done * 100 / total);
                        file_milestone = (done * 100 / total / 25 + 1) * 25;
                    }
                    match &msg {
                        Message::FileChunk { index, offset, data, .. } => {
                            HOST_FILES.entry_data(stream_id, *index, offset + data.len() as u64)
                        }
                        _ => HOST_FILES.touch(stream_id),
                    }
                }
                Ok(FilesEvent::Ignored) => {
                    if let Message::FileEntry { index, size, .. } = &msg {
                        if let Some(path) = clips.entry_path(*index) {
                            HOST_FILES.entry_begun(stream_id, *index, path, *size);
                        }
                    }
                }
                Ok(FilesEvent::Done { clip_id, paths }) => {
                    info!("Host files of clip {} received: {} item(s) staged", clip_id, paths.len());
                    HOST_FILES.finish(clip_id, Ok(()));
                }
                Err(e) => {
                    warn!("Host file transfer of clip {} failed: {}", stream_id, e);
                    HOST_FILES.finish(stream_id, Err(e));
                }
            },

            Ok(Message::ClipFilesRequest { clip_id }) => {
                spawn_files_sender(stream_writer.clone(), guest_offer.clone(), clip_id)
            }

            Ok(Message::GuestDragProbe { x, y }) => dragout.probe(x, y),
            Ok(Message::GuestDragFetch { drag_id }) => dragout.fetch(drag_id),
            Ok(Message::GuestDragEnd { .. }) => dragout.end(),

            Ok(Message::DropBegin { transfer_id, x, y, file_count }) => {
                if let Err(e) = dnd.begin_drop(transfer_id, x, y, file_count) {
                    warn!("Could not begin drop operation: {}", e);
                    let reply = Message::DropResult { success: false, message: e };
                    if let Ok(mut writer) = stream_writer.lock() {
                        let _ = write_frame(&mut *writer, &reply);
                    }
                }
            }

            Ok(Message::DropFileStart { transfer_id, operation_id, filename, total_bytes }) => {
                if let Err(e) = dnd.start_file(transfer_id, &filename, total_bytes, operation_id) {
                    warn!("Could not start dropped file: {}", e);
                    let reply = Message::DropResult { success: false, message: e };
                    if let Ok(mut writer) = stream_writer.lock() {
                        let _ = write_frame(&mut *writer, &reply);
                    }
                }
            }

            Ok(Message::DropFileChunk { transfer_id, offset, data }) => {
                if let Err(e) = dnd.write_chunk(transfer_id, offset, &data) {
                    warn!("Dropped file chunk failed: {}", e);
                }
            }

            Ok(Message::DropFileFinish { transfer_id, success, error: _ }) => {
                let (ok, msg) = match dnd.finish_file(transfer_id, success) {
                    Ok(Some(message)) => (true, message),
                    Ok(None) => (true, "File received; waiting for remaining files".into()),
                    Err(e) => (false, e),
                };
                let reply = Message::DropResult { success: ok, message: msg };
                if let Ok(mut writer) = stream_writer.lock() {
                    let _ = write_frame(&mut *writer, &reply);
                }
            }

            Ok(Message::DropText { text, x, y }) => {
                let (ok, msg) = match dnd.handle_drop_text(&text, x, y) {
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

/// Answers a `ClipFilesRequest`: streams the files of the newest guest copy to the host on a
/// worker thread, so the message loop (Pong, status, drops) keeps running. The writer lock is
/// taken per message only.
fn spawn_files_sender(writer: Arc<Mutex<VirtioSerialStream>>, offer: Arc<GuestOffer>, clip_id: u32) {
    let paths = match offer.claim(clip_id) {
        Ok(paths) => paths,
        Err(why) => {
            warn!("Host asked for clipboard files, ignored: {}", why);
            return;
        }
    };
    let spawned = std::thread::Builder::new().name("winbridge-clip-files".into()).spawn({
        let offer = offer.clone();
        move || {
            info!("Host pastes: streaming {} copied path(s) of clip {}", paths.len(), clip_id);
            let started = std::time::Instant::now();
            let mut send = |msg: Message| {
                let mut writer = writer.lock().map_err(|e| e.to_string())?;
                write_frame(&mut *writer, &msg).map_err(|e| e.to_string())
            };
            let mut next_milestone = 25u64;
            let result = clip::send_files(clip_id, &paths, &mut send, |sent, total| {
                if total > 0 && sent * 100 / total >= next_milestone {
                    info!("Clip {}: {} of {} bytes sent ({}%)", clip_id, sent, total, sent * 100 / total);
                    next_milestone = (sent * 100 / total / 25 + 1) * 25;
                }
            });
            match result {
                Ok(()) => info!("Clip {}: files sent in {:.1} s", clip_id, started.elapsed().as_secs_f32()),
                Err(e) => {
                    warn!("Clip {}: files could not be sent: {}", clip_id, e);
                    // A stream that never began still needs an ending for the host to give up.
                    let _ = send(Message::FilesEnd { stream_id: clip_id, success: false, error: Some(e) });
                }
            }
            offer.release(clip_id);
        }
    });
    if let Err(e) = spawned {
        warn!("Could not start the file sender thread: {}", e);
        offer.release(clip_id);
    }
}
