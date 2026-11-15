// SPDX-License-Identifier: GPL-3.0-or-later

//! Clipboard snapshots shared by host and agent.
//!
//! A snapshot travels as `ClipboardOffer` plus one `ClipboardItem` per non-file format. Copied
//! files are **not** sent with the snapshot: the offer only lists `FileList`. The side that wants
//! to paste asks for them with `ClipFilesRequest { clip_id }` at the moment of the paste, and the
//! offering side answers with a file stream (`FilesBegin`...`FilesEnd`) whose `stream_id` is the
//! `clip_id`. [`Assembler::handle_files`] reports progress while that stream arrives.

use crate::files::{self, Receiver};
use crate::{ClipboardFormat, ManifestEntry, Message};
use std::path::PathBuf;

/// Largest single non-file clipboard item that is sent.
pub const MAX_ITEM_BYTES: usize = 48 * 1024 * 1024;
/// Largest total size of copied files that is offered and sent.
pub const CLIPBOARD_FILES_CAP: u64 = 8 * 1024 * 1024 * 1024;

/// Everything the clipboard holds that WinBridge carries.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ClipBundle {
    pub text: Option<String>,
    pub html: Option<String>,
    pub rtf: Option<Vec<u8>>,
    /// PNG file bytes.
    pub png: Option<Vec<u8>>,
    /// BMP file bytes (a DIB with a file header).
    pub bmp: Option<Vec<u8>>,
    /// Sender side: local paths of copied files and directories. Receiver side: always empty;
    /// the files arrive on demand (see [`ClipBundle::files_offered`]).
    pub files: Vec<PathBuf>,
    /// Receiver side: the sender offers copied files that [`Message::ClipFilesRequest`] fetches.
    pub files_offered: bool,
    /// Receiver side: names and sizes of the offered files (stream order), from `ClipFilesManifest`.
    pub files_manifest: Vec<ManifestEntry>,
}

impl ClipBundle {
    pub fn is_empty(&self) -> bool {
        self.text.is_none()
            && self.html.is_none()
            && self.rtf.is_none()
            && self.png.is_none()
            && self.bmp.is_none()
            && self.files.is_empty()
            && !self.files_offered
    }

    /// Sends `ClipboardOffer` and the non-file items. Items above [`MAX_ITEM_BYTES`] and file
    /// selections above [`CLIPBOARD_FILES_CAP`] (or that cannot be scanned) are left out; the
    /// returned strings say what was skipped. Files are only listed in the offer; the caller keeps
    /// `self.files` and answers a `ClipFilesRequest` with [`send_files`]. Fails only when `send` fails.
    pub fn send<F>(&self, clip_id: u32, send: &mut F) -> Result<Vec<String>, String>
    where
        F: FnMut(Message) -> Result<(), String>,
    {
        let mut warnings = Vec::new();
        let mut items: Vec<(ClipboardFormat, Vec<u8>)> = Vec::new();
        let mut push = |format: ClipboardFormat, data: Option<Vec<u8>>, warnings: &mut Vec<String>| {
            if let Some(data) = data {
                if data.len() > MAX_ITEM_BYTES {
                    warnings.push(format!("{} item of {} bytes skipped (too large)", format, data.len()));
                } else {
                    items.push((format, data));
                }
            }
        };
        push(ClipboardFormat::Text, self.text.as_ref().map(|t| t.as_bytes().to_vec()), &mut warnings);
        push(ClipboardFormat::Html, self.html.as_ref().map(|t| t.as_bytes().to_vec()), &mut warnings);
        push(ClipboardFormat::Rtf, self.rtf.clone(), &mut warnings);
        push(ClipboardFormat::Png, self.png.clone(), &mut warnings);
        push(ClipboardFormat::Bmp, self.bmp.clone(), &mut warnings);

        let manifest: Option<Vec<ManifestEntry>> = if self.files.is_empty() {
            None
        } else {
            match files::scan(&self.files, CLIPBOARD_FILES_CAP) {
                Ok(entries) => Some(
                    entries
                        .iter()
                        .map(|e| ManifestEntry { rel_path: e.rel.clone(), is_dir: e.is_dir, size: e.size })
                        .collect(),
                ),
                Err(e) => {
                    warnings.push(format!("files not offered: {}", e));
                    None
                }
            }
        };

        let mut formats: Vec<ClipboardFormat> = items.iter().map(|(f, _)| *f).collect();
        if manifest.is_some() {
            formats.push(ClipboardFormat::FileList);
        }
        if formats.is_empty() {
            return Ok(warnings);
        }
        send(Message::ClipboardOffer { clip_id, formats })?;
        if let Some(entries) = manifest {
            send(Message::ClipFilesManifest { clip_id, entries })?;
        }
        for (format, data) in items {
            send(Message::ClipboardItem { clip_id, format, data })?;
        }
        Ok(warnings)
    }
}

