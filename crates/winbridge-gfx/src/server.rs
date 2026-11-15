// SPDX-License-Identifier: GPL-3.0-or-later

//! Frame loop of one guest connection.

use crate::session::Session;
use std::fmt;
use tracing::{info, warn};
use winbridge_gfx_proto::codec::{Dec, Enc};
use winbridge_gfx_proto::frame::{FrameError, MessageTransport};
use winbridge_gfx_proto::{
    Hello, MemRead, MemUpload, ScAcquire, ScCreate, ScPresent, OP_SC_ACQUIRE, OP_SC_CREATE, OP_SC_DESTROY, OP_SC_PRESENT, MEM_CHUNK_MAX, OP_ERROR, OP_HELLO, OP_MEM_DOWNLOAD, OP_MEM_UPLOAD, OP_PING, OP_PONG,
    OP_VK_BATCH, OP_VK_CALL, OP_VK_REPLY, PROTOCOL_VERSION, BATCH_MEM_UPLOAD, BATCH_VK_CALL,
};

/// Receive window granted to the guest. Advertised only; the host does not enforce it yet.
pub const ADVERTISED_WINDOW: u32 = 16 * 1024 * 1024;

#[derive(Debug)]
pub enum ServeError {
    Frame(FrameError),
    /// The guest broke the protocol; an `OP_ERROR` frame was sent first.
    Protocol(String),
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frame(e) => write!(f, "{e}"),
            Self::Protocol(s) => write!(f, "protocol error: {s}"),
        }
    }
}

impl std::error::Error for ServeError {}

impl From<FrameError> for ServeError {
    fn from(e: FrameError) -> Self {
        Self::Frame(e)
    }
}

