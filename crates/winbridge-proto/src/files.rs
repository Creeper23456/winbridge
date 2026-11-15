// SPDX-License-Identifier: GPL-3.0-or-later

//! File streams: the same walk, chunking and safe extraction on both ends of the channel.
//!
//! A sender scans local paths into [`FileEntryInfo`] values and sends them with
//! [`send_stream`]; a receiver feeds the incoming `FilesBegin`/`FileEntry`/`FileChunk`/`FilesEnd`
//! messages into a [`Receiver`], which writes below one staging directory and never outside it.

use crate::Message;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Size of one `FileChunk` payload.
pub const CHUNK_SIZE: usize = 256 * 1024;
/// Upper bound on entries in one stream.
pub const MAX_ENTRIES: usize = 100_000;

/// One file or directory to send.
#[derive(Debug, Clone)]
pub struct FileEntryInfo {
    pub abs: PathBuf,
    /// `/`-separated, starting with the unique top-level name.
    pub rel: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Walks `paths` (files or directories, symlinks skipped). Fails when the total size exceeds
/// `cap_bytes` or there are too many entries.
pub fn scan(paths: &[PathBuf], cap_bytes: u64) -> Result<Vec<FileEntryInfo>, String> {
    let mut entries = Vec::new();
    let mut total = 0u64;
    let mut tops: HashMap<String, u32> = HashMap::new();
    for path in paths {
        let meta = fs::symlink_metadata(path).map_err(|e| format!("{}: {}", path.display(), e))?;
        if meta.file_type().is_symlink() {
            continue;
        }
        let base = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "item".to_string());
        let top = unique_name(&mut tops, &base);
        walk(path, &top, &mut entries, &mut total, cap_bytes)?;
    }
    if entries.is_empty() {
        return Err("nothing to transfer".to_string());
    }
    Ok(entries)
}

fn walk(
    path: &Path,
    rel: &str,
    entries: &mut Vec<FileEntryInfo>,
    total: &mut u64,
    cap_bytes: u64,
) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if entries.len() >= MAX_ENTRIES {
        return Err(format!("more than {} entries", MAX_ENTRIES));
    }
    if meta.is_dir() {
        entries.push(FileEntryInfo { abs: path.to_path_buf(), rel: rel.to_string(), is_dir: true, size: 0 });
        let mut children: Vec<_> = fs::read_dir(path)
            .map_err(|e| format!("{}: {}", path.display(), e))?
            .filter_map(Result::ok)
            .collect();
        children.sort_by_key(|c| c.file_name());
        for child in children {
            let name = child.file_name().to_string_lossy().into_owned();
            walk(&child.path(), &format!("{}/{}", rel, name), entries, total, cap_bytes)?;
        }
    } else if meta.is_file() {
        *total = total.saturating_add(meta.len());
        if *total > cap_bytes {
            return Err(format!("larger than the {} MiB limit", cap_bytes / (1024 * 1024)));
        }
        entries.push(FileEntryInfo { abs: path.to_path_buf(), rel: rel.to_string(), is_dir: false, size: meta.len() });
    }
    Ok(())
}

/// Total file bytes of `entries`.
pub fn total_bytes(entries: &[FileEntryInfo]) -> u64 {
    entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum()
}

/// Top-level names of `entries`, in order.
pub fn top_names(entries: &[FileEntryInfo]) -> Vec<String> {
    entries
        .iter()
        .filter(|e| !e.rel.contains('/'))
        .map(|e| e.rel.clone())
        .collect()
}

/// Sends `entries` as one stream through `send`. On a read error the stream is closed with
/// `FilesEnd { success: false }` and the error is returned.
pub fn send_stream<F>(stream_id: u32, entries: &[FileEntryInfo], send: &mut F) -> Result<(), String>
where
    F: FnMut(Message) -> Result<(), String>,
{
    send(Message::FilesBegin {
        stream_id,
        entries: entries.len() as u32,
        total_bytes: total_bytes(entries),
    })?;
    let mut buf = vec![0u8; CHUNK_SIZE];
    for (index, entry) in entries.iter().enumerate() {
        let index = index as u32;
        send(Message::FileEntry {
            stream_id,
            index,
            rel_path: entry.rel.clone(),
            is_dir: entry.is_dir,
            size: entry.size,
        })?;
        if entry.is_dir {
            continue;
        }
        if let Err(e) = send_file(stream_id, index, entry, &mut buf, send) {
            let _ = send(Message::FilesEnd { stream_id, success: false, error: Some(e.clone()) });
            return Err(e);
        }
    }
    send(Message::FilesEnd { stream_id, success: true, error: None })
}

