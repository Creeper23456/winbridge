// SPDX-License-Identifier: GPL-3.0-or-later

//! Host-side VirtIO-serial communication client connecting to QEMU chardev socket.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{info, warn};
use winbridge_proto::async_io::{read_frame_async, write_frame_async};
use winbridge_proto::clip::{self, ClipBundle};
use winbridge_proto::ring::{Endpoint, Inbox, LinkConfig, Pop, Routed, REGION_LEN};
use winbridge_proto::Message;
use winbridge_shm::file::{self as shmfile, FileMapping};

use crate::shm::{HostShm, ShmSettings, OFFER_TIMEOUT};
use crate::transfer::{self, Direction, Transfer};

const DROP_CHUNK_SIZE: usize = 256 * 1024; // 256 KB file streaming chunks

/// Outgoing path of the handle: a lane when shared memory is active, else the serial queue.
/// With shared memory off every message takes the serial queue, as before.
#[derive(Clone)]
struct Tx {
    serial: mpsc::Sender<Message>,
    ep: Arc<Endpoint>,
}

impl Tx {
    /// Queues without waiting; fails when the queue is full.
    fn try_send(&self, msg: Message) -> Result<(), String> {
        match self.ep.try_route(msg) {
            Ok(Routed::Sent) => Ok(()),
            Ok(Routed::Serial(m)) => self.serial.try_send(m).map_err(|e| e.to_string()),
            Err(_) => Err("shared-memory queue full".to_string()),
        }
    }

    /// Waits for room (call from a blocking thread, never from async code).
    fn blocking_send(&self, msg: Message) -> Result<(), String> {
        match self.ep.route(msg) {
            Routed::Sent => Ok(()),
            Routed::Serial(m) => self.serial.blocking_send(m).map_err(|e| e.to_string()),
        }
    }

    /// Waits for room without blocking the runtime thread.
    async fn send(&self, msg: Message) -> Result<(), String> {
        let mut msg = msg;
        loop {
            match self.ep.try_route(msg) {
                Ok(Routed::Sent) => return Ok(()),
                Ok(Routed::Serial(m)) => return self.serial.send(m).await.map_err(|e| e.to_string()),
                Err(back) => {
                    msg = back;
                    tokio::time::sleep(Duration::from_micros(250)).await;
                }
            }
        }
    }
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct SerialClientHandle {
    tx: Tx,
    connected: Arc<AtomicBool>,
    /// [`now_ms`] of the last frame received from the agent (any message counts as an answer).
    last_rx_ms: Arc<AtomicU64>,
}

/// Milliseconds since the first call; a cheap monotonic stamp that fits an atomic.
fn now_ms() -> u64 {
    static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
    EPOCH.elapsed().as_millis() as u64
}

#[allow(dead_code)]
impl SerialClientHandle {
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// How long the agent has been silent: no frame at all since this long (or since the
    /// channel connected). The host pings regularly, so a live agent never stays silent.
    pub fn silent_for(&self) -> Duration {
        Duration::from_millis(now_ms().saturating_sub(self.last_rx_ms.load(Ordering::SeqCst)))
    }

    pub fn send_resolution(&self, width: u32, height: u32) {
        let msg = Message::SetResolution { width, height };
        let _ = self.tx.try_send(msg);
    }

    pub fn send_dpi_scale(&self, scale_percent: u32) {
        let msg = Message::SetDpiScale { scale_percent };
        let _ = self.tx.try_send(msg);
    }

    /// Queues one message without waiting; dropped when the queue is full.
    pub fn send_message(&self, msg: Message) {
        let _ = self.tx.try_send(msg);
    }

