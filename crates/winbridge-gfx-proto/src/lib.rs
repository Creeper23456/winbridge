// SPDX-License-Identifier: GPL-3.0-or-later

//! Wire protocol of the WinBridge GPU forwarding channel (`org.winbridge.gfx.0`).
//!
//! `frame` is the transport framing, `codec` the byte codec shared with the generated Vulkan
//! serializers, and this module the opcodes and the non-Vulkan payloads.

pub mod codec;
pub mod frame;

use codec::{CodecError, Dec, Enc};

/// Protocol version carried in `Hello` (independent of the frame header version).
pub const PROTOCOL_VERSION: u16 = 1;

pub const OP_HELLO: u16 = 0x0001;
pub const OP_PING: u16 = 0x0002;
pub const OP_PONG: u16 = 0x0003;
/// Guest to host: serialized Vulkan call. Payload starts with the command id (u16).
pub const OP_VK_CALL: u16 = 0x0010;
/// Host to guest: reply to an `OP_VK_CALL` with the same seq.
pub const OP_VK_REPLY: u16 = 0x0011;
/// Guest to host: shadow memory contents (`MemUpload`). Replied to with an empty `OP_MEM_UPLOAD`.
pub const OP_MEM_UPLOAD: u16 = 0x0020;
/// Guest to host: request (`MemRead`); reply: the bytes, with the same opcode and seq.
pub const OP_MEM_DOWNLOAD: u16 = 0x0021;
/// Host to guest: fatal error, payload is a UTF-8 message.
pub const OP_ERROR: u16 = 0x00F0;

/// Guest to host, no reply: a sequence of asynchronous items, executed in order.
/// Payload: repeated `[kind: u8][len: u32 LE][bytes]`.
pub const OP_VK_BATCH: u16 = 0x0012;
/// Batch item: a serialized Vulkan call (as in `OP_VK_CALL`), its reply is discarded.
pub const BATCH_VK_CALL: u8 = 0;
/// Batch item: a `MemUpload`.
pub const BATCH_MEM_UPLOAD: u8 = 1;

/// Appends one item to a batch payload.
pub fn batch_push(batch: &mut Vec<u8>, kind: u8, bytes: &[u8]) {
    batch.push(kind);
    batch.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    batch.extend_from_slice(bytes);
}

/// Splits a batch payload into `(kind, bytes)` items.
pub fn batch_items(payload: &[u8]) -> Result<Vec<(u8, &[u8])>, CodecError> {
    let mut d = Dec::new(payload);
    let mut out = Vec::new();
    while d.remaining() > 0 {
        let kind = d.u8()?;
        out.push((kind, d.blob()?));
    }
    Ok(out)
}

/// Guest to host: create the host images of a swapchain (`ScCreate`); reply: `i32` result.
pub const OP_SC_CREATE: u16 = 0x0040;
/// Guest to host: destroy a swapchain (`u64` id); reply: empty.
pub const OP_SC_DESTROY: u16 = 0x0041;
/// Guest to host: acquire the image `index` (`ScAcquire`); reply: `i32` result.
pub const OP_SC_ACQUIRE: u16 = 0x0042;
/// Guest to host: present (`ScPresent`); reply: `i32` result followed by the frame (`Frame`).
pub const OP_SC_PRESENT: u16 = 0x0043;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScCreate {
    pub device: u64,
    pub swapchain: u64,
    pub width: u32,
    pub height: u32,
    /// Raw `VkFormat`.
    pub format: u32,
    /// Raw `VkImageUsageFlags` requested by the application.
    pub usage: u32,
    /// Guest-assigned ids of the images, one per swapchain image.
    pub images: Vec<u64>,
}

