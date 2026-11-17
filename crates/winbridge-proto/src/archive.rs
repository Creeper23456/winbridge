// SPDX-License-Identifier: GPL-3.0-or-later

//! Folder transfers: a drop or drag that contains a folder travels as one zip.
//!
//! The sender walks the dragged paths ([`pack`]), leaves out whatever the receiving system could
//! not create (names Windows forbids, links, files it cannot read) and reports what it left out.
//! The receiver unpacks the zip below a private staging directory ([`unpack`]) and refuses an
//! archive with anything unsafe in it. The four steps (pack, send, unpack, delete the zip) make up
//! the whole progress bar; [`bar`] maps a step and its progress to the bar.
//!
//! Files are stored, not compressed: the link is local (shared memory or virtio-serial on the same
//! machine), so compressing costs more CPU time than the transfer saves.

use crate::files::MAX_ENTRIES;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// The steps of a folder transfer, in order.
pub const PHASE_PACK: u8 = 1;
pub const PHASE_SEND: u8 = 2;
pub const PHASE_UNPACK: u8 = 3;
pub const PHASE_DELETE: u8 = 4;

/// Share of the bar of each step, in percent (their sum is 100).
const WEIGHT: [u64; 4] = [30, 35, 30, 5];
/// The bar's full scale.
pub const BAR_TOTAL: u64 = 1000;

/// Position on the bar (0 to [`BAR_TOTAL`]) for `done` of `total` in `phase` (1 to 4). Steps
/// outside 1 to 4 count as the last one.
pub fn bar(phase: u8, done: u64, total: u64) -> u64 {
    let index = (phase.clamp(1, 4) - 1) as usize;
    let before: u64 = WEIGHT[..index].iter().sum();
    let inside = if total == 0 { 0 } else { done.min(total) * WEIGHT[index] / total };
    (before + inside) * BAR_TOTAL / 100
}

/// What the step is called on the bar.
pub fn phase_name(phase: u8) -> &'static str {
    match phase {
        PHASE_PACK => "Packing",
        PHASE_SEND => "Sending",
        PHASE_UNPACK => "Unpacking",
        _ => "Cleaning up",
    }
}

/// Which system a path must be valid on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dest {
    Windows,
    Linux,
}

/// Longest relative path the receiver is given, in characters: the staging and the destination
/// folder come on top of it (Windows paths are limited to 260).
const MAX_REL_CHARS: usize = 200;
/// How many left-out paths a report names.
pub const MAX_LISTED: usize = 8;
const CHUNK: usize = 256 * 1024;
/// Files from this size on are written with zip64 fields.
const LARGE_FILE: u64 = 2 * 1024 * 1024 * 1024;

/// What [`pack`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub files: u32,
    pub dirs: u32,
    /// Content bytes in the zip.
    pub bytes: u64,
    /// Entries left out.
    pub skipped: u32,
    /// The first [`MAX_LISTED`] left-out paths with the reason.
    pub listed: Vec<String>,
}

impl Report {
    fn skip(&mut self, what: &str, why: &str) {
        self.skipped += 1;
        if self.listed.len() < MAX_LISTED {
            self.listed.push(format!("{}: {}", what, why));
        }
    }

    /// The notice for the user, `None` when nothing was left out.
    pub fn notice(&self) -> Option<String> {
        notice(self.skipped, &self.listed)
    }
}

/// "Some files were not transferred" with the count and the first few paths.
pub fn notice(skipped: u32, listed: &[String]) -> Option<String> {
    if skipped == 0 {
        return None;
    }
    let mut text = format!("Some files were not transferred ({} left out):", skipped);
    for line in listed {
        text.push_str("\n  ");
        text.push_str(line);
    }
    if skipped as usize > listed.len() {
        text.push_str("\n  ...");
    }
    Some(text)
}

/// Why `name` cannot be one path component on Windows.
fn windows_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("empty name");
    }
    if name.encode_utf16().count() > 255 {
        return Some("name longer than 255 characters");
    }
    if name.chars().any(|c| (c as u32) < 0x20 || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')) {
        return Some("a character Windows does not allow");
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return Some("ends with a space or a dot");
    }
    let stem = name.split('.').next().unwrap_or("").trim_end_matches(' ').to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && matches!(stem.as_bytes()[3], b'0'..=b'9'));
    if reserved {
        return Some("a reserved device name");
    }
    None
}

