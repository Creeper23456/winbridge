// SPDX-License-Identifier: GPL-3.0-or-later

//! Minimal asynchronous QEMU Machine Protocol client.
//!
//! Every command runs on its own short-lived connection: the socket is opened,
//! negotiated, used once and dropped. A shared ordering lock keeps commands from
//! interleaving, so hotplug operations reach QEMU in the order they were issued.

use serde_json::{json, Value};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout, Instant};

/// Upper bound for the greeting and capabilities exchange of one connection attempt.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);
/// Pause between connection attempts while QEMU's socket is missing or not answering.
const RETRY_INTERVAL: Duration = Duration::from_millis(100);
/// No new connection attempt starts once this much time has passed.
const RETRY_DEADLINE: Duration = Duration::from_secs(3);

type Lines = tokio::io::Lines<BufReader<OwnedReadHalf>>;

#[derive(Debug)]
pub enum QmpError {
    /// No QMP session could be established. No command was sent.
    Unavailable(String),
    /// The session broke after the command was written; QEMU may or may not have applied it.
    Transport(String),
    /// QEMU rejected the command. Carries QEMU's own `desc` text.
    Command(String),
}

impl QmpError {
    /// True when QEMU's state is unknown because the session could not be used.
    pub fn is_disconnected(&self) -> bool {
        matches!(self, Self::Unavailable(_) | Self::Transport(_))
    }
}

impl fmt::Display for QmpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) | Self::Transport(message) | Self::Command(message) => {
                f.write_str(message)
            }
        }
    }
}

impl From<QmpError> for String {
    fn from(error: QmpError) -> Self {
        error.to_string()
    }
}

#[derive(Clone)]
pub struct QmpClient {
    path: Arc<PathBuf>,
    ordering: Arc<Mutex<()>>,
}

impl QmpClient {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path: Arc::new(path),
            ordering: Arc::new(Mutex::new(())),
        }
    }

    /// The socket this client talks to.
    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    /// Runs one command on a fresh connection. The ordering lock is held for the whole
    /// connect, command and disconnect cycle.
    pub async fn execute(&self, execute: &str, arguments: Value) -> Result<Value, QmpError> {
        let _ordering = self.ordering.lock().await;
        let mut connection = Connection::open(&self.path).await?;
        connection.execute(execute, arguments).await
        // The connection drops here and closes the socket.
    }
}

struct Connection {
    lines: Lines,
    writer: OwnedWriteHalf,
}

impl Connection {
    /// Opens a session, retrying while QEMU's socket is missing or not answering yet.
    async fn open(path: &Path) -> Result<Self, QmpError> {
        let deadline = Instant::now() + RETRY_DEADLINE;
        loop {
            match Self::handshake(path).await {
                Ok(connection) => return Ok(connection),
                Err(error) if Instant::now() >= deadline => return Err(error),
                Err(_) => sleep(RETRY_INTERVAL).await,
            }
        }
    }

    async fn handshake(path: &Path) -> Result<Self, QmpError> {
        let stream = UnixStream::connect(path).await.map_err(|e| {
            QmpError::Unavailable(format!("Cannot connect to QMP socket {}: {e}", path.display()))
        })?;
        let (read_half, mut writer) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        match timeout(HANDSHAKE_TIMEOUT, negotiate(&mut lines, &mut writer)).await {
            Ok(result) => result?,
            Err(_) => return Err(QmpError::Unavailable("QMP handshake timed out".to_string())),
        }
        Ok(Self { lines, writer })
    }

