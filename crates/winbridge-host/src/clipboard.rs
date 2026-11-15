// SPDX-License-Identifier: GPL-3.0-or-later

//! Clipboard sharing with the guest: text, HTML, RTF, images and files, both directions.
//!
//! Local changes are read from the GTK clipboard, packed into a [`ClipBundle`] and offered; copied
//! files are only listed, and streamed when the guest pastes (`ClipFilesRequest`). A snapshot
//! offered by the guest is reassembled on the Tokio side ([`ClipIn`]) and applied to the GTK
//! clipboard on the main thread ([`apply`]); its files are fetched lazily by
//! [`LazyFilesProvider`] when something pastes. Content we set ourselves reports
//! `Clipboard::is_local()`, which is what keeps the two sides from echoing a snapshot back.

use crate::serial_client::SerialClientHandle;
use crate::transfer::{self, Abort, Direction, Transfer};
use gtk4::gdk::{self, ContentProvider};
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};
use winbridge_proto::clip::{Assembler, ClipBundle, FilesEvent, MAX_ITEM_BYTES};
use winbridge_proto::Message;

/// `$XDG_CACHE_HOME/winbridge` (or `~/.cache/winbridge`): staging for files that come from the guest.
pub fn cache_root() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("winbridge")
}

/// Removes staged files of earlier sessions. Called once at startup.
pub fn purge_staging() {
    for dir in ["clip-in", "drag-in"] {
        let _ = std::fs::remove_dir_all(cache_root().join(dir));
    }
}

fn clip_staging() -> PathBuf {
    cache_root().join("clip-in")
}

/// Guest-to-host reassembly and the lazy file fetch. Lives on the Tokio side because the
/// messages arrive there; the GTK side reaches it through [`CLIP_IN`].
pub struct ClipIn {
    state: Mutex<State>,
}

pub static CLIP_IN: LazyLock<ClipIn> = LazyLock::new(ClipIn::new);

type FilesResult = Result<Vec<PathBuf>, String>;

/// The file fetch of the newest guest offer.
enum Lazy {
    /// Nothing running and nothing finished for the newest offer.
    Idle,
    /// `ClipFilesRequest` sent; every concurrent paste request shares this one fetch.
    Fetching {
        clip_id: u32,
        /// `FilesBegin` arrived (leftovers of an abandoned stream are ignored until it does).
        began: bool,
        xfer: Transfer,
        waiters: Vec<oneshot::Sender<FilesResult>>,
    },
    /// Fetched and staged; later requests for the same snapshot reuse the paths.
    Done { clip_id: u32, paths: Vec<PathBuf> },
}

struct State {
    assembler: Assembler,
    lazy: Lazy,
    /// `clip_id` of the newest `ClipboardOffer`.
    offer_id: u32,
    /// The newest offer, when it lists files that can still be fetched.
    files_clip: Option<u32>,
}

impl State {
    /// Ends the running fetch (if any) and returns its transfer and waiters.
    fn take_fetch(&mut self) -> Option<(Transfer, Vec<oneshot::Sender<FilesResult>>)> {
        match std::mem::replace(&mut self.lazy, Lazy::Idle) {
            Lazy::Fetching { xfer, waiters, .. } => Some((xfer, waiters)),
            other => {
                self.lazy = other;
                None
            }
        }
    }
}

/// What a paste request gets: the paths now, or a receiver for when the fetch ends.
pub enum Files {
    Ready(FilesResult),
    Wait(oneshot::Receiver<FilesResult>),
}

impl ClipIn {
    fn new() -> Self {
        Self {
            state: Mutex::new(State {
                assembler: Assembler::new(clip_staging()),
                lazy: Lazy::Idle,
                offer_id: 0,
                files_clip: None,
            }),
        }
    }

