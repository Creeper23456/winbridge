// SPDX-License-Identifier: GPL-3.0-or-later

//! The connection to the host: request/reply plumbing and handle allocation.

use crate::transport;
use winbridge_gfx_proto::frame::{MessageTransport, FLAG_REPLY_EXPECTED};
use winbridge_gfx_proto::{
    DispatchableHeader, Hello, MemRead, MemUpload, OP_ERROR, OP_HELLO, OP_MEM_DOWNLOAD, OP_VK_CALL,
    OP_VK_BATCH, OP_VK_REPLY, PROTOCOL_VERSION, BATCH_MEM_UPLOAD, BATCH_VK_CALL, batch_push,
};
use winbridge_gfx_vk::{GuestError, Link, LinkError};

/// Bytes per `MemUpload` / `MemRead` request.
const MEM_CHUNK: usize = 4 * 1024 * 1024;
/// The queued batch is sent once it reaches this size (the M0 numbers: writes of 16 KiB and up
/// run at the channel's bandwidth, small ones at about 30 us each).
const BATCH_SEND_LEN: usize = 32 * 1024;

pub struct HostLink {
    channel: Box<dyn MessageTransport>,
    seq: u32,
    next_id: u64,
    dead: bool,
    /// Queued asynchronous items (see `OP_VK_BATCH`).
    pending: Vec<u8>,
    /// Identity the host reported in its hello.
    pub host_name: String,
}

pub fn log(msg: &str) {
    use std::io::Write;
    if let Ok(path) = std::env::var("WINBRIDGE_GFX_LOG") {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "[icd pid {}] {msg}", std::process::id());
        }
    }
}

impl HostLink {
    pub fn connect() -> Result<HostLink, String> {
        let channel = transport::open().map_err(|e| format!("open channel: {e}"))?;
        let mut l = HostLink {
            channel,
            seq: 0,
            next_id: 1,
            dead: false,
            pending: Vec::new(),
            host_name: String::new(),
        };
        let hello = Hello { version: PROTOCOL_VERSION, features: 0, window: 0, name: format!("winbridge-gfx-icd {}", env!("CARGO_PKG_VERSION")) };
        let reply = l.roundtrip(OP_HELLO, &hello.encode()).map_err(|e| format!("hello: {e}"))?;
        let h = Hello::decode(&reply).map_err(|e| format!("hello reply: {e}"))?;
        if h.version != PROTOCOL_VERSION {
            return Err(format!("host speaks protocol {}, this ICD {PROTOCOL_VERSION}", h.version));
        }
        l.host_name = h.name;
        log(&format!("connected to {}", l.host_name));
        Ok(l)
    }

    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// A guest-assigned id for a non-dispatchable handle.
    pub fn new_handle_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// One synchronous request of any opcode; its reply carries the same opcode and seq.
    pub fn request(&mut self, op: u16, payload: &[u8]) -> Result<Vec<u8>, LinkError> {
        self.roundtrip(op, payload)
    }

    /// Sends one request frame and returns the payload of the reply carrying the same opcode
    /// family (`OP_VK_REPLY` for calls, the request's own opcode otherwise).
    fn roundtrip(&mut self, op: u16, payload: &[u8]) -> Result<Vec<u8>, LinkError> {
        if self.dead {
            return Err(LinkError("channel is dead".into()));
        }
        self.flush_pending()?;
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        let r = (|| {
            self.channel.send(op, FLAG_REPLY_EXPECTED, seq, payload).map_err(|e| e.to_string())?;
            let m = self.channel.recv().map_err(|e| e.to_string())?;
            let want = if op == OP_VK_CALL { OP_VK_REPLY } else { op };
            if m.opcode == OP_ERROR {
                return Err(format!("host error: {}", String::from_utf8_lossy(&m.payload)));
            }
            if m.opcode != want || m.seq != seq {
                return Err(format!("unexpected reply opcode {:#06x} seq {} (wanted {want:#06x} seq {seq})", m.opcode, m.seq));
            }
            Ok(m.payload)
        })();
        r.map_err(|e| {
            self.dead = true;
            log(&format!("channel failed: {e}"));
            LinkError(e)
        })
    }

