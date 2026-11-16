// SPDX-License-Identifier: GPL-3.0-or-later

//! Drag-and-drop file and text reception in Windows guest.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use crate::owner;
use std::path::{Path, PathBuf};
use winbridge_proto::archive;
#[allow(unused_imports)]
use tracing::{info, warn};

#[allow(dead_code)]
pub struct ActiveTransfer {
    pub file: File,
    pub path: PathBuf,
    pub received_bytes: u64,
    pub total_bytes: u64,
    operation_id: u32,
    /// The file is the zip of a folder drop.
    archive: bool,
}

struct DropOperation {
    /// Where the files finally go: the Explorer folder or desktop under the cursor, else `target_dir`.
    target_dir: PathBuf,
    /// Private per-operation directory the files are received into before they are moved.
    staging_dir: PathBuf,
    staged_paths: Vec<PathBuf>,
    completed_files: u32,
    file_count: u32,
}

pub struct DndManager {
    transfers: HashMap<u32, ActiveTransfer>,
    operations: HashMap<u32, DropOperation>,
    /// Fallback destination (Downloads\WinBridge) when the drop point is not a folder window or the desktop.
    target_dir: PathBuf,
    staging_root: PathBuf,
}

impl DndManager {
    pub fn new() -> Self {
        let target_dir = get_default_drop_dir();
        let staging_root = get_staging_root();
        // Whatever an interrupted drop left behind.
        let _ = fs::remove_dir_all(&staging_root);
        if let Err(e) = fs::create_dir_all(&target_dir) {
            warn!("Failed to create drop directory {:?}: {}", target_dir, e);
        } else {
            // Created by an earlier agent run, it may still belong to Administrators.
            owner::fix_owner(&target_dir);
            info!("Dropped files are staged in {:?}, then moved to the drop location or {:?}", staging_root, target_dir);
        }

        Self {
            transfers: HashMap::new(),
            operations: HashMap::new(),
            target_dir,
            staging_root,
        }
    }

    /// Resolves the window under the drop point once for the whole file operation.
    pub fn begin_drop(
        &mut self,
        operation_id: u32,
        x: u32,
        y: u32,
        file_count: u32,
    ) -> Result<(), String> {
        if file_count == 0 {
            return Err("Drop contains no files".into());
        }
        if self.operations.contains_key(&operation_id) {
            return Err(format!("Drop operation {} is already active", operation_id));
        }

        let point = (x.min(i32::MAX as u32) as i32, y.min(i32::MAX as u32) as i32);
        let target_dir = resolve_shell_drop_dir(point.0, point.1).unwrap_or_else(|| self.target_dir.clone());
        let staging_dir = self.staging_root.join(operation_id.to_string());
        fs::create_dir_all(&staging_dir)
            .map_err(|e| format!("Failed to create staging directory {:?}: {}", staging_dir, e))?;
        info!("Drop operation {} will end up in {:?}", operation_id, target_dir);
        self.operations.insert(
            operation_id,
            DropOperation {
                target_dir,
                staging_dir,
                staged_paths: Vec::with_capacity(file_count as usize),
                completed_files: 0,
                file_count,
            },
        );
        Ok(())
    }

    /// Handles initiation of a file drop transfer.
    pub fn start_file(
        &mut self,
        transfer_id: u32,
        filename: &str,
        total_bytes: u64,
        operation_id: u32,
    ) -> Result<String, String> {
        if self.transfers.contains_key(&transfer_id) {
            return Err(format!("Transfer {} is already active", transfer_id));
        }
        let operation = self
            .operations
            .get_mut(&operation_id)
            .ok_or_else(|| format!("Unknown drop operation {}", operation_id))?;
        let staging_dir = operation.staging_dir.clone();

        let clean_name = sanitize_filename(filename);
        let (dest_path, file) = create_unique_file(&staging_dir, &clean_name)?;
        info!(
            "Starting drop file transfer id={}, operation={}, filename={}, total_bytes={}, staged at {:?}",
            transfer_id, operation_id, clean_name, total_bytes, dest_path
        );

        self.transfers.insert(
            transfer_id,
            ActiveTransfer {
                file,
                path: dest_path.clone(),
                received_bytes: 0,
                total_bytes,
                operation_id,
                archive: false,
            },
        );

        Ok(dest_path.to_string_lossy().to_string())
    }