/// Why `name` cannot be one path component on Linux.
fn linux_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("empty name");
    }
    if name.contains(['/', '\0']) {
        return Some("a character Linux does not allow");
    }
    if name.len() > 255 {
        return Some("name longer than 255 bytes");
    }
    None
}

fn problem(dest: Dest, name: &str) -> Option<&'static str> {
    match dest {
        Dest::Windows => windows_problem(name),
        Dest::Linux => linux_problem(name),
    }
}

struct Item {
    abs: PathBuf,
    rel: String,
    is_dir: bool,
    size: u64,
}

/// True for links and the other reparse points (junctions, mount points, cloud placeholders).
fn is_link(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

struct Scan<'a> {
    dest: Dest,
    items: Vec<Item>,
    total: u64,
    cap_bytes: u64,
    report: &'a mut Report,
}

impl Scan<'_> {
    /// Adds `abs` (named `rel` in the zip) and, for a directory, what is in it.
    fn walk(&mut self, abs: &Path, rel: String) -> Result<(), String> {
        let shown = rel.clone();
        if rel.chars().count() > MAX_REL_CHARS {
            self.report.skip(&shown, "path too long");
            return Ok(());
        }
        let meta = match fs::symlink_metadata(abs) {
            Ok(m) => m,
            Err(e) => {
                self.report.skip(&shown, &e.to_string());
                return Ok(());
            }
        };
        if is_link(&meta) {
            self.report.skip(&shown, "link");
            return Ok(());
        }
        if meta.is_dir() {
            let read = match fs::read_dir(abs) {
                Ok(r) => r,
                Err(e) => {
                    self.report.skip(&shown, &e.to_string());
                    return Ok(());
                }
            };
            self.push(Item { abs: abs.to_path_buf(), rel: rel.clone(), is_dir: true, size: 0 })?;
            let mut children: Vec<_> = read.filter_map(Result::ok).map(|e| e.file_name()).collect();
            children.sort();
            for name in children {
                let Some(text) = name.to_str() else {
                    self.report.skip(&format!("{}/{}", rel, name.to_string_lossy()), "name is not valid Unicode");
                    continue;
                };
                let child = format!("{}/{}", rel, text);
                if let Some(why) = problem(self.dest, text) {
                    self.report.skip(&child, why);
                    continue;
                }
                self.walk(&abs.join(&name), child)?;
            }
        } else if meta.is_file() {
            self.total = self.total.saturating_add(meta.len());
            if self.total > self.cap_bytes {
                return Err(format!("more than {} bytes to transfer", self.cap_bytes));
            }
            self.push(Item { abs: abs.to_path_buf(), rel, is_dir: false, size: meta.len() })?;
        } else {
            self.report.skip(&shown, "not a regular file");
        }
        Ok(())
    }

    fn push(&mut self, item: Item) -> Result<(), String> {
        if self.items.len() >= MAX_ENTRIES {
            return Err(format!("more than {} entries to transfer", MAX_ENTRIES));
        }
        self.items.push(item);
        Ok(())
    }
}

/// Packs `paths` (files and folders, named by their last component) into the zip `zip_path`, for
/// a receiver on `dest`. Whatever `dest` cannot hold is left out and counted in the report;
/// unreadable files too. `progress(done, total)` gets content bytes and returns false to cancel
/// (the zip is removed and the result is the error `"cancelled"`). The zip is removed on any error.
pub fn pack(
    paths: &[PathBuf],
    dest: Dest,
    zip_path: &Path,
    cap_bytes: u64,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Report, String> {
    let mut report = Report::default();
    let items = {
        let mut scan = Scan { dest, items: Vec::new(), total: 0, cap_bytes, report: &mut report };
        for path in paths {
            let shown = path.display().to_string();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                scan.report.skip(&shown, "name is not valid Unicode");
                continue;
            };
            if let Some(why) = problem(dest, name) {
                scan.report.skip(name, why);
                continue;
            }
            scan.walk(path, name.to_string())?;
        }
        scan.items
    };
    let total: u64 = items.iter().map(|i| i.size).sum();
    let result = write_zip(&items, zip_path, total, &mut report, progress);
    if result.is_err() {
        let _ = fs::remove_file(zip_path);
    }
    result?;
    Ok(report)
}