impl ScCreate {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u64(self.device);
        e.u64(self.swapchain);
        e.u32(self.width);
        e.u32(self.height);
        e.u32(self.format);
        e.u32(self.usage);
        e.u32(self.images.len() as u32);
        for i in &self.images {
            e.u64(*i);
        }
        e.into_vec()
    }

    pub fn decode(b: &[u8]) -> Result<ScCreate, CodecError> {
        let mut d = Dec::new(b);
        let (device, swapchain, width, height, format, usage) = (d.u64()?, d.u64()?, d.u32()?, d.u32()?, d.u32()?, d.u32()?);
        let n = d.count(8)?;
        let images = (0..n).map(|_| d.u64()).collect::<Result<_, _>>()?;
        d.finish()?;
        Ok(ScCreate { device, swapchain, width, height, format, usage, images })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScAcquire {
    pub swapchain: u64,
    pub index: u32,
    /// Semaphore to signal (0: none).
    pub semaphore: u64,
    /// Fence to signal (0: none).
    pub fence: u64,
    /// Any queue of the swapchain's device.
    pub queue: u64,
}

impl ScAcquire {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u64(self.swapchain);
        e.u32(self.index);
        e.u64(self.semaphore);
        e.u64(self.fence);
        e.u64(self.queue);
        e.into_vec()
    }

    pub fn decode(b: &[u8]) -> Result<ScAcquire, CodecError> {
        let mut d = Dec::new(b);
        let r = ScAcquire { swapchain: d.u64()?, index: d.u32()?, semaphore: d.u64()?, fence: d.u64()?, queue: d.u64()? };
        d.finish()?;
        Ok(r)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScPresent {
    pub queue: u64,
    /// Queue family index of `queue`.
    pub family: u32,
    pub swapchain: u64,
    pub index: u32,
    pub waits: Vec<u64>,
}

impl ScPresent {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u64(self.queue);
        e.u32(self.family);
        e.u64(self.swapchain);
        e.u32(self.index);
        e.u32(self.waits.len() as u32);
        for w in &self.waits {
            e.u64(*w);
        }
        e.into_vec()
    }

    pub fn decode(b: &[u8]) -> Result<ScPresent, CodecError> {
        let mut d = Dec::new(b);
        let (queue, family, swapchain, index) = (d.u64()?, d.u32()?, d.u64()?, d.u32()?);
        let n = d.count(8)?;
        let waits = (0..n).map(|_| d.u64()).collect::<Result<_, _>>()?;
        d.finish()?;
        Ok(ScPresent { queue, family, swapchain, index, waits })
    }
}

/// A presented frame as BGRA8 rows, sent as the bands that changed since the previous frame
/// of the same swapchain (all bands for the first one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Rows per band (the last band may be shorter).
    pub band_rows: u32,
    /// `(band index, pixel bytes)`.
    pub bands: Vec<(u32, Vec<u8>)>,
}

impl Frame {
    pub fn encode_into(&self, e: &mut Enc) {
        e.u32(self.width);
        e.u32(self.height);
        e.u32(self.band_rows);
        e.u32(self.bands.len() as u32);
        for (i, b) in &self.bands {
            e.u32(*i);
            e.blob(b);
        }
    }

    pub fn decode_from(d: &mut Dec) -> Result<Frame, CodecError> {
        let (width, height, band_rows) = (d.u32()?, d.u32()?, d.u32()?);
        if width == 0 || height == 0 || band_rows == 0 || width > 16384 || height > 16384 {
            return Err(CodecError::Invalid("frame dimensions"));
        }
        let n = d.count(8)?;
        let mut bands = Vec::with_capacity(n);
        let band_count = height.div_ceil(band_rows);
        for _ in 0..n {
            let i = d.u32()?;
            let b = d.blob()?;
            let rows = band_rows.min(height - i.min(band_count) * band_rows.min(height));
            if i >= band_count || b.len() != (rows as usize) * (width as usize) * 4 {
                return Err(CodecError::Invalid("frame band"));
            }
            bands.push((i, b.to_vec()));
        }
        Ok(Frame { width, height, band_rows, bands })
    }

    /// Rows per band for a frame of this width: about 64 KiB of pixels.
    pub fn band_rows_for(width: u32) -> u32 {
        (65536 / (width.max(1) * 4)).max(1)
    }
}