/// Scans `paths` and sends them as one file stream with `stream_id == clip_id`. `on_progress`
/// gets `(bytes_sent, total_bytes)` after every chunk. Used to answer `ClipFilesRequest`.
pub fn send_files<F, P>(clip_id: u32, paths: &[PathBuf], send: &mut F, mut on_progress: P) -> Result<(), String>
where
    F: FnMut(Message) -> Result<(), String>,
    P: FnMut(u64, u64),
{
    let entries = files::scan(paths, CLIPBOARD_FILES_CAP)?;
    let total = files::total_bytes(&entries);
    let mut sent = 0u64;
    files::send_stream(clip_id, &entries, &mut |msg| {
        if let Message::FileChunk { data, .. } = &msg {
            sent += data.len() as u64;
        }
        send(msg)?;
        on_progress(sent, total);
        Ok(())
    })
}

/// Reassembles one offered snapshot. Complete once the manifest (when files are offered) and
/// every offered non-file format have arrived.
pub struct Incoming {
    clip_id: u32,
    expected: Vec<ClipboardFormat>,
    got: Vec<ClipboardFormat>,
    manifest_pending: bool,
    bundle: ClipBundle,
}

impl Incoming {
    pub fn new(clip_id: u32, formats: Vec<ClipboardFormat>) -> Self {
        let files_offered = formats.contains(&ClipboardFormat::FileList);
        let expected = formats
            .into_iter()
            .filter(|f| *f != ClipboardFormat::FileList)
            .collect();
        Self {
            clip_id,
            expected,
            got: Vec::new(),
            manifest_pending: files_offered,
            bundle: ClipBundle { files_offered, ..ClipBundle::default() },
        }
    }

    pub fn clip_id(&self) -> u32 {
        self.clip_id
    }

    /// The names and sizes of the offered files.
    pub fn manifest(&mut self, entries: Vec<ManifestEntry>) {
        if self.manifest_pending {
            self.bundle.files_manifest = entries;
            self.manifest_pending = false;
        }
    }

    pub fn item(&mut self, format: ClipboardFormat, data: Vec<u8>) {
        if !self.expected.contains(&format) || self.got.contains(&format) {
            return;
        }
        match format {
            ClipboardFormat::Text => self.bundle.text = Some(String::from_utf8_lossy(&data).into_owned()),
            ClipboardFormat::Html => self.bundle.html = Some(String::from_utf8_lossy(&data).into_owned()),
            ClipboardFormat::Rtf => self.bundle.rtf = Some(data),
            ClipboardFormat::Png => self.bundle.png = Some(data),
            ClipboardFormat::Bmp => self.bundle.bmp = Some(data),
            ClipboardFormat::FileList => return,
        }
        self.got.push(format);
    }

    pub fn is_complete(&self) -> bool {
        !self.manifest_pending && self.expected.iter().all(|f| self.got.contains(f))
    }

    pub fn into_bundle(self) -> ClipBundle {
        self.bundle
    }
}

/// What a file-stream message did.
#[derive(Debug, Clone, PartialEq)]
pub enum FilesEvent {
    /// The message did not belong to the stream being fetched.
    Ignored,
    /// Bytes received so far and the stream total.
    Progress { done: u64, total: u64 },
    /// The stream ended; these are the top-level paths it created below the staging directory.
    Done { clip_id: u32, paths: Vec<PathBuf> },
}

struct Fetch {
    clip_id: u32,
    receiver: Receiver,
    done: u64,
    total: u64,
}

/// Feeds clipboard messages into [`Incoming`] snapshots (one at a time) and file-stream messages
/// into the fetch that a `ClipFilesRequest` started. A new offer replaces, and cleans up, the
/// previous snapshot and its staged files.
pub struct Assembler {
    staging_root: PathBuf,
    current: Option<Incoming>,
    /// `clip_id` of the newest complete offer that listed files: the only stream accepted.
    files_clip: Option<u32>,
    fetch: Option<Fetch>,
}

impl Assembler {
    pub fn new(staging_root: PathBuf) -> Self {
        Self { staging_root, current: None, files_clip: None, fetch: None }
    }

    /// Where the files of snapshot `clip_id` are staged.
    pub fn staging_dir(&self, clip_id: u32) -> PathBuf {
        self.staging_root.join(clip_id.to_string())
    }