fn write_zip(
    items: &[Item],
    zip_path: &Path,
    total: u64,
    report: &mut Report,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<(), String> {
    if let Some(parent) = zip_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {}", parent.display(), e))?;
    }
    let file = File::create(zip_path).map_err(|e| format!("{}: {}", zip_path.display(), e))?;
    let mut zip = ZipWriter::new(std::io::BufWriter::new(file));
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    if !progress(0, total) {
        return Err("cancelled".to_string());
    }
    for item in items {
        if item.is_dir {
            zip.add_directory(item.rel.as_str(), stored).map_err(|e| format!("{}: {}", item.rel, e))?;
            report.dirs += 1;
            continue;
        }
        // Opened before the entry is started: a file that cannot be read is left out whole.
        let mut input = match File::open(&item.abs) {
            Ok(f) => f,
            Err(e) => {
                report.skip(&item.rel, &e.to_string());
                done += item.size;
                continue;
            }
        };
        zip.start_file(item.rel.as_str(), stored.large_file(item.size >= LARGE_FILE))
            .map_err(|e| format!("{}: {}", item.rel, e))?;
        loop {
            let n = input.read(&mut buf).map_err(|e| format!("{}: {}", item.abs.display(), e))?;
            if n == 0 {
                break;
            }
            zip.write_all(&buf[..n]).map_err(|e| format!("{}: {}", zip_path.display(), e))?;
            done += n as u64;
            report.bytes += n as u64;
            if !progress(done.min(total), total) {
                return Err("cancelled".to_string());
            }
        }
        report.files += 1;
    }
    let mut out = zip.finish().map_err(|e| format!("{}: {}", zip_path.display(), e))?;
    out.flush().map_err(|e| format!("{}: {}", zip_path.display(), e))?;
    Ok(())
}

/// What one entry of a zip is called in the destination, checked for `here`: no absolute path,
/// no `.` or `..`, no component `here` cannot hold.
fn checked_components(name: &str, here: Dest) -> Result<Vec<&str>, String> {
    let bad = |why: &str| Err(format!("{}: {}", name, why));
    if name.starts_with('/') || name.contains('\\') {
        return bad("not a relative path with `/` separators");
    }
    if name.chars().count() > MAX_REL_CHARS {
        return bad("path too long");
    }
    let parts: Vec<&str> = name.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return bad("empty name");
    }
    for part in &parts {
        if *part == "." || *part == ".." {
            return bad("contains `.` or `..`");
        }
        if let Some(why) = problem(here, part) {
            return bad(why);
        }
    }
    Ok(parts)
}