/// Upper bound of one `MemUpload` / `MemRead` transfer.
pub const MEM_CHUNK_MAX: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub version: u16,
    /// Feature bitmap; no bits are defined yet.
    pub features: u64,
    /// Receive window in bytes the sender grants (advertised, not yet enforced).
    pub window: u32,
    /// Human-readable identity: the host GPU for the host's reply, the client for the guest's hello.
    pub name: String,
}

impl Hello {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u16(self.version);
        e.u64(self.features);
        e.u32(self.window);
        e.blob(self.name.as_bytes());
        e.into_vec()
    }

    pub fn decode(b: &[u8]) -> Result<Hello, CodecError> {
        let mut d = Dec::new(b);
        let h = Hello {
            version: d.u16()?,
            features: d.u64()?,
            window: d.u32()?,
            name: String::from_utf8_lossy(d.blob()?).into_owned(),
        };
        d.finish()?;
        Ok(h)
    }
}

/// Bytes written into a host-visible allocation, addressed by the allocation's guest id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemUpload<'a> {
    pub memory: u64,
    pub offset: u64,
    pub data: &'a [u8],
}

impl MemUpload<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::with_capacity(20 + self.data.len());
        e.u64(self.memory);
        e.u64(self.offset);
        e.blob(self.data);
        e.into_vec()
    }
}

impl<'a> MemUpload<'a> {
    pub fn decode(b: &'a [u8]) -> Result<MemUpload<'a>, CodecError> {
        let mut d = Dec::new(b);
        let m = MemUpload { memory: d.u64()?, offset: d.u64()?, data: d.blob()? };
        d.finish()?;
        Ok(m)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemRead {
    pub memory: u64,
    pub offset: u64,
    pub len: u64,
}

impl MemRead {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::with_capacity(24);
        e.u64(self.memory);
        e.u64(self.offset);
        e.u64(self.len);
        e.into_vec()
    }

    pub fn decode(b: &[u8]) -> Result<MemRead, CodecError> {
        let mut d = Dec::new(b);
        let m = MemRead { memory: d.u64()?, offset: d.u64()?, len: d.u64()? };
        d.finish()?;
        Ok(m)
    }
}

/// First field of every dispatchable object the guest ICD hands to the Vulkan loader.
///
/// The loader overwrites `loader_data` with its dispatch table pointer; `id` is the handle id
/// that travels over the wire.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DispatchableHeader {
    pub loader_data: usize,
    pub id: u64,
}

#[cfg(test)]
mod tests {
    use super::codec::CodecError;
    use super::frame::*;
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::thread;

