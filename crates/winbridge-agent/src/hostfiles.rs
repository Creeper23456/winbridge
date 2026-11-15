// SPDX-License-Identifier: GPL-3.0-or-later

//! Host -> guest file paste, the part that does not depend on Windows.
//!
//! The host's files are published to the guest clipboard as virtual files: names and sizes come
//! from the manifest of the offer, so presenting them (and Explorer probing the clipboard) costs
//! nothing. The first read of a file's content sends one `ClipFilesRequest`; the message loop then
//! feeds the arriving stream into [`HostFiles`] while the readers block in
//! [`HostFiles::read_blocking`] until the bytes they want are staged on disk.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};
use winbridge_proto::{files, ManifestEntry};

/// How long a reader waits without any sign of progress before it gives up.
pub const STALL: Duration = Duration::from_secs(60);

type Requester = Box<dyn Fn(u32) -> Result<(), String> + Send + Sync>;

/// Sends `ClipFilesRequest` to the host.
static REQUESTER: OnceLock<Requester> = OnceLock::new();

pub fn set_requester<F>(requester: F)
where
    F: Fn(u32) -> Result<(), String> + Send + Sync + 'static,
{
    let _ = REQUESTER.set(Box::new(requester));
}

fn send_request(clip_id: u32) -> Result<(), String> {
    match REQUESTER.get() {
        Some(request) => request(clip_id),
        None => Err("no connection to the host".to_string()),
    }
}

fn relock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Entry {
    is_dir: bool,
    size: u64,
    /// Bytes of this entry that are on disk.
    received: u64,
    path: Option<PathBuf>,
}

enum Outcome {
    Running,
    Done,
    Failed(String),
}

struct Slot {
    clip_id: Option<u32>,
    requested: bool,
    outcome: Outcome,
    entries: Vec<Entry>,
    progress: Instant,
}

/// What a reader may take next.
#[derive(Debug, PartialEq)]
pub enum Avail {
    /// The position is at or past the end of the entry.
    Eof,
    /// `bytes` (at least one) can be read from `path` at the position asked for.
    Ready { path: PathBuf, bytes: u64 },
}

/// The one instance shared by the message loop and the clipboard (OLE) thread.
pub static HOST_FILES: LazyLock<HostFiles> = LazyLock::new(HostFiles::new);

pub struct HostFiles {
    slot: Mutex<Slot>,
    changed: Condvar,
}

impl HostFiles {
    fn new() -> Self {
        Self {
            slot: Mutex::new(Slot {
                clip_id: None,
                requested: false,
                outcome: Outcome::Running,
                entries: Vec::new(),
                progress: Instant::now(),
            }),
            changed: Condvar::new(),
        }
    }

    /// A new snapshot with files was offered: it replaces whatever was there.
    pub fn offer(&self, clip_id: u32, manifest: &[ManifestEntry]) {
        let mut slot = relock(&self.slot);
        slot.clip_id = Some(clip_id);
        slot.requested = false;
        slot.outcome = Outcome::Running;
        slot.entries = manifest
            .iter()
            .map(|e| Entry { is_dir: e.is_dir, size: e.size, received: 0, path: None })
            .collect();
        slot.progress = Instant::now();
        self.changed.notify_all();
    }

    /// The host clipboard changed (or is about to): readers of the old snapshot fail at once.
    pub fn invalidate(&self) {
        relock(&self.slot).clip_id = None;
        self.changed.notify_all();
    }

    /// The stream of `clip_id` began: restarts the stall timer.
    pub fn touch(&self, clip_id: u32) {
        let mut slot = relock(&self.slot);
        if slot.clip_id == Some(clip_id) {
            slot.progress = Instant::now();
            self.changed.notify_all();
        }
    }

    /// Entry `index` of the stream was created at `path`; the sender states its real `size`.
    pub fn entry_begun(&self, clip_id: u32, index: u32, path: PathBuf, size: u64) {
        let mut slot = relock(&self.slot);
        if slot.clip_id != Some(clip_id) {
            return;
        }
        if let Some(entry) = slot.entries.get_mut(index as usize) {
            entry.path = Some(path);
            entry.size = size;
        }
        slot.progress = Instant::now();
        self.changed.notify_all();
    }

