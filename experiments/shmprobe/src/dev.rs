// SPDX-License-Identifier: GPL-3.0-or-later

//! The shared memory as seen by one process: a [`Map`] implementing `winbridge_shm::Mapping`,
//! obtained from the ivshmem driver (Windows) or from a plain file (Unix: host role and the
//! loopback self-test of the guest role).

use std::any::Any;
use std::sync::Arc;

use winbridge_shm::Mapping;

/// BAR cache mode requested from the driver (`IVSHMEM_CACHE_*`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Noncached = 0,
    Cached = 1,
    WriteCombined = 2,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Noncached, Mode::Cached, Mode::WriteCombined];

    pub fn name(self) -> &'static str {
        match self {
            Mode::Noncached => "noncached",
            Mode::Cached => "cached",
            Mode::WriteCombined => "writecombined",
        }
    }

    pub fn from_u8(v: u8) -> Mode {
        match v {
            0 => Mode::Noncached,
            1 => Mode::Cached,
            _ => Mode::WriteCombined,
        }
    }

    /// `0,1,2` or names, comma separated; default all three.
    pub fn parse_list(s: Option<String>) -> Vec<Mode> {
        let Some(s) = s else { return Mode::ALL.to_vec() };
        s.split(',')
            .map(|t| match t.trim() {
                "0" | "noncached" | "uc" => Mode::Noncached,
                "1" | "cached" | "wb" => Mode::Cached,
                "2" | "writecombined" | "wc" => Mode::WriteCombined,
                other => {
                    eprintln!("unknown cache mode {other}");
                    std::process::exit(2)
                }
            })
            .collect()
    }
}

/// A live mapping of one shared-memory device. Dropping it releases the mapping.
pub struct Map {
    base: *mut u8,
    len: usize,
    _owner: Box<dyn Any + Send + Sync>,
}

impl Map {
    pub(crate) fn new(base: *mut u8, len: usize, owner: Box<dyn Any + Send + Sync>) -> Self {
        Self { base, len, _owner: owner }
    }
}

// SAFETY: the pointer refers to device/file memory that lives as long as `_owner`.
unsafe impl Send for Map {}
unsafe impl Sync for Map {}

// SAFETY: base/len are fixed for the lifetime of the value (the owner keeps the mapping alive).
unsafe impl Mapping for Map {
    fn base(&self) -> *mut u8 {
        self.base
    }
    fn len(&self) -> usize {
        self.len
    }
}

/// Cloneable handle on a [`Map`]: `Region::create` consumes its mapping, the tests re-create
/// regions on the same memory.
#[derive(Clone)]
pub struct Shared(pub Arc<Map>);

// SAFETY: forwards to the inner mapping, which stays alive while any clone exists.
unsafe impl Mapping for Shared {
    fn base(&self) -> *mut u8 {
        self.0.base
    }
    fn len(&self) -> usize {
        self.0.len
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::fs::OpenOptions;
    use std::time::Duration;

    pub struct Dev {
        pub path: String,
    }

    pub fn list(files: &[String]) -> Result<Vec<Dev>, String> {
        if files.is_empty() {
            return Err("no --file given; the ivshmem driver exists only on Windows (use --file for the loopback self-test)".into());
        }
        Ok(files.iter().map(|p| Dev { path: p.clone() }).collect())
    }

    impl Dev {
        pub fn label(&self) -> String {
            self.path.clone()
        }
        pub fn describe(&self) -> String {
            format!("file {}", self.path)
        }
        pub fn map(&self, _mode: Mode) -> Result<Map, String> {
            map_file(&self.path)
        }
    }

    pub fn map_file(path: &str) -> Result<Map, String> {
        let f = OpenOptions::new().read(true).write(true).open(path).map_err(|e| format!("open {path}: {e}"))?;
        // SAFETY: the file is shared with the peer on purpose; all access goes through raw pointers.
        let mut m = unsafe { memmap2::MmapMut::map_mut(&f) }.map_err(|e| format!("mmap {path}: {e}"))?;
        let (base, len) = (m.as_mut_ptr(), m.len());
        Ok(Map::new(base, len, Box::new(m)))
    }

    pub fn cpu_time() -> Duration {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: valid out pointer.
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
        Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
    }
}

#[cfg(windows)]
mod imp {
    pub use crate::winimp::*;
}

pub use imp::*;