    /// Sends a clipboard offer and its non-file items from a blocking thread, so large items
    /// never stall the GTK main loop. Files are only listed; see [`Self::send_clip_files`].
    pub fn send_bundle(&self, clip_id: u32, bundle: ClipBundle) {
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            match bundle.send(clip_id, &mut |msg| tx.blocking_send(msg).map_err(|e| e.to_string())) {
                Ok(warnings) => {
                    for warning in warnings {
                        warn!("Clipboard: {}", warning);
                    }
                }
                Err(e) => warn!("Clipboard send failed: {}", e),
            }
        });
    }

    /// Streams the files of a clipboard snapshot the guest asked for (`ClipFilesRequest`), from
    /// a blocking thread so the GTK main loop and the link reader never wait for it. Progress
    /// drives the transfer hub; a cancel or timeout stops the stream and tells the guest.
    pub fn send_clip_files(&self, clip_id: u32, paths: Vec<PathBuf>) {
        let tx = self.tx.clone();
        let label = match paths.len() {
            1 => "Sending 1 file to Windows".to_string(),
            n => format!("Sending {} files to Windows", n),
        };
        let xfer = transfer::begin(label, Direction::ToGuest, 0, None);
        tokio::task::spawn_blocking(move || {
            let result = clip::send_files(
                clip_id,
                &paths,
                &mut |msg| {
                    if !xfer.is_active() {
                        return Err("cancelled".to_string());
                    }
                    tx.blocking_send(msg).map_err(|e| e.to_string())
                },
                |sent, total| {
                    xfer.progress(sent, total);
                },
            );
            match result {
                Ok(()) => {
                    info!("Sent the files of host clipboard snapshot {} to the guest", clip_id);
                    xfer.finish();
                }
                Err(e) => {
                    warn!("Sending the files of clipboard snapshot {} failed: {}", clip_id, e);
                    // Let the guest drop its half-received stream.
                    let _ = tx.blocking_send(Message::FilesEnd {
                        stream_id: clip_id,
                        success: false,
                        error: Some(e.clone()),
                    });
                    xfer.fail(&e);
                }
            }
        });
    }

    /// Streams dropped files to the guest. Progress counts bytes handed to the link queue, which
    /// is bounded, so it paces with the link. `total` is the sum of the file sizes. Returns
    /// `Err` when anything fails or the transfer was cancelled.
    pub async fn send_drop_files(
        &self,
        paths: Vec<PathBuf>,
        transfer_id: u32,
        x: u32,
        y: u32,
        xfer: Transfer,
        total: u64,
    ) -> Result<(), String> {
        self.tx
            .send(Message::DropBegin {
                transfer_id,
                x,
                y,
                file_count: paths.len().try_into().map_err(|_| "Too many dropped files")?,
            })
            .await
            .map_err(|e| e.to_string())?;
        info!(
            "Starting drop operation {} at guest ({}, {}) with {} file(s), {} bytes",
            transfer_id, x, y, paths.len(), total
        );
        let mut sent = 0u64;
        for (index, path) in paths.into_iter().enumerate() {
            let file_id = transfer_id
                .checked_add(index as u32)
                .ok_or_else(|| "Drop transfer ID overflow".to_string())?;
            self.send_drop_file(path, file_id, transfer_id, xfer, &mut sent, total).await?;
        }
        Ok(())
    }

    async fn send_drop_file(
        &self,
        path: PathBuf,
        transfer_id: u32,
        operation_id: u32,
        xfer: Transfer,
        sent: &mut u64,
        total: u64,
    ) -> Result<(), String> {
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
                operation_id,
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
            *sent += n as u64;
            if !xfer.progress(*sent, total) {
                // Cancelled or timed out: tell the guest to drop the partial file.
                let _ = self
                    .tx
                    .send(Message::DropFileFinish {
                        transfer_id,
                        success: false,
                        error: Some("cancelled".to_string()),
                    })
                    .await;
                return Err("cancelled".to_string());
            }
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

    pub fn send_drop_text(&self, text: String, x: u32, y: u32) {
        let msg = Message::DropText { text, x, y };
        let _ = self.tx.try_send(msg);
    }
}

pub struct SerialClient;

/// Queues `m` on the serial path from an async task or a plain thread; never blocks the runtime.
fn serial_send(tx: &mpsc::Sender<Message>, m: Message) {
    match tx.try_send(m) {
        Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
        Err(mpsc::error::TrySendError::Full(m)) => {
            if tokio::runtime::Handle::try_current().is_ok() {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(m).await;
                });
            } else {
                let _ = tx.blocking_send(m);
            }
        }
    }
}

type SharedShm = Arc<Mutex<HostShm<FileMapping>>>;

fn lock_shm(shm: &SharedShm) -> std::sync::MutexGuard<'_, HostShm<FileMapping>> {
    shm.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One message from the serial channel: negotiation first, then the dispatch queue. Transport