    /// Handles `ClipboardOffer` and `ClipboardItem`; returns the snapshot once it is complete.
    /// Everything else is ignored. A snapshot that lists files has `files_offered` set.
    pub fn handle(&mut self, msg: &Message) -> Result<Option<ClipBundle>, String> {
        match msg {
            Message::ClipboardOffer { clip_id, formats } => {
                self.forget_files();
                self.current = Some(Incoming::new(*clip_id, formats.clone()));
                Ok(self.take_complete())
            }
            Message::ClipboardItem { clip_id, format, data } => {
                if let Some(incoming) = self.current.as_mut().filter(|i| i.clip_id() == *clip_id) {
                    incoming.item(*format, data.clone());
                }
                Ok(self.take_complete())
            }
            Message::ClipFilesManifest { clip_id, entries } => {
                if let Some(incoming) = self.current.as_mut().filter(|i| i.clip_id() == *clip_id) {
                    incoming.manifest(entries.clone());
                }
                Ok(self.take_complete())
            }
            _ => Ok(None),
        }
    }

    /// The current snapshot once offer, manifest (when files are offered) and items are all here.
    fn take_complete(&mut self) -> Option<ClipBundle> {
        if !self.current.as_ref().is_some_and(Incoming::is_complete) {
            return None;
        }
        let incoming = self.current.take().expect("checked above");
        self.files_clip = incoming.bundle.files_offered.then_some(incoming.clip_id);
        Some(incoming.into_bundle())
    }

    /// Handles `FilesBegin`, `FileEntry`, `FileChunk` and `FilesEnd` of the stream that was
    /// requested for the newest offer. An error abandons the fetch and deletes what it staged.
    pub fn handle_files(&mut self, msg: &Message) -> Result<FilesEvent, String> {
        let stream_id = match msg {
            Message::FilesBegin { stream_id, .. }
            | Message::FileEntry { stream_id, .. }
            | Message::FileChunk { stream_id, .. }
            | Message::FilesEnd { stream_id, .. } => *stream_id,
            _ => return Ok(FilesEvent::Ignored),
        };
        if self.files_clip != Some(stream_id) {
            return Ok(FilesEvent::Ignored);
        }
        let result = self.apply_files(stream_id, msg);
        if result.is_err() {
            self.forget_files();
        }
        result
    }

    fn apply_files(&mut self, stream_id: u32, msg: &Message) -> Result<FilesEvent, String> {
        match msg {
            Message::FilesBegin { entries, total_bytes, .. } => {
                let receiver = Receiver::new(self.staging_dir(stream_id), *entries, *total_bytes, CLIPBOARD_FILES_CAP)?;
                self.fetch = Some(Fetch { clip_id: stream_id, receiver, done: 0, total: *total_bytes });
                Ok(FilesEvent::Progress { done: 0, total: *total_bytes })
            }
            Message::FileEntry { index, rel_path, is_dir, size, .. } => {
                self.fetch_mut()?.receiver.entry(*index, rel_path, *is_dir, *size)?;
                Ok(FilesEvent::Ignored)
            }
            Message::FileChunk { index, offset, data, .. } => {
                let fetch = self.fetch_mut()?;
                fetch.receiver.chunk(*index, *offset, data)?;
                fetch.done += data.len() as u64;
                Ok(FilesEvent::Progress { done: fetch.done, total: fetch.total })
            }
            Message::FilesEnd { success, error, .. } => {
                let fetch = self.fetch.take().ok_or_else(|| "FilesEnd before FilesBegin".to_string())?;
                let paths = fetch.receiver.finish(*success, error.clone())?;
                Ok(FilesEvent::Done { clip_id: fetch.clip_id, paths })
            }
            _ => Ok(FilesEvent::Ignored),
        }
    }

    fn fetch_mut(&mut self) -> Result<&mut Fetch, String> {
        self.fetch.as_mut().ok_or_else(|| "file data before FilesBegin".to_string())
    }

    /// Drops an unfinished fetch and deletes the staged files of the previous snapshot.
    fn forget_files(&mut self) {
        self.fetch = None;
        if let Some(old) = self.files_clip.take() {
            let _ = std::fs::remove_dir_all(self.staging_dir(old));
        }
        if let Some(old) = self.current.take() {
            let _ = std::fs::remove_dir_all(self.staging_dir(old.clip_id()));
        }
    }

    /// Cancels the fetch in progress (the user gave up); staged files are deleted.
    pub fn cancel_files(&mut self) {
        self.fetch = None;
        if let Some(clip_id) = self.files_clip {
            let _ = std::fs::remove_dir_all(self.staging_dir(clip_id));
        }
    }

