// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows clipboard synchronization: text, HTML, RTF, images and files.
//!
//! Everything the guest clipboard holds is packed into a [`ClipBundle`] and sent to the host; a
//! bundle from the host replaces the clipboard content. Content that WinBridge itself wrote is
//! tagged with a private clipboard format, which keeps the two sides from echoing a snapshot
//! back and forth.
//!
//! Local changes are debounced: applications flush their formats for a moment after
//! `WM_CLIPBOARDUPDATE`, and reading (and holding the clipboard open) in that window makes their
//! own writes fail. Files the host copied are published as virtual files (see `hostfiles`), so
//! Explorer's paste-state probing never starts a transfer.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Ids for clipboard snapshots and drag-and-drop streams. One counter keeps the two kinds
/// of stream ids from ever colliding on the host.
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

pub fn next_id() -> u32 {
    NEXT_ID.fetch_add(1, Ordering::SeqCst)
}

fn relock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Guest -> host file paste: the files of the newest offer, and the stream being sent.
#[derive(Default)]
pub struct GuestOffer {
    offer: Mutex<Option<(u32, Vec<PathBuf>)>>,
    sending: Mutex<Option<u32>>,
}

impl GuestOffer {
    /// Remembers the files of the newest offer (an offer without files clears them).
    pub fn remember(&self, clip_id: u32, files: &[PathBuf]) {
        *relock(&self.offer) = (!files.is_empty()).then(|| (clip_id, files.to_vec()));
    }

    /// The files `clip_id` stands for, and marks the stream as running. Fails for an unknown or
    /// stale id and while that same stream is already being sent.
    pub fn claim(&self, clip_id: u32) -> Result<Vec<PathBuf>, String> {
        let paths = match &*relock(&self.offer) {
            Some((id, paths)) if *id == clip_id => paths.clone(),
            _ => return Err(format!("clip {} is unknown or stale", clip_id)),
        };
        let mut sending = relock(&self.sending);
        if *sending == Some(clip_id) {
            return Err(format!("clip {} is already being sent", clip_id));
        }
        *sending = Some(clip_id);
        Ok(paths)
    }

    /// The stream of `clip_id` ended.
    pub fn release(&self, clip_id: u32) {
        let mut sending = relock(&self.sending);
        if *sending == Some(clip_id) {
            *sending = None;
        }
    }
}

/// Decides which `WM_CLIPBOARDUPDATE` bursts are worth a read. The clipboard sequence number
/// changes with every change, so a burst (or a duplicate timer) for content that was already
/// processed is skipped.
#[derive(Debug)]
pub struct ChangeGate {
    last_read: Option<u32>,
    busy: u32,
}

impl ChangeGate {
    /// Quiet time after the last update before the clipboard is read.
    pub const DEBOUNCE: Duration = Duration::from_millis(150);
    /// Pause before the next try when another process holds the clipboard.
    pub const BUSY_RETRY: Duration = Duration::from_millis(100);
    /// Tries (after the first) before a busy clipboard is given up on.
    pub const MAX_BUSY_RETRIES: u32 = 5;

    pub fn new() -> Self {
        Self { last_read: None, busy: 0 }
    }

    /// Whether the content at sequence number `seq` still needs a read (0: unknown, always).
    pub fn should_read(&self, seq: u32) -> bool {
        seq == 0 || self.last_read != Some(seq)
    }

    /// The content at `seq` was processed (read, or recognised as our own).
    pub fn read_done(&mut self, seq: u32) {
        self.last_read = (seq != 0).then_some(seq);
        self.busy = 0;
    }

    /// The clipboard was held by someone else; true while another try is allowed.
    pub fn read_busy(&mut self) -> bool {
        self.busy += 1;
        if self.busy > Self::MAX_BUSY_RETRIES {
            self.busy = 0;
            return false;
        }
        true
    }
}

/// Builds the CF_HTML clipboard payload (header with byte offsets, then the document).
/// A document that already marks its fragment keeps its own markers.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn build_cf_html(html: &str) -> Vec<u8> {
    const START: &str = "<!--StartFragment-->";
    const END: &str = "<!--EndFragment-->";
    let (body, fragment_start, fragment_end) = match (html.find(START), html.find(END)) {
        (Some(s), Some(e)) if s + START.len() <= e => (html.to_string(), s + START.len(), e),
        _ => {
            let prefix = format!("<html><body>\r\n{}", START);
            let suffix = format!("{}\r\n</body></html>", END);
            let fragment_start = prefix.len();
            let fragment_end = fragment_start + html.len();
            (format!("{}{}{}", prefix, html, suffix), fragment_start, fragment_end)
        }
    };
    // The header has a fixed length: every offset is a ten-digit number.
    const HEADER_LEN: usize = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n".len();
    let header = format!(
        "Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n",
        HEADER_LEN,
        HEADER_LEN + body.len(),
        HEADER_LEN + fragment_start,
        HEADER_LEN + fragment_end,
    );
    let mut out = header.into_bytes();
    out.extend_from_slice(body.as_bytes());
    out.push(0);
    out
}