/// messages (`ShmKick`, `ShmFallback`) never wait behind the queue.
async fn incoming(ep: &Arc<Endpoint>, shm: &SharedShm, msg: Message) {
    if let Message::StatusReply { os_description, .. } = &msg {
        lock_shm(shm).on_status(os_description);
    }
    let answer = match ep.try_on_serial(msg) {
        Ok(answer) => answer,
        Err(msg) => {
            // The serial queue is full: wait for room on a blocking thread, in order.
            let ep = ep.clone();
            tokio::task::spawn_blocking(move || ep.on_serial(msg)).await.unwrap_or(None)
        }
    };
    match answer {
        Some(Message::ShmReady { ok, reason }) => lock_shm(shm).on_ready(ok, &reason),
        Some(other) => warn!("Unexpected message from the agent during negotiation: {:?}", other),
        None => {}
    }
}

impl SerialClient {
    /// Starts the background communication worker connecting to the VirtIO-serial socket.
    /// `on_message` runs on one dispatcher thread (inside the Tokio runtime context), fed by the
    /// serial reader and, when shared memory is active, by the lane readers. Needs a Tokio
    /// runtime context.
    pub fn start<F>(
        socket_path: PathBuf,
        shm_settings: ShmSettings,
        on_message: F,
    ) -> SerialClientHandle
    where
        F: Fn(Message) + Send + Sync + 'static,
    {
        let (serial_tx, mut rx) = mpsc::channel::<Message>(128);
        let connected = Arc::new(AtomicBool::new(false));
        let connected_clone = connected.clone();
        let last_rx_ms = Arc::new(AtomicU64::new(now_ms()));
        let last_rx_clone = last_rx_ms.clone();

        let inbox = Inbox::new(256, 1024, 8);
        let ep = Endpoint::new(
            {
                let tx = serial_tx.clone();
                Arc::new(move |m| serial_send(&tx, m))
            },
            inbox.clone(),
            LinkConfig::default(),
        );
        let path = shm_settings.path.clone();
        let shm: SharedShm = Arc::new(Mutex::new(HostShm::new(
            shm_settings.enabled,
            ep.clone(),
            Box::new(move || shmfile::open(&path, REGION_LEN)),
            OFFER_TIMEOUT,
        )));
        if shm_settings.enabled {
            info!("Shared-memory transport enabled (backing file {:?})", shm_settings.path);
        }

        // The single dispatcher: what the serial reader used to do inline.
        let runtime = tokio::runtime::Handle::current();
        let spawned = std::thread::Builder::new().name("wb-dispatch".into()).spawn(move || {
            let _runtime = runtime.enter();
            loop {
                match inbox.pop(Duration::from_secs(1)) {
                    Pop::Msg(msg) => on_message(msg),
                    Pop::Timeout => {}
                    Pop::Closed => break,
                }
            }
        });
        if let Err(e) = spawned {
            warn!("Cannot start the dispatcher thread: {}", e);
        }

        let shm_tick = shm.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(250));
            loop {
                interval.tick().await;
                lock_shm(&shm_tick).tick(Instant::now());
            }
        });

        let ep_loop = ep.clone();
        tokio::spawn(async move {
            let ep = ep_loop;
            loop {
                info!("Attempting connection to VirtIO-serial socket at {:?}", socket_path);
                match UnixStream::connect(&socket_path).await {
                    Ok(stream) => {
                        info!("Connected to VirtIO-serial socket at {:?}", socket_path);
                        connected_clone.store(true, Ordering::SeqCst);
                        last_rx_clone.store(now_ms(), Ordering::SeqCst);
                        // A new serial session: any earlier shared-memory link is left and the
                        // agent's announcement in the reply below starts a new negotiation.
                        lock_shm(&shm).new_session();

                        let (mut reader, mut writer) = stream.into_split();
                        // Ask the agent for its state: it only announces itself once at startup.
                        let _ = write_frame_async(&mut writer, &Message::QueryStatus).await;
                        let last_rx_reader = last_rx_clone.clone();
                        let ep_reader = ep.clone();
                        let shm_reader = shm.clone();

                        let mut read_task = tokio::spawn(async move {
                            loop {
                                match read_frame_async(&mut reader).await {
                                    Ok(msg) => {
                                        last_rx_reader.store(now_ms(), Ordering::SeqCst);
                                        incoming(&ep_reader, &shm_reader, msg).await;
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
                        lock_shm(&shm).new_session();
                        warn!("VirtIO-serial connection closed, reconnecting in 2s...");
                    }
                    Err(e) => {
                        warn!("VirtIO-serial socket connect failed: {}, retrying in 2s...", e);
                    }
                }

                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            }
        });

        SerialClientHandle {
            tx: Tx { serial: serial_tx, ep },
            connected,
            last_rx_ms,
        }
    }
}