    /// Where the staged entry `index` of the stream being fetched lives (after its `FileEntry`).
    pub fn entry_path(&self, index: u32) -> Option<PathBuf> {
        self.fetch.as_ref().and_then(|f| f.receiver.entry_path(index)).map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn items_arrive_with_the_offer_and_files_only_on_request() {
        let dir = std::env::temp_dir().join(format!("winbridge-clip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), b"file body").unwrap();

        let bundle = ClipBundle {
            text: Some("plain".into()),
            html: Some("<b>bold</b>".into()),
            png: Some(vec![0x89, b'P', b'N', b'G']),
            files: vec![dir.join("note.txt")],
            ..ClipBundle::default()
        };

        let mut asm = Assembler::new(dir.join("staging"));
        let mut snapshot = None;
        let mut sent = Vec::new();
        let warnings = bundle
            .send(3, &mut |msg| {
                sent.push(msg);
                Ok(())
            })
            .unwrap();
        assert!(warnings.is_empty());
        assert!(!sent.iter().any(|m| matches!(m, Message::FilesBegin { .. })), "files must not be pushed");
        for msg in &sent {
            if let Some(done) = asm.handle(msg).unwrap() {
                snapshot = Some(done);
            }
        }
        let received = snapshot.expect("snapshot completes without the files");
        assert_eq!(received.text.as_deref(), Some("plain"));
        assert_eq!(received.png, bundle.png);
        assert!(received.files_offered && received.files.is_empty());
        assert_eq!(
            received.files_manifest,
            vec![ManifestEntry { rel_path: "note.txt".into(), is_dir: false, size: 9 }]
        );
        let offer_pos = sent.iter().position(|m| matches!(m, Message::ClipboardOffer { .. })).unwrap();
        assert!(matches!(sent[offer_pos + 1], Message::ClipFilesManifest { clip_id: 3, .. }));

        // The paste: the offering side streams on request.
        let mut events = Vec::new();
        let mut progress = Vec::new();
        send_files(3, &bundle.files, &mut |msg| {
            events.push(asm.handle_files(&msg).unwrap());
            Ok(())
        }, |done, total| progress.push((done, total)))
        .unwrap();
        assert_eq!(progress.last(), Some(&(9, 9)));
        match events.last() {
            Some(FilesEvent::Done { clip_id: 3, paths }) => {
                assert_eq!(std::fs::read(&paths[0]).unwrap(), b"file body");
            }
            other => panic!("unexpected {:?}", other),
        }
    }

    #[test]
    fn a_stream_nobody_asked_for_is_ignored() {
        let mut asm = Assembler::new(PathBuf::from("/nonexistent"));
        let msg = Message::FilesBegin { stream_id: 5, entries: 1, total_bytes: 1 };
        assert_eq!(asm.handle_files(&msg).unwrap(), FilesEvent::Ignored);
    }

    #[test]
    fn incomplete_snapshot_is_not_applied() {
        let mut incoming = Incoming::new(1, vec![ClipboardFormat::Text, ClipboardFormat::Html]);
        incoming.item(ClipboardFormat::Text, b"x".to_vec());
        assert!(!incoming.is_complete());
    }

    #[test]
    fn staging_dir_is_per_snapshot() {
        let asm = Assembler::new(Path::new("/s").to_path_buf());
        assert_eq!(asm.staging_dir(4), Path::new("/s").join("4"));
    }

    #[test]
    fn a_files_snapshot_completes_only_with_offer_manifest_and_items() {
        let mut asm = Assembler::new(PathBuf::from("/nonexistent"));
        let offer = Message::ClipboardOffer {
            clip_id: 8,
            formats: vec![ClipboardFormat::Text, ClipboardFormat::FileList],
        };
        let item = Message::ClipboardItem { clip_id: 8, format: ClipboardFormat::Text, data: b"t".to_vec() };
        let entries = vec![ManifestEntry { rel_path: "a".into(), is_dir: true, size: 0 }];
        let manifest = Message::ClipFilesManifest { clip_id: 8, entries: entries.clone() };
        assert!(asm.handle(&offer).unwrap().is_none());
        assert!(asm.handle(&item).unwrap().is_none(), "manifest still missing");
        // A manifest of another snapshot does not count.
        let other = Message::ClipFilesManifest { clip_id: 9, entries: Vec::new() };
        assert!(asm.handle(&other).unwrap().is_none());
        let done = asm.handle(&manifest).unwrap().expect("complete");
        assert_eq!(done.files_manifest, entries);
        assert!(done.files_offered);
        assert_eq!(done.text.as_deref(), Some("t"));

        // Files only: complete with the manifest alone, items may arrive in any order.
        let offer = Message::ClipboardOffer { clip_id: 10, formats: vec![ClipboardFormat::FileList] };
        assert!(asm.handle(&offer).unwrap().is_none());
        let manifest = Message::ClipFilesManifest { clip_id: 10, entries: entries.clone() };
        assert!(asm.handle(&manifest).unwrap().is_some());
    }
}
