// SPDX-License-Identifier: GPL-3.0-or-later

//! Which programs use the 3D acceleration.
//!
//! The Venus driver (`vulkan_virtio.dll`) opens `\\.\pipe\winbridge-gfx` when a process claims a
//! slot and keeps the handle until the process ends. Nothing is ever read from the pipe: a
//! connected client is a program using the driver, and the end of the connection is the end of
//! its use, however the process dies. The agent takes the identity of the program from the
//! process itself (the pid of the pipe client), never from what a client could write.
//!
//! The list of programs is sent to the host as [`Message::Gfx3dActive`], which the viewer uses to
//! switch its game mode on and off. System programs and WinBridge's own are not counted.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use winbridge_proto::Message;

/// The pipe the driver connects to.
#[cfg_attr(not(windows), allow(dead_code))]
pub const PIPE_NAME: &str = r"\\.\pipe\winbridge-gfx";

/// How often the list is repeated while it is not empty.
const REPEAT: Duration = Duration::from_secs(5);

/// Tells programs of the system from the ones the user runs.
#[derive(Debug)]
pub struct Classifier {
    /// Lower-case directories with a trailing backslash.
    system_roots: Vec<String>,
}

fn normalize(path: &str) -> String {
    path.replace('/', "\\").to_lowercase()
}

impl Classifier {
    /// `roots`: directories whose programs are not counted (the Windows directory, the folder
    /// the agent runs from).
    pub fn new(roots: &[String]) -> Self {
        let system_roots = roots
            .iter()
            .filter(|r| !r.is_empty())
            .map(|r| {
                let mut r = normalize(r);
                if !r.ends_with('\\') {
                    r.push('\\');
                }
                r
            })
            .collect();
        Self { system_roots }
    }

    /// The file name to report for the program at `exe`, or `None` for a system program.
    pub fn app_name(&self, exe: &str) -> Option<String> {
        let path = normalize(exe);
        if self.system_roots.iter().any(|root| path.starts_with(root.as_str())) {
            return None;
        }
        let name = exe.rsplit(['\\', '/']).next().unwrap_or(exe);
        (!name.is_empty()).then(|| name.to_string())
    }
}

#[derive(Default)]
struct State {
    /// One entry per connected client: its connection number and its program's file name.
    clients: BTreeMap<u64, String>,
    next_id: u64,
    /// Counts every change of `clients`, so a waiting sender cannot miss one.
    version: u64,
    closed: bool,
}

impl State {
    /// The names of the programs, sorted, each once.
    fn apps(&self) -> Vec<String> {
        let mut names: Vec<String> = self.clients.values().cloned().collect();
        names.sort();
        names.dedup();
        names
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn connect(&self, name: String) -> u64 {
        let mut st = self.state.lock().unwrap();
        st.next_id += 1;
        let id = st.next_id;
        st.clients.insert(id, name);
        st.version += 1;
        self.changed.notify_all();
        id
    }

    fn disconnect(&self, id: u64) {
        let mut st = self.state.lock().unwrap();
        if st.clients.remove(&id).is_some() {
            st.version += 1;
            self.changed.notify_all();
        }
    }

    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.changed.notify_all();
    }

    /// Sends the list when it changed and, while it is not empty, every `repeat`. Returns when
    /// the watcher is closed.
    fn run_sender(&self, send: &(dyn Fn(Message) -> Result<(), String> + Send + Sync), repeat: Duration) {
        // What the host heard last. Nothing sent yet means the same as an empty list.
        let mut sent: Vec<String> = Vec::new();
        let mut at = Instant::now();
        loop {
            let (apps, version) = {
                let st = self.state.lock().unwrap();
                if st.closed {
                    return;
                }
                (st.apps(), st.version)
            };
            if apps != sent || (!apps.is_empty() && at.elapsed() >= repeat) {
                // A lost message is repaired by the repeat; the viewer also lets a list lapse.
                let _ = send(Message::Gfx3dActive { apps: apps.clone() });
                sent = apps;
                at = Instant::now();
                continue;
            }
            let wait = if sent.is_empty() {
                Duration::from_secs(3600)
            } else {
                repeat.saturating_sub(at.elapsed())
            };
            let st = self.state.lock().unwrap();
            if st.version == version && !st.closed {
                let _ = self.changed.wait_timeout(st, wait).unwrap();
            }
        }
    }
}

