// SPDX-License-Identifier: GPL-3.0-or-later

//! Drag-and-drop file and text reception in Windows guest.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
#[allow(unused_imports)]
use tracing::{info, warn};

#[allow(dead_code)]
pub struct ActiveTransfer {
    pub file: File,
    pub path: PathBuf,
    pub received_bytes: u64,
    pub total_bytes: u64,
}

pub struct DndManager {
    transfers: HashMap<u32, ActiveTransfer>,
    target_dir: PathBuf,
}

impl DndManager {
    pub fn new() -> Self {
        let target_dir = get_default_drop_dir();
        if let Err(e) = fs::create_dir_all(&target_dir) {
            warn!("Failed to create drop directory {:?}: {}", target_dir, e);
        } else {
            info!("Drag-and-drop target directory: {:?}", target_dir);
        }

        Self {
            transfers: HashMap::new(),
            target_dir,
        }
    }

    /// Handles initiation of a file drop transfer.
    pub fn start_file(
        &mut self,
        transfer_id: u32,
        filename: &str,
        total_bytes: u64,
    ) -> Result<String, String> {
        let clean_name = sanitize_filename(filename);
        let dest_path = self.target_dir.join(&clean_name);

        info!(
            "Starting drop file transfer id={}, filename={}, total_bytes={}",
            transfer_id, clean_name, total_bytes
        );

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&dest_path)
            .map_err(|e| format!("Failed to open destination file {:?}: {}", dest_path, e))?;

        self.transfers.insert(
            transfer_id,
            ActiveTransfer {
                file,
                path: dest_path.clone(),
                received_bytes: 0,
                total_bytes,
            },
        );

        Ok(dest_path.to_string_lossy().to_string())
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

    /// Finalizes a dropped file transfer.
    pub fn finish_file(
        &mut self,
        transfer_id: u32,
        success: bool,
    ) -> Result<String, String> {
        let transfer = self
            .transfers
            .remove(&transfer_id)
            .ok_or_else(|| format!("Unknown transfer_id {}", transfer_id))?;

        if !success {
            let _ = fs::remove_file(&transfer.path);
            return Err("Transfer cancelled by host".into());
        }

        let saved_path = transfer.path.to_string_lossy().to_string();
        info!(
            "Drop file transfer {} completed: {} bytes saved to {}",
            transfer_id, transfer.received_bytes, saved_path
        );

        Ok(saved_path)
    }

    /// Saves dropped text into a timestamped file and copies to clipboard.
    pub fn handle_drop_text(&self, text: &str) -> Result<String, String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let filename = format!("dropped_text_{}.txt", now);
        let path = self.target_dir.join(filename);

        fs::write(&path, text)
            .map_err(|e| format!("Failed to write dropped text: {}", e))?;

        Ok(path.to_string_lossy().to_string())
    }
}

/// Resolves standard Downloads/WinBridge directory.
fn get_default_drop_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(profile) = std::env::var("USERPROFILE") {
            let path = PathBuf::from(profile).join("Downloads").join("WinBridge");
            return path;
        }
    }

    #[cfg(not(windows))]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join("Downloads").join("WinBridge");
        }
    }

    PathBuf::from("C:\\WinBridgeDrops")
}

/// Strips directory separators and relative elements to prevent path traversal.
fn sanitize_filename(filename: &str) -> String {
    let p = Path::new(filename);
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("dropped_file");
    name.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_")
}