    /// Handles the start of a folder drop: the zip of the operation is received into the staging
    /// root (not into the operation's directory, which it is unpacked into).
    pub fn start_archive(&mut self, transfer_id: u32, operation_id: u32, total_bytes: u64) -> Result<(), String> {
        if self.transfers.contains_key(&transfer_id) {
            return Err(format!("Transfer {} is already active", transfer_id));
        }
        if !self.operations.contains_key(&operation_id) {
            return Err(format!("Unknown drop operation {}", operation_id));
        }
        fs::create_dir_all(&self.staging_root)
            .map_err(|e| format!("Failed to create {:?}: {}", self.staging_root, e))?;
        let path = self.staging_root.join(format!("{}.zip", operation_id));
        let file = File::create(&path).map_err(|e| format!("Failed to create {:?}: {}", path, e))?;
        info!(
            "Starting folder drop transfer id={}, operation={}, total_bytes={}, zip at {:?}",
            transfer_id, operation_id, total_bytes, path
        );
        self.transfers.insert(
            transfer_id,
            ActiveTransfer { file, path, received_bytes: 0, total_bytes, operation_id, archive: true },
        );
        Ok(())
    }

    /// Writes a chunk of dropped file data.
    pub fn write_chunk(
        &mut self,
        transfer_id: u32,
        offset: u64,
        data: &[u8],
    ) -> Result<(), String> {
        let transfer = self
            .transfers
            .get_mut(&transfer_id)
            .ok_or_else(|| format!("Unknown transfer_id {}", transfer_id))?;
        let new_bytes = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| "Drop chunk offset overflow".to_string())?;
        if new_bytes > transfer.total_bytes {
            return Err(format!(
                "Drop chunk exceeds declared size for transfer {}",
                transfer_id
            ));
        }

        transfer
            .file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| format!("Seek failed: {}", e))?;
        transfer
            .file
            .write_all(data)
            .map_err(|e| format!("Write failed: {}", e))?;
        transfer.received_bytes += data.len() as u64;
        Ok(())
    }

    /// Finalizes one file. After the last one every staged file is moved to the drop location; a
    /// folder drop (an archive) is handed back as an [`UnpackJob`] for a worker thread instead.
    pub fn finish_file(&mut self, transfer_id: u32, success: bool) -> Result<Finished, String> {
        let transfer = self
            .transfers
            .remove(&transfer_id)
            .ok_or_else(|| format!("Unknown transfer_id {}", transfer_id))?;
        let operation_id = transfer.operation_id;
        let (received_bytes, total_bytes, path) = (transfer.received_bytes, transfer.total_bytes, transfer.path.clone());
        let is_archive = transfer.archive;
        // The staged file must be closed before it is deleted or moved (sharing locks).
        drop(transfer);

        if !success || received_bytes != total_bytes {
            if is_archive {
                let _ = fs::remove_file(&path);
            }
            self.abandon_operation(operation_id);
            return Err(if success {
                format!(
                    "Transfer {} incomplete: received {} of {} bytes; partial files removed",
                    transfer_id, received_bytes, total_bytes
                )
            } else {
                "Transfer cancelled by host; partial files removed".into()
            });
        }

        if is_archive {
            let operation = self
                .operations
                .remove(&operation_id)
                .ok_or_else(|| format!("Unknown drop operation {}", operation_id))?;
            info!("Folder drop {} received: {} bytes of zip at {:?}", operation_id, received_bytes, path);
            return Ok(Finished::Unpack(UnpackJob {
                operation_id,
                zip: path,
                staging_dir: operation.staging_dir,
                target_dir: operation.target_dir,
            }));
        }

        let operation = self
            .operations
            .get_mut(&operation_id)
            .ok_or_else(|| format!("Unknown drop operation {}", operation_id))?;
        operation.staged_paths.push(path.clone());
        operation.completed_files += 1;
        info!(
            "Drop file transfer {} completed: {} bytes staged at {:?}",
            transfer_id, received_bytes, path
        );
        if operation.completed_files < operation.file_count {
            return Ok(Finished::Waiting);
        }

        let operation = self.operations.remove(&operation_id).expect("operation was just looked up");
        let moved = move_into(&operation.staged_paths, &operation.target_dir);
        let _ = fs::remove_dir_all(&operation.staging_dir);
        let moved = moved?;
        let names = moved
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", ");
        info!("Drop operation {} moved to {:?}", operation_id, operation.target_dir);
        Ok(Finished::Saved(format!("Saved to {}", names)))
    }

    /// Forgets an operation and deletes whatever it staged.
    fn abandon_operation(&mut self, operation_id: u32) {
        if let Some(operation) = self.operations.remove(&operation_id) {
            let _ = fs::remove_dir_all(&operation.staging_dir);
        }
    }

    /// Saves dropped text as a .txt file at the drop location (a folder window or the desktop), else in Downloads\WinBridge.
    pub fn handle_drop_text(&self, text: &str, x: u32, y: u32) -> Result<String, String> {
        let dir = resolve_shell_drop_dir(x.min(i32::MAX as u32) as i32, y.min(i32::MAX as u32) as i32)
            .unwrap_or_else(|| self.target_dir.clone());
        fs::create_dir_all(&dir)
            .map_err(|e| format!("Failed to create text drop directory: {}", e))?;
        let filename = format!(
            "dropped_text_{}.txt",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        );
        let (path, mut file) = create_unique_file(&dir, &filename)?;
        file.write_all(text.as_bytes())
            .map_err(|e| format!("Failed to write dropped text: {}", e))?;
        Ok(path.to_string_lossy().to_string())
    }
}