    /// Consumes `ClipboardOffer` and `ClipboardItem`; returns the snapshot (with its `clip_id`)
    /// once it is complete. A new offer ends the fetch of the previous one. Other messages are
    /// ignored.
    pub fn handle(&self, msg: &Message) -> Option<(u32, ClipBundle)> {
        let mut guard = self.state.lock().unwrap();
        let st = &mut *guard;
        if let Message::ClipboardOffer { clip_id, .. } = msg {
            st.offer_id = *clip_id;
            st.files_clip = None;
            let stale = std::mem::replace(&mut st.lazy, Lazy::Idle);
            if let Lazy::Fetching { xfer, waiters, .. } = stale {
                info!("Guest clipboard changed during a file fetch; fetch dropped");
                xfer.finish();
                for waiter in waiters {
                    let _ = waiter.send(Err("the guest clipboard changed".to_string()));
                }
            }
        }
        match st.assembler.handle(msg) {
            Ok(Some(bundle)) => {
                if bundle.files_offered {
                    st.files_clip = Some(st.offer_id);
                }
                Some((st.offer_id, bundle))
            }
            Ok(None) => None,
            Err(e) => {
                warn!("Guest clipboard transfer failed: {}", e);
                None
            }
        }
    }

    /// A paste wants the files of snapshot `clip_id`. The first request sends
    /// `ClipFilesRequest`; concurrent ones share the fetch; later ones reuse the finished paths.
    /// Never blocks.
    pub fn request_files(&self, clip_id: u32, serial: &SerialClientHandle) -> Files {
        let mut guard = self.state.lock().unwrap();
        let st = &mut *guard;
        if st.files_clip != Some(clip_id) {
            return Files::Ready(Err("the guest clipboard changed; copy the files again".to_string()));
        }
        match &mut st.lazy {
            Lazy::Done { clip_id: done, paths } if *done == clip_id => {
                return Files::Ready(Ok(paths.clone()));
            }
            Lazy::Fetching { clip_id: running, waiters, .. } if *running == clip_id => {
                let (tx, rx) = oneshot::channel();
                waiters.push(tx);
                return Files::Wait(rx);
            }
            _ => {}
        }
        let (tx, rx) = oneshot::channel();
        let xfer = transfer::begin(
            "Receiving files from Windows",
            Direction::FromGuest,
            0,
            Some(Box::new(move |why| CLIP_IN.abort_fetch(clip_id, why))),
        );
        st.lazy = Lazy::Fetching { clip_id, began: false, xfer, waiters: vec![tx] };
        info!("Paste of guest snapshot {}: requesting its files", clip_id);
        serial.send_message(Message::ClipFilesRequest { clip_id });
        Files::Wait(rx)
    }

    /// Cancel or timeout from the transfer hub: abandon the fetch and fail the waiting pastes.
    fn abort_fetch(&self, clip_id: u32, why: Abort) {
        let mut guard = self.state.lock().unwrap();
        let st = &mut *guard;
        if !matches!(&st.lazy, Lazy::Fetching { clip_id: running, .. } if *running == clip_id) {
            return;
        }
        let Some((_, waiters)) = st.take_fetch() else { return };
        st.assembler.cancel_files();
        let reason = match why {
            Abort::Cancelled => "the transfer was cancelled",
            Abort::TimedOut => "the guest stopped sending files",
        };
        warn!("File fetch of guest snapshot {} abandoned: {}", clip_id, reason);
        for waiter in waiters {
            let _ = waiter.send(Err(reason.to_string()));
        }
    }

    /// Feeds `FilesBegin`/`FileEntry`/`FileChunk`/`FilesEnd` to the running fetch, drives the
    /// progress hub and completes the waiting pastes when the stream ends.
    pub fn handle_files(&self, msg: &Message) {
        let mut guard = self.state.lock().unwrap();
        let st = &mut *guard;
        let Lazy::Fetching { clip_id, began, xfer, .. } = &mut st.lazy else {
            debug!("File stream message ignored: no fetch is running");
            return;
        };
        let is_begin = matches!(msg, Message::FilesBegin { .. });
        if !*began && !is_begin {
            return;
        }
        let (clip_id, xfer) = (*clip_id, *xfer);
        match st.assembler.handle_files(msg) {
            Ok(FilesEvent::Ignored) => {}
            Ok(FilesEvent::Progress { done, total }) => {
                if is_begin {
                    if let Lazy::Fetching { began, .. } = &mut st.lazy {
                        *began = true;
                    }
                }
                xfer.progress(done, total);
            }
            Ok(FilesEvent::Done { clip_id: done_id, paths }) => {
                info!("Files of guest snapshot {} received: {} top-level item(s)", done_id, paths.len());
                let waiters = st.take_fetch().map(|(_, w)| w).unwrap_or_default();
                st.lazy = Lazy::Done { clip_id, paths: paths.clone() };
                xfer.finish();
                for waiter in waiters {
                    let _ = waiter.send(Ok(paths.clone()));
                }
            }
            Err(e) => {
                warn!("Guest file transfer failed: {}", e);
                // The assembler dropped the snapshot's files along with the failed stream.
                st.files_clip = None;
                let waiters = st.take_fetch().map(|(_, w)| w).unwrap_or_default();
                xfer.fail(&e);
                for waiter in waiters {
                    let _ = waiter.send(Err(e.clone()));
                }
            }
        }
    }
}