/// Keeps the watcher running; dropping it stops the sender.
pub struct GfxWatch {
    shared: Arc<Shared>,
}

impl Drop for GfxWatch {
    fn drop(&mut self) {
        self.shared.close();
    }
}

/// Starts the sender and, on Windows, the pipe server. `send` hands a message to the host.
pub fn start(send: Arc<dyn Fn(Message) -> Result<(), String> + Send + Sync>) -> GfxWatch {
    let shared = Arc::new(Shared::default());
    let for_sender = shared.clone();
    std::thread::Builder::new()
        .name("winbridge-gfx-send".into())
        .spawn(move || for_sender.run_sender(&*send, REPEAT))
        .expect("spawn the 3D activity sender");
    #[cfg(windows)]
    {
        let mut roots = vec![std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())];
        if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
            roots.push(dir.to_string_lossy().into_owned());
        }
        let classifier = Classifier::new(&roots);
        let for_pipe = shared.clone();
        std::thread::Builder::new()
            .name("winbridge-gfx-pipe".into())
            .spawn(move || {
                if let Err(e) = pipe::serve(for_pipe, classifier) {
                    tracing::warn!("3D activity pipe: {}", e);
                }
            })
            .expect("spawn the 3D activity pipe server");
    }
    GfxWatch { shared }
}

#[cfg(windows)]
mod pipe {
    use super::{Classifier, Shared, PIPE_NAME};
    use std::ptr;
    use std::sync::Arc;
    use tracing::{info, warn};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{ReadFile, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Everyone may write (the programs run as the user, some at a low integrity level); only
    /// local clients are accepted. `first` makes creation fail when the name is already taken,
    /// so another process cannot sit on it.
    unsafe fn create_instance(first: bool) -> Result<HANDLE, String> {
        let sddl = wide("D:(A;;GRGW;;;WD)(A;;GA;;;SY)(A;;GA;;;BA)S:(ML;;NW;;;LW)");
        let mut descriptor = ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut descriptor, ptr::null_mut()) == 0 {
            return Err(format!("security descriptor: {}", std::io::Error::last_os_error()));
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let handle = CreateNamedPipeW(
            wide(PIPE_NAME).as_ptr(),
            PIPE_ACCESS_INBOUND | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 },
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            64,
            64,
            0,
            &attributes,
        );
        let error = std::io::Error::last_os_error();
        LocalFree(descriptor);
        if handle == INVALID_HANDLE_VALUE {
            return Err(format!("CreateNamedPipe {}: {}", PIPE_NAME, error));
        }
        Ok(handle)
    }