/// Extracts the HTML document from a CF_HTML payload (`StartHTML`..`EndHTML`).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn parse_cf_html(data: &[u8]) -> Option<String> {
    let end_of_data = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    let data = &data[..end_of_data];
    let text = String::from_utf8_lossy(data);
    let field = |name: &str| -> Option<usize> {
        let at = text.find(name)? + name.len();
        text[at..]
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()
    };
    let start = field("StartHTML:")?;
    let end = field("EndHTML:").unwrap_or(data.len()).min(data.len());
    if start >= end {
        return None;
    }
    Some(String::from_utf8_lossy(&data[start..end]).into_owned())
}

/// Wraps a clipboard DIB into a BMP file (adds the 14-byte file header).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn dib_to_bmp(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let u32_at = |at: usize| u32::from_le_bytes(dib[at..at + 4].try_into().unwrap());
    let header_size = u32_at(0) as usize;
    let bit_count = u16::from_le_bytes(dib[14..16].try_into().unwrap());
    let compression = u32_at(16);
    let colors_used = u32_at(32) as usize;
    let palette = if bit_count <= 8 {
        if colors_used > 0 { colors_used } else { 1usize << bit_count }
    } else {
        colors_used
    };
    let masks = if header_size == 40 && (compression == 3 || compression == 6) {
        if compression == 3 { 12 } else { 16 }
    } else {
        0
    };
    let offset = 14 + header_size + palette * 4 + masks;
    let mut bmp = Vec::with_capacity(14 + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&(offset as u32).to_le_bytes());
    bmp.extend_from_slice(dib);
    Some(bmp)
}
#[cfg(windows)]
mod imp {
    use super::{build_cf_html, dib_to_bmp, parse_cf_html, relock, ChangeGate};
    use crate::hostfiles::{self, HOST_FILES};
    use std::ffi::c_void;
    use std::fs::File;
    use std::mem::ManuallyDrop;
    use std::path::PathBuf;
    use std::ptr;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;
    use tracing::{error, info, warn};
    use windows::core::{implement, Error as WinError, Ref, Result as WinResult, BOOL, HRESULT};
    use windows::Win32::Foundation::{
        DATA_S_SAMEFORMATETC, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_NOTIMPL, E_POINTER, HGLOBAL,
        OLE_E_ADVISENOTSUPPORTED, STG_E_ACCESSDENIED, STG_E_INVALIDFUNCTION, STG_E_READFAULT,
    };
    use windows::Win32::System::Com::{
        IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, ISequentialStream_Impl,
        IStream, IStream_Impl, DVASPECT_CONTENT, FORMATETC, LOCKTYPE, STATFLAG, STATSTG, STGC, STGMEDIUM,
        STGMEDIUM_0, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED_HGLOBAL,
        TYMED_ISTREAM,
    };
    use windows::Win32::System::Ole::{OleInitialize, OleSetClipboard, OleUninitialize};
    use windows::Win32::UI::Shell::SHCreateStdEnumFmtEtc;
    use windows_sys::Win32::Foundation::{GlobalFree, HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::DataExchange::{
        AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
        GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
        RemoveClipboardFormatListener, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GHND};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::Shell::DragQueryFileW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer,
        PeekMessageW, PostMessageW, PostThreadMessageW, RegisterClassW, SetTimer, TranslateMessage, MSG,
        PM_NOREMOVE, WM_CLIPBOARDUPDATE, WM_QUIT, WM_TIMER, WM_USER, WNDCLASSW,
    };
    use winbridge_proto::clip::ClipBundle;
    use winbridge_proto::ManifestEntry;

    const CF_UNICODETEXT: u32 = 13;
    const CF_DIB: u32 = 8;
    const CF_HDROP: u32 = 15;
    const WM_STOP_CLIPBOARD: u32 = WM_USER + 101;
    /// Private thread message to the OLE thread: `lparam` is a `Box<(ClipBundle, u32)>`.
    const WM_APPLY_FILES: u32 = WM_USER + 102;
    const DEBOUNCE_TIMER: usize = 1;
    const DROPEFFECT_COPY: u32 = 1;
    const DATADIR_GET: u32 = 1;
    /// Largest piece served by one `IStream::Read`.
    const READ_MAX: usize = 1 << 20;

    /// Thread id of the OLE thread (0 until it runs).
    static OLE_THREAD: AtomicU32 = AtomicU32::new(0);

    struct Formats {
        html: u32,
        rtf: u32,
        png: u32,
        origin: u32,
        drop_effect: u32,
        file_group: u32,
        file_contents: u32,
    }

    static FORMATS: std::sync::LazyLock<Formats> = std::sync::LazyLock::new(|| {
        let register = |name: &str| {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe { RegisterClipboardFormatW(wide.as_ptr()) }
        };
        Formats {
            html: register("HTML Format"),
            rtf: register("Rich Text Format"),
            png: register("PNG"),
            origin: register("WinBridge.Origin"),
            drop_effect: register("Preferred DropEffect"),
            file_group: register("FileGroupDescriptorW"),
            file_contents: register("FileContents"),
        }
    });