/// What finishing a dropped file leads to.
pub enum Finished {
    /// More files of the operation are still to come.
    Waiting,
    /// The operation is complete; the message says where the files went.
    Saved(String),
    /// A folder drop: the zip is complete and waits to be unpacked on a worker thread.
    Unpack(UnpackJob),
}

/// A received folder drop (a zip) waiting to be unpacked.
pub struct UnpackJob {
    operation_id: u32,
    zip: PathBuf,
    staging_dir: PathBuf,
    target_dir: PathBuf,
}

impl UnpackJob {
    /// The drop operation the zip belongs to.
    pub fn operation_id(&self) -> u32 {
        self.operation_id
    }
}

/// Largest folder drop (content bytes) the guest unpacks.
const DROP_CAP_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Unpacks the zip into the operation's staging directory, deletes it and moves the top-level
/// items to the drop location. `report(phase, done, total)` tells the host's bar about the unpack
/// and delete steps. The zip and the staging directory are gone afterwards, success or not.
pub fn run_unpack(job: UnpackJob, report: &mut dyn FnMut(u8, u64, u64)) -> Result<String, String> {
    let UnpackJob { zip, staging_dir, target_dir, .. } = job;
    let outcome = (|| {
        let tops = archive::unpack(&zip, &staging_dir, archive::Dest::Windows, DROP_CAP_BYTES, &mut |done, total| {
            report(archive::PHASE_UNPACK, done, total);
            true
        })?;
        report(archive::PHASE_DELETE, 0, 1);
        fs::remove_file(&zip).map_err(|e| format!("{}: {}", zip.display(), e))?;
        report(archive::PHASE_DELETE, 1, 1);
        move_into(&tops, &target_dir)
    })();
    let _ = fs::remove_file(&zip);
    let _ = fs::remove_dir_all(&staging_dir);
    let moved = outcome?;
    let names = moved.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join(", ");
    info!("Folder drop moved to {:?}", target_dir);
    Ok(format!("Saved to {}", names))
}