mod lazy_imp {
    use super::*;

    pub struct Data {
        pub clip_id: u32,
        pub serial: SerialClientHandle,
    }

    #[derive(Default)]
    pub struct LazyFilesProvider {
        pub data: RefCell<Option<Data>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LazyFilesProvider {
        const NAME: &'static str = "WinBridgeLazyFilesProvider";
        type Type = super::LazyFilesProvider;
        type ParentType = ContentProvider;
    }

    impl ObjectImpl for LazyFilesProvider {}

    impl LazyFilesProvider {
        fn data(&self) -> (u32, SerialClientHandle) {
            let data = self.data.borrow();
            let data = data.as_ref().expect("provider data");
            (data.clip_id, data.serial.clone())
        }
    }

    impl ContentProviderImpl for LazyFilesProvider {
        fn formats(&self) -> gdk::ContentFormats {
            gdk::ContentFormatsBuilder::new()
                .add_type(gdk::FileList::static_type())
                .add_mime_type("text/uri-list")
                .add_mime_type("x-special/gnome-copied-files")
                .build()
        }

        /// A clipboard manager must not trigger a transfer from the guest to persist this.
        fn storable_formats(&self) -> gdk::ContentFormats {
            gdk::ContentFormatsBuilder::new().build()
        }

        fn write_mime_type_future(
            &self,
            mime_type: &str,
            stream: &gio::OutputStream,
            io_priority: glib::Priority,
        ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>> + 'static>> {
            let (clip_id, serial) = self.data();
            let mime = mime_type.to_string();
            info!("Lazy files provider: {} requested for snapshot {}", mime, clip_id);
            let stream = stream.clone();
            Box::pin(async move {
                if mime != "text/uri-list" && mime != "x-special/gnome-copied-files" {
                    return Err(glib::Error::new(gio::IOErrorEnum::NotSupported, "unsupported clipboard format"));
                }
                let paths = fetch_paths(clip_id, &serial).await?;
                let uris: Vec<String> =
                    paths.iter().map(|p| gio::File::for_path(p).uri().to_string()).collect();
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

        /// Synchronous by contract, so it must not wait: it answers once the files are staged
        /// and otherwise starts the fetch and reports "not yet".
        fn value(&self, type_: glib::Type) -> Result<glib::Value, glib::Error> {
            if type_ != gdk::FileList::static_type() {
                return self.parent_value(type_);
            }
            let (clip_id, serial) = self.data();
            info!("Lazy files provider: FileList value requested for snapshot {}", clip_id);
            match CLIP_IN.request_files(clip_id, &serial) {
                Files::Ready(Ok(paths)) => {
                    let files: Vec<gio::File> = paths.iter().map(gio::File::for_path).collect();
                    Ok(gdk::FileList::from_array(&files).to_value())
                }
                Files::Ready(Err(e)) => Err(failed(&e)),
                Files::Wait(_) => Err(glib::Error::new(
                    gio::IOErrorEnum::Pending,
                    "The files are being fetched from Windows; paste again when the transfer ends",
                )),
            }
        }
    }
}

glib::wrapper! {
    /// Guest clipboard files, fetched when something pastes (not when they were copied).
    pub struct LazyFilesProvider(ObjectSubclass<lazy_imp::LazyFilesProvider>) @extends ContentProvider;
}

impl LazyFilesProvider {
    fn new(clip_id: u32, serial: SerialClientHandle) -> Self {
        let obj: Self = glib::Object::new();
        *obj.imp().data.borrow_mut() = Some(lazy_imp::Data { clip_id, serial });
        obj
    }
}

fn failed(reason: &str) -> glib::Error {
    glib::Error::new(gio::IOErrorEnum::Failed, &format!("Guest file transfer failed: {}", reason))
}

/// Waits for the staged files of guest snapshot `clip_id` without blocking the main loop.
async fn fetch_paths(clip_id: u32, serial: &SerialClientHandle) -> Result<Vec<PathBuf>, glib::Error> {
    match CLIP_IN.request_files(clip_id, serial) {
        Files::Ready(result) => result.map_err(|e| failed(&e)),
        Files::Wait(rx) => match rx.await {
            Ok(result) => result.map_err(|e| failed(&e)),
            Err(_) => Err(failed("the fetch was dropped")),
        },
    }
}

/// Newest offer of host files: `(clip_id, paths)`. The guest's `ClipFilesRequest` is answered
/// from here; reachable from the serial message handler.
static LOCAL_OFFER: Mutex<Option<(u32, Vec<PathBuf>)>> = Mutex::new(None);

/// The guest pastes: streams the files of host snapshot `clip_id` if it is the newest offer.
pub fn on_files_request(serial: &SerialClientHandle, clip_id: u32) {
    let paths = match &*LOCAL_OFFER.lock().unwrap() {
        Some((id, paths)) if *id == clip_id && !paths.is_empty() => paths.clone(),
        other => {
            warn!(
                "ClipFilesRequest for snapshot {} ignored (newest offer: {:?})",
                clip_id,
                other.as_ref().map(|(id, _)| *id)
            );
            return;
        }
    };
    info!("Guest pastes host snapshot {}: streaming {} top-level item(s)", clip_id, paths.len());
    serial.send_clip_files(clip_id, paths);
}

/// Watches the GTK clipboard and sends every local change to the guest.
pub fn watch(clipboard: &gdk::Clipboard, serial: SerialClientHandle) {
    let generation = Rc::new(Cell::new(0u64));
    let next_id = Rc::new(AtomicU32::new(1));
    clipboard.connect_changed(move |clip| {
        // Content we put there ourselves came from the guest.
        if clip.is_local() {
            return;
        }
        let this = generation.get() + 1;
        generation.set(this);
        let clip = clip.clone();
        let serial = serial.clone();
        let generation = generation.clone();
        let next_id = next_id.clone();
        glib::spawn_future_local(async move {
            // Apps announce a copy in several steps; wait until the burst is over.
            glib::timeout_future(Duration::from_millis(150)).await;
            if generation.get() != this || clip.is_local() {
                return;
            }
            let bundle = read_local(&clip).await;
            if bundle.is_empty() {
                return;
            }
            let id = next_id.fetch_add(1, Ordering::SeqCst);
            info!(
                "Host clipboard changed (text={} html={} rtf={} image={} files={}), syncing to guest",
                bundle.text.is_some(),
                bundle.html.is_some(),
                bundle.rtf.is_some(),
                bundle.png.is_some() || bundle.bmp.is_some(),
                bundle.files.len()
            );
            *LOCAL_OFFER.lock().unwrap() = Some((id, bundle.files.clone()));
            serial.send_bundle(id, bundle);
        });
    });
}

const RTF_TYPES: [&str; 3] = ["text/rtf", "application/rtf", "text/richtext"];

/// File paths of a `text/uri-list` or `x-special/gnome-copied-files` body: `file://` URIs only
/// (no host or `localhost`), percent-decoded; blank lines, `#` comments and the `copy`/`cut`
/// action line are skipped.
fn parse_uri_list(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && *l != "copy" && *l != "cut")
        .filter_map(file_uri_to_path)
        .collect()
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let head = uri.get(..7)?;
    if !head.eq_ignore_ascii_case("file://") {
        return None;
    }
    let rest = &uri[7..];
    let slash = rest.find('/')?;
    let host = &rest[..slash];
    if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
        return None;
    }
    let raw = rest[slash..].as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
        if raw[i] == b'%' && i + 2 < raw.len() && hex(raw[i + 1]).is_some() && hex(raw[i + 2]).is_some() {
            out.push((hex(raw[i + 1]).unwrap() << 4) | hex(raw[i + 2]).unwrap());
            i += 3;
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// Reads everything the clipboard holds that the guest can use.
async fn read_local(clip: &gdk::Clipboard) -> ClipBundle {
    let formats = clip.formats();
    let mut bundle = ClipBundle::default();

    // Plain MIME lists first: non-GTK owners (xclip, other file managers) only offer these, and
    // reading `GdkFileList` from them can fail.
    for mime in ["text/uri-list", "x-special/gnome-copied-files"] {
        if bundle.files.is_empty() && formats.contain_mime_type(mime) {
            if let Some(bytes) = read_mime(clip, &[mime]).await {
                let paths: Vec<PathBuf> = parse_uri_list(&String::from_utf8_lossy(&bytes))
                    .into_iter()
                    .filter(|p| p.exists())
                    .collect();
                info!("Host clipboard {} lists {} existing file(s)", mime, paths.len());
                bundle.files = paths;
            }
        }
    }
    if bundle.files.is_empty() && formats.contains_type(gdk::FileList::static_type()) {
        match clip.read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT).await {
            Ok(value) => {
                if let Ok(list) = value.get::<gdk::FileList>() {
                    bundle.files = list.files().iter().filter_map(|f| f.path()).collect();
                }
            }
            Err(e) => warn!("Reading copied files failed: {}", e),
        }
    }
    if !bundle.files.is_empty() {
        // Copied files: the guest pastes files, text and pictures of them would only confuse.
        return bundle;
    }

    if formats.contains_type(String::static_type()) {
        if let Ok(Some(text)) = clip.read_text_future().await {
            bundle.text = Some(text.to_string());
        }
    }
    if formats.contain_mime_type("text/html") {
        if let Some(bytes) = read_mime(clip, &["text/html"]).await {
            bundle.html = Some(String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    if let Some(mime) = RTF_TYPES.iter().find(|m| formats.contain_mime_type(m)) {
        bundle.rtf = read_mime(clip, &[mime]).await;
    }
    if formats.contains_type(gdk::Texture::static_type()) {
        if let Ok(Some(texture)) = clip.read_texture_future().await {
            bundle.png = Some(texture.save_to_png_bytes().to_vec());
            bundle.bmp = Some(texture_to_bmp(&texture));
        }
    }
    bundle
}

/// Reads one MIME type completely, up to [`MAX_ITEM_BYTES`].
async fn read_mime(clip: &gdk::Clipboard, mimes: &[&str]) -> Option<Vec<u8>> {
    let (stream, _) = clip.read_future(mimes, glib::Priority::DEFAULT).await.ok()?;
    let mut data = Vec::new();
    loop {
        let chunk = stream.read_bytes_future(64 * 1024, glib::Priority::DEFAULT).await.ok()?;
        if chunk.is_empty() {
            break;
        }
        data.extend_from_slice(&chunk);
        if data.len() > MAX_ITEM_BYTES {
            warn!("Clipboard item larger than {} bytes skipped", MAX_ITEM_BYTES);
            return None;
        }
    }
    (!data.is_empty()).then_some(data)
}

/// A 32-bit BMP for the guest's legacy bitmap format. Transparency is flattened onto white; the
/// PNG sent alongside keeps the alpha channel.
fn texture_to_bmp(texture: &gdk::Texture) -> Vec<u8> {
    let (w, h) = (texture.width() as usize, texture.height() as usize);
    let mut pixels = vec![0u8; w * h * 4];
    // Default download format: B8G8R8A8 premultiplied.
    texture.download(&mut pixels, w * 4);

    const HEADER: usize = 14 + 40 + 12;
    let image_size = w * h * 4;
    let mut out = Vec::with_capacity(HEADER + image_size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((HEADER + image_size) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(HEADER as u32).to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes()); // positive: bottom-up rows
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS
    out.extend_from_slice(&(image_size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 16]);
    out.extend_from_slice(&0x00FF_0000u32.to_le_bytes());
    out.extend_from_slice(&0x0000_FF00u32.to_le_bytes());
    out.extend_from_slice(&0x0000_00FFu32.to_le_bytes());
    for row in (0..h).rev() {
        for px in pixels[row * w * 4..(row + 1) * w * 4].chunks_exact(4) {
            // Premultiplied colour over white: c + (255 - a).
            let white = 255 - px[3];
            out.extend_from_slice(&[
                px[0].saturating_add(white),
                px[1].saturating_add(white),
                px[2].saturating_add(white),
                255,
            ]);
        }
    }
    out
}

/// Puts a snapshot from the guest on the GTK clipboard. Main thread only. Offered files are not
/// transferred here: a [`LazyFilesProvider`] fetches them when something pastes.
pub fn apply(clip: &gdk::Clipboard, clip_id: u32, bundle: ClipBundle, serial: &SerialClientHandle) {
    let mut providers: Vec<ContentProvider> = Vec::new();

    if bundle.files_offered {
        providers.push(LazyFilesProvider::new(clip_id, serial.clone()).upcast());
    } else {
        if let Some(text) = &bundle.text {
            providers.push(ContentProvider::for_value(&text.to_value()));
        }
        if let Some(html) = bundle.html {
            providers.push(ContentProvider::for_bytes(
                "text/html",
                &glib::Bytes::from_owned(html.into_bytes()),
            ));
        }
        if let Some(rtf) = bundle.rtf {
            let bytes = glib::Bytes::from_owned(rtf);
            providers.push(ContentProvider::for_bytes("text/rtf", &bytes));
            providers.push(ContentProvider::for_bytes("application/rtf", &bytes));
        }
        if let Some(texture) = texture_from(&bundle.png, &bundle.bmp) {
            providers.push(ContentProvider::for_value(&texture.to_value()));
        }
    }

    if providers.is_empty() {
        return;
    }
    match clip.set_content(Some(&ContentProvider::new_union(&providers))) {
        Ok(()) => info!("Host clipboard set from guest ({} format group(s))", providers.len()),
        Err(e) => warn!("Setting host clipboard failed: {}", e),
    }
}

/// Prefers the PNG (it has alpha); falls back to decoding the BMP.
fn texture_from(png: &Option<Vec<u8>>, bmp: &Option<Vec<u8>>) -> Option<gdk::Texture> {
    if let Some(png) = png {
        match gdk::Texture::from_bytes(&glib::Bytes::from(png.as_slice())) {
            Ok(texture) => return Some(texture),
            Err(e) => warn!("Guest PNG could not be decoded: {}", e),
        }
    }
    let bmp = bmp.as_ref()?;
    let loader = gtk4::gdk_pixbuf::PixbufLoader::with_type("bmp").ok()?;
    loader.write(bmp).ok()?;
    loader.close().ok()?;
    loader.pixbuf().map(|pixbuf| gdk::Texture::for_pixbuf(&pixbuf))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_uri_list() {
        let text = "# comment\r\nfile:///home/u/a%20b.txt\r\n\r\nhttps://example.org/x\r\nfile:///tmp/dir/\r\n";
        assert_eq!(
            parse_uri_list(text),
            vec![PathBuf::from("/home/u/a b.txt"), PathBuf::from("/tmp/dir/")]
        );
    }

    #[test]
    fn parses_gnome_copied_files() {
        assert_eq!(
            parse_uri_list("copy\nfile:///a/one\nfile:///a/two"),
            vec![PathBuf::from("/a/one"), PathBuf::from("/a/two")]
        );
        assert_eq!(parse_uri_list("cut\nfile:///a/one"), vec![PathBuf::from("/a/one")]);
    }

    #[test]
    fn decodes_percent_encoded_utf8() {
        // "Übung €.txt"
        let got = parse_uri_list("file:///d/%C3%9Cbung%20%E2%82%AC.txt");
        assert_eq!(got, vec![PathBuf::from("/d/Übung €.txt")]);
        // A stray percent stays literal.
        assert_eq!(parse_uri_list("file:///d/100%"), vec![PathBuf::from("/d/100%")]);
    }

    #[test]
    fn accepts_local_hosts_only_and_file_scheme_only() {
        assert_eq!(parse_uri_list("file://localhost/x"), vec![PathBuf::from("/x")]);
        assert!(parse_uri_list("file://otherhost/x").is_empty());
        assert!(parse_uri_list("ftp://h/x\nsmb://h/x\nmailto:a@b").is_empty());
        assert!(parse_uri_list("file:relative").is_empty());
        assert!(parse_uri_list("").is_empty());
    }
}