fn send_file<F>(
    stream_id: u32,
    index: u32,
    entry: &FileEntryInfo,
    buf: &mut [u8],
    send: &mut F,
) -> Result<(), String>
where
    F: FnMut(Message) -> Result<(), String>,
{
    let mut file = File::open(&entry.abs).map_err(|e| format!("{}: {}", entry.abs.display(), e))?;
    let mut offset = 0u64;
    while offset < entry.size {
        let want = ((entry.size - offset) as usize).min(buf.len());
        let n = file
            .read(&mut buf[..want])
            .map_err(|e| format!("{}: {}", entry.abs.display(), e))?;
        if n == 0 {
            return Err(format!("{} shrank while it was read", entry.abs.display()));
        }
        send(Message::FileChunk { stream_id, index, offset, data: buf[..n].to_vec() })?;
        offset += n as u64;
    }
    Ok(())
}

/// Picks `base`, or `base (n)` when taken, and records the choice in `taken`.
fn unique_name(taken: &mut HashMap<String, u32>, base: &str) -> String {
    let count = taken.entry(base.to_string()).or_insert(0);
    *count += 1;
    if *count == 1 {
        return base.to_string();
    }
    let n = *count - 1;
    let candidate = match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{} ({}).{}", stem, n, ext),
        _ => format!("{} ({})", base, n),
    };
    // The numbered name could itself collide with a real entry; record it too.
    taken.entry(candidate.clone()).or_insert(1);
    candidate
}

/// The path components of `rel_path` as the receiver would create them (separators, `.` and `..`
/// dropped, characters illegal on Windows replaced). Used to present offered files by name.
pub fn safe_components(rel_path: &str) -> Vec<String> {
    rel_path
        .split('/')
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .map(sanitize_component)
        .collect()
}

/// Makes one path component safe for both Linux and Windows file systems.
fn sanitize_component(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "_".to_string()
    } else {
        trimmed.to_string()
    }
}

struct Current {
    index: u32,
    file: File,
    size: u64,
    written: u64,
}

/// Writes one incoming file stream below `root`.
pub struct Receiver {
    root: PathBuf,
    expected_entries: u32,
    seen_entries: u32,
    current: Option<Current>,
    top_names: HashMap<String, String>,
    taken: HashMap<String, u32>,
    tops: Vec<PathBuf>,
    entry_paths: Vec<PathBuf>,
}

impl Receiver {
    /// Creates `root` and prepares for `entries` entries. Rejects streams above `cap_bytes`.
    pub fn new(root: PathBuf, entries: u32, total_bytes: u64, cap_bytes: u64) -> Result<Self, String> {
        if total_bytes > cap_bytes {
            return Err(format!("stream of {} bytes exceeds the limit", total_bytes));
        }
        if entries as usize > MAX_ENTRIES {
            return Err(format!("stream with {} entries exceeds the limit", entries));
        }
        fs::create_dir_all(&root).map_err(|e| format!("{}: {}", root.display(), e))?;
        Ok(Self {
            root,
            expected_entries: entries,
            seen_entries: 0,
            current: None,
            top_names: HashMap::new(),
            taken: HashMap::new(),
            tops: Vec::new(),
            entry_paths: Vec::new(),
        })
    }

    /// The directory this stream extracts into.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where entry `index` was created (known once its `FileEntry` arrived).
    pub fn entry_path(&self, index: u32) -> Option<&Path> {
        self.entry_paths.get(index as usize).map(PathBuf::as_path)
    }

    fn close_current(&mut self) -> Result<(), String> {
        if let Some(cur) = self.current.take() {
            if cur.written != cur.size {
                return Err(format!(
                    "entry {} incomplete: {} of {} bytes",
                    cur.index, cur.written, cur.size
                ));
            }
        }
        Ok(())
    }