    /// Sends the queued batch as one frame (or a few fragments).
    fn flush_pending(&mut self) -> Result<(), LinkError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        if self.dead {
            return Err(LinkError("channel is dead".into()));
        }
        self.seq = self.seq.wrapping_add(1);
        let batch = std::mem::take(&mut self.pending);
        self.channel.send(OP_VK_BATCH, 0, self.seq, &batch).map_err(|e| {
            self.dead = true;
            log(&format!("channel failed: {e}"));
            LinkError(e.to_string())
        })
    }

    fn queue(&mut self, kind: u8, bytes: &[u8]) -> Result<(), LinkError> {
        batch_push(&mut self.pending, kind, bytes);
        if self.pending.len() >= BATCH_SEND_LEN {
            self.flush_pending()?;
        }
        Ok(())
    }

    /// Queues `data` for the host allocation `memory` at `offset`; ordered with the queued calls.
    pub fn mem_upload(&mut self, memory: u64, offset: u64, data: &[u8]) -> Result<(), LinkError> {
        for (i, chunk) in data.chunks(MEM_CHUNK).enumerate() {
            let up = MemUpload { memory, offset: offset + (i * MEM_CHUNK) as u64, data: chunk };
            self.queue(BATCH_MEM_UPLOAD, &up.encode())?;
        }
        Ok(())
    }

    /// Fills `out` from the host allocation `memory` at `offset`.
    pub fn mem_download(&mut self, memory: u64, offset: u64, out: &mut [u8]) -> Result<(), LinkError> {
        let mut done = 0usize;
        while done < out.len() {
            let n = (out.len() - done).min(MEM_CHUNK);
            let req = MemRead { memory, offset: offset + done as u64, len: n as u64 };
            let bytes = self.roundtrip(OP_MEM_DOWNLOAD, &req.encode())?;
            if bytes.len() != n {
                self.dead = true;
                return Err(LinkError(format!("host returned {} bytes, wanted {n}", bytes.len())));
            }
            out[done..done + n].copy_from_slice(&bytes);
            done += n;
        }
        Ok(())
    }
}

impl Link for HostLink {
    fn call(&mut self, req: &[u8]) -> Result<Vec<u8>, LinkError> {
        let reply = self.roundtrip(OP_VK_CALL, req);
        if std::env::var_os("WINBRIDGE_GFX_LOG").is_some() {
            let cmd = req.get(..2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize).and_then(|i| winbridge_gfx_vk::COMMAND_NAMES.get(i));
            let status = match &reply {
                Ok(r) => r.get(..4).map(|b| format!("first i32 {}", i32::from_le_bytes([b[0], b[1], b[2], b[3]]))).unwrap_or_else(|| "empty reply".into()),
                Err(e) => format!("failed: {e}"),
            };
            log(&format!("{} -> {status}", cmd.copied().unwrap_or("?")));
        }
        reply
    }

    fn send_async(&mut self, req: &[u8]) -> Result<(), LinkError> {
        if self.dead {
            return Err(LinkError("channel is dead".into()));
        }
        self.queue(BATCH_VK_CALL, req)
    }

    fn flush(&mut self) -> Result<(), LinkError> {
        self.flush_pending()
    }

    fn new_handle(&mut self, dispatchable: bool, _kind: &'static str) -> (u64, u64) {
        let id = self.next_id;
        self.next_id += 1;
        if dispatchable {
            // The loader owns the first word of every dispatchable object it is given.
            let obj = Box::into_raw(Box::new(DispatchableHeader { loader_data: 0, id }));
            (id, obj as u64)
        } else {
            (id, id)
        }
    }

    fn discard_handle(&mut self, raw: u64, dispatchable: bool) {
        if dispatchable && raw != 0 {
            // SAFETY: `raw` came from `new_handle` and is released exactly once.
            drop(unsafe { Box::from_raw(raw as *mut DispatchableHeader) });
        }
    }

    fn failed(&mut self, command: &str, err: &GuestError) {
        self.dead = true;
        log(&format!("{command} failed: {err}"));
    }
}
