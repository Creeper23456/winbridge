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
#[cfg(windows)]
mod ivshmem;
mod hostfiles;
mod logging;
mod medium;
mod owner;

use channel::{Outgoing, VirtioSerialStream};
use clipboard::{write_bundle, ClipboardWatcher, GuestOffer};
use hostfiles::HOST_FILES;
use dnd::DndManager;
use dragout::DragOut;
use std::sync::{Arc, Mutex};
#[allow(unused_imports)]
use tracing::{error, info, warn};
use winbridge_proto::clip::{self, Assembler, ClipBundle, FilesEvent};
use winbridge_proto::ring::{Endpoint, Inbox, LinkConfig, Pop};
use winbridge_proto::{caps, Message};

fn main() {
    let has_console = logging::attach_console();
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
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("WinBridge Windows Guest Agent v{}", env!("CARGO_PKG_VERSION"));
        println!("Usage: winbridge-agent [OPTIONS]");
        println!();
        println!("Options:");
        println!("  --port <DEVICE_PATH>   VirtIO-serial port path (default: \\\\.\\Global\\org.winbridge.0)");
        println!("  --test-ping            Run protocol ping self-test and exit");
        println!("  --version, -V          Print version");
        println!("  --install-ivshmem <DIR>  Install the signed ivshmem driver package from DIR (add --force to replace another driver), then exit");
        println!("  --uninstall-ivshmem    Remove the ivshmem driver package this tool installed, then exit");
        println!("  --ivshmem-status       Print ivshmem devices, DACLs and bound drivers (read-only), then exit");
        println!("  --ivshmem-selftest [user]  Shared-memory device self-test (build VM), then exit");
        println!("  --ivshmem-output <FILE>  With the ivshmem commands: also write their output to FILE");
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
    #[cfg(windows)]
    if let Some(code) = ivshmem::run_cli(&args) {
        std::process::exit(code);
    }
    // Before the first file or folder is created: the user, not Administrators, owns them.
    owner::adopt_user_ownership();
    display::init_dpi_awareness();

    info!("Starting WinBridge Windows Guest Agent v{}", env!("CARGO_PKG_VERSION"));
    // The serial ports of the Venus slots are created for administrators only; the Vulkan
    // processes of the interactive user must open them. The ports exist from boot, but the driver
    // may still be starting, so a missing one is retried for a while.
    #[cfg(windows)]
    std::thread::spawn(|| {
        for _ in 0..30 {
            match ivshmem::open_venus_ports() {
                Ok(0) => std::thread::sleep(std::time::Duration::from_secs(1)),
                Ok(n) => {
                    info!("Opened {} Venus slot port(s) to the interactive user", n);
                    return;
                }
                Err(e) => {
                    warn!("Venus slot ports: {}", e);
                    return;
                }
            }
        }
    });

    let port_arg = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str());
    let clip_staging = std::env::temp_dir().join("WinBridge").join("clip");
    let _ = std::fs::remove_dir_all(&clip_staging);

    let reader = VirtioSerialStream::open(port_arg);
    // One inbox for everything the host sends (serial port and lanes); one outgoing path that
    // picks a lane when shared memory is active and the serial port otherwise.
    let serial = Arc::new(Mutex::new(reader.clone()));
    let inbox = Inbox::new(64, 1024, 4);
    let endpoint = Endpoint::new(channel::serial_sender(serial.clone()), inbox.clone(), LinkConfig::default());
    let out = Outgoing::new(endpoint.clone(), serial);
    // Keepalives arriving on the control lane are answered by the lane's reader thread: a
    // dispatch loop busy with a slow disk write must not delay the Pong.
    endpoint.set_fast_path(Arc::new({
        let out = out.clone();
        move |msg| match msg {
            Message::Ping { seq, timestamp_ms } => {
                let _ = out.send(Message::Pong { seq: *seq, timestamp_ms: *timestamp_ms });
                true
            }
            _ => false,
        }
    }));
    let instance = channel::instance_id();

    // Clipboard changes are sent by a worker so a big copy never blocks the message loop.
    let (clip_tx, clip_rx) = std::sync::mpsc::channel::<ClipBundle>();
    let out_for_clipboard = out.clone();
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
            let result = bundle.send(clip_id, &mut |msg| out_for_clipboard.send(msg));
            match result {
                Ok(warnings) => warnings.iter().for_each(|w| warn!("Clipboard: {}", w)),
                Err(e) => warn!("Clipboard send failed: {}", e),
            }
        }
    });
    // A guest application reads the content of host files: ask the host for them (once).
    let out_for_requests = out.clone();
    hostfiles::set_requester(move |clip_id| {
        info!("Paste of host files (clip {}): asking the host for them", clip_id);
        out_for_requests.send(Message::ClipFilesRequest { clip_id })
    });
    let _watcher = ClipboardWatcher::start(move |bundle| {
        let _ = clip_tx.send(bundle);
    });

    let out_for_drag = out.clone();
    let dragout = DragOut::start(Arc::new(move |msg| out_for_drag.send(msg)));
    let mut clips = Assembler::new(clip_staging.clone());
    let mut dnd = DndManager::new();

    // Notify host agent is alive with initial status
    {
        let (w, h, dpi) = display::get_display_info();
        let _ = out.send(Message::StatusReply {
            current_width: w,
            current_height: h,
            current_dpi: dpi,
            os_description: caps::announce("Windows 11 x64", instance),
        });
    }

    info!("Agent ready, listening for host messages...");

    let mut file_milestone = 25u64;
    // The serial port is read by its own thread: it never waits for this loop (except when the
    // serial queue is full, as before) and never for the lanes.
    channel::spawn_serial_reader(reader, endpoint.clone());
    loop {
        // Everything the host sent arrives in one inbox, from the serial port and, when shared
        // memory is active, from the lane readers: control lane first, then serial, then bulk.
        let msg = match inbox.pop(std::time::Duration::from_secs(1)) {
            Pop::Msg(msg) => msg,
            Pop::Timeout => continue,
            Pop::Closed => break,
        };

        match msg {
            Message::Ping { seq, timestamp_ms } => {
                let _ = out.send(Message::Pong { seq, timestamp_ms });
            }

            Message::SetResolution { width, height } => {
                info!("Received SetResolution: {}x{}", width, height);
                let (success, error) = match display::set_resolution(width, height) {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(e)),
                };
                let _ = out.send(Message::ResolutionResult {
                    width,
                    height,
                    success,
                    error,
                });
            }

            Message::SetDpiScale { scale_percent } => {
                info!("Received SetDpiScale: {}%", scale_percent);
                let (success, error) = match display::set_dpi_scale(scale_percent) {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(e)),
                };
                let _ = out.send(Message::DpiScaleResult {
                    scale_percent,
                    success,
                    error,
                });
            }

            Message::SetRefreshRate { millihertz } => {
                info!("Received SetRefreshRate: {} mHz", millihertz);
                let (success, error) = match display::set_refresh_rate(millihertz) {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(e)),
                };
                let _ = out.send(Message::RefreshRateResult {
                    millihertz,
                    success,
                    error,
                });
            }

            Message::QueryStatus => {
                let (w, h, dpi) = display::get_display_info();
                let _ = out.send(Message::StatusReply {
                    current_width: w,
                    current_height: h,
                    current_dpi: dpi,
                    os_description: caps::announce("Windows 11 x64", instance),
                });
            }

            msg @ (Message::ClipboardOffer { clip_id, .. }
            | Message::ClipboardItem { clip_id, .. }
            | Message::ClipFilesManifest { clip_id, .. }) => {
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

            msg @ (Message::FilesBegin { stream_id, .. }
            | Message::FileEntry { stream_id, .. }
            | Message::FileChunk { stream_id, .. }
            | Message::FilesEnd { stream_id, .. }) => match clips.handle_files(&msg) {
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

            Message::ClipFilesRequest { clip_id } => {
                spawn_files_sender(out.clone(), guest_offer.clone(), clip_id)
            }

            Message::GuestDragProbe { x, y } => dragout.probe(x, y),
            Message::GuestDragFetch { drag_id } => dragout.fetch(drag_id),
            Message::GuestDragEnd { .. } => dragout.end(),

            Message::DropBegin { transfer_id, x, y, file_count } => {
                if let Err(e) = dnd.begin_drop(transfer_id, x, y, file_count) {
                    warn!("Could not begin drop operation: {}", e);
                    let _ = out.send(Message::DropResult { success: false, message: e });
                }
            }

            Message::DropFileStart { transfer_id, operation_id, filename, total_bytes } => {
                if let Err(e) = dnd.start_file(transfer_id, &filename, total_bytes, operation_id) {
                    warn!("Could not start dropped file: {}", e);
                    let _ = out.send(Message::DropResult { success: false, message: e });
                }
            }

            Message::DropFileChunk { transfer_id, offset, data } => {
                if let Err(e) = dnd.write_chunk(transfer_id, offset, &data) {
                    warn!("Dropped file chunk failed: {}", e);
                }
            }

            Message::DropFileFinish { transfer_id, success, error: _ } => {
                let (ok, msg) = match dnd.finish_file(transfer_id, success) {
                    Ok(Some(message)) => (true, message),
                    Ok(None) => (true, "File received; waiting for remaining files".into()),
                    Err(e) => (false, e),
                };
                let _ = out.send(Message::DropResult { success: ok, message: msg });
            }

            Message::DropText { text, x, y } => {
                let (ok, msg) = match dnd.handle_drop_text(&text, x, y) {
                    Ok(path) => (true, format!("Text saved to {}", path)),
                    Err(e) => (false, e),
                };
                let _ = out.send(Message::DropResult { success: ok, message: msg });
            }

            _ => {}
        }
    }
}

/// Answers a `ClipFilesRequest`: streams the files of the newest guest copy to the host on a
/// worker thread, so the message loop (Pong, status, drops) keeps running. Messages are handed
/// to the transport one by one (a lane when shared memory is active, else the serial port).
fn spawn_files_sender(out: Outgoing, offer: Arc<GuestOffer>, clip_id: u32) {
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
            let mut send = |msg: Message| out.send(msg);
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
