// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared communication protocol for WinBridge.
//!
//! Provides message structures, clipboard representations, and length-prefixed
//! framing routines used across the Linux host viewer and the Windows guest agent.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, Read, Write};

/// One offered file or directory (see [`Message::ClipFilesManifest`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// `/`-separated, starting with the top-level name; the same value `FileEntry` carries.
    pub rel_path: String,
    pub is_dir: bool,
    pub size: u64,
}

pub mod clip;
pub mod files;
pub mod caps;
pub mod ring;

/// Wire framing constants.
pub const MAGIC: [u8; 4] = *b"WB01";
pub const HEADER_LEN: usize = 8; // 4 bytes magic + 4 bytes payload len
pub const MAX_PAYLOAD_SIZE: usize = 64 * 1024 * 1024; // 64 MB maximum payload

/// Supported clipboard data formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClipboardFormat {
    /// UTF-8 plain text.
    Text,
    /// PNG formatted image data.
    Png,
    /// Windows DIB / BMP formatted image data.
    Bmp,
    /// Files and directories, carried by a file stream.
    FileList,
    /// HTML document (UTF-8), without the Windows CF_HTML header.
    Html,
    /// Rich Text Format bytes.
    Rtf,
}

impl fmt::Display for ClipboardFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text => write!(f, "Text"),
            Self::Png => write!(f, "Png"),
            Self::Bmp => write!(f, "Bmp"),
            Self::FileList => write!(f, "FileList"),
            Self::Html => write!(f, "Html"),
            Self::Rtf => write!(f, "Rtf"),
        }
    }
}