/// Moves staged files and folders into `dir` without overwriting anything: a file's destination
/// name is claimed first, then the staged file replaces the placeholder (copy and delete across
/// volumes); a folder takes the first free name (see [`move_dir`]).
fn move_into(paths: &[PathBuf], dir: &Path) -> Result<Vec<PathBuf>, String> {
    fs::create_dir_all(dir).map_err(|e| format!("Failed to create {:?}: {}", dir, e))?;
    let mut moved = Vec::with_capacity(paths.len());
    for path in paths {
        if path.is_dir() {
            moved.push(move_dir(path, dir)?);
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("dropped_file");
        let (dest, placeholder) = create_unique_file(dir, name)?;
        drop(placeholder);
        if fs::rename(path, &dest).is_err() {
            fs::copy(path, &dest).map_err(|e| {
                let _ = fs::remove_file(&dest);
                format!("Failed to move {:?} to {:?}: {}", path, dest, e)
            })?;
            let _ = fs::remove_file(path);
        }
        // A rename keeps the staged file's DACL and owner; the destination folder's inheritable
        // entries and the user as owner are what the user expects.
        if let Err(e) = owner::claim(&dest) {
            warn!("Could not apply the folder's permissions to {:?}: {}", dest, e);
        }
        moved.push(dest);
    }
    Ok(moved)
}

/// Moves the staged directory `src` into `dir` under a name not taken there (`name`, `name (1)`,
/// ...): a rename, else a copy of the tree and a delete (another volume). The user becomes the
/// owner of everything in it.
fn move_dir(src: &Path, dir: &Path) -> Result<PathBuf, String> {
    let name = src.file_name().and_then(|n| n.to_str()).unwrap_or("dropped_folder");
    for index in 0..10_000u32 {
        let candidate = if index == 0 { dir.join(name) } else { dir.join(format!("{} ({})", name, index)) };
        if candidate.exists() {
            continue;
        }
        match fs::rename(src, &candidate) {
            Ok(()) => {}
            Err(_) if candidate.exists() => continue,
            Err(_) => {
                copy_tree(src, &candidate).map_err(|e| {
                    let _ = fs::remove_dir_all(&candidate);
                    format!("Failed to move {:?} to {:?}: {}", src, candidate, e)
                })?;
                let _ = fs::remove_dir_all(src);
            }
        }
        claim_tree(&candidate);
        return Ok(candidate);
    }
    Err(format!("No free name for {:?} in {:?}", name, dir))
}

fn copy_tree(src: &Path, dest: &Path) -> std::io::Result<()> {
    fs::create_dir(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Applies the destination's inheritable permissions and the user as owner to `path` and
/// everything below it (a rename keeps what the staged items had).
fn claim_tree(path: &Path) {
    if let Err(e) = owner::claim(path) {
        warn!("Could not apply the folder's permissions to {:?}: {}", path, e);
    }
    if let Ok(read) = fs::read_dir(path) {
        for entry in read.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                claim_tree(&entry.path());
            } else if let Err(e) = owner::claim(&entry.path()) {
                warn!("Could not apply the folder's permissions to {:?}: {}", entry.path(), e);
            }
        }
    }
}

/// Private receive area, outside every user-visible folder.
fn get_staging_root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("WinBridge")
        .join("incoming")
}

fn create_unique_file(dir: &Path, filename: &str) -> Result<(PathBuf, File), String> {
    let clean_name = sanitize_filename(filename);
    let source = Path::new(&clean_name);
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("dropped_file");
    let extension = source.extension().and_then(|s| s.to_str());

    for index in 0..10_000u32 {
        let candidate_name = if index == 0 {
            clean_name.clone()
        } else {
            match extension {
                Some(ext) => format!("{} ({}).{}", stem, index, ext),
                None => format!("{} ({})", stem, index),
            }
        };
        let path = dir.join(candidate_name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Failed to create destination file {:?}: {}", path, e)),
        }
    }
    Err(format!("Could not find an unused filename in {:?}", dir))
}

/// Resolves the persistent Downloads/WinBridge application staging directory.
fn get_default_drop_dir() -> PathBuf {
    #[cfg(windows)]
    if let Some(path) = known_folder(&windows::Win32::UI::Shell::FOLDERID_Downloads) {
        return path.join("WinBridge");
    }

    #[cfg(not(windows))]
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join("Downloads").join("WinBridge");
    }

    PathBuf::from("WinBridgeDrops")
}

