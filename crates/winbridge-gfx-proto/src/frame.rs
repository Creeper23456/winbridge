// SPDX-License-Identifier: GPL-3.0-or-later

//! Frame layer of the GPU forwarding channel (docs/GFX-DESIGN.md section 8).
//!
//! Header, 16 bytes, little-endian: magic `WBGF`, version (1), flags, opcode (u16), seq (u32),
//! length (u32, payload bytes after the header). A message larger than the fragment size is
//! sent as several frames with the same opcode, seq and flags; every frame but the last
//! carries `FLAG_MORE`. The stream is ordered and reliable, so there is no resync: after any
//! framing error the reader is poisoned and the channel must be closed.

use std::fmt;
use std::io::{self, Read, Write};

pub const MAGIC: [u8; 4] = *b"WBGF";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 16;
/// Largest payload of a single frame (same limit as `winbridge-proto`).
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;
/// Largest reassembled message.
pub const MAX_MESSAGE_LEN: usize = 256 * 1024 * 1024;
/// Fragment size used by `FrameWriter::new`. Chosen from `chanbench` (docs/GFX-DESIGN.md 3.6):
/// host-to-guest throughput peaks with 16..64 KiB writes and is 10-13% lower with 1 MiB ones.
pub const DEFAULT_FRAGMENT_LEN: usize = 64 * 1024;
/// Payloads up to this size are sent with the header in one `write` call.
const COALESCE_LEN: usize = 16 * 1024;

/// Reserved: the payload is zstd-compressed. Not implemented yet; readers reject it.
pub const FLAG_ZSTD: u8 = 0b001;
/// The sender expects a reply with the same seq.
pub const FLAG_REPLY_EXPECTED: u8 = 0b010;
/// More fragments of this message follow.
pub const FLAG_MORE: u8 = 0b100;
const KNOWN_FLAGS: u8 = FLAG_ZSTD | FLAG_REPLY_EXPECTED | FLAG_MORE;

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    /// The peer closed the stream between two frames.
    Closed,
    /// The stream ended inside a frame.
    Truncated,
    BadMagic([u8; 4]),
    BadVersion(u8),
    /// A flag bit that this implementation does not handle (including the unimplemented zstd flag).
    UnsupportedFlags(u8),
    FrameTooLarge(u32),
    MessageTooLarge(usize),
    /// A fragment changed opcode, seq or flags relative to the first fragment.
    FragmentMismatch,
    /// An earlier framing error left the stream out of sync.
    Poisoned,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "i/o error: {e}"),
            Self::Closed => f.write_str("channel closed"),
            Self::Truncated => f.write_str("stream ended inside a frame"),
            Self::BadMagic(m) => write!(f, "bad frame magic {m:02x?}"),
            Self::BadVersion(v) => write!(f, "unsupported frame version {v}"),
            Self::UnsupportedFlags(x) => write!(f, "unsupported frame flags {x:#05b}"),
            Self::FrameTooLarge(n) => write!(f, "frame length {n} exceeds {MAX_FRAME_LEN}"),
            Self::MessageTooLarge(n) => write!(f, "message length exceeds {MAX_MESSAGE_LEN} (at least {n})"),
            Self::FragmentMismatch => f.write_str("fragment header differs from the first fragment"),
            Self::Poisoned => f.write_str("reader poisoned by an earlier framing error"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub flags: u8,
    pub opcode: u16,
    pub seq: u32,
    pub len: u32,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0..4].copy_from_slice(&MAGIC);
        h[4] = VERSION;
        h[5] = self.flags;
        h[6..8].copy_from_slice(&self.opcode.to_le_bytes());
        h[8..12].copy_from_slice(&self.seq.to_le_bytes());
        h[12..16].copy_from_slice(&self.len.to_le_bytes());
        h
    }

    /// Validates magic, version, flags and length.
    pub fn decode(h: &[u8; HEADER_LEN]) -> Result<Header, FrameError> {
        let magic: [u8; 4] = h[0..4].try_into().unwrap();
        if magic != MAGIC {
            return Err(FrameError::BadMagic(magic));
        }
        if h[4] != VERSION {
            return Err(FrameError::BadVersion(h[4]));
        }
        let flags = h[5];
        if flags & !KNOWN_FLAGS != 0 || flags & FLAG_ZSTD != 0 {
            return Err(FrameError::UnsupportedFlags(flags));
        }
        let len = u32::from_le_bytes(h[12..16].try_into().unwrap());
        if len as usize > MAX_FRAME_LEN {
            return Err(FrameError::FrameTooLarge(len));
        }
        Ok(Header {
            flags,
            opcode: u16::from_le_bytes(h[6..8].try_into().unwrap()),
            seq: u32::from_le_bytes(h[8..12].try_into().unwrap()),
            len,
        })
    }
}