/// Core protocol messages exchanged between WinBridge host and guest agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// Keepalive ping request.
    Ping {
        seq: u64,
        timestamp_ms: u64,
    },
    /// Keepalive pong response.
    Pong {
        seq: u64,
        timestamp_ms: u64,
    },

    /// Host requests guest display resolution update.
    SetResolution {
        width: u32,
        height: u32,
    },
    /// Guest returns result of resolution update attempt.
    ResolutionResult {
        width: u32,
        height: u32,
        success: bool,
        error: Option<String>,
    },

    /// Host requests guest display DPI scaling percentage (e.g. 100, 125, 150, 200).
    SetDpiScale {
        scale_percent: u32,
    },
    /// Guest returns result of DPI scaling update attempt.
    DpiScaleResult {
        scale_percent: u32,
        success: bool,
        error: Option<String>,
    },

    /// Host requests current guest status.
    QueryStatus,
    /// Guest reports active status, resolution, and DPI settings.
    StatusReply {
        current_width: u32,
        current_height: u32,
        current_dpi: u32,
        os_description: String,
    },


    /// Start one drag-and-drop operation at guest screen coordinates.
    DropBegin {
        transfer_id: u32,
        x: u32,
        y: u32,
        file_count: u32,
    },
    /// Initiate drag-and-drop file transfer into guest.
    DropFileStart {
        transfer_id: u32,
        operation_id: u32,
        filename: String,
        total_bytes: u64,
    },
    /// Stream chunk of dropped file.
    DropFileChunk {
        transfer_id: u32,
        offset: u64,
        data: Vec<u8>,
    },
    /// Finalize drag-and-drop file transfer.
    DropFileFinish {
        transfer_id: u32,
        success: bool,
        error: Option<String>,
    },
    /// Send dropped plain text at guest screen coordinates.
    DropText {
        text: String,
        x: u32,
        y: u32,
    },
    /// Acknowledge drag-and-drop operation.
    DropResult {
        success: bool,
        message: String,
    },

    /// Offers one clipboard snapshot. Every format except `FileList` follows as a
    /// `ClipboardItem`. `FileList` means files were copied: they are NOT streamed with the offer,
    /// the pasting side fetches them with `ClipFilesRequest` at the moment of the paste.
    ClipboardOffer {
        clip_id: u32,
        formats: Vec<ClipboardFormat>,
    },
    /// One non-file clipboard format of the snapshot `clip_id`.
    ClipboardItem {
        clip_id: u32,
        format: ClipboardFormat,
        data: Vec<u8>,
    },

    /// Starts a file stream: `entries` entries (files and directories) follow.
    FilesBegin {
        stream_id: u32,
        entries: u32,
        total_bytes: u64,
    },
    /// Announces one entry. `rel_path` uses `/` separators and starts with a top-level name.
    FileEntry {
        stream_id: u32,
        index: u32,
        rel_path: String,
        is_dir: bool,
        size: u64,
    },
    /// File content of entry `index`, in order.
    FileChunk {
        stream_id: u32,
        index: u32,
        offset: u64,
        data: Vec<u8>,
    },
    /// Ends a file stream.
    FilesEnd {
        stream_id: u32,
        success: bool,
        error: Option<String>,
    },

    /// Host -> guest: the pointer left the canvas with the left button held; is a guest
    /// drag-and-drop running at these guest pixels?
    GuestDragProbe {
        x: u32,
        y: u32,
    },
    /// Guest -> host: a drag started in the guest. `names` are the top-level file names (empty for
    /// a text drag), `text` the dragged text if any.
    GuestDragOffer {
        drag_id: u32,
        names: Vec<String>,
        total_bytes: u64,
        text: Option<String>,
    },
    /// Guest -> host: no drag was found.
    GuestDragNone,
    /// Host -> guest: a Linux application asked for the dragged files; stream them as a file
    /// stream with `stream_id == drag_id`.
    GuestDragFetch {
        drag_id: u32,
    },
    /// Host -> guest: the drag is over (dropped or cancelled).
    GuestDragEnd {
        drag_id: u32,
    },
    /// Either direction: the user pasted files from the snapshot `clip_id` this side was offered.
    /// The offering side answers with a file stream (`FilesBegin`...`FilesEnd`) whose
    /// `stream_id` is `clip_id`.
    ClipFilesRequest {
        clip_id: u32,
    },
    /// Sender -> receiver, right after a `ClipboardOffer` that lists `FileList`: names and sizes
    /// of the offered entries in stream order, so the receiver can present the files (virtual
    /// files) without transferring anything. The data still only follows a `ClipFilesRequest`.
    ClipFilesManifest {
        clip_id: u32,
        entries: Vec<ManifestEntry>,
    },
    /// Host -> agent, over the serial channel, after the normal handshake: the host created a
    /// shared-memory region (`docs/IVSHMEM-TRANSPORT.md`) and offers to move the control and bulk
    /// lanes onto it. `shm_version` is the region layout version, `region_len` the length of the
    /// BAR the guest must see. The host keeps using the serial channel until `ShmReady` arrives.
    ShmOffer {
        shm_version: u32,
        region_len: u64,
    },
    /// Agent -> host: answer to `ShmOffer`. `ok` is true only when the guest mapped the device
    /// (cached), hardened its access control and attached the region header successfully; on false
    /// `reason` is a short human-readable cause and the serial channel stays in use.
    ShmReady {
        ok: bool,
        reason: String,
    },
    /// Either side -> other, over the serial channel: wake the peer's ring consumer (one per
    /// wake-up; the consumer published its sleeping flag). Carries no data.
    ShmKick,
    /// Either side -> other, over the serial channel, as the last message before leaving
    /// the rings (shutdown, error): both sides return to the serial transport. `reason` is for logs.
    ShmFallback {
        reason: String,
    },
}

impl Message {
    /// Lane choice for the shared-memory transport (`docs/IVSHMEM-TRANSPORT.md`): true for the
    /// messages that travel on the **bulk** lane. These are the file-data streams (clipboard and
    /// drag-and-drop file streams), the clipboard snapshot (`ClipboardOffer`, `ClipFilesManifest`
    /// and `ClipboardItem`, whose items may be tens of MiB and must stay ordered with each other)
    /// and the drop stream `DropBegin` .. `DropFileFinish`. Everything else is a **control**
    /// message. Messages of one stream always share a lane, so their order is kept; the order
    /// between a control message and a bulk message is **not** guaranteed on the shared-memory
    /// transport and no code may rely on it. The four `Shm*` messages never travel on a lane
    /// (they are serial-only, see [`Message::is_shm_control`]) and are reported as not bulk.
    pub fn is_bulk(&self) -> bool {
        matches!(
            self,
            Message::ClipboardOffer { .. }
                | Message::ClipboardItem { .. }
                | Message::ClipFilesManifest { .. }
                | Message::FilesBegin { .. }
                | Message::FileEntry { .. }
                | Message::FileChunk { .. }
                | Message::FilesEnd { .. }
                | Message::DropBegin { .. }
                | Message::DropFileStart { .. }
                | Message::DropFileChunk { .. }
                | Message::DropFileFinish { .. }
        )
    }

