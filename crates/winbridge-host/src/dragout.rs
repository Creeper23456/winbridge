// SPDX-License-Identifier: GPL-3.0-or-later

//! Dragging files and text out of the guest onto Linux applications.
//!
//! When the guest reports a drag (`GuestDragOffer`) while the left button is still held, the
//! viewer starts a GTK drag with a [`GuestDragProvider`]. The provider is lazy: the files are
//! only fetched from the guest when the Linux target actually asks for them (at the drop), staged
//! under the cache directory, and handed over as URIs. Where the target puts them is the
//! target's choice, so the files end up wherever they were dropped.

use crate::clipboard::cache_root;
use crate::serial_client::SerialClientHandle;
use crate::transfer::{self, Abort, Direction, Transfer};
use gtk4::gdk::{self, ContentProvider};
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn};
use winbridge_proto::archive;
use winbridge_proto::files::Receiver;
use winbridge_proto::Message;

/// Largest drag the viewer accepts (files are staged on the host first).
const DRAG_CAP_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// A fetch that has not finished after this long is abandoned.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15 * 60);

type FetchResult = Result<Vec<PathBuf>, String>;

#[derive(Default)]
struct Sink {
    expect: Option<u32>,
    receiver: Option<(u32, Receiver)>,
    /// A folder drag: the zip being received.
    archive: Option<ArchiveIn>,
    result: Option<(u32, FetchResult)>,
    /// The progress-hub transfer of the running fetch, with bytes received and the stream total.
    xfer: Option<Transfer>,
    done: u64,
    total: u64,
}

static SINK: Mutex<Sink> = Mutex::new(Sink {
    expect: None,
    receiver: None,
    archive: None,
    result: None,
    xfer: None,
    done: 0,
    total: 0,
});

/// The zip of a folder drag on its way in (`ArchiveBegin`, `FileChunk`s with index 0, `FilesEnd`).
struct ArchiveIn {
    zip: PathBuf,
    file: std::fs::File,
    written: u64,
    total: u64,
    /// Entries the guest left out before packing, and the first few with the reason.
    skipped: u32,
    listed: Vec<String>,
}

impl ArchiveIn {
    fn create(id: u32, total: u64, skipped: u32, listed: Vec<String>) -> Result<Self, String> {
        if total > DRAG_CAP_BYTES {
            return Err(format!("the zip of {} bytes exceeds the limit", total));
        }
        let zip = cache_root().join("drag-in").join(format!("{}.zip", id));
        if let Some(parent) = zip.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {}", parent.display(), e))?;
        }
        let file = std::fs::File::create(&zip).map_err(|e| format!("{}: {}", zip.display(), e))?;
        Ok(Self { zip, file, written: 0, total, skipped, listed })
    }

    fn write(&mut self, index: u32, offset: u64, data: &[u8]) -> Result<(), String> {
        if index != 0 || offset != self.written || self.written + data.len() as u64 > self.total {
            return Err("a chunk of the zip is out of order or too large".to_string());
        }
        self.file.write_all(data).map_err(|e| format!("{}: {}", self.zip.display(), e))?;
        self.written += data.len() as u64;
        Ok(())
    }
}

/// Drops a half-received zip.
fn discard_archive(sink: &mut Sink) {
    if let Some(received) = sink.archive.take() {
        let _ = std::fs::remove_file(&received.zip);
    }
}

/// Registers that the file stream `id` is the answer to a `GuestDragFetch`.
fn expect(id: u32) {
    let mut sink = SINK.lock().unwrap();
    if let Some(old) = sink.xfer.take() {
        old.finish();
    }
    sink.expect = Some(id);
    sink.receiver = None;
    discard_archive(&mut sink);
    sink.result = None;
    sink.done = 0;
    sink.total = 0;
}