    fn pattern(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    fn send_to_vec(fragment: usize, op: u16, flags: u8, seq: u32, payload: &[u8]) -> Vec<u8> {
        let mut w = FrameWriter::with_fragment_len(Vec::new(), fragment);
        w.send(op, flags, seq, payload).unwrap();
        w.into_inner()
    }

    #[test]
    fn header_round_trip() {
        let h = Header { flags: FLAG_REPLY_EXPECTED | FLAG_MORE, opcode: 0xABCD, seq: 0x0102_0304, len: 4096 };
        let b = h.encode();
        assert_eq!(&b[0..4], b"WBGF");
        assert_eq!(b[4], 1);
        assert_eq!(Header::decode(&b).unwrap(), h);
    }

    #[test]
    fn message_round_trip_all_sizes() {
        for &n in &[0usize, 1, 15, 16, 17, 4095, 16 * 1024, 16 * 1024 + 1, 65535, 65536, 1 << 20, (1 << 20) + 3] {
            let p = pattern(n, n as u8);
            let wire = send_to_vec(DEFAULT_FRAGMENT_LEN, OP_VK_CALL, FLAG_REPLY_EXPECTED, 77, &p);
            let mut r = FrameReader::new(wire.as_slice());
            let m = r.recv().unwrap();
            assert_eq!(m, Message { opcode: OP_VK_CALL, flags: FLAG_REPLY_EXPECTED, seq: 77, payload: p });
            assert!(matches!(r.recv(), Err(FrameError::Closed)), "size {n}");
        }
    }

    #[test]
    fn fragmentation_splits_and_reassembles() {
        let p = pattern(1050, 3);
        let wire = send_to_vec(100, OP_MEM_UPLOAD, FLAG_REPLY_EXPECTED, 9, &p);
        // 11 frames: 10 full ones with MORE, one of 50 bytes without.
        let mut off = 0;
        let mut frames = 0;
        while off < wire.len() {
            let h = Header::decode(wire[off..off + HEADER_LEN].try_into().unwrap()).unwrap();
            frames += 1;
            assert_eq!(h.seq, 9);
            assert_eq!(h.opcode, OP_MEM_UPLOAD);
            let last = off + HEADER_LEN + h.len as usize == wire.len();
            assert_eq!(h.flags & FLAG_MORE != 0, !last, "frame {frames}");
            assert_eq!(h.len, if last { 50 } else { 100 });
            off += HEADER_LEN + h.len as usize;
        }
        assert_eq!(frames, 11);
        let m = FrameReader::new(wire.as_slice()).recv().unwrap();
        assert_eq!(m.payload, p);
        assert_eq!(m.flags, FLAG_REPLY_EXPECTED, "FLAG_MORE must not leak into the message");
    }

    #[test]
    fn exact_multiple_has_no_empty_tail() {
        let wire = send_to_vec(100, OP_VK_CALL, 0, 1, &pattern(200, 0));
        assert_eq!(wire.len(), 2 * (HEADER_LEN + 100));
        let m = FrameReader::new(wire.as_slice()).recv().unwrap();
        assert_eq!(m.payload.len(), 200);
    }

    #[test]
    fn consecutive_messages_keep_boundaries() {
        let mut w = FrameWriter::with_fragment_len(Vec::new(), 64);
        for seq in 0..5u32 {
            w.send(OP_VK_CALL, 0, seq, &pattern(seq as usize * 70, seq as u8)).unwrap();
        }
        let wire = w.into_inner();
        let mut r = FrameReader::new(wire.as_slice());
        for seq in 0..5u32 {
            let m = r.recv().unwrap();
            assert_eq!(m.seq, seq);
            assert_eq!(m.payload, pattern(seq as usize * 70, seq as u8));
        }
    }

    #[test]
    fn bad_magic_poisons_the_reader() {
        let mut wire = send_to_vec(DEFAULT_FRAGMENT_LEN, OP_PING, 0, 1, b"x");
        wire[0] = b'X';
        let good = send_to_vec(DEFAULT_FRAGMENT_LEN, OP_PING, 0, 2, b"y");
        wire.extend_from_slice(&good);
        let mut r = FrameReader::new(wire.as_slice());
        assert!(matches!(r.recv(), Err(FrameError::BadMagic(m)) if m == *b"XBGF"));
        // No resync: the valid frame behind the garbage is not delivered.
        assert!(matches!(r.recv(), Err(FrameError::Poisoned)));
    }

    #[test]
    fn bad_version_flags_and_length_are_rejected() {
        let ok = send_to_vec(DEFAULT_FRAGMENT_LEN, OP_PING, 0, 1, b"");
        let mut v = ok.clone();
        v[4] = 2;
        assert!(matches!(FrameReader::new(v.as_slice()).recv(), Err(FrameError::BadVersion(2))));
        let mut f = ok.clone();
        f[5] = 0b1000;
        assert!(matches!(FrameReader::new(f.as_slice()).recv(), Err(FrameError::UnsupportedFlags(0b1000))));
        let mut z = ok.clone();
        z[5] = FLAG_ZSTD;
        assert!(matches!(FrameReader::new(z.as_slice()).recv(), Err(FrameError::UnsupportedFlags(FLAG_ZSTD))));
        let mut l = ok;
        l[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(FrameReader::new(l.as_slice()).recv(), Err(FrameError::FrameTooLarge(u32::MAX))));
    }

    #[test]
    fn truncation_is_distinct_from_clean_close() {
        let wire = send_to_vec(DEFAULT_FRAGMENT_LEN, OP_VK_CALL, 0, 1, &pattern(100, 1));
        for cut in [1, HEADER_LEN - 1, HEADER_LEN, HEADER_LEN + 50, wire.len() - 1] {
            let mut r = FrameReader::new(&wire[..cut]);
            assert!(matches!(r.recv(), Err(FrameError::Truncated)), "cut at {cut}");
        }
        assert!(matches!(FrameReader::new(&wire[..0]).recv(), Err(FrameError::Closed)));
        // Cut between two fragments of one message is also truncation.
        let frag = send_to_vec(50, OP_VK_CALL, 0, 1, &pattern(100, 1));
        let mut r = FrameReader::new(&frag[..HEADER_LEN + 50]);
        assert!(matches!(r.recv(), Err(FrameError::Truncated)));
    }

    #[test]
    fn fragment_header_mismatch_is_rejected() {
        let a = send_to_vec(50, OP_VK_CALL, 0, 1, &pattern(100, 1));
        let b = send_to_vec(50, OP_VK_CALL, 0, 2, &pattern(100, 1));
        let mut wire = a[..HEADER_LEN + 50].to_vec();
        wire.extend_from_slice(&b[HEADER_LEN + 50..]);
        assert!(matches!(FrameReader::new(wire.as_slice()).recv(), Err(FrameError::FragmentMismatch)));
        let c = send_to_vec(50, OP_VK_REPLY, 0, 1, &pattern(100, 1));
        let mut wire = a[..HEADER_LEN + 50].to_vec();
        wire.extend_from_slice(&c[HEADER_LEN + 50..]);
        assert!(matches!(FrameReader::new(wire.as_slice()).recv(), Err(FrameError::FragmentMismatch)));
    }

    #[test]
    fn writer_rejects_reserved_flags() {
        let mut w = FrameWriter::new(Vec::new());
        assert!(matches!(w.send(OP_PING, FLAG_MORE, 0, b""), Err(FrameError::UnsupportedFlags(_))));
        assert!(matches!(w.send(OP_PING, FLAG_ZSTD, 0, b""), Err(FrameError::UnsupportedFlags(_))));
    }

    #[test]
    fn unix_loopback_request_reply() {
        let (a, b) = UnixStream::pair().unwrap();
        let echo = thread::spawn(move || {
            let mut r = FrameReader::new(b.try_clone().unwrap());
            let mut w = FrameWriter::with_fragment_len(b, 4096);
            let mut served = 0u32;
            loop {
                match r.recv() {
                    Ok(m) => {
                        assert!(m.reply_expected());
                        w.send(OP_PONG, 0, m.seq, &m.payload).unwrap();
                        served += 1;
                    }
                    Err(FrameError::Closed) => return served,
                    Err(e) => panic!("echo side: {e}"),
                }
            }
        });
        let mut r = FrameReader::new(a.try_clone().unwrap());
        let mut w = FrameWriter::with_fragment_len(a, 1000);
        let sizes = [0usize, 16, 1000, 1001, 5000, 70_000, 300_000];
        for seq in 0..200u32 {
            let n = sizes[seq as usize % sizes.len()];
            let p = pattern(n, seq as u8);
            w.send(OP_PING, FLAG_REPLY_EXPECTED, seq, &p).unwrap();
            let m = r.recv().unwrap();
            assert_eq!((m.opcode, m.seq), (OP_PONG, seq));
            assert_eq!(m.payload, p);
        }
        drop(w); // closes our write side
        drop(r);
        // Dropping both halves of `a` closes the socket; the echo side sees a clean close.
        assert_eq!(echo.join().unwrap(), 200);
    }

    #[test]
    fn unix_stream_delivers_in_small_pieces() {
        // The reader must cope with a peer that writes a frame one byte at a time.
        let (a, mut b) = UnixStream::pair().unwrap();
        let p = pattern(300, 5);
        let wire = send_to_vec(128, OP_VK_REPLY, 0, 4, &p);
        let t = thread::spawn(move || {
            for byte in wire {
                b.write_all(&[byte]).unwrap();
                b.flush().unwrap();
            }
        });
        let m = FrameReader::new(a).recv().unwrap();
        t.join().unwrap();
        assert_eq!(m.payload, p);
    }

    #[test]
    fn bad_magic_over_unix_stream_closes_the_channel() {
        let (a, mut b) = UnixStream::pair().unwrap();
        b.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        let mut r = FrameReader::new(a);
        assert!(matches!(r.recv(), Err(FrameError::BadMagic(_))));
        assert!(matches!(r.recv(), Err(FrameError::Poisoned)));
        // The owner closes the stream; the peer sees EOF, or a reset because unread bytes remained.
        drop(r);
        let mut rest = Vec::new();
        match b.read_to_end(&mut rest) {
            Ok(_) => {}
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset),
        }
    }