    /// True for the shared-memory negotiation messages, which always use the serial channel.
    pub fn is_shm_control(&self) -> bool {
        matches!(
            self,
            Message::ShmOffer { .. }
                | Message::ShmReady { .. }
                | Message::ShmKick
                | Message::ShmFallback { .. }
        )
    }
}

/// Errors occurring during protocol framing or serialization.
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] bincode::Error),

    #[error("Invalid magic: expected {expected:?}, got {actual:?}")]
    InvalidMagic { expected: [u8; 4], actual: [u8; 4] },

    #[error("Payload size {size} exceeds maximum allowable {max}")]
    PayloadTooLarge { size: usize, max: usize },

    #[error("Unexpected end of stream")]
    UnexpectedEof,
}

/// Encodes a message into a complete wire frame (magic + 4-byte length + payload).
pub fn encode_frame(msg: &Message) -> Result<Vec<u8>, ProtoError> {
    let payload = bincode::serialize(msg)?;
    let payload_len = payload.len();
    if payload_len > MAX_PAYLOAD_SIZE {
        return Err(ProtoError::PayloadTooLarge {
            size: payload_len,
            max: MAX_PAYLOAD_SIZE,
        });
    }

    let mut frame = Vec::with_capacity(HEADER_LEN + payload_len);
    frame.extend_from_slice(&MAGIC);
    frame.extend_from_slice(&(payload_len as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Writes a framed message to a synchronous writer.
pub fn write_frame<W: Write>(writer: &mut W, msg: &Message) -> Result<(), ProtoError> {
    let frame = encode_frame(msg)?;
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}

/// Reads a framed message from a synchronous reader.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Message, ProtoError> {
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header)?;

    let mut magic = [0u8; 4];
    magic.copy_from_slice(&header[0..4]);
    if magic != MAGIC {
        return Err(ProtoError::InvalidMagic {
            expected: MAGIC,
            actual: magic,
        });
    }

    let payload_len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if payload_len > MAX_PAYLOAD_SIZE {
        return Err(ProtoError::PayloadTooLarge {
            size: payload_len,
            max: MAX_PAYLOAD_SIZE,
        });
    }

    let mut payload = vec![0u8; payload_len];
    reader.read_exact(&mut payload)?;

    let msg = bincode::deserialize(&payload)?;
    Ok(msg)
}

/// Decodes one complete wire frame held in `frame` (as written by [`encode_frame`]). The frame
/// must fill the slice exactly: a record on a shared-memory lane carries exactly one frame.
pub fn decode_frame(frame: &[u8]) -> Result<Message, ProtoError> {
    if frame.len() < HEADER_LEN {
        return Err(ProtoError::UnexpectedEof);
    }
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&frame[0..4]);
    if magic != MAGIC {
        return Err(ProtoError::InvalidMagic {
            expected: MAGIC,
            actual: magic,
        });
    }
    let payload_len = u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]) as usize;
    if payload_len > MAX_PAYLOAD_SIZE {
        return Err(ProtoError::PayloadTooLarge {
            size: payload_len,
            max: MAX_PAYLOAD_SIZE,
        });
    }
    if frame.len() != HEADER_LEN + payload_len {
        return Err(ProtoError::UnexpectedEof);
    }
    Ok(bincode::deserialize(&frame[HEADER_LEN..])?)
}

#[cfg(feature = "tokio")]
pub mod async_io {
    use super::*;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    /// Writes a framed message to an asynchronous writer.
    pub async fn write_frame_async<W: AsyncWrite + Unpin>(
        writer: &mut W,
        msg: &Message,
    ) -> Result<(), ProtoError> {
        let frame = encode_frame(msg)?;
        writer.write_all(&frame).await?;
        writer.flush().await?;
        Ok(())
    }