    async fn execute(&mut self, execute: &str, arguments: Value) -> Result<Value, QmpError> {
        let command = json!({"execute": execute, "arguments": arguments});
        let mut encoded = serde_json::to_vec(&command)
            .map_err(|e| QmpError::Transport(format!("Cannot encode QMP command: {e}")))?;
        encoded.push(b'\n');
        self.writer
            .write_all(&encoded)
            .await
            .map_err(|e| QmpError::Transport(format!("Cannot write QMP command: {e}")))?;

        loop {
            let value = read_json(&mut self.lines).await.map_err(QmpError::Transport)?;
            // Asynchronous events are skipped; this connection carries only one command.
            if value.get("event").is_some() {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(QmpError::Command(describe_error(error)));
            }
            return Ok(value.get("return").cloned().unwrap_or(Value::Null));
        }
    }
}

async fn negotiate(lines: &mut Lines, writer: &mut OwnedWriteHalf) -> Result<(), QmpError> {
    let greeting = read_json(lines).await.map_err(QmpError::Unavailable)?;
    if greeting.get("QMP").is_none() {
        return Err(QmpError::Unavailable("QMP socket did not send a greeting".to_string()));
    }

    writer
        .write_all(b"{\"execute\":\"qmp_capabilities\"}\n")
        .await
        .map_err(|e| QmpError::Unavailable(format!("Cannot negotiate QMP capabilities: {e}")))?;
    loop {
        let value = read_json(lines).await.map_err(QmpError::Unavailable)?;
        if let Some(error) = value.get("error") {
            return Err(QmpError::Unavailable(format!(
                "QMP capabilities negotiation failed: {}",
                describe_error(error)
            )));
        }
        if value.get("return").is_some() {
            return Ok(());
        }
        // Asynchronous events may precede the capabilities response.
    }
}

/// QEMU's `desc` is the human-readable text; fall back to the raw object when it is absent.
fn describe_error(error: &Value) -> String {
    error
        .get("desc")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| error.to_string())
}

async fn read_json(lines: &mut Lines) -> Result<Value, String> {
    let line = lines
        .next_line()
        .await
        .map_err(|e| format!("Cannot read from QMP socket: {e}"))?
        .ok_or_else(|| "QMP disconnected".to_string())?;
    serde_json::from_str(&line).map_err(|e| format!("Invalid QMP message: {e}"))
}

/// True only when QEMU is provably gone: the QMP socket file is gone (QEMU unlinks it on a clean
/// exit), or it refuses connections and no process still holds it open. A process whose
/// descriptors cannot be read and that looks like QEMU counts as a holder, so the answer errs
/// toward "not gone".
pub async fn qemu_confirmed_gone(path: &Path) -> bool {
    match UnixStream::connect(path).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {}
        _ => return false,
    }
    let wanted = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match socket_inode(path).or_else(|| socket_inode(&wanted)) {
        Some(inode) => !socket_held(&inode),
        None => false,
    }
}

/// Inode of the unix socket bound at `path`, from /proc/net/unix.
fn socket_inode(path: &Path) -> Option<String> {
    let table = std::fs::read_to_string("/proc/net/unix").ok()?;
    table.lines().skip(1).find_map(|line| {
        let mut columns = line.split_whitespace();
        let inode = columns.nth(6)?;
        let bound = columns.next()?;
        (Path::new(bound) == path).then(|| inode.to_string())
    })
}

/// True when a process other than this one holds a descriptor for the socket inode.
fn socket_held(inode: &str) -> bool {
    let target = format!("socket:[{inode}]");
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return true;
    };
    for process in processes.flatten() {
        let name = process.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let fds = match std::fs::read_dir(process.path().join("fd")) {
            Ok(fds) => fds,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                if looks_like_qemu(&process.path()) {
                    return true;
                }
                continue;
            }
            Err(_) => continue,
        };
        for fd in fds.flatten() {
            if std::fs::read_link(fd.path()).is_ok_and(|link| link.to_str() == Some(target.as_str())) {
                return true;
            }
        }
    }
    false
}

/// Name check for processes whose descriptors this user cannot read.
fn looks_like_qemu(process: &Path) -> bool {
    std::fs::read_to_string(process.join("comm")).is_ok_and(|name| name.contains("qemu"))
}
