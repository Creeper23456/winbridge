// SPDX-License-Identifier: GPL-3.0-or-later

//! The shared-memory backing file (`/dev/shm/winbridge-ctl`, the `memory-backend-file` of QEMU's
//! `ivshmem-plain` device) mapped into the host process.
//!
//! The file is created and sized by the VM launcher; the host only opens an existing file and
//! refuses one that is not what it expects: a symlink (never followed), anything but a regular
//! file, a file owned by another user, a world-writable file, or one whose size differs from the
//! region length.

use std::fs::OpenOptions;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use memmap2::MmapRaw;
use winbridge_shm::Mapping;

/// Default backing file of the control region.
pub const DEFAULT_PATH: &str = "/dev/shm/winbridge-ctl";

/// A read-write shared mapping of the backing file.
#[derive(Debug)]
pub struct FileMapping {
    map: MmapRaw,
}

// SAFETY: the pointer and length of the mapping never change while the value lives, and the
// mapped file is plain shared memory (the ring protocol governs concurrent access).
unsafe impl Mapping for FileMapping {
    fn base(&self) -> *mut u8 {
        self.map.as_mut_ptr()
    }

    fn len(&self) -> usize {
        self.map.len()
    }
}

/// Opens and maps `path`, which must be exactly `expected_len` bytes long.
pub fn open(path: &Path, expected_len: u64) -> Result<FileMapping, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| format!("{}: {}", path.display(), e))?;
    // The checks use the opened descriptor, so the file cannot be swapped after the check.
    let md = file.metadata().map_err(|e| format!("{}: {}", path.display(), e))?;
    if !md.file_type().is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    // SAFETY: `geteuid` has no preconditions.
    if md.uid() != unsafe { libc::geteuid() } {
        return Err(format!("{} belongs to another user", path.display()));
    }
    if md.mode() & 0o002 != 0 {
        return Err(format!("{} is world-writable", path.display()));
    }
    if md.len() != expected_len {
        return Err(format!(
            "{} is {} bytes, expected {}",
            path.display(),
            md.len(),
            expected_len
        ));
    }
    let map = MmapRaw::map_raw(&file).map_err(|e| format!("mapping {}: {}", path.display(), e))?;
    Ok(FileMapping { map })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use winbridge_shm::Mapping;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wb-shmfile-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn file(path: &Path, len: u64, mode: u32) {
        let f = std::fs::File::create(path).unwrap();
        f.set_len(len).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn accepts_a_private_file_of_the_right_size() {
        let d = dir("ok");
        let p = d.join("ctl");
        file(&p, 1 << 20, 0o600);
        let m = open(&p, 1 << 20).unwrap();
        assert_eq!(m.len(), 1 << 20);
        assert_eq!(m.base() as usize % 64, 0);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn refuses_unsafe_files() {
        let d = dir("bad");
        let p = d.join("ctl");
        file(&p, 4096, 0o666);
        assert!(open(&p, 4096).unwrap_err().contains("world-writable"));
        file(&p, 4096, 0o600);
        assert!(open(&p, 8192).unwrap_err().contains("expected 8192"));
        let link = d.join("link");
        symlink(&p, &link).unwrap();
        assert!(open(&link, 4096).is_err(), "a symlink must not be followed");
        assert!(open(&d, 4096).is_err(), "a directory is not a region");
        assert!(open(&d.join("missing"), 4096).is_err());
        std::fs::remove_dir_all(d).unwrap();
    }
}
