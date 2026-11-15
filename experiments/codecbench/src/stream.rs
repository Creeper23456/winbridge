// SPDX-License-Identifier: GPL-3.0-or-later
//! Encoded stream container.
//!
//! File: `CBSTRM01`, `count: u32`, then `count` index entries `{off: u64, len: u32, pad: u32, hash: u64}`
//! (`hash` = hash of the expected 1080p BGRA output after decoding that frame), then the payloads.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"CBSTRM01";
const ENTRY: usize = 24;

pub struct Stream {
    data: Vec<u8>,
    index: Vec<(usize, usize, u64)>,
}

impl Stream {
    pub fn load(path: &Path) -> io::Result<Stream> {
        let data = fs::read(path)?;
        if data.len() < 12 || &data[..8] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad stream magic"));
        }
        let count = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
        let mut index = Vec::with_capacity(count);
        for i in 0..count {
            let e = &data[12 + i * ENTRY..12 + (i + 1) * ENTRY];
            let off = u64::from_le_bytes(e[0..8].try_into().unwrap()) as usize;
            let len = u32::from_le_bytes(e[8..12].try_into().unwrap()) as usize;
            let hash = u64::from_le_bytes(e[16..24].try_into().unwrap());
            index.push((off, len, hash));
        }
        Ok(Stream { data, index })
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn frame(&self, i: usize) -> &[u8] {
        let (off, len, _) = self.index[i];
        &self.data[off..off + len]
    }

    pub fn hash(&self, i: usize) -> u64 {
        self.index[i].2
    }
}

pub fn stream_path(dir: &Path, class: &str, codec: &str) -> PathBuf {
    dir.join(format!("{class}.{codec}.cbs"))
}

/// Appends frames in order; the index is written at the front by `finish`.
#[cfg(feature = "gen")]
pub struct StreamWriter {
    f: io::BufWriter<fs::File>,
    count: usize,
    pos: u64,
    index: Vec<(u64, u32, u64)>,
}

#[cfg(feature = "gen")]
impl StreamWriter {
    pub fn create(path: &Path, count: usize) -> io::Result<StreamWriter> {
        use io::{Seek, SeekFrom};
        let mut f = io::BufWriter::with_capacity(1 << 20, fs::File::create(path)?);
        let pos = (12 + count * ENTRY) as u64;
        f.seek(SeekFrom::Start(pos))?;
        Ok(StreamWriter { f, count, pos, index: Vec::with_capacity(count) })
    }

    pub fn push(&mut self, payload: &[u8], hash: u64) -> io::Result<()> {
        use io::Write;
        self.f.write_all(payload)?;
        self.index.push((self.pos, payload.len() as u32, hash));
        self.pos += payload.len() as u64;
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<()> {
        use io::{Seek, SeekFrom, Write};
        assert_eq!(self.index.len(), self.count, "stream frame count");
        self.f.seek(SeekFrom::Start(0))?;
        self.f.write_all(MAGIC)?;
        self.f.write_all(&(self.count as u32).to_le_bytes())?;
        for (off, len, hash) in &self.index {
            self.f.write_all(&off.to_le_bytes())?;
            self.f.write_all(&len.to_le_bytes())?;
            self.f.write_all(&0u32.to_le_bytes())?;
            self.f.write_all(&hash.to_le_bytes())?;
        }
        self.f.flush()
    }
}