/// Cancel or timeout from the transfer hub: the fetch fails and the waiting drop is answered.
fn abort(id: u32, why: Abort) {
    let mut sink = SINK.lock().unwrap();
    if sink.expect != Some(id) {
        return;
    }
    let reason = match why {
        Abort::Cancelled => "the transfer was cancelled",
        Abort::TimedOut => "the guest stopped sending files",
    };
    warn!("Guest drag fetch {} abandoned: {}", id, reason);
    sink.receiver = None;
    discard_archive(&mut sink);
    sink.expect = None;
    sink.xfer = None;
    sink.result = Some((id, Err(reason.to_string())));
}

fn peek_result(id: u32) -> Option<FetchResult> {
    SINK.lock()
        .unwrap()
        .result
        .as_ref()
        .filter(|(rid, _)| *rid == id)
        .map(|(_, r)| r.clone())
}

/// Offers a file-stream message to the drag fetch. Returns true when it belonged to one.
pub fn handle(msg: &Message) -> bool {
    let mut sink = SINK.lock().unwrap();
    let Some(expected) = sink.expect else {
        return false;
    };
    let fail = |sink: &mut Sink, id: u32, error: String| {
        warn!("Guest drag transfer failed: {}", error);
        sink.receiver = None;
        discard_archive(sink);
        sink.expect = None;
        if let Some(xfer) = sink.xfer.take() {
            xfer.fail(&error);
        }
        sink.result = Some((id, Err(error)));
    };
    match msg {
        Message::ArchiveBegin { stream_id, total_bytes, skipped, listed } if *stream_id == expected => {
            match ArchiveIn::create(*stream_id, *total_bytes, *skipped, listed.clone()) {
                Ok(received) => {
                    sink.archive = Some(received);
                    sink.done = 0;
                    sink.total = *total_bytes;
                    if let Some(xfer) = sink.xfer {
                        xfer.set_label("Receiving files from Windows");
                        xfer.progress(archive::bar(archive::PHASE_SEND, 0, *total_bytes), archive::BAR_TOTAL);
                    }
                }
                Err(e) => fail(&mut sink, *stream_id, e),
            }
            true
        }
        Message::FilesBegin { stream_id, entries, total_bytes } if *stream_id == expected => {
            let root = cache_root().join("drag-in").join(stream_id.to_string());
            let _ = std::fs::remove_dir_all(&root);
            match Receiver::new(root, *entries, *total_bytes, DRAG_CAP_BYTES) {
                Ok(receiver) => {
                    sink.receiver = Some((*stream_id, receiver));
                    sink.done = 0;
                    sink.total = *total_bytes;
                    if let Some(xfer) = sink.xfer {
                        xfer.progress(0, *total_bytes);
                    }
                }
                Err(e) => fail(&mut sink, *stream_id, e),
            }
            true
        }
        Message::FileEntry { stream_id, index, rel_path, is_dir, size } if *stream_id == expected => {
            if let Some((_, receiver)) = sink.receiver.as_mut() {
                if let Err(e) = receiver.entry(*index, rel_path, *is_dir, *size) {
                    fail(&mut sink, *stream_id, e);
                }
            }
            true
        }
        Message::FileChunk { stream_id, index, offset, data } if *stream_id == expected => {
            let mut failure = None;
            let st: &mut Sink = &mut sink;
            if let Some(a) = st.archive.as_mut() {
                match a.write(*index, *offset, data) {
                    Ok(()) => {
                        if let Some(xfer) = st.xfer {
                            xfer.progress(archive::bar(archive::PHASE_SEND, a.written, a.total), archive::BAR_TOTAL);
                        }
                    }
                    Err(e) => failure = Some(e),
                }
            } else if let Some((_, receiver)) = st.receiver.as_mut() {
                match receiver.chunk(*index, *offset, data) {
                    Ok(()) => {
                        st.done += data.len() as u64;
                        if let Some(xfer) = st.xfer {
                            xfer.progress(st.done, st.total);
                        }
                    }
                    Err(e) => failure = Some(e),
                }
            }
            if let Some(e) = failure {
                fail(&mut sink, *stream_id, e);
            }
            true
        }
        Message::FilesEnd { stream_id, success, error } if *stream_id == expected => {
            if let Some(received) = sink.archive.take() {
                if !*success || received.written != received.total {
                    let error = error.clone().unwrap_or_else(|| {
                        format!("the zip is incomplete: {} of {} bytes", received.written, received.total)
                    });
                    let _ = std::fs::remove_file(&received.zip);
                    fail(&mut sink, *stream_id, error);
                } else {
                    let xfer = sink.xfer;
                    let id = *stream_id;
                    let spawned = std::thread::Builder::new()
                        .name("winbridge-drag-unzip".into())
                        .spawn(move || unpack_archive(id, received, xfer));
                    if let Err(e) = spawned {
                        fail(&mut sink, id, format!("cannot start unpacking: {}", e));
                    }
                }
                return true;
            }
            let outcome = match sink.receiver.take() {
                Some((_, receiver)) => receiver.finish(*success, error.clone()),
                None => Err(error.clone().unwrap_or_else(|| "the guest sent no files".to_string())),
            };
            sink.expect = None;
            if let Some(xfer) = sink.xfer.take() {
                match &outcome {
                    Ok(_) => xfer.finish(),
                    Err(e) => xfer.fail(e),
                }
            }
            sink.result = Some((*stream_id, outcome));
            true
        }
        _ => false,
    }
}