    #[test]
    fn swapchain_messages_and_frames_round_trip() {
        let c = ScCreate { device: 3, swapchain: 9, width: 1280, height: 720, format: 44, usage: 0x17, images: vec![10, 11, 12] };
        assert_eq!(ScCreate::decode(&c.encode()).unwrap(), c);
        let a = ScAcquire { swapchain: 9, index: 2, semaphore: 5, fence: 0, queue: 4 };
        assert_eq!(ScAcquire::decode(&a.encode()).unwrap(), a);
        let p = ScPresent { queue: 4, family: 1, swapchain: 9, index: 2, waits: vec![5, 6] };
        assert_eq!(ScPresent::decode(&p.encode()).unwrap(), p);
        assert!(ScPresent::decode(&p.encode()[..10]).is_err());
        // 10x5 frame, 2 rows per band: 3 bands, the last has 1 row.
        let f = Frame { width: 10, height: 5, band_rows: 2, bands: vec![(0, vec![1; 80]), (2, vec![2; 40])] };
        let mut e = Enc::new();
        f.encode_into(&mut e);
        assert_eq!(Frame::decode_from(&mut Dec::new(e.as_slice())).unwrap(), f);
        let bad = Frame { width: 10, height: 5, band_rows: 2, bands: vec![(2, vec![2; 80])] };
        let mut e = Enc::new();
        bad.encode_into(&mut e);
        assert!(Frame::decode_from(&mut Dec::new(e.as_slice())).is_err(), "a short band must have the short length");
        assert_eq!(Frame::band_rows_for(1280), 12);
    }