    /// The path of the program of process `pid`, if it can be read.
    fn image_path(pid: u32) -> Option<String> {
        // SAFETY: plain Win32 calls with valid out pointers; the handle is closed below.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return None;
            }
            let mut buf = vec![0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len) != 0;
            CloseHandle(process);
            ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
        }
    }

    /// Serves one client until it closes its end (or its process ends).
    fn client(pipe: usize, shared: &Shared, classifier: &Classifier) {
        let handle = pipe as HANDLE;
        let mut pid = 0u32;
        // SAFETY: `handle` is a connected pipe instance owned by this thread.
        let exe = unsafe { (GetNamedPipeClientProcessId(handle, &mut pid) != 0).then(|| image_path(pid)).flatten() };
        match exe.as_deref().and_then(|exe| classifier.app_name(exe)) {
            Some(name) => {
                let id = shared.connect(name.clone());
                info!("3D acceleration in use by {} (pid {})", name, pid);
                let mut sink = [0u8; 16];
                loop {
                    let mut got = 0u32;
                    // SAFETY: valid buffer and out pointer, synchronous handle.
                    let ok = unsafe { ReadFile(handle, sink.as_mut_ptr().cast(), sink.len() as u32, &mut got, ptr::null_mut()) };
                    if ok == 0 || got == 0 {
                        break;
                    }
                }
                shared.disconnect(id);
                info!("3D acceleration no longer in use by {} (pid {})", name, pid);
            }
            None => info!("Venus client pid {} ({:?}) is not counted", pid, exe),
        }
        // SAFETY: closing the instance this thread owns.
        unsafe { CloseHandle(handle) };
    }

    /// Accepts clients forever; an error means the pipe could not be created at all.
    pub fn serve(shared: Arc<Shared>, classifier: Classifier) -> Result<(), String> {
        let classifier = Arc::new(classifier);
        // SAFETY: Win32 calls on handles this function owns; each connected instance moves to its
        // own thread, which closes it.
        unsafe {
            let mut pipe = create_instance(true)?;
            loop {
                if ConnectNamedPipe(pipe, ptr::null_mut()) == 0 && GetLastError() != ERROR_PIPE_CONNECTED {
                    warn!("ConnectNamedPipe: {}", std::io::Error::last_os_error());
                    CloseHandle(pipe);
                    pipe = create_instance(false)?;
                    continue;
                }
                let connected = pipe as usize;
                // The next instance exists before the connected one is served.
                pipe = create_instance(false)?;
                let (shared, classifier) = (shared.clone(), classifier.clone());
                let spawned = std::thread::Builder::new()
                    .name("winbridge-gfx-client".into())
                    .spawn(move || client(connected, &shared, &classifier));
                if let Err(e) = spawned {
                    warn!("cannot serve a 3D client: {}", e);
                    CloseHandle(connected as HANDLE);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn classifier() -> Classifier {
        Classifier::new(&[r"C:\Windows".to_string(), r"C:\Program Files\WinBridge\".to_string()])
    }

    #[test]
    fn system_and_own_programs_are_not_counted_but_the_users_are() {
        let c = classifier();
        assert_eq!(c.app_name(r"C:\Windows\System32\dwm.exe"), None);
        assert_eq!(c.app_name(r"c:\WINDOWS\explorer.exe"), None);
        assert_eq!(c.app_name(r"C:\Program Files\WinBridge\winbridge-agent.exe"), None);
        assert_eq!(c.app_name(r"C:\Games\Doom\doom.exe").as_deref(), Some("doom.exe"));
        // A sibling whose name only starts like a system directory is the user's.
        assert_eq!(c.app_name(r"C:\Windows.old\tool.exe").as_deref(), Some("tool.exe"));
        assert_eq!(c.app_name(r"C:\Program Files\WinBridgeGame\g.exe").as_deref(), Some("g.exe"));
    }

    fn collect(repeat: Duration) -> (Arc<Shared>, mpsc::Receiver<Vec<String>>, std::thread::JoinHandle<()>) {
        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let for_thread = shared.clone();
        let handle = std::thread::spawn(move || {
            for_thread.run_sender(
                &move |msg| {
                    if let Message::Gfx3dActive { apps } = msg {
                        let _ = tx.lock().unwrap().send(apps);
                    }
                    Ok(())
                },
                repeat,
            );
        });
        (shared, rx, handle)
    }

    #[test]
    fn the_list_follows_connections_and_repeats_only_while_it_is_not_empty() {
        let (shared, rx, handle) = collect(Duration::from_millis(60));
        let wait = Duration::from_secs(2);

        // Nothing to say before the first program.
        assert!(rx.recv_timeout(Duration::from_millis(150)).is_err());

        let a = shared.connect("b.exe".into());
        assert_eq!(rx.recv_timeout(wait).unwrap(), vec!["b.exe".to_string()]);
        let b = shared.connect("a.exe".into());
        let c = shared.connect("a.exe".into());
        // Sorted, each name once, whatever order the updates arrive in.
        let mut last = rx.recv_timeout(wait).unwrap();
        while last.len() < 2 {
            last = rx.recv_timeout(wait).unwrap();
        }
        assert_eq!(last, vec!["a.exe".to_string(), "b.exe".to_string()]);
        // Repeated while non-empty.
        assert_eq!(rx.recv_timeout(wait).unwrap(), last);

        shared.disconnect(b);
        shared.disconnect(c);
        shared.disconnect(a);
        let mut empty = rx.recv_timeout(wait).unwrap();
        while !empty.is_empty() {
            empty = rx.recv_timeout(wait).unwrap();
        }
        // Said once, then silent.
        assert!(rx.recv_timeout(Duration::from_millis(250)).is_err());

        shared.close();
        handle.join().unwrap();
    }
}