/// A reassembled message. `flags` never contains `FLAG_MORE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub opcode: u16,
    pub flags: u8,
    pub seq: u32,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn reply_expected(&self) -> bool {
        self.flags & FLAG_REPLY_EXPECTED != 0
    }
}

pub struct FrameWriter<W: Write> {
    w: W,
    fragment_len: usize,
    scratch: Vec<u8>,
}

impl<W: Write> FrameWriter<W> {
    pub fn new(w: W) -> Self {
        Self::with_fragment_len(w, DEFAULT_FRAGMENT_LEN)
    }

    /// `fragment_len` is the largest payload per frame, clamped to `1..=MAX_FRAME_LEN`.
    pub fn with_fragment_len(w: W, fragment_len: usize) -> Self {
        Self { w, fragment_len: fragment_len.clamp(1, MAX_FRAME_LEN), scratch: Vec::new() }
    }

    pub fn into_inner(self) -> W {
        self.w
    }

    pub fn get_ref(&self) -> &W {
        &self.w
    }

    /// Sends one message, fragmenting it if needed. `flags` must not contain `FLAG_MORE` or `FLAG_ZSTD`.
    pub fn send(&mut self, opcode: u16, flags: u8, seq: u32, payload: &[u8]) -> Result<(), FrameError> {
        if flags & !FLAG_REPLY_EXPECTED != 0 {
            return Err(FrameError::UnsupportedFlags(flags));
        }
        if payload.len() > MAX_MESSAGE_LEN {
            return Err(FrameError::MessageTooLarge(payload.len()));
        }
        let mut chunks = payload.chunks(self.fragment_len);
        // An empty message is one empty frame.
        let first: &[u8] = chunks.next().unwrap_or(&[]);
        let mut chunk = first;
        loop {
            let next = chunks.next();
            let hdr = Header {
                flags: flags | if next.is_some() { FLAG_MORE } else { 0 },
                opcode,
                seq,
                len: chunk.len() as u32,
            }
            .encode();
            if chunk.len() <= COALESCE_LEN {
                self.scratch.clear();
                self.scratch.extend_from_slice(&hdr);
                self.scratch.extend_from_slice(chunk);
                self.w.write_all(&self.scratch)?;
            } else {
                self.w.write_all(&hdr)?;
                self.w.write_all(chunk)?;
            }
            match next {
                Some(n) => chunk = n,
                None => break,
            }
        }
        self.w.flush()?;
        Ok(())
    }
}

pub struct FrameReader<R: Read> {
    r: R,
    poisoned: bool,
}

impl<R: Read> FrameReader<R> {
    pub fn new(r: R) -> Self {
        Self { r, poisoned: false }
    }

    pub fn into_inner(self) -> R {
        self.r
    }