    #[test]
    fn batch_round_trip_and_truncation() {
        let mut b = Vec::new();
        batch_push(&mut b, BATCH_VK_CALL, b"abc");
        batch_push(&mut b, BATCH_MEM_UPLOAD, b"");
        batch_push(&mut b, BATCH_VK_CALL, &pattern(70_000, 3));
        let items = batch_items(&b).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!((items[0].0, items[0].1), (BATCH_VK_CALL, &b"abc"[..]));
        assert_eq!(items[1], (BATCH_MEM_UPLOAD, &b""[..]));
        assert_eq!(items[2].1, &pattern(70_000, 3)[..]);
        for cut in 1..b.len().min(40) {
            let _ = batch_items(&b[..cut]); // must not panic
        }
        assert!(batch_items(&b[..b.len() - 1]).is_err());
    }

    #[test]
    fn hello_and_memory_payloads_round_trip() {
        let h = Hello { version: PROTOCOL_VERSION, features: 0x5, window: 1 << 20, name: "llvmpipe".into() };
        assert_eq!(Hello::decode(&h.encode()).unwrap(), h);
        assert!(Hello::decode(&h.encode()[..5]).is_err());
        let data = pattern(33, 1);
        let u = MemUpload { memory: 12, offset: 4096, data: &data };
        assert_eq!(MemUpload::decode(&u.encode()).unwrap(), u);
        let r = MemRead { memory: 12, offset: 8, len: 64 };
        assert_eq!(MemRead::decode(&r.encode()).unwrap(), r);
        let mut bad = u.encode();
        bad.push(0);
        assert_eq!(MemUpload::decode(&bad), Err(CodecError::Invalid("trailing bytes")));
    }
}