/// Unpacks `zip_path` into the (new or empty) directory `into` on a machine of kind `here` and
/// returns the top-level paths that were created. Refuses the whole archive when any entry is
/// unsafe (path outside `into`, a name `here` cannot hold, a link, a duplicate) or when it
/// exceeds `cap_bytes` or the entry limit; the caller removes `into` on error.
/// `progress(done, total)` gets content bytes and returns false to cancel.
pub fn unpack(
    zip_path: &Path,
    into: &Path,
    here: Dest,
    cap_bytes: u64,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<PathBuf>, String> {
    let file = File::open(zip_path).map_err(|e| format!("{}: {}", zip_path.display(), e))?;
    let mut archive = ZipArchive::new(std::io::BufReader::new(file)).map_err(|e| format!("not a zip: {}", e))?;
    if archive.len() > MAX_ENTRIES {
        return Err(format!("archive with {} entries exceeds the limit", archive.len()));
    }
    // Everything is checked before the first byte is written.
    let mut total = 0u64;
    let mut tops: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        let entry = archive.by_index(i).map_err(|e| format!("entry {}: {}", i, e))?;
        let name = entry.name().map_err(|e| format!("entry {}: {}", i, e))?.into_owned();
        let parts = checked_components(&name, here)?;
        if entry.is_symlink() {
            return Err(format!("{}: a link", name));
        }
        if !tops.iter().any(|t| t == parts[0]) {
            tops.push(parts[0].to_string());
        }
        total = total.saturating_add(entry.size());
        if total > cap_bytes {
            return Err(format!("archive content exceeds {} bytes", cap_bytes));
        }
    }
    fs::create_dir_all(into).map_err(|e| format!("{}: {}", into.display(), e))?;
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    if !progress(0, total) {
        return Err("cancelled".to_string());
    }
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("entry {}: {}", i, e))?;
        let name = entry.name().map_err(|e| format!("entry {}: {}", i, e))?.into_owned();
        let mut path = into.to_path_buf();
        for part in checked_components(&name, here)? {
            path.push(part);
        }
        if entry.is_dir() {
            fs::create_dir_all(&path).map_err(|e| format!("{}: {}", path.display(), e))?;
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {}", parent.display(), e))?;
        }
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("{}: {}", path.display(), e))?;
        loop {
            let n = entry.read(&mut buf).map_err(|e| format!("{}: {}", name, e))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).map_err(|e| format!("{}: {}", path.display(), e))?;
            done += n as u64;
            if !progress(done.min(total), total) {
                return Err("cancelled".to_string());
            }
        }
    }
    Ok(tops.into_iter().map(|t| into.join(t)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!("wb-archive-{}-{}-{}", std::process::id(), tag, N.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn no_progress() -> impl FnMut(u64, u64) -> bool {
        |_, _| true
    }

    #[test]
    fn a_folder_survives_pack_and_unpack() {
        let dir = scratch("roundtrip");
        let src = dir.join("Photos");
        fs::create_dir_all(src.join("2026/trip")).unwrap();
        fs::create_dir_all(src.join("empty")).unwrap();
        let big: Vec<u8> = (0..600_000u32).map(|i| (i * 7 % 251) as u8).collect();
        fs::write(src.join("2026/trip/a.bin"), &big).unwrap();
        fs::write(src.join("note.txt"), b"hello").unwrap();
        fs::write(dir.join("loose.txt"), b"loose").unwrap();
        let zip = dir.join("t.zip");
        let report = pack(&[src.clone(), dir.join("loose.txt")], Dest::Windows, &zip, u64::MAX, &mut no_progress()).unwrap();
        assert_eq!((report.files, report.skipped), (3, 0));
        assert_eq!(report.bytes, big.len() as u64 + 5 + 5);
        let out = dir.join("out");
        let tops = unpack(&zip, &out, Dest::Linux, u64::MAX, &mut no_progress()).unwrap();
        assert_eq!(tops, vec![out.join("Photos"), out.join("loose.txt")]);
        assert_eq!(fs::read(out.join("Photos/2026/trip/a.bin")).unwrap(), big);
        assert_eq!(fs::read(out.join("Photos/note.txt")).unwrap(), b"hello");
        assert!(out.join("Photos/empty").is_dir());
        assert_eq!(fs::read(out.join("loose.txt")).unwrap(), b"loose");
        let _ = fs::remove_dir_all(&dir);
    }

    // The fixture names cannot be created on Windows, which is the point: only a Linux sender
    // meets them.
    #[cfg(unix)]
    #[test]
    fn names_windows_cannot_hold_are_left_out_and_reported() {
        let dir = scratch("illegal");
        let src = dir.join("Mixed");
        fs::create_dir_all(src.join("bad?dir")).unwrap();
        fs::create_dir_all(src.join("fine")).unwrap();
        for name in ["ok.txt", "a:b.txt", "CON.txt", "trail.", "bad?dir/inner.txt", "fine/keep.txt"] {
            fs::write(src.join(name), b"x").unwrap();
        }
        let zip = dir.join("t.zip");
        let report = pack(&[src], Dest::Windows, &zip, u64::MAX, &mut no_progress()).unwrap();
        assert_eq!(report.files, 2, "{:?}", report);
        assert_eq!(report.skipped, 4, "{:?}", report);
        let out = dir.join("out");
        unpack(&zip, &out, Dest::Windows, u64::MAX, &mut no_progress()).unwrap();
        assert!(out.join("Mixed/ok.txt").is_file() && out.join("Mixed/fine/keep.txt").is_file());
        assert!(!out.join("Mixed/bad?dir").exists() && !out.join("Mixed/CON.txt").exists());
        let text = report.notice().unwrap();
        assert!(text.starts_with("Some files were not transferred (4 left out):"), "{}", text);
        assert!(text.contains("Mixed/a:b.txt") && text.contains("Mixed/bad?dir"), "{}", text);
        // The same folder is fine for a Linux receiver.
        let linux = pack(&[dir.join("Mixed")], Dest::Linux, &dir.join("l.zip"), u64::MAX, &mut no_progress()).unwrap();
        assert_eq!(linux.skipped, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn links_and_unreadable_files_are_left_out() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = scratch("links");
        let src = dir.join("D");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.txt"), b"a").unwrap();
        symlink("a.txt", src.join("link")).unwrap();
        fs::write(src.join("secret"), b"s").unwrap();
        fs::set_permissions(src.join("secret"), fs::Permissions::from_mode(0o000)).unwrap();
        let readable = File::open(src.join("secret")).is_ok(); // root reads it anyway
        let report = pack(&[src], Dest::Linux, &dir.join("t.zip"), u64::MAX, &mut no_progress()).unwrap();
        assert_eq!(report.skipped, if readable { 1 } else { 2 }, "{:?}", report);
        assert!(report.listed.iter().any(|l| l.starts_with("D/link")), "{:?}", report);
        let _ = fs::remove_dir_all(&dir);
    }

    fn zip_with(path: &Path, names: &[&str]) {
        let mut zip = ZipWriter::new(File::create(path).unwrap());
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for name in names {
            zip.start_file(*name, stored).unwrap();
            zip.write_all(b"x").unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn an_archive_with_an_unsafe_entry_is_refused_before_anything_is_written() {
        let dir = scratch("unsafe");
        for (n, bad) in ["../evil", "a/../../evil", "/abs", "a\\b", "con.txt", "x:y"].iter().enumerate() {
            let zip = dir.join(format!("{}.zip", n));
            zip_with(&zip, &["fine.txt", bad]);
            let out = dir.join(format!("out{}", n));
            let err = unpack(&zip, &out, Dest::Windows, u64::MAX, &mut no_progress()).unwrap_err();
            assert!(err.contains(bad), "{}: {}", bad, err);
            assert!(!out.exists(), "{} created {:?}", bad, out);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn limits_and_cancel_are_honoured() {
        let dir = scratch("limits");
        let src = dir.join("D");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a"), vec![1u8; 1000]).unwrap();
        let zip = dir.join("t.zip");
        assert!(pack(&[src.clone()], Dest::Linux, &zip, 999, &mut no_progress()).unwrap_err().contains("bytes"));
        assert!(pack(&[src.clone()], Dest::Linux, &zip, u64::MAX, &mut |_, _| false).unwrap_err() == "cancelled");
        assert!(!zip.exists(), "a cancelled pack leaves no zip");
        pack(&[src], Dest::Linux, &zip, u64::MAX, &mut no_progress()).unwrap();
        assert!(unpack(&zip, &dir.join("o1"), Dest::Linux, 999, &mut no_progress()).unwrap_err().contains("bytes"));
        assert!(unpack(&zip, &dir.join("o2"), Dest::Linux, u64::MAX, &mut |_, _| false).unwrap_err() == "cancelled");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_bar_only_moves_forward_and_ends_full() {
        let mut last = 0;
        for phase in 1..=4u8 {
            for step in 0..=10u64 {
                let at = bar(phase, step, 10);
                assert!(at >= last, "phase {} step {}: {} < {}", phase, step, at, last);
                last = at;
            }
        }
        assert_eq!(bar(1, 0, 10), 0);
        assert_eq!(bar(2, 0, 10), 300);
        assert_eq!(bar(4, 7, 7), BAR_TOTAL);
        assert_eq!(bar(3, 0, 0), 650, "an unknown total stays at the start of the step");
    }
}