/// Progress of a step the guest runs itself (packing the dragged folder). True when `id` is the
/// running fetch.
pub fn progress(id: u32, phase: u8, done: u64, total: u64) -> bool {
    let sink = SINK.lock().unwrap();
    if sink.expect != Some(id) {
        return false;
    }
    if let Some(xfer) = sink.xfer {
        xfer.set_label(format!("{} in Windows", archive::phase_name(phase)));
        xfer.progress(archive::bar(phase, done, total), archive::BAR_TOTAL);
    }
    true
}

/// Unpacks the received zip below the drag's staging directory and deletes it (the last two
/// steps of the bar), then answers the waiting drop. Runs on its own thread: the link reader
/// must not wait for it.
fn unpack_archive(id: u32, received: ArchiveIn, xfer: Option<Transfer>) {
    let ArchiveIn { zip, file, skipped, listed, .. } = received;
    drop(file);
    let root = cache_root().join("drag-in").join(id.to_string());
    let _ = std::fs::remove_dir_all(&root);
    let step = |phase: u8, done: u64, total: u64| {
        xfer.is_none_or(|x| x.progress(archive::bar(phase, done, total), archive::BAR_TOTAL))
    };
    if let Some(x) = xfer {
        x.set_label("Unpacking");
    }
    let outcome = archive::unpack(&zip, &root, archive::Dest::Linux, DRAG_CAP_BYTES, &mut |d, t| {
        step(archive::PHASE_UNPACK, d, t)
    })
    .and_then(|tops| {
        if let Some(x) = xfer {
            x.set_label("Deleting the zip");
        }
        step(archive::PHASE_DELETE, 0, 1);
        std::fs::remove_file(&zip).map_err(|e| format!("{}: {}", zip.display(), e))?;
        step(archive::PHASE_DELETE, 1, 1);
        Ok(tops)
    });
    let _ = std::fs::remove_file(&zip);
    if outcome.is_err() {
        let _ = std::fs::remove_dir_all(&root);
    }
    let mut sink = SINK.lock().unwrap();
    if sink.expect != Some(id) {
        // Cancelled or timed out meanwhile: nobody waits for the files.
        let _ = std::fs::remove_dir_all(&root);
        return;
    }
    sink.expect = None;
    if let Some(xfer) = sink.xfer.take() {
        match &outcome {
            Ok(_) => match archive::notice(skipped, &listed) {
                Some(note) => xfer.finish_with_note(&note),
                None => xfer.finish(),
            },
            Err(e) => xfer.fail(e),
        }
    }
    sink.result = Some((id, outcome));
}

mod imp {
    use super::*;

    pub struct Data {
        pub drag_id: u32,
        pub names: Vec<String>,
        pub serial: SerialClientHandle,
        pub started: bool,
    }