    /// `received` bytes of entry `index` are on disk.
    pub fn entry_data(&self, clip_id: u32, index: u32, received: u64) {
        let mut slot = relock(&self.slot);
        if slot.clip_id != Some(clip_id) {
            return;
        }
        if let Some(entry) = slot.entries.get_mut(index as usize) {
            entry.received = entry.received.max(received);
        }
        slot.progress = Instant::now();
        self.changed.notify_all();
    }

    /// The stream of `clip_id` ended.
    pub fn finish(&self, clip_id: u32, result: Result<(), String>) {
        let mut slot = relock(&self.slot);
        if slot.clip_id == Some(clip_id) {
            slot.outcome = match result {
                Ok(()) => Outcome::Done,
                Err(e) => Outcome::Failed(e),
            };
            self.changed.notify_all();
        }
    }

    /// Runs `request` (which asks the host for the files) the first time anybody reads content of
    /// snapshot `clip_id`; later calls return at once.
    pub fn request_once<R>(&self, clip_id: u32, request: R) -> Result<(), String>
    where
        R: FnOnce(u32) -> Result<(), String>,
    {
        let mut slot = relock(&self.slot);
        if slot.clip_id != Some(clip_id) {
            return Err("the host clipboard changed in the meantime".into());
        }
        if slot.requested {
            return Ok(());
        }
        slot.requested = true;
        slot.progress = Instant::now();
        drop(slot);
        let sent = request(clip_id);
        if let Err(e) = sent {
            let mut slot = relock(&self.slot);
            if slot.clip_id == Some(clip_id) {
                slot.requested = false;
            }
            return Err(format!("the file request could not be sent: {}", e));
        }
        Ok(())
    }

    /// [`request_once`](Self::request_once) with the connection to the host.
    pub fn request_files(&self, clip_id: u32) -> Result<(), String> {
        self.request_once(clip_id, send_request)
    }