    /// Handles `FileEntry`.
    pub fn entry(&mut self, index: u32, rel_path: &str, is_dir: bool, size: u64) -> Result<(), String> {
        self.close_current()?;
        if index != self.seen_entries || self.seen_entries >= self.expected_entries {
            return Err(format!("unexpected entry index {}", index));
        }
        self.seen_entries += 1;

        let mut parts = rel_path.split('/').filter(|p| !p.is_empty() && *p != "." && *p != "..");
        let Some(top_original) = parts.next() else {
            return Err("entry without a name".to_string());
        };
        let top = match self.top_names.get(top_original) {
            Some(assigned) => assigned.clone(),
            None => {
                let assigned = unique_name(&mut self.taken, &sanitize_component(top_original));
                self.top_names.insert(top_original.to_string(), assigned.clone());
                self.tops.push(self.root.join(&assigned));
                assigned
            }
        };
        let mut path = self.root.join(top);
        for part in parts {
            path.push(sanitize_component(part));
        }
        self.entry_paths.push(path.clone());
        if is_dir {
            fs::create_dir_all(&path).map_err(|e| format!("{}: {}", path.display(), e))?;
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("{}: {}", parent.display(), e))?;
            }
            let file = File::create(&path).map_err(|e| format!("{}: {}", path.display(), e))?;
            self.current = Some(Current { index, file, size, written: 0 });
        }
        Ok(())
    }

    /// Handles `FileChunk`.
    pub fn chunk(&mut self, index: u32, offset: u64, data: &[u8]) -> Result<(), String> {
        let cur = self
            .current
            .as_mut()
            .filter(|c| c.index == index)
            .ok_or_else(|| format!("chunk for entry {} without a matching entry", index))?;
        if offset != cur.written || cur.written + data.len() as u64 > cur.size {
            return Err(format!("chunk for entry {} out of order or too large", index));
        }
        cur.file.write_all(data).map_err(|e| e.to_string())?;
        cur.written += data.len() as u64;
        Ok(())
    }

    /// Handles `FilesEnd`. Returns the top-level paths that were created.
    pub fn finish(mut self, success: bool, error: Option<String>) -> Result<Vec<PathBuf>, String> {
        if !success {
            return Err(error.unwrap_or_else(|| "sender reported a failure".to_string()));
        }
        self.close_current()?;
        if self.seen_entries != self.expected_entries {
            return Err(format!("{} of {} entries arrived", self.seen_entries, self.expected_entries));
        }
        Ok(self.tops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("winbridge-files-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn stream_round_trip_keeps_tree_and_content() {
        let src = temp_dir("src");
        fs::create_dir_all(src.join("album/sub")).unwrap();
        fs::write(src.join("album/a.bin"), vec![7u8; CHUNK_SIZE + 5]).unwrap();
        fs::write(src.join("album/sub/b.txt"), b"hello").unwrap();
        fs::write(src.join("single.txt"), b"one").unwrap();

        let entries = scan(&[src.join("album"), src.join("single.txt")], 1 << 30).unwrap();
        assert_eq!(top_names(&entries), vec!["album", "single.txt"]);

        let dst = temp_dir("dst");
        let mut receiver: Option<Receiver> = None;
        let mut result = None;
        send_stream(5, &entries, &mut |msg| {
            match msg {
                Message::FilesBegin { entries, total_bytes, .. } => {
                    receiver = Some(Receiver::new(dst.clone(), entries, total_bytes, 1 << 30)?);
                }
                Message::FileEntry { index, rel_path, is_dir, size, .. } => {
                    receiver.as_mut().unwrap().entry(index, &rel_path, is_dir, size)?;
                }
                Message::FileChunk { index, offset, data, .. } => {
                    receiver.as_mut().unwrap().chunk(index, offset, &data)?;
                }
                Message::FilesEnd { success, error, .. } => {
                    result = Some(receiver.take().unwrap().finish(success, error)?);
                }
                _ => unreachable!(),
            }
            Ok(())
        })
        .unwrap();

        let tops = result.unwrap();
        assert_eq!(tops, vec![dst.join("album"), dst.join("single.txt")]);
        assert_eq!(fs::read(dst.join("album/a.bin")).unwrap().len(), CHUNK_SIZE + 5);
        assert_eq!(fs::read(dst.join("album/sub/b.txt")).unwrap(), b"hello");
    }

    #[test]
    fn hostile_paths_stay_inside_the_staging_directory() {
        let dst = temp_dir("evil");
        let mut receiver = Receiver::new(dst.clone(), 2, 4, 1 << 20).unwrap();
        receiver.entry(0, "../../escape.txt", false, 2).unwrap();
        receiver.chunk(0, 0, b"hi").unwrap();
        receiver.entry(1, "a:b/c*.txt", false, 2).unwrap();
        receiver.chunk(1, 0, b"yo").unwrap();
        let tops = receiver.finish(true, None).unwrap();
        for top in tops {
            assert!(top.starts_with(&dst), "{:?} escaped {:?}", top, dst);
        }
        assert!(dst.join("escape.txt").exists());
        assert!(dst.join("a_b/c_.txt").exists());
    }

    #[test]
    fn duplicate_top_level_names_are_numbered() {
        let src = temp_dir("dup");
        fs::create_dir_all(src.join("x")).unwrap();
        fs::create_dir_all(src.join("y")).unwrap();
        fs::write(src.join("x/f.txt"), b"1").unwrap();
        fs::write(src.join("y/f.txt"), b"2").unwrap();
        let entries = scan(&[src.join("x/f.txt"), src.join("y/f.txt")], 1 << 20).unwrap();
        assert_eq!(top_names(&entries), vec!["f.txt", "f (1).txt"]);
    }

    #[test]
    fn oversized_selection_is_refused() {
        let src = temp_dir("big");
        fs::write(src.join("big.bin"), vec![0u8; 2048]).unwrap();
        assert!(scan(&[src.join("big.bin")], 1024).is_err());
    }
}