    #[derive(Default)]
    pub struct GuestDragProvider {
        pub data: RefCell<Option<Data>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for GuestDragProvider {
        const NAME: &'static str = "WinBridgeGuestDragProvider";
        type Type = super::GuestDragProvider;
        type ParentType = ContentProvider;
    }

    impl ObjectImpl for GuestDragProvider {}

    impl ContentProviderImpl for GuestDragProvider {
        fn formats(&self) -> gdk::ContentFormats {
            gdk::ContentFormatsBuilder::new()
                .add_mime_type("text/uri-list")
                .add_mime_type("x-special/gnome-copied-files")
                .build()
        }

        fn write_mime_type_future(
            &self,
            mime_type: &str,
            stream: &gio::OutputStream,
            io_priority: glib::Priority,
        ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>> + 'static>> {
            let obj = self.obj().clone();
            let mime = mime_type.to_string();
            let stream = stream.clone();
            Box::pin(async move {
                let paths = obj.fetch().await?;
                let uris: Vec<String> = paths.iter().map(|p| gio::File::for_path(p).uri().to_string()).collect();
                let body = if mime == "x-special/gnome-copied-files" {
                    format!("copy\n{}", uris.join("\n"))
                } else {
                    uris.iter().map(|u| format!("{}\r\n", u)).collect::<String>()
                };
                stream
                    .write_all_future(body.into_bytes(), io_priority)
                    .await
                    .map_err(|(_, e)| e)?;
                Ok(())
            })
        }
    }
}

glib::wrapper! {
    /// Content of one guest-to-host file drag.
    pub struct GuestDragProvider(ObjectSubclass<imp::GuestDragProvider>) @extends ContentProvider;
}

impl GuestDragProvider {
    fn new(drag_id: u32, names: Vec<String>, serial: SerialClientHandle) -> Self {
        let obj: Self = glib::Object::new();
        *obj.imp().data.borrow_mut() = Some(imp::Data { drag_id, names, serial, started: false });
        obj
    }

    /// Asks the guest for the files (once) and waits until they are staged on the host.
    async fn fetch(&self) -> Result<Vec<PathBuf>, glib::Error> {
        let (drag_id, names) = {
            let mut data = self.imp().data.borrow_mut();
            let data = data.as_mut().expect("provider data");
            if !data.started {
                data.started = true;
                let id = data.drag_id;
                expect(id);
                // Begin the transfer (progress bar, input lock) before asking, so no stream
                // message can arrive without a transfer to report to.
                let xfer = transfer::begin(
                    "Receiving files from Windows",
                    Direction::FromGuest,
                    0,
                    Some(Box::new(move |why| abort(id, why))),
                );
                SINK.lock().unwrap().xfer = Some(xfer);
                info!("Fetching dragged files from the guest: {:?}", data.names);
                data.serial.send_message(Message::GuestDragFetch { drag_id: id });
            }
            (data.drag_id, data.names.clone())
        };
        let begun = Instant::now();
        loop {
            if let Some(result) = peek_result(drag_id) {
                return result
                    .map_err(|e| glib::Error::new(gio::IOErrorEnum::Failed, &format!("Guest transfer failed: {}", e)));
            }
            if begun.elapsed() > FETCH_TIMEOUT {
                return Err(glib::Error::new(
                    gio::IOErrorEnum::TimedOut,
                    &format!("The guest did not deliver {:?} in time", names),
                ));
            }
            glib::timeout_future(Duration::from_millis(50)).await;
        }
    }
}

/// Content for a drag that left the guest: lazily fetched files, plus the dragged text.
pub fn provider(
    drag_id: u32,
    names: Vec<String>,
    text: Option<String>,
    serial: SerialClientHandle,
) -> Option<ContentProvider> {
    let mut parts: Vec<ContentProvider> = Vec::new();
    if !names.is_empty() {
        parts.push(GuestDragProvider::new(drag_id, names, serial).upcast());
    }
    if let Some(text) = text.filter(|t| !t.is_empty()) {
        parts.push(ContentProvider::for_value(&text.to_value()));
    }
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(ContentProvider::new_union(&parts)),
    }
}