    /// Waits until a byte of entry `index` at `pos` is on disk (or the entry ends).
    pub fn wait_bytes(&self, clip_id: u32, index: u32, pos: u64, want: u64, stall: Duration) -> Result<Avail, String> {
        let mut slot = relock(&self.slot);
        loop {
            if slot.clip_id != Some(clip_id) {
                return Err("the host clipboard changed in the meantime".into());
            }
            let finished = matches!(slot.outcome, Outcome::Done);
            let Some(entry) = slot.entries.get(index as usize) else {
                return Err(format!("no entry {}", index));
            };
            if entry.is_dir {
                return Err(format!("entry {} is a directory", index));
            }
            if pos >= entry.size {
                return Ok(Avail::Eof);
            }
            if let Outcome::Failed(reason) = &slot.outcome {
                return Err(reason.clone());
            }
            let have = if finished { entry.size } else { entry.received };
            match &entry.path {
                Some(path) if have > pos => {
                    return Ok(Avail::Ready { path: path.clone(), bytes: (have - pos).min(want.max(1)) });
                }
                None if finished => return Err(format!("entry {} never arrived", index)),
                _ => {}
            }
            let idle = slot.progress.elapsed();
            if idle >= stall {
                let reason = format!("no progress for {} s", stall.as_secs());
                slot.outcome = Outcome::Failed(reason.clone());
                self.changed.notify_all();
                return Err(reason);
            }
            slot = self
                .changed
                .wait_timeout(slot, stall - idle)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Fills `buf` from entry `index` starting at `pos`, blocking until the bytes are staged. A
    /// result shorter than `buf` means the end of the entry. `file` caches the open staging file.
    pub fn read_blocking(
        &self,
        clip_id: u32,
        index: u32,
        pos: u64,
        buf: &mut [u8],
        file: &mut Option<File>,
        stall: Duration,
    ) -> Result<usize, String> {
        let mut got = 0usize;
        while got < buf.len() {
            let at = pos + got as u64;
            match self.wait_bytes(clip_id, index, at, (buf.len() - got) as u64, stall)? {
                Avail::Eof => break,
                Avail::Ready { path, bytes } => {
                    if file.is_none() {
                        *file = Some(File::open(&path).map_err(|e| format!("{}: {}", path.display(), e))?);
                    }
                    let f = file.as_mut().expect("opened above");
                    f.seek(SeekFrom::Start(at)).map_err(|e| e.to_string())?;
                    let n = f.read(&mut buf[got..got + bytes as usize]).map_err(|e| e.to_string())?;
                    if n == 0 {
                        return Err(format!("{} is shorter than announced", path.display()));
                    }
                    got += n;
                }
            }
        }
        Ok(got)
    }
}

const FD_ATTRIBUTES: u32 = 0x0000_0004;
const FD_FILESIZE: u32 = 0x0000_0040;
const FD_PROGRESSUI: u32 = 0x0000_4000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
/// `sizeof(FILEDESCRIPTORW)`.
const DESCRIPTOR_LEN: usize = 592;
const NAME_OFFSET: usize = 72;
/// `MAX_PATH - 1` UTF-16 units fit in `cFileName`.
const NAME_UNITS: usize = 259;

/// The name a descriptor shows for `rel_path`: sanitised components joined with backslashes.
pub fn descriptor_name(rel_path: &str) -> String {
    let parts = files::safe_components(rel_path);
    if parts.is_empty() {
        "_".to_string()
    } else {
        parts.join("\\")
    }
}

/// A `FILEGROUPDESCRIPTORW` (the `FileGroupDescriptorW` clipboard format) for `entries`.
pub fn file_group_descriptor(entries: &[ManifestEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + entries.len() * DESCRIPTOR_LEN);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for entry in entries {
        let mut d = [0u8; DESCRIPTOR_LEN];
        d[0..4].copy_from_slice(&(FD_ATTRIBUTES | FD_FILESIZE | FD_PROGRESSUI).to_le_bytes());
        let attributes = if entry.is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
        d[36..40].copy_from_slice(&attributes.to_le_bytes());
        d[64..68].copy_from_slice(&((entry.size >> 32) as u32).to_le_bytes());
        d[68..72].copy_from_slice(&(entry.size as u32).to_le_bytes());
        for (i, unit) in descriptor_name(&entry.rel_path).encode_utf16().take(NAME_UNITS).enumerate() {
            d[NAME_OFFSET + i * 2..NAME_OFFSET + i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&d);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn manifest() -> Vec<ManifestEntry> {
        vec![
            ManifestEntry { rel_path: "dir".into(), is_dir: true, size: 0 },
            ManifestEntry { rel_path: "dir/a:b.bin".into(), is_dir: false, size: 10 },
        ]
    }

    #[test]
    fn descriptor_has_names_sizes_and_flags() {
        let d = file_group_descriptor(&manifest());
        assert_eq!(u32::from_le_bytes(d[0..4].try_into().unwrap()), 2);
        assert_eq!(d.len(), 4 + 2 * DESCRIPTOR_LEN);
        let second = &d[4 + DESCRIPTOR_LEN..];
        let flags = u32::from_le_bytes(second[0..4].try_into().unwrap());
        assert_eq!(flags, FD_ATTRIBUTES | FD_FILESIZE | FD_PROGRESSUI);
        assert_eq!(u32::from_le_bytes(second[36..40].try_into().unwrap()), FILE_ATTRIBUTE_NORMAL);
        assert_eq!(u32::from_le_bytes(second[68..72].try_into().unwrap()), 10);
        let units: Vec<u16> = second[NAME_OFFSET..NAME_OFFSET + 520]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), "dir\\a_b.bin");
        let first = &d[4..];
        assert_eq!(u32::from_le_bytes(first[36..40].try_into().unwrap()), FILE_ATTRIBUTE_DIRECTORY);
    }

    #[test]
    fn long_names_are_cut_and_large_sizes_split() {
        let long = "x".repeat(400);
        let entries = vec![ManifestEntry { rel_path: long, is_dir: false, size: (3u64 << 32) | 5 }];
        let d = file_group_descriptor(&entries);
        let desc = &d[4..];
        assert_eq!(u32::from_le_bytes(desc[64..68].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(desc[68..72].try_into().unwrap()), 5);
        let last = &desc[NAME_OFFSET + 518..NAME_OFFSET + 520];
        assert_eq!(last, [0, 0], "the name stays NUL-terminated");
    }

    #[test]
    fn the_first_read_requests_once_and_stale_reads_fail() {
        let files = HostFiles::new();
        files.offer(4, &manifest());
        let calls = std::sync::atomic::AtomicU32::new(0);
        for _ in 0..3 {
            files
                .request_once(4, |_| {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(files.request_once(5, |_| Ok(())).is_err(), "unknown clip");
        // A send failure allows a retry.
        files.offer(6, &manifest());
        assert!(files.request_once(6, |_| Err("closed".into())).is_err());
        assert!(files.request_once(6, |_| Ok(())).is_ok());
    }

    #[test]
    fn readers_block_until_bytes_are_staged() {
        let dir = std::env::temp_dir().join(format!("wb-hostfiles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.bin");
        std::fs::write(&path, b"").unwrap();

        let files = Arc::new(HostFiles::new());
        files.offer(1, &manifest());
        files.entry_begun(1, 0, dir.clone(), 0);
        files.entry_begun(1, 1, path.clone(), 10);

        let feeder = files.clone();
        let feed_path = path.clone();
        let feeding = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            std::fs::write(&feed_path, b"01234").unwrap();
            feeder.entry_data(1, 1, 5);
            std::thread::sleep(Duration::from_millis(50));
            std::fs::write(&feed_path, b"0123456789").unwrap();
            feeder.entry_data(1, 1, 10);
            feeder.finish(1, Ok(()));
        });

        let mut file = None;
        let mut buf = [0u8; 8];
        let started = Instant::now();
        // 8 bytes need both parts: the reader waits for the second one.
        assert_eq!(files.read_blocking(1, 1, 0, &mut buf, &mut file, Duration::from_secs(5)).unwrap(), 8);
        assert_eq!(&buf, b"01234567");
        assert!(started.elapsed() >= Duration::from_millis(90));
        // Then the end of the entry: a short read.
        let mut rest = [0u8; 8];
        assert_eq!(files.read_blocking(1, 1, 8, &mut rest, &mut file, Duration::from_secs(5)).unwrap(), 2);
        assert_eq!(&rest[..2], b"89");
        assert_eq!(files.read_blocking(1, 1, 10, &mut rest, &mut file, Duration::from_secs(5)).unwrap(), 0);
        // Directories have no content.
        assert!(files.wait_bytes(1, 0, 0, 1, Duration::from_millis(10)).is_err());
        feeding.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn readers_give_up_on_stall_failure_and_new_offers() {
        let files = HostFiles::new();
        files.offer(2, &manifest());
        files.entry_begun(2, 1, PathBuf::from("never"), 10);
        let started = Instant::now();
        let err = files.wait_bytes(2, 1, 0, 1, Duration::from_millis(80)).unwrap_err();
        assert!(err.contains("no progress"), "{}", err);
        assert!(started.elapsed() >= Duration::from_millis(80));
        // The failure is sticky.
        assert!(files.wait_bytes(2, 1, 0, 1, Duration::from_secs(5)).is_err());

        files.offer(3, &manifest());
        files.finish(3, Err("disk full".into()));
        assert_eq!(files.wait_bytes(3, 1, 0, 1, Duration::from_secs(5)).unwrap_err(), "disk full");

        let files = Arc::new(HostFiles::new());
        files.offer(4, &manifest());
        files.entry_begun(4, 1, PathBuf::from("never"), 10);
        let other = files.clone();
        let invalidating = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            other.invalidate();
        });
        let err = files.wait_bytes(4, 1, 0, 1, Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("changed"), "{}", err);
        invalidating.join().unwrap();
    }
}
