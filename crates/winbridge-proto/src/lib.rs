// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared communication protocol for WinBridge.
//!
//! Provides message structures, clipboard representations, and length-prefixed
//! framing routines used across the Linux host viewer and the Windows guest agent.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, Read, Write};

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
    /// List of file names/paths (null or newline separated).
    FileList,
}

impl fmt::Display for ClipboardFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text => write!(f, "Text"),
            Self::Png => write!(f, "Png"),
            Self::Bmp => write!(f, "Bmp"),
            Self::FileList => write!(f, "FileList"),
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

    /// Either party announces clipboard formats are currently available.
    ClipboardAnnounce {
        formats: Vec<ClipboardFormat>,
    },
    /// Request specific clipboard format data.
    ClipboardRequest {
        format: ClipboardFormat,
    },
    /// Send clipboard format data payload.
    ClipboardPayload {
        format: ClipboardFormat,
        data: Vec<u8>,
    },

    /// Initiate drag-and-drop file transfer into guest.
    DropFileStart {
        transfer_id: u32,
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
    /// Send dropped plain text.
    DropText {
        text: String,
    },
    /// Acknowledge drag-and-drop operation.
    DropResult {
        success: bool,
        message: String,
    },
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
            Message::ClipboardAnnounce {
                formats: vec![ClipboardFormat::Text, ClipboardFormat::Png],
            },
            Message::ClipboardRequest {
                format: ClipboardFormat::Text,
            },
            Message::ClipboardPayload {
                format: ClipboardFormat::Text,
                data: "Hello from Linux host!".as_bytes().to_vec(),
            },
            Message::DropFileStart {
                transfer_id: 1,
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
            },
            Message::DropResult {
                success: true,
                message: "File written successfully".to_string(),
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