/// Strips directory separators and relative elements to prevent path traversal.
fn sanitize_filename(filename: &str) -> String {
    let basename = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("dropped_file");
    let name = basename.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");
    let trimmed = name.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() || trimmed.chars().any(|c| c.is_control()) {
        "dropped_file".into()
    } else {
        trimmed.into()
    }
}

#[cfg(windows)]
fn known_folder(folder: &windows::core::GUID) -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};

    unsafe {
        let value = SHGetKnownFolderPath(folder, KNOWN_FOLDER_FLAG(0), None).ok()?;
        if value.is_null() {
            return None;
        }
        let mut len = 0;
        while *value.0.add(len) != 0 {
            len += 1;
        }
        let path = PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(value.0, len)));
        CoTaskMemFree(Some(value.0.cast()));
        Some(path)
    }
}

#[cfg(not(windows))]
fn resolve_shell_drop_dir(_x: i32, _y: i32) -> Option<PathBuf> {
    None
}

#[cfg(windows)]
fn resolve_shell_drop_dir(x: i32, y: i32) -> Option<PathBuf> {
    windows_drop::resolve_shell_drop_dir(x, y)
}

#[cfg(windows)]
mod windows_drop {
    use super::{known_folder, PathBuf};
    use std::ffi::c_void;
    use std::sync::{Arc, Condvar, Mutex};
    use tracing::{info, warn};
    use windows::core::{Interface, IUnknown, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::Variant::{VARIANT, VT_I4};
    use windows::Win32::UI::Shell::{
        IShellWindows, IWebBrowser2, PathCreateFromUrlW, ShellWindows, FOLDERID_Desktop,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetClassNameW, WindowFromPoint, GA_ROOT};

    pub fn resolve_shell_drop_dir(x: i32, y: i32) -> Option<PathBuf> {
        unsafe {
            let child = WindowFromPoint(POINT { x, y });
            if child.0.is_null() {
                return None;
            }
            let root = GetAncestor(child, GA_ROOT);
            let class = window_class(root);
            info!("Drop hit test: root={:?}, class={}", root, class);

            if matches!(class.as_str(), "Progman" | "WorkerW" | "SHELLDLL_DefView") {
                return known_folder(&FOLDERID_Desktop);
            }
            if class != "CabinetWClass" {
                return None;
            }

            // IShellWindows is apartment-bound. A short-lived STA avoids changing the reader
            // thread's COM model and guarantees uninitialization on every return path.
            let root_handle = root.0 as usize;
            let pair = Arc::new((Mutex::new(None), Condvar::new()));
            let worker_pair = pair.clone();
            std::thread::spawn(move || {
                let result = resolve_explorer_folder(HWND(root_handle as *mut c_void));
                let (lock, ready) = &*worker_pair;
                *lock.lock().unwrap() = Some(result);
                ready.notify_one();
            });
            let (lock, ready) = &*pair;
            let mut result = lock.lock().unwrap();
            while result.is_none() {
                result = ready.wait(result).unwrap();
            }
            result.take().flatten()
        }
    }

    unsafe fn resolve_explorer_folder(root: HWND) -> Option<PathBuf> {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.is_err() {
            warn!("COM initialization for Explorer lookup failed: {:?}", hr);
            return None;
        }
        let result = resolve_explorer_folder_inner(root);
        CoUninitialize();
        result
    }

    unsafe fn resolve_explorer_folder_inner(root: HWND) -> Option<PathBuf> {
        let windows: IShellWindows = match CoCreateInstance(&ShellWindows, None::<&IUnknown>, CLSCTX_LOCAL_SERVER) {
            Ok(windows) => windows,
            Err(e) => {
                warn!("IShellWindows creation failed: {}", e);
                return None;
            }
        };
        let count = windows.Count().ok()?;
        for index in 0..count {
            let mut variant = VARIANT::default();
            (*variant.Anonymous.Anonymous).vt = VT_I4;
            (*variant.Anonymous.Anonymous).Anonymous.lVal = index;
            let dispatch = match windows.Item(&variant) {
                Ok(dispatch) => dispatch,
                Err(_) => continue,
            };
            let browser: IWebBrowser2 = match dispatch.cast() {
                Ok(browser) => browser,
                Err(_) => continue,
            };
            if browser.HWND().map(|hwnd| hwnd.0 as *mut c_void) != Ok(root.0) {
                continue;
            }
            let url = browser.LocationURL().ok()?;
            let mut path = vec![0u16; 32_768];
            let mut len = path.len() as u32;
            PathCreateFromUrlW(PCWSTR(url.as_ptr()), PWSTR(path.as_mut_ptr()), &mut len, 0).ok()?;
            // The buffer is NUL-terminated and `len` counts differently across Windows versions.
            let end = path.iter().position(|&c| c == 0).unwrap_or(path.len());
            let path = PathBuf::from(String::from_utf16(&path[..end]).ok()?);
            if path.is_absolute() && path.is_dir() {
                return Some(path);
            }
            warn!("Explorer location is not a filesystem directory: {}", url.to_string());
            return None;
        }
        warn!("No IShellWindows entry matched Explorer window {:?}", root);
        None
    }

    fn window_class(hwnd: HWND) -> String {
        let mut buffer = [0u16; 256];
        unsafe {
            let len = GetClassNameW(hwnd, &mut buffer);
            if len <= 0 {
                String::new()
            } else {
                String::from_utf16_lossy(&buffer[..len as usize])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("winbridge-dnd-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn manager(root: &Path) -> DndManager {
        let target_dir = root.join("drop");
        fs::create_dir_all(&target_dir).unwrap();
        DndManager {
            transfers: HashMap::new(),
            operations: HashMap::new(),
            target_dir,
            staging_root: root.join("stage"),
        }
    }

    #[test]
    fn a_dropped_folder_arrives_whole_under_the_first_free_name() {
        let root = scratch("folder");
        let photos = root.join("src").join("photos");
        fs::create_dir_all(photos.join("sub")).unwrap();
        fs::write(photos.join("a.txt"), b"alpha").unwrap();
        fs::write(photos.join("sub").join("b.txt"), b"beta").unwrap();
        let zip = root.join("sent.zip");
        archive::pack(&[photos], archive::Dest::Windows, &zip, u64::MAX, &mut |_, _| true).unwrap();
        let bytes = fs::read(&zip).unwrap();

        let mut dnd = manager(&root);
        fs::create_dir_all(root.join("drop").join("photos")).unwrap();
        dnd.begin_drop(7, 0, 0, 1).unwrap();
        dnd.start_archive(1, 7, bytes.len() as u64).unwrap();
        let half = bytes.len() / 2;
        dnd.write_chunk(1, 0, &bytes[..half]).unwrap();
        dnd.write_chunk(1, half as u64, &bytes[half..]).unwrap();
        let job = match dnd.finish_file(1, true).unwrap() {
            Finished::Unpack(job) => job,
            _ => panic!("a folder drop must be handed back for unpacking"),
        };
        assert_eq!(job.operation_id(), 7);

        let mut phases = Vec::new();
        let message = run_unpack(job, &mut |phase, _, _| phases.push(phase)).unwrap();
        let moved = root.join("drop").join("photos (1)");
        assert_eq!(fs::read(moved.join("a.txt")).unwrap(), b"alpha");
        assert_eq!(fs::read(moved.join("sub").join("b.txt")).unwrap(), b"beta");
        assert!(message.contains("photos (1)"), "{}", message);
        assert!(phases.contains(&archive::PHASE_UNPACK) && phases.contains(&archive::PHASE_DELETE));
        assert!(!root.join("stage").join("7.zip").exists());
        assert!(!root.join("stage").join("7").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_drop_cut_short_leaves_nothing_behind() {
        let root = scratch("short");
        let mut dnd = manager(&root);
        dnd.begin_drop(3, 0, 0, 1).unwrap();
        dnd.start_archive(1, 3, 100).unwrap();
        dnd.write_chunk(1, 0, &[0u8; 40]).unwrap();
        assert!(dnd.finish_file(1, true).is_err());
        assert!(!root.join("stage").join("3.zip").exists());
        assert_eq!(fs::read_dir(root.join("drop")).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&root);
    }
}