    /// Reads one frame header; `Closed` only if the stream ended before its first byte.
    fn read_header(&mut self) -> Result<Header, FrameError> {
        let mut h = [0u8; HEADER_LEN];
        let mut got = 0;
        while got < HEADER_LEN {
            match self.r.read(&mut h[got..]) {
                Ok(0) if got == 0 => return Err(FrameError::Closed),
                Ok(0) => return Err(FrameError::Truncated),
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Header::decode(&h)
    }

    fn read_payload(&mut self, len: usize, out: &mut Vec<u8>) -> Result<(), FrameError> {
        // Grow only as bytes arrive, so a lying length cannot force a large allocation.
        let start = out.len();
        out.reserve(len.min(DEFAULT_FRAGMENT_LEN));
        let got = (&mut self.r).take(len as u64).read_to_end(out)?;
        if got != len {
            out.truncate(start);
            return Err(FrameError::Truncated);
        }
        Ok(())
    }

    /// Reads and reassembles the next message. After any error the stream position is
    /// unknown: the reader is poisoned and every later call fails with `Poisoned`.
    pub fn recv(&mut self) -> Result<Message, FrameError> {
        if self.poisoned {
            return Err(FrameError::Poisoned);
        }
        let r = self.recv_inner();
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }

    fn recv_inner(&mut self) -> Result<Message, FrameError> {
        let first = self.read_header()?;
        let mut payload = Vec::new();
        self.read_payload(first.len as usize, &mut payload).map_err(eof_in_frame)?;
        let mut more = first.flags & FLAG_MORE != 0;
        while more {
            let h = self.read_header().map_err(eof_in_frame)?;
            if h.opcode != first.opcode || h.seq != first.seq || (h.flags ^ first.flags) & !FLAG_MORE != 0 {
                return Err(FrameError::FragmentMismatch);
            }
            let total = payload.len() + h.len as usize;
            if total > MAX_MESSAGE_LEN {
                return Err(FrameError::MessageTooLarge(total));
            }
            self.read_payload(h.len as usize, &mut payload).map_err(eof_in_frame)?;
            more = h.flags & FLAG_MORE != 0;
        }
        Ok(Message { opcode: first.opcode, flags: first.flags & !FLAG_MORE, seq: first.seq, payload })
    }
}

/// End of stream after the first byte of a message is truncation, not a clean close.
fn eof_in_frame(e: FrameError) -> FrameError {
    match e {
        FrameError::Closed => FrameError::Truncated,
        e => e,
    }
}

/// What every layer above the wire needs from a transport: whole messages out, whole messages
/// in, blocking. Fragmentation, overlapped I/O, timeouts and shared-memory rings are the
/// transport's business. Swapping the virtio-serial stream for another transport means
/// implementing this trait; the opcodes, the codecs and the dispatcher stay as they are.
pub trait MessageTransport: Send {
    /// Sends one message; returns when it is handed to the transport.
    fn send(&mut self, opcode: u16, flags: u8, seq: u32, payload: &[u8]) -> Result<(), FrameError>;
    /// Blocks until the next whole message arrives. `Err(FrameError::Closed)` is a clean close.
    fn recv(&mut self) -> Result<Message, FrameError>;
}

/// `MessageTransport` over an ordered byte stream (a serial port, a Unix socket): frames with
/// fragmentation, as described at the top of this module.
pub struct StreamTransport<R: Read, W: Write> {
    reader: FrameReader<R>,
    writer: FrameWriter<W>,
}

impl<R: Read, W: Write> StreamTransport<R, W> {
    pub fn new(r: R, w: W) -> Self {
        Self { reader: FrameReader::new(r), writer: FrameWriter::new(w) }
    }
}

impl<R: Read + Send, W: Write + Send> MessageTransport for StreamTransport<R, W> {
    fn send(&mut self, opcode: u16, flags: u8, seq: u32, payload: &[u8]) -> Result<(), FrameError> {
        self.writer.send(opcode, flags, seq, payload)
    }

    fn recv(&mut self) -> Result<Message, FrameError> {
        self.reader.recv()
    }
}
