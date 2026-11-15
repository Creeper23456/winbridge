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
use std::path::PathBuf;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn};
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
    result: Option<(u32, FetchResult)>,
    /// The progress-hub transfer of the running fetch, with bytes received and the stream total.
    xfer: Option<Transfer>,
    done: u64,
    total: u64,
}

static SINK: Mutex<Sink> =
    Mutex::new(Sink { expect: None, receiver: None, result: None, xfer: None, done: 0, total: 0 });

/// Registers that the file stream `id` is the answer to a `GuestDragFetch`.
fn expect(id: u32) {
    let mut sink = SINK.lock().unwrap();
    if let Some(old) = sink.xfer.take() {
        old.finish();
    }
    sink.expect = Some(id);
    sink.receiver = None;
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
        sink.expect = None;
        if let Some(xfer) = sink.xfer.take() {
            xfer.fail(&error);
        }
        sink.result = Some((id, Err(error)));
    };
    match msg {
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
            if let Some((_, receiver)) = st.receiver.as_mut() {
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