    fn formats() -> &'static Formats {
        &FORMATS
    }

    /// Opens the clipboard, trying briefly (`tries` x 10 ms); another process may hold it.
    unsafe fn open_clipboard(tries: u32) -> bool {
        for attempt in 0..tries {
            if OpenClipboard(ptr::null_mut()) != 0 {
                return true;
            }
            if attempt + 1 < tries {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        false
    }

    /// Copies the bytes of one clipboard format. The clipboard must be open.
    unsafe fn read_global(format: u32) -> Option<Vec<u8>> {
        if IsClipboardFormatAvailable(format) == 0 {
            return None;
        }
        let handle = GetClipboardData(format);
        if handle.is_null() {
            return None;
        }
        let size = GlobalSize(handle);
        let ptr = GlobalLock(handle) as *const u8;
        if ptr.is_null() {
            return None;
        }
        let data = std::slice::from_raw_parts(ptr, size).to_vec();
        GlobalUnlock(handle);
        Some(data)
    }

    /// A movable global block holding `data` (ownership passes to whoever receives it).
    unsafe fn alloc_global(data: &[u8]) -> Option<*mut c_void> {
        let handle = GlobalAlloc(GHND, data.len().max(1));
        if handle.is_null() {
            return None;
        }
        let ptr = GlobalLock(handle) as *mut u8;
        if ptr.is_null() {
            GlobalFree(handle);
            return None;
        }
        ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        GlobalUnlock(handle);
        Some(handle)
    }

    /// Hands `data` to the open clipboard as `format`.
    unsafe fn set_global(format: u32, data: &[u8]) -> bool {
        let Some(handle) = alloc_global(data) else {
            return false;
        };
        if SetClipboardData(format, handle).is_null() {
            GlobalFree(handle);
            return false;
        }
        true
    }

    fn utf16_bytes_to_string(data: &[u8]) -> String {
        let units: Vec<u16> = data
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        String::from_utf16_lossy(&units)
    }

    /// Paths of a CF_HDROP block. The clipboard must be open.
    unsafe fn read_hdrop() -> Vec<PathBuf> {
        if IsClipboardFormatAvailable(CF_HDROP) == 0 {
            return Vec::new();
        }
        let handle = GetClipboardData(CF_HDROP);
        if handle.is_null() {
            return Vec::new();
        }
        hdrop_paths(handle)
    }

    /// Paths inside an HDROP handle (clipboard or OLE data object).
    pub unsafe fn hdrop_paths(hdrop: *mut c_void) -> Vec<PathBuf> {
        use std::os::windows::ffi::OsStringExt;
        let count = DragQueryFileW(hdrop, u32::MAX, ptr::null_mut(), 0);
        let mut paths = Vec::new();
        for index in 0..count {
            let len = DragQueryFileW(hdrop, index, ptr::null_mut(), 0) as usize;
            let mut buf = vec![0u16; len + 1];
            let copied = DragQueryFileW(hdrop, index, buf.as_mut_ptr(), buf.len() as u32) as usize;
            paths.push(PathBuf::from(std::ffi::OsString::from_wide(&buf[..copied])));
        }
        paths
    }

    /// What one read of the clipboard found.
    enum Read {
        /// Another process held the clipboard.
        Busy,
        /// Content WinBridge wrote itself: not to be sent.
        Own,
        Bundle(ClipBundle),
    }

    /// Reads the clipboard once, keeping it open only while the bytes are copied.
    fn read_clipboard() -> Read {
        let f = formats();
        unsafe {
            if !open_clipboard(5) {
                return Read::Busy;
            }
            // Our own content is recognised by its tag alone: no format is rendered for it.
            if IsClipboardFormatAvailable(f.origin) != 0 {
                CloseClipboard();
                return Read::Own;
            }
            let mut bundle = ClipBundle { files: read_hdrop(), ..ClipBundle::default() };
            if bundle.files.is_empty() {
                bundle.text = read_global(CF_UNICODETEXT).map(|d| utf16_bytes_to_string(&d));
                bundle.html = read_global(f.html).and_then(|d| parse_cf_html(&d));
                bundle.rtf = read_global(f.rtf).map(|mut d| {
                    if let Some(end) = d.iter().position(|&b| b == 0) {
                        d.truncate(end);
                    }
                    d
                });
                bundle.png = read_global(f.png);
                if bundle.png.is_none() {
                    bundle.bmp = read_global(CF_DIB).and_then(|d| dib_to_bmp(&d));
                }
            }
            CloseClipboard();
            Read::Bundle(bundle)
        }
    }

    /// The non-file formats of `bundle` as clipboard payloads.
    fn eager_items(bundle: &ClipBundle) -> Vec<(u32, Vec<u8>)> {
        let f = formats();
        let mut items = Vec::new();
        if let Some(text) = &bundle.text {
            let utf16: Vec<u8> = text
                .encode_utf16()
                .chain(std::iter::once(0))
                .flat_map(|u| u.to_le_bytes())
                .collect();
            items.push((CF_UNICODETEXT, utf16));
        }
        if let Some(html) = &bundle.html {
            items.push((f.html, build_cf_html(html)));
        }
        if let Some(rtf) = &bundle.rtf {
            let mut data = rtf.clone();
            data.push(0);
            items.push((f.rtf, data));
        }
        if let Some(png) = &bundle.png {
            items.push((f.png, png.clone()));
        }
        if let Some(bmp) = bundle.bmp.as_ref().filter(|b| b.len() > 14) {
            items.push((CF_DIB, bmp[14..].to_vec()));
        }
        items
    }

    /// Replaces the clipboard with `bundle` and tags it as WinBridge's own. Files the host offered
    /// are published as virtual files by the OLE thread, so for those the result only says the
    /// job was queued (failures are logged there).
    pub fn write_bundle(bundle: ClipBundle, clip_id: u32) -> bool {
        if bundle.files_offered {
            let thread = OLE_THREAD.load(Ordering::SeqCst);
            if thread == 0 {
                warn!("No OLE thread; host files were not offered to the guest");
                return false;
            }
            let job = Box::into_raw(Box::new((bundle, clip_id)));
            return unsafe {
                if PostThreadMessageW(thread, WM_APPLY_FILES, 0, job as LPARAM) == 0 {
                    drop(Box::from_raw(job));
                    false
                } else {
                    true
                }
            };
        }
        let f = formats();
        unsafe {
            if !open_clipboard(10) {
                warn!("Clipboard is busy; host content was not applied");
                return false;
            }
            EmptyClipboard();
            let mut ok = true;
            for (format, data) in eager_items(&bundle) {
                ok &= set_global(format, &data);
            }
            set_global(f.origin, &[1]);
            CloseClipboard();
            ok
        }
    }

    /// Publishes the host's files as virtual files (plus the eager formats of `bundle`). Runs on
    /// the OLE thread.
    unsafe fn publish_virtual_files(bundle: &ClipBundle, clip_id: u32) {
        let f = formats();
        let mut items = eager_items(bundle);
        items.push((f.drop_effect, DROPEFFECT_COPY.to_le_bytes().to_vec()));
        items.push((f.origin, vec![1]));
        let object: IDataObject = VirtualFiles {
            clip_id,
            descriptor: hostfiles::file_group_descriptor(&bundle.files_manifest),
            entries: bundle.files_manifest.clone(),
            items,
        }
        .into();
        let mut last = None;
        for attempt in 0..5 {
            match OleSetClipboard(&object) {
                Ok(()) => {
                    info!(
                        "Host files of clip {} are on the guest clipboard as {} virtual item(s); they transfer when pasted",
                        clip_id,
                        bundle.files_manifest.len()
                    );
                    return;
                }
                Err(e) => {
                    last = Some(e);
                    if attempt < 4 {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
            }
        }
        if let Some(e) = last {
            warn!("The guest clipboard could not be set (host files, clip {}): {}", clip_id, e);
        }
    }

    /// The OLE clipboard data object of the host's files. Names and sizes come from the manifest,
    /// so describing the files costs nothing; content is only served (and requested) on demand.
    #[implement(IDataObject)]
    struct VirtualFiles {
        clip_id: u32,
        descriptor: Vec<u8>,
        entries: Vec<ManifestEntry>,
        /// Eager formats: (clipboard format, bytes).
        items: Vec<(u32, Vec<u8>)>,
    }

    fn format_etc(format: u32, lindex: i32, tymed: u32) -> FORMATETC {
        FORMATETC {
            cfFormat: format as u16,
            ptd: ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0 as u32,
            lindex,
            tymed,
        }
    }

    impl VirtualFiles_Impl {
        fn eager(&self, format: u32) -> Option<&[u8]> {
            let f = formats();
            if format == f.file_group {
                return Some(&self.descriptor);
            }
            self.items.iter().find(|(cf, _)| *cf == format).map(|(_, d)| d.as_slice())
        }
    }

    impl IDataObject_Impl for VirtualFiles_Impl {
        fn GetData(&self, pformatetcin: *const FORMATETC) -> WinResult<STGMEDIUM> {
            let f = formats();
            let fe = unsafe { pformatetcin.as_ref() }.ok_or_else(|| WinError::from(E_POINTER))?;
            let format = fe.cfFormat as u32;
            if format == f.file_contents {
                if fe.tymed & TYMED_ISTREAM.0 as u32 == 0 {
                    return Err(DV_E_TYMED.into());
                }
                let entry = usize::try_from(fe.lindex)
                    .ok()
                    .and_then(|i| self.entries.get(i))
                    .filter(|e| !e.is_dir)
                    .ok_or_else(|| WinError::from(DV_E_LINDEX))?;
                let stream: IStream = ContentStream {
                    clip_id: self.clip_id,
                    index: fe.lindex as u32,
                    size: entry.size,
                    state: Mutex::new((0, None)),
                }
                .into();
                return Ok(STGMEDIUM {
                    tymed: TYMED_ISTREAM.0 as u32,
                    u: STGMEDIUM_0 { pstm: ManuallyDrop::new(Some(stream)) },
                    pUnkForRelease: ManuallyDrop::new(None),
                });
            }
            let data = self.eager(format).ok_or_else(|| WinError::from(DV_E_FORMATETC))?;
            if fe.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
                return Err(DV_E_TYMED.into());
            }
            let handle = unsafe { alloc_global(data) }.ok_or_else(|| WinError::from(E_POINTER))?;
            Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: HGLOBAL(handle) },
                pUnkForRelease: ManuallyDrop::new(None),
            })
        }

        fn GetDataHere(&self, _pformatetc: *const FORMATETC, _pmedium: *mut STGMEDIUM) -> WinResult<()> {
            Err(E_NOTIMPL.into())
        }

        fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
            let f = formats();
            let Some(fe) = (unsafe { pformatetc.as_ref() }) else {
                return E_POINTER;
            };
            let format = fe.cfFormat as u32;
            if format == f.file_contents {
                return if fe.tymed & TYMED_ISTREAM.0 as u32 != 0 { HRESULT(0) } else { DV_E_TYMED };
            }
            if self.eager(format).is_none() {
                return DV_E_FORMATETC;
            }
            if fe.tymed & TYMED_HGLOBAL.0 as u32 != 0 { HRESULT(0) } else { DV_E_TYMED }
        }

        fn GetCanonicalFormatEtc(&self, _pformatectin: *const FORMATETC, pformatetcout: *mut FORMATETC) -> HRESULT {
            if let Some(out) = unsafe { pformatetcout.as_mut() } {
                out.ptd = ptr::null_mut();
            }
            DATA_S_SAMEFORMATETC
        }

        fn SetData(&self, _pformatetc: *const FORMATETC, _pmedium: *const STGMEDIUM, _frelease: BOOL) -> WinResult<()> {
            // Explorer reports "PerformedDropEffect" here; a copy needs nothing from us.
            Err(E_NOTIMPL.into())
        }

        fn EnumFormatEtc(&self, dwdirection: u32) -> WinResult<IEnumFORMATETC> {
            if dwdirection != DATADIR_GET {
                return Err(E_NOTIMPL.into());
            }
            let f = formats();
            let hglobal = TYMED_HGLOBAL.0 as u32;
            let mut list = vec![
                format_etc(f.file_group, -1, hglobal),
                format_etc(f.file_contents, -1, TYMED_ISTREAM.0 as u32),
            ];
            list.extend(self.items.iter().map(|(cf, _)| format_etc(*cf, -1, hglobal)));
            unsafe { SHCreateStdEnumFmtEtc(&list) }
        }

        fn DAdvise(&self, _pformatetc: *const FORMATETC, _advf: u32, _padvsink: Ref<IAdviseSink>) -> WinResult<u32> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }

        fn DUnadvise(&self, _dwconnection: u32) -> WinResult<()> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }

        fn EnumDAdvise(&self) -> WinResult<IEnumSTATDATA> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }
    }

    /// The content of one virtual file. Reading asks the host for the files the first time and
    /// then blocks until the bytes have been staged by the message loop.
    #[implement(IStream)]
    struct ContentStream {
        clip_id: u32,
        index: u32,
        size: u64,
        /// Read position and the open staging file.
        state: Mutex<(u64, Option<File>)>,
    }

    impl ISequentialStream_Impl for ContentStream_Impl {
        fn Read(&self, pv: *mut c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
            if pv.is_null() {
                return E_POINTER;
            }
            let want = (cb as usize).min(READ_MAX);
            let buf = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, want) };
            let mut state = relock(&self.state);
            let (pos, file) = &mut *state;
            if let Err(e) = HOST_FILES.request_files(self.clip_id) {
                warn!("Paste of host files (clip {}): {}", self.clip_id, e);
                return STG_E_READFAULT;
            }
            match HOST_FILES.read_blocking(self.clip_id, self.index, *pos, buf, file, hostfiles::STALL) {
                Ok(n) => {
                    *pos += n as u64;
                    if let Some(out) = unsafe { pcbread.as_mut() } {
                        *out = n as u32;
                    }
                    HRESULT(0)
                }
                Err(e) => {
                    warn!("Paste of host files (clip {}, item {}) failed: {}", self.clip_id, self.index, e);
                    STG_E_READFAULT
                }
            }
        }

        fn Write(&self, _pv: *const c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
            STG_E_ACCESSDENIED
        }
    }

    impl IStream_Impl for ContentStream_Impl {
        fn Seek(&self, dlibmove: i64, dworigin: STREAM_SEEK, plibnewposition: *mut u64) -> WinResult<()> {
            let mut state = relock(&self.state);
            let base = if dworigin == STREAM_SEEK_SET {
                0
            } else if dworigin == STREAM_SEEK_CUR {
                state.0 as i64
            } else if dworigin == STREAM_SEEK_END {
                self.size as i64
            } else {
                return Err(STG_E_INVALIDFUNCTION.into());
            };
            let target = base
                .checked_add(dlibmove)
                .filter(|t| *t >= 0)
                .ok_or_else(|| WinError::from(STG_E_INVALIDFUNCTION))?;
            state.0 = target as u64;
            if let Some(out) = unsafe { plibnewposition.as_mut() } {
                *out = target as u64;
            }
            Ok(())
        }

        fn SetSize(&self, _libnewsize: u64) -> WinResult<()> {
            Err(STG_E_INVALIDFUNCTION.into())
        }

        fn CopyTo(&self, _pstm: Ref<IStream>, _cb: u64, _pcbread: *mut u64, _pcbwritten: *mut u64) -> WinResult<()> {
            Err(E_NOTIMPL.into())
        }

        fn Commit(&self, _grfcommitflags: &STGC) -> WinResult<()> {
            Ok(())
        }

        fn Revert(&self) -> WinResult<()> {
            Err(STG_E_INVALIDFUNCTION.into())
        }

        fn LockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: &LOCKTYPE) -> WinResult<()> {
            Err(STG_E_INVALIDFUNCTION.into())
        }

        fn UnlockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: u32) -> WinResult<()> {
            Err(STG_E_INVALIDFUNCTION.into())
        }

        fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> WinResult<()> {
            let out = unsafe { pstatstg.as_mut() }.ok_or_else(|| WinError::from(E_POINTER))?;
            *out = unsafe { std::mem::zeroed() };
            out.r#type = 2; // STGTY_STREAM
            out.cbSize = self.size;
            Ok(())
        }

        fn Clone(&self) -> WinResult<IStream> {
            let pos = relock(&self.state).0;
            Ok(ContentStream {
                clip_id: self.clip_id,
                index: self.index,
                size: self.size,
                state: Mutex::new((pos, None)),
            }
            .into())
        }
    }

    /// The thread that owns the OLE clipboard data object. Serving a virtual file blocks it until
    /// the bytes are here, so it is not the thread that watches the clipboard.
    fn run_ole_thread(ready: std::sync::mpsc::Sender<u32>) {
        unsafe {
            if let Err(e) = OleInitialize(None) {
                warn!("OleInitialize failed; host files cannot be pasted: {}", e);
                let _ = ready.send(0);
                return;
            }
            let mut msg: MSG = std::mem::zeroed();
            // Creates the thread's message queue before the id is published.
            PeekMessageW(&mut msg, ptr::null_mut(), WM_USER, WM_USER, PM_NOREMOVE);
            let _ = ready.send(GetCurrentThreadId());
            while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
                if msg.message == WM_APPLY_FILES {
                    let (bundle, clip_id) = *Box::from_raw(msg.lParam as *mut (ClipBundle, u32));
                    publish_virtual_files(&bundle, clip_id);
                    continue;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            OleUninitialize();
        }
    }

    pub struct ClipboardWatcher {
        hwnd: HWND,
    }

    impl ClipboardWatcher {
        /// Calls `on_change` once per clipboard change that did not come from WinBridge, after
        /// the clipboard has been quiet for [`ChangeGate::DEBOUNCE`].
        pub fn start<F>(on_change: F) -> Self
        where
            F: Fn(ClipBundle) + Send + 'static,
        {
            let (ole_tx, ole_rx) = std::sync::mpsc::channel::<u32>();
            std::thread::Builder::new()
                .name("winbridge-clip-ole".into())
                .spawn(move || run_ole_thread(ole_tx))
                .ok();
            OLE_THREAD.store(ole_rx.recv().unwrap_or(0), Ordering::SeqCst);

            let (tx, rx) = std::sync::mpsc::channel::<isize>();
            std::thread::spawn(move || {
                let class_name: Vec<u16> = "WinBridgeClipboardClass\0".encode_utf16().collect();
                let wc = WNDCLASSW {
                    style: 0,
                    lpfnWndProc: Some(wnd_proc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: ptr::null_mut(),
                    hIcon: ptr::null_mut(),
                    hCursor: ptr::null_mut(),
                    hbrBackground: ptr::null_mut(),
                    lpszMenuName: ptr::null(),
                    lpszClassName: class_name.as_ptr(),
                };

                unsafe {
                    RegisterClassW(&wc);
                    let hwnd = CreateWindowExW(
                        0,
                        class_name.as_ptr(),
                        ptr::null(),
                        0,
                        0,
                        0,
                        0,
                        0,
                        ptr::null_mut(), // HWND_MESSAGE
                        ptr::null_mut(),
                        ptr::null_mut(),
                        ptr::null(),
                    );

                    if hwnd.is_null() {
                        error!("Failed to create clipboard message window");
                        let _ = tx.send(0);
                        return;
                    }

                    if AddClipboardFormatListener(hwnd) == 0 {
                        error!("Failed to register clipboard format listener");
                    }

                    let _ = tx.send(hwnd as isize);

                    let debounce_ms = ChangeGate::DEBOUNCE.as_millis() as u32;
                    let retry_ms = ChangeGate::BUSY_RETRY.as_millis() as u32;
                    let mut gate = ChangeGate::new();
                    let mut msg: MSG = std::mem::zeroed();
                    while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
                        if msg.message == WM_STOP_CLIPBOARD {
                            KillTimer(hwnd, DEBOUNCE_TIMER);
                            DestroyWindow(hwnd);
                            break;
                        }

                        if msg.hwnd == hwnd && msg.message == WM_CLIPBOARDUPDATE {
                            // Re-arming restarts the wait: only the last update of a burst reads.
                            SetTimer(hwnd, DEBOUNCE_TIMER, debounce_ms, None);
                        } else if msg.hwnd == hwnd && msg.message == WM_TIMER && msg.wParam == DEBOUNCE_TIMER {
                            KillTimer(hwnd, DEBOUNCE_TIMER);
                            let seq = GetClipboardSequenceNumber();
                            if gate.should_read(seq) {
                                match read_clipboard() {
                                    Read::Busy => {
                                        if gate.read_busy() {
                                            SetTimer(hwnd, DEBOUNCE_TIMER, retry_ms, None);
                                        } else {
                                            warn!("The clipboard stayed busy; this change was not sent to the host");
                                        }
                                    }
                                    Read::Own => gate.read_done(seq),
                                    Read::Bundle(bundle) => {
                                        gate.read_done(seq);
                                        if !bundle.is_empty() {
                                            on_change(bundle);
                                        }
                                    }
                                }
                            }
                        }

                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }

                    RemoveClipboardFormatListener(hwnd);
                }
            });

            let hwnd = rx.recv().unwrap_or(0) as HWND;
            Self { hwnd }
        }
    }

    unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    impl Drop for ClipboardWatcher {
        fn drop(&mut self) {
            unsafe {
                if !self.hwnd.is_null() {
                    PostMessageW(self.hwnd, WM_STOP_CLIPBOARD, 0, 0);
                }
                let ole = OLE_THREAD.swap(0, Ordering::SeqCst);
                if ole != 0 {
                    PostThreadMessageW(ole, WM_QUIT, 0, 0);
                }
            }
        }
    }
}

