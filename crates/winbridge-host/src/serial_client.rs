// SPDX-License-Identifier: GPL-3.0-or-later

//! Host-side VirtIO-serial communication client connecting to QEMU chardev socket.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{info, warn};
use winbridge_proto::async_io::{read_frame_async, write_frame_async};
use winbridge_proto::{ClipboardFormat, Message};

const DROP_CHUNK_SIZE: usize = 256 * 1024; // 256 KB file streaming chunks

#[derive(Clone)]
#[allow(dead_code)]
pub struct SerialClientHandle {
    tx: mpsc::Sender<Message>,
    connected: Arc<AtomicBool>,
}

#[allow(dead_code)]
impl SerialClientHandle {
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    pub fn send_resolution(&self, width: u32, height: u32) {
        let msg = Message::SetResolution { width, height };
        let _ = self.tx.try_send(msg);
    }

    pub fn send_dpi_scale(&self, scale_percent: u32) {
        let msg = Message::SetDpiScale { scale_percent };
        let _ = self.tx.try_send(msg);
    }

    pub fn send_clipboard_text(&self, text: String) {
        let msg = Message::ClipboardPayload {
            format: ClipboardFormat::Text,
            data: text.into_bytes(),
        };
        let _ = self.tx.try_send(msg);
    }

    pub async fn send_drop_file(&self, path: PathBuf, transfer_id: u32) -> Result<(), String> {
        let mut file = File::open(&path)
            .await
            .map_err(|e| format!("Failed to open drop file {:?}: {}", path, e))?;

        let metadata = file
            .metadata()
            .await
            .map_err(|e| format!("Failed to query metadata: {}", e))?;

        let total_bytes = metadata.len();
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();

        info!(
            "Streaming dropped file '{}' ({} bytes) to guest",
            filename, total_bytes
        );

        self.tx
            .send(Message::DropFileStart {
                transfer_id,
                filename,
                total_bytes,
            })
            .await
            .map_err(|e| e.to_string())?;

        let mut offset = 0;
        let mut buf = vec![0u8; DROP_CHUNK_SIZE];

        loop {
            let n = file
                .read(&mut buf)
                .await
                .map_err(|e| format!("Read chunk failed: {}", e))?;
            if n == 0 {
                break;
            }

            self.tx
                .send(Message::DropFileChunk {
                    transfer_id,
                    offset,
                    data: buf[0..n].to_vec(),
                })
                .await
                .map_err(|e| e.to_string())?;

            offset += n as u64;
        }

        self.tx
            .send(Message::DropFileFinish {
                transfer_id,
                success: true,
                error: None,
            })
            .await
            .map_err(|e| e.to_string())?;

        Ok(())
    }

    pub fn send_drop_text(&self, text: String) {
        let msg = Message::DropText { text };
        let _ = self.tx.try_send(msg);
    }
}

pub struct SerialClient;

impl SerialClient {
    /// Starts the background communication worker connecting to the VirtIO-serial socket.
    pub fn start<F>(
        socket_path: PathBuf,
        on_message: F,
    ) -> SerialClientHandle
    where
        F: Fn(Message) + Send + Sync + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<Message>(128);
        let connected = Arc::new(AtomicBool::new(false));
        let connected_clone = connected.clone();
        let on_message = Arc::new(on_message);

        tokio::spawn(async move {
            loop {
                info!("Attempting connection to VirtIO-serial socket at {:?}", socket_path);
                match UnixStream::connect(&socket_path).await {
                    Ok(stream) => {
                        info!("Connected to VirtIO-serial socket at {:?}", socket_path);
                        connected_clone.store(true, Ordering::SeqCst);

                        let (mut reader, mut writer) = stream.into_split();
                        // Ask the agent for its state: it only announces itself once at startup.
                        let _ = write_frame_async(&mut writer, &Message::QueryStatus).await;
                        let on_msg_recv = on_message.clone();

                        let mut read_task = tokio::spawn(async move {
                            loop {
                                match read_frame_async(&mut reader).await {
                                    Ok(msg) => {
                                        on_msg_recv(msg);
                                    }
                                    Err(e) => {
                                        warn!("Serial reader disconnected: {}", e);
                                        break;
                                    }
                                }
                            }
                        });

                        loop {
                            tokio::select! {
                                outgoing = rx.recv() => {
                                    match outgoing {
                                        Some(msg) => {
                                            if let Err(e) = write_frame_async(&mut writer, &msg).await {
                                                warn!("Failed to write outgoing message: {}", e);
                                                break;
                                            }
                                        }
                                        None => return, // Handle dropped
                                    }
                                }
                                _ = &mut read_task => {
                                    break;
                                }
                            }
                        }

                        connected_clone.store(false, Ordering::SeqCst);
                        warn!("VirtIO-serial connection closed, reconnecting in 2s...");
                    }
                    Err(e) => {
                        warn!("VirtIO-serial socket connect failed: {}, retrying in 2s...", e);
                    }
                }

                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            }
        });

        SerialClientHandle { tx, connected }
    }
}