    /// Reads a framed message from an asynchronous reader.
    pub async fn read_frame_async<R: AsyncRead + Unpin>(
        reader: &mut R,
    ) -> Result<Message, ProtoError> {
        let mut header = [0u8; HEADER_LEN];
        reader.read_exact(&mut header).await?;

        let mut magic = [0u8; 4];
        magic.copy_from_slice(&header[0..4]);
        if magic != MAGIC {
            return Err(ProtoError::InvalidMagic {
                expected: MAGIC,
                actual: magic,
            });
        }

        let payload_len =
            u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if payload_len > MAX_PAYLOAD_SIZE {
            return Err(ProtoError::PayloadTooLarge {
                size: payload_len,
                max: MAX_PAYLOAD_SIZE,
            });
        }

        let mut payload = vec![0u8; payload_len];
        reader.read_exact(&mut payload).await?;

        let msg = bincode::deserialize(&payload)?;
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_roundtrip_all_message_variants() {
        let messages = vec![
            Message::Ping {
                seq: 42,
                timestamp_ms: 123456789,
            },
            Message::Pong {
                seq: 42,
                timestamp_ms: 123456789,
            },
            Message::SetResolution {
                width: 1920,
                height: 1080,
            },
            Message::ResolutionResult {
                width: 1920,
                height: 1080,
                success: true,
                error: None,
            },
            Message::ResolutionResult {
                width: 3840,
                height: 2160,
                success: false,
                error: Some("Mode not supported".to_string()),
            },
            Message::SetDpiScale {
                scale_percent: 150,
            },
            Message::DpiScaleResult {
                scale_percent: 150,
                success: true,
                error: None,
            },
            Message::QueryStatus,
            Message::StatusReply {
                current_width: 2560,
                current_height: 1440,
                current_dpi: 125,
                os_description: "Windows 11 Pro 23H2".to_string(),
            },
            Message::ClipboardOffer {
                clip_id: 7,
                formats: vec![ClipboardFormat::Text, ClipboardFormat::Html, ClipboardFormat::FileList],
            },
            Message::ClipboardItem {
                clip_id: 7,
                format: ClipboardFormat::Text,
                data: "Hello from Linux host!".as_bytes().to_vec(),
            },
            Message::FilesBegin { stream_id: 7, entries: 2, total_bytes: 4 },
            Message::FileEntry { stream_id: 7, index: 0, rel_path: "dir".into(), is_dir: true, size: 0 },
            Message::FileEntry { stream_id: 7, index: 1, rel_path: "dir/a.bin".into(), is_dir: false, size: 4 },
            Message::FileChunk { stream_id: 7, index: 1, offset: 0, data: vec![1, 2, 3, 4] },
            Message::FilesEnd { stream_id: 7, success: true, error: None },
            Message::GuestDragProbe { x: 1900, y: 400 },
            Message::GuestDragOffer {
                drag_id: 9,
                names: vec!["a.txt".into()],
                total_bytes: 12,
                text: Some("a.txt".into()),
            },
            Message::GuestDragNone,
            Message::GuestDragFetch { drag_id: 9 },
            Message::GuestDragEnd { drag_id: 9 },
            Message::ClipFilesRequest { clip_id: 3 },
            Message::ClipFilesManifest {
                clip_id: 3,
                entries: vec![
                    ManifestEntry { rel_path: "dir".into(), is_dir: true, size: 0 },
                    ManifestEntry { rel_path: "dir/a.bin".into(), is_dir: false, size: 4 },
                ],
            },
            Message::DropBegin {
                transfer_id: 1,
                x: 640,
                y: 360,
                file_count: 1,
            },
            Message::DropFileStart {
                transfer_id: 1,
                operation_id: 1,
                filename: "document.pdf".to_string(),
                total_bytes: 1024,
            },
            Message::DropFileChunk {
                transfer_id: 1,
                offset: 0,
                data: vec![0xDE, 0xAD, 0xBE, 0xEF],
            },
            Message::DropFileFinish {
                transfer_id: 1,
                success: true,
                error: None,
            },
            Message::DropText {
                text: "Pasted text from host".to_string(),
                x: 640,
                y: 360,
            },
            Message::DropResult {
                success: true,
                message: "File written successfully".to_string(),
            },
            Message::ShmOffer {
                shm_version: 1,
                region_len: 256 << 20,
            },
            Message::ShmReady {
                ok: false,
                reason: "no ivshmem device".to_string(),
            },
            Message::ShmKick,
            Message::ShmFallback {
                reason: "lane corrupt".to_string(),
            },
        ];

        for msg in messages {
            let mut buf = Vec::new();
            write_frame(&mut buf, &msg).expect("write_frame failed");

            let mut cursor = Cursor::new(buf);
            let decoded = read_frame(&mut cursor).expect("read_frame failed");
            assert_eq!(msg, decoded);
        }
    }

    #[test]
    fn lane_choice_keeps_streams_together() {
        let bulk = [
            Message::FilesBegin { stream_id: 1, entries: 1, total_bytes: 1 },
            Message::FileChunk { stream_id: 1, index: 0, offset: 0, data: vec![0] },
            Message::ClipboardOffer { clip_id: 1, formats: vec![] },
            Message::DropFileChunk { transfer_id: 1, offset: 0, data: vec![] },
        ];
        assert!(bulk.iter().all(Message::is_bulk));
        let control = [
            Message::Ping { seq: 1, timestamp_ms: 1 },
            Message::QueryStatus,
            Message::ClipFilesRequest { clip_id: 1 },
            Message::DropResult { success: true, message: String::new() },
            Message::ShmKick,
        ];
        assert!(control.iter().all(|m| !m.is_bulk()));
    }

    #[test]
    fn decode_frame_is_exact() {
        let frame = encode_frame(&Message::QueryStatus).unwrap();
        assert_eq!(decode_frame(&frame).unwrap(), Message::QueryStatus);
        let mut longer = frame.clone();
        longer.push(0);
        assert!(decode_frame(&longer).is_err());
        assert!(decode_frame(&frame[..frame.len() - 1]).is_err());
        assert!(decode_frame(&frame[..4]).is_err());
    }

    /// `bincode` is positional: existing variants keep their index, the shared-memory
    /// negotiation messages sit at the end, and serial frames are byte-identical to before.
    #[test]
    fn wire_positions_are_stable() {
        let index = |m: &Message| {
            let f = encode_frame(m).unwrap();
            u32::from_le_bytes([f[HEADER_LEN], f[HEADER_LEN + 1], f[HEADER_LEN + 2], f[HEADER_LEN + 3]])
        };
        assert_eq!(index(&Message::Ping { seq: 0, timestamp_ms: 0 }), 0);
        assert_eq!(index(&Message::QueryStatus), 6);
        assert_eq!(index(&Message::ClipFilesManifest { clip_id: 0, entries: vec![] }), 26);
        assert_eq!(index(&Message::ShmOffer { shm_version: 0, region_len: 0 }), 27);
        assert_eq!(index(&Message::ShmReady { ok: true, reason: String::new() }), 28);
        assert_eq!(index(&Message::ShmKick), 29);
        assert_eq!(index(&Message::ShmFallback { reason: String::new() }), 30);
        let ping = encode_frame(&Message::Ping { seq: 1, timestamp_ms: 2 }).unwrap();
        let mut want = b"WB01".to_vec();
        want.extend_from_slice(&20u32.to_le_bytes());
        want.extend_from_slice(&0u32.to_le_bytes());
        want.extend_from_slice(&1u64.to_le_bytes());
        want.extend_from_slice(&2u64.to_le_bytes());
        assert_eq!(ping, want);
    }

    #[test]
    fn test_corrupt_magic() {
        let mut frame = encode_frame(&Message::QueryStatus).unwrap();
        frame[0] = b'X'; // Corrupt magic

        let mut cursor = Cursor::new(frame);
        let res = read_frame(&mut cursor);
        match res {
            Err(ProtoError::InvalidMagic { .. }) => {}
            other => panic!("Expected InvalidMagic, got {:?}", other),
        }
    }

    #[test]
    fn test_stream_concatenation() {
        let msg1 = Message::Ping {
            seq: 1,
            timestamp_ms: 100,
        };
        let msg2 = Message::SetResolution {
            width: 1280,
            height: 720,
        };
        let msg3 = Message::DropText {
            text: "Hello".into(),
            x: 100,
            y: 200,
        };

        let mut stream = Vec::new();
        write_frame(&mut stream, &msg1).unwrap();
        write_frame(&mut stream, &msg2).unwrap();
        write_frame(&mut stream, &msg3).unwrap();

        let mut cursor = Cursor::new(stream);
        assert_eq!(read_frame(&mut cursor).unwrap(), msg1);
        assert_eq!(read_frame(&mut cursor).unwrap(), msg2);
        assert_eq!(read_frame(&mut cursor).unwrap(), msg3);
    }
}