#[cfg(not(windows))]
#[allow(dead_code, unused_imports)]
mod imp {
    use winbridge_proto::clip::ClipBundle;

    pub fn write_bundle(_bundle: ClipBundle, _clip_id: u32) -> bool {
        false
    }

    pub struct ClipboardWatcher;

    impl ClipboardWatcher {
        pub fn start<F>(_on_change: F) -> Self
        where
            F: Fn(ClipBundle) + Send + 'static,
        {
            Self
        }
    }
}

pub use imp::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_html_round_trips_through_the_header() {
        let payload = build_cf_html("<b>hi</b>");
        let doc = parse_cf_html(&payload).unwrap();
        assert!(doc.contains("<!--StartFragment--><b>hi</b><!--EndFragment-->"));
        // Offsets in the header point at the markers.
        let text = String::from_utf8_lossy(&payload);
        let start: usize = text["Version:0.9\r\nStartHTML:".len()..][..10].parse().unwrap();
        assert_eq!(&payload[start..start + 6], b"<html>");
    }

    #[test]
    fn dib_gets_a_file_header_with_the_pixel_offset() {
        let mut dib = vec![0u8; 40 + 8];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[14..16].copy_from_slice(&24u16.to_le_bytes());
        let bmp = dib_to_bmp(&dib).unwrap();
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(u32::from_le_bytes(bmp[10..14].try_into().unwrap()), 14 + 40);
        assert_eq!(bmp.len(), 14 + dib.len());
    }

    // (host file paste state is tested in `hostfiles`)
    #[test]
    fn guest_offer_only_serves_the_newest_clip_once_at_a_time() {
        let offer = GuestOffer::default();
        assert!(offer.claim(1).is_err());
        offer.remember(1, &[PathBuf::from("a")]);
        assert_eq!(offer.claim(1).unwrap(), vec![PathBuf::from("a")]);
        assert!(offer.claim(1).is_err(), "same stream twice");
        offer.release(1);
        assert!(offer.claim(1).is_ok());
        offer.release(1);
        offer.remember(2, &[PathBuf::from("b")]);
        assert!(offer.claim(1).is_err(), "stale");
        offer.remember(3, &[]);
        assert!(offer.claim(2).is_err(), "an offer without files clears the old one");
    }

    #[test]
    fn one_copy_is_read_once_and_an_unchanged_clipboard_is_skipped() {
        let mut gate = ChangeGate::new();
        assert!(gate.should_read(10));
        gate.read_done(10);
        // The timer firing again for the same content reads nothing.
        assert!(!gate.should_read(10));
        assert!(gate.should_read(12));
        gate.read_done(12);
        assert!(!gate.should_read(12));
        // Without a sequence number (0) every change is read.
        assert!(gate.should_read(0));
        gate.read_done(0);
        assert!(gate.should_read(0));
    }

    #[test]
    fn a_busy_clipboard_is_retried_a_few_times_then_dropped() {
        let mut gate = ChangeGate::new();
        assert!(gate.should_read(5));
        for _ in 0..ChangeGate::MAX_BUSY_RETRIES {
            assert!(gate.read_busy());
        }
        assert!(!gate.read_busy(), "gives up after the limit");
        // The content stays unread, so the next update reads it again.
        assert!(gate.should_read(5));
        gate.read_done(5);
        assert!(gate.read_busy(), "a successful read resets the counter");
    }

    #[test]
    fn the_debounce_outlasts_a_flush_but_not_a_paste() {
        assert!(ChangeGate::DEBOUNCE >= std::time::Duration::from_millis(100));
        assert!(ChangeGate::DEBOUNCE <= std::time::Duration::from_millis(300));
    }

    /// Uses the real OLE clipboard, so it needs an interactive window station.
    #[cfg(windows)]
    #[test]
    fn virtual_files_cost_nothing_until_their_content_is_read() {
        use crate::hostfiles::{self, HOST_FILES};
        use std::ffi::c_void;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL, TYMED_ISTREAM};
        use windows::Win32::System::Ole::{OleGetClipboard, OleInitialize, OleUninitialize};
        use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;
        use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
        use winbridge_proto::clip::ClipBundle;
        use winbridge_proto::ManifestEntry;

        let dir = std::env::temp_dir().join(format!("wb-virtual-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let staged = dir.join("f.txt");
        std::fs::write(&staged, b"").unwrap();

        let requests = Arc::new(AtomicU32::new(0));
        let counter = requests.clone();
        let feed_path = staged.clone();
        hostfiles::set_requester(move |clip_id| {
            counter.fetch_add(1, Ordering::SeqCst);
            let feed_path = feed_path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                HOST_FILES.touch(clip_id);
                HOST_FILES.entry_begun(clip_id, 1, feed_path.clone(), 5);
                std::fs::write(&feed_path, b"hel").unwrap();
                HOST_FILES.entry_data(clip_id, 1, 3);
                std::thread::sleep(Duration::from_millis(200));
                std::fs::write(&feed_path, b"hello").unwrap();
                HOST_FILES.entry_data(clip_id, 1, 5);
                HOST_FILES.finish(clip_id, Ok(()));
            });
            Ok(())
        });
        let echoed = Arc::new(AtomicBool::new(false));
        let echo = echoed.clone();
        let _watcher = ClipboardWatcher::start(move |_| echo.store(true, Ordering::SeqCst));

        let manifest = vec![
            ManifestEntry { rel_path: "d".into(), is_dir: true, size: 0 },
            ManifestEntry { rel_path: "d/f.txt".into(), is_dir: false, size: 5 },
        ];
        HOST_FILES.offer(4242, &manifest);
        let bundle = ClipBundle { files_offered: true, files_manifest: manifest, ..ClipBundle::default() };
        assert!(write_bundle(bundle, 4242));
        std::thread::sleep(Duration::from_millis(700));

        let reader = std::thread::spawn({
            let requests = requests.clone();
            move || unsafe {
                OleInitialize(None).unwrap();
                let data: IDataObject = OleGetClipboard().unwrap();
                let register = |name: &str| {
                    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
                    RegisterClipboardFormatW(wide.as_ptr())
                };
                let etc = |format: u32, lindex: i32, tymed: i32| FORMATETC {
                    cfFormat: format as u16,
                    ptd: std::ptr::null_mut(),
                    dwAspect: DVASPECT_CONTENT.0 as u32,
                    lindex,
                    tymed: tymed as u32,
                };
                // The descriptor: names and sizes, no transfer.
                let mut medium = data.GetData(&etc(register("FileGroupDescriptorW"), -1, TYMED_HGLOBAL.0)).unwrap();
                let handle = medium.u.hGlobal.0;
                let size = GlobalSize(handle);
                let ptr = GlobalLock(handle) as *const u8;
                let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
                GlobalUnlock(handle);
                assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()), 2);
                windows::Win32::System::Ole::ReleaseStgMedium(&mut medium);
                assert_eq!(requests.load(Ordering::SeqCst), 0, "describing files must not start a transfer");

                // Asking for a stream does not either; reading it does, once.
                let contents = register("FileContents");
                let mut medium = data.GetData(&etc(contents, 1, TYMED_ISTREAM.0)).unwrap();
                let stream = (*medium.u.pstm).clone().unwrap();
                assert_eq!(requests.load(Ordering::SeqCst), 0, "a stream handle is free");
                let mut buf = [0u8; 5];
                let mut read = 0u32;
                let started = std::time::Instant::now();
                stream.Read(buf.as_mut_ptr() as *mut c_void, 5, Some(&mut read)).ok().unwrap();
                assert_eq!((read, &buf), (5, b"hello"));
                assert!(started.elapsed() >= Duration::from_millis(350), "the read waited for the data");
                assert_eq!(requests.load(Ordering::SeqCst), 1);
                drop(stream);
                windows::Win32::System::Ole::ReleaseStgMedium(&mut medium);

                // A second paste of the same snapshot reuses what was staged.
                let medium2 = data.GetData(&etc(contents, 1, TYMED_ISTREAM.0)).unwrap();
                let again = (*medium2.u.pstm).clone().unwrap();
                let mut buf2 = [0u8; 5];
                again.Read(buf2.as_mut_ptr() as *mut c_void, 5, Some(&mut read)).ok().unwrap();
                assert_eq!(&buf2, b"hello");
                assert_eq!(requests.load(Ordering::SeqCst), 1, "the files are requested once");
                // A directory has no content.
                assert!(data.GetData(&etc(contents, 0, TYMED_ISTREAM.0)).is_err());
                drop(data);
                OleUninitialize();
            }
        });
        reader.join().unwrap();
        assert!(!echoed.load(Ordering::SeqCst), "our own virtual-file clipboard is never re-offered");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