/// Serves one connection until the guest closes it (`Ok`) or a framing or protocol error ends it.
pub fn serve<T: MessageTransport + ?Sized>(writer: &mut T, session: &mut Session) -> Result<(), ServeError> {
    let mut hello_done = false;
    loop {
        let msg = match writer.recv() {
            Ok(m) => m,
            Err(FrameError::Closed) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let seq = msg.seq;
        let fail = |writer: &mut T, why: String| -> ServeError {
            warn!("guest connection closed: {why}");
            let _ = writer.send(OP_ERROR, 0, seq, why.as_bytes());
            ServeError::Protocol(why)
        };
        match msg.opcode {
            OP_HELLO => {
                let h = match Hello::decode(&msg.payload) {
                    Ok(h) => h,
                    Err(e) => return Err(fail(writer, format!("bad hello: {e}"))),
                };
                if h.version != PROTOCOL_VERSION {
                    return Err(fail(writer, format!("protocol version {} unsupported (host speaks {PROTOCOL_VERSION})", h.version)));
                }
                info!("guest hello: {}", h.name);
                if hello_done {
                    // A new guest process on the same port: whatever the old one left behind goes.
                    session.release_all();
                }
                hello_done = true;
                let reply = Hello {
                    version: PROTOCOL_VERSION,
                    features: 0,
                    window: ADVERTISED_WINDOW,
                    name: format!("winbridge-gfx {}", env!("CARGO_PKG_VERSION")),
                };
                writer.send(OP_HELLO, 0, seq, &reply.encode())?;
            }
            _ if !hello_done => return Err(fail(writer, "first frame must be HELLO".into())),
            OP_PING => writer.send(OP_PONG, 0, seq, &msg.payload)?,
            OP_VK_CALL => {
                // SAFETY: `dispatch` validates every handle and length it decodes.
                match unsafe { winbridge_gfx_vk::dispatch(session, &msg.payload) } {
                    Ok(reply) => writer.send(OP_VK_REPLY, 0, seq, &reply)?,
                    Err(e) => return Err(fail(writer, format!("vk call: {e}"))),
                }
            }
            OP_VK_BATCH => {
                let items = match winbridge_gfx_proto::batch_items(&msg.payload) {
                    Ok(i) => i,
                    Err(e) => return Err(fail(writer, format!("bad batch: {e}"))),
                };
                for (kind, bytes) in items {
                    match kind {
                        BATCH_VK_CALL => {
                            // SAFETY: as for OP_VK_CALL; the reply of an asynchronous call is dropped.
                            if let Err(e) = unsafe { winbridge_gfx_vk::dispatch(session, bytes) } {
                                return Err(fail(writer, format!("async vk call: {e}")));
                            }
                        }
                        BATCH_MEM_UPLOAD => {
                            let up = match MemUpload::decode(bytes) {
                                Ok(u) => u,
                                Err(e) => return Err(fail(writer, format!("bad batched upload: {e}"))),
                            };
                            if let Err(e) = session.mem_upload(up.memory, up.offset, up.data) {
                                return Err(fail(writer, format!("batched upload: {e}")));
                            }
                        }
                        k => return Err(fail(writer, format!("unknown batch item kind {k}"))),
                    }
                }
            }
            OP_SC_CREATE => {
                let req = match ScCreate::decode(&msg.payload) {
                    Ok(r) => r,
                    Err(e) => return Err(fail(writer, format!("bad swapchain create: {e}"))),
                };
                match session.sc_create(&req) {
                    Ok(r) => writer.send(OP_SC_CREATE, 0, seq, &r.as_raw().to_le_bytes())?,
                    Err(e) => return Err(fail(writer, format!("swapchain create: {e}"))),
                }
            }
            OP_SC_DESTROY => {
                let id = match Dec::new(&msg.payload).u64() {
                    Ok(i) => i,
                    Err(e) => return Err(fail(writer, format!("bad swapchain destroy: {e}"))),
                };
                session.sc_destroy(id);
                writer.send(OP_SC_DESTROY, 0, seq, &[])?;
            }
            OP_SC_ACQUIRE => {
                let req = match ScAcquire::decode(&msg.payload) {
                    Ok(r) => r,
                    Err(e) => return Err(fail(writer, format!("bad acquire: {e}"))),
                };
                match session.sc_acquire(&req) {
                    Ok(r) => writer.send(OP_SC_ACQUIRE, 0, seq, &r.as_raw().to_le_bytes())?,
                    Err(e) => return Err(fail(writer, format!("acquire: {e}"))),
                }
            }
            OP_SC_PRESENT => {
                let req = match ScPresent::decode(&msg.payload) {
                    Ok(r) => r,
                    Err(e) => return Err(fail(writer, format!("bad present: {e}"))),
                };
                match session.sc_present(&req) {
                    Ok((r, frame)) => {
                        let mut e = Enc::new();
                        e.i32(r.as_raw());
                        frame.encode_into(&mut e);
                        writer.send(OP_SC_PRESENT, 0, seq, e.as_slice())?;
                    }
                    Err(e) => return Err(fail(writer, format!("present: {e}"))),
                }
            }
            OP_MEM_UPLOAD => {
                let up = match MemUpload::decode(&msg.payload) {
                    Ok(u) => u,
                    Err(e) => return Err(fail(writer, format!("bad upload: {e}"))),
                };
                if let Err(e) = session.mem_upload(up.memory, up.offset, up.data) {
                    return Err(fail(writer, format!("upload: {e}")));
                }
                writer.send(OP_MEM_UPLOAD, 0, seq, &[])?;
            }
            OP_MEM_DOWNLOAD => {
                let rd = match MemRead::decode(&msg.payload) {
                    Ok(r) if r.len <= MEM_CHUNK_MAX => r,
                    Ok(_) => return Err(fail(writer, "download larger than MEM_CHUNK_MAX".into())),
                    Err(e) => return Err(fail(writer, format!("bad download request: {e}"))),
                };
                match session.mem_download(rd.memory, rd.offset, rd.len) {
                    Ok(bytes) => writer.send(OP_MEM_DOWNLOAD, 0, seq, &bytes)?,
                    Err(e) => return Err(fail(writer, format!("download: {e}"))),
                }
            }
            op => return Err(fail(writer, format!("unknown opcode {op:#06x}"))),
        }
    }
}
