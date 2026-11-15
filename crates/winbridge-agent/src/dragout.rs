// SPDX-License-Identifier: GPL-3.0-or-later

//! Drags that start in Windows and end on the Linux host: the agent's side.
//!
//! The host reports that the pointer left the canvas with the left button held
//! (`GuestDragProbe`). An OLE drag-and-drop running in the guest keeps asking the window under
//! the cursor whether it accepts the drop, so a nearly invisible window with an `IDropTarget`
//! has to be there for a moment. That window cannot live in the agent: the agent is elevated,
//! the drag source (Explorer) is not, and Windows refuses OLE drags into a higher-integrity
//! window. So the window lives in a helper process (`dragprobe`) that this controller starts at
//! medium integrity (`medium`), talks to over a private pipe (`dragipc`), restarts when it dies
//! (with a back-off), and that is killed with the agent.
//!
//! The helper reports the paths and text a drag carries (`GuestDragOffer` goes to the host from
//! here). The files themselves are only streamed when the host asks (`GuestDragFetch`), i.e. when
//! a Linux application actually accepts the drop, and only paths of the drag that were offered
//! (absolute and existing). Only files that exist on disk (`CF_HDROP`) and text can be taken this
//! way; virtual files (archives, e-mail attachments, browser downloads) have no path to read.

use winbridge_proto::Message;

/// Sends one message to the host.
pub type Sink = std::sync::Arc<dyn Fn(Message) -> Result<(), String> + Send + Sync>;

#[cfg(windows)]
pub use imp::{self_test, DragOut};

#[cfg(windows)]
mod imp {
    use super::Sink;
    use crate::clipboard::next_id;
    use crate::dragipc::{read_message, valid_offer_paths, write_message, Backoff, FromHelper, ToHelper};
    use crate::medium::{self, Helper, MEDIUM_RID};
    use std::fs::File;
    use std::os::windows::io::FromRawHandle;
    use std::path::PathBuf;
    use std::ptr;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tracing::{info, warn};
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess};
    use winbridge_proto::files::{self, FileEntryInfo};
    use winbridge_proto::Message;

    /// Largest drag that is offered.
    const DRAG_CAP_BYTES: u64 = 8 * 1024 * 1024 * 1024;
    /// How long a freshly started helper may take to come up.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
    const PROCESS_TERMINATE: u32 = 0x1;

    struct Offer {
        id: u32,
        paths: Vec<PathBuf>,
    }

    struct Shared {
        sink: Sink,
        /// The last offered drag; kept after the drag ends because the host fetches then.
        offer: Mutex<Option<Offer>>,
        /// Process id and integrity level of the connected helper.
        status: Mutex<Option<(u32, u32)>>,
        /// The helper reported `Ready` on the current pipe.
        connected: AtomicBool,
        /// Counts helper sessions, so a stale reader's events are ignored.
        session: AtomicU64,
    }

    impl Shared {
        fn send(&self, msg: Message) {
            let kind = std::mem::discriminant(&msg);
            if let Err(e) = (self.sink)(msg) {
                warn!("Sending {:?} failed: {}", kind, e);
            }
        }
    }

    pub struct DragOut {
        shared: Arc<Shared>,
        commands: Sender<ToHelper>,
    }

    impl DragOut {
        /// Starts the helper supervisor.
        pub fn start(sink: Sink) -> Self {
            let shared = Arc::new(Shared {
                sink,
                offer: Mutex::new(None),
                status: Mutex::new(None),
                connected: AtomicBool::new(false),
                session: AtomicU64::new(0),
            });
            let (commands, queue) = std::sync::mpsc::channel();
            let supervised = shared.clone();
            std::thread::Builder::new()
                .name("winbridge-drag-supervisor".into())
                .spawn(move || supervise(supervised, queue))
                .ok();
            Self { shared, commands }
        }

        /// The connected helper's process id and integrity RID.
        pub fn helper_info(&self) -> Option<(u32, u32)> {
            *self.shared.status.lock().unwrap()
        }

        /// The host's pointer left the canvas with the button held at guest pixels `x`, `y`.
        pub fn probe(&self, x: u32, y: u32) {
            if !self.shared.connected.load(Ordering::SeqCst) {
                // Nobody can catch a drag: tell the host at once instead of leaving it waiting.
                warn!("Guest drag probe at ({}, {}) but the drag probe helper is not running", x, y);
                self.shared.send(Message::GuestDragNone);
                return;
            }
            info!("Forwarding guest drag probe ({}, {}) to the helper", x, y);
            let _ = self.commands.send(ToHelper::Probe { x, y });
        }

        /// The host's drag is over (dropped or cancelled).
        pub fn end(&self) {
            if self.shared.connected.load(Ordering::SeqCst) {
                info!("Forwarding the end of the guest drag to the helper");
                let _ = self.commands.send(ToHelper::End);
            }
        }

        /// Streams the files of drag `drag_id` to the host (a Linux application accepted the drop).
        pub fn fetch(&self, drag_id: u32) {
            let shared = self.shared.clone();
            std::thread::Builder::new()
                .name("winbridge-drag-fetch".into())
                .spawn(move || {
                    info!("Host fetches guest drag {}", drag_id);
                    let paths = {
                        let offer = shared.offer.lock().unwrap();
                        offer.as_ref().filter(|o| o.id == drag_id).map(|o| o.paths.clone())
                    };
                    let entries = paths
                        .ok_or_else(|| "the drag is no longer available".to_string())
                        .and_then(|paths| files::scan(&paths, DRAG_CAP_BYTES));
                    let result = entries.and_then(|entries: Vec<FileEntryInfo>| {
                        info!("Guest drag {}: streaming {} dragged entries to the host", drag_id, entries.len());
                        files::send_stream(drag_id, &entries, &mut |msg| (shared.sink)(msg))
                    });
                    if let Err(e) = result {
                        warn!("Dragged files could not be sent: {}", e);
                        // A stream that never began still needs an ending for the host to give up.
                        shared.send(Message::FilesEnd { stream_id: drag_id, success: false, error: Some(e) });
                    }
                })
                .ok();
        }
    }

    /// Keeps one helper alive: start, serve, and on any failure restart with a back-off.
    fn supervise(shared: Arc<Shared>, queue: Receiver<ToHelper>) {
        let mut backoff = Backoff::new();
        loop {
            let started = Instant::now();
            match run_session(&shared, &queue) {
                Ok(()) => info!("Drag probe helper session ended"),
                Err(e) => warn!("Drag probe helper session failed: {}", e),
            }
            shared.connected.store(false, Ordering::SeqCst);
            *shared.status.lock().unwrap() = None;
            // Commands meant for the dead helper are stale.
            while queue.try_recv().is_ok() {}
            if started.elapsed() > Duration::from_secs(60) {
                backoff.reset();
            }
            let delay = backoff.next_delay();
            info!("Restarting the drag probe helper in {} s", delay.as_secs());
            std::thread::sleep(delay);
        }
    }

    fn run_session(shared: &Arc<Shared>, queue: &Receiver<ToHelper>) -> Result<(), String> {
        static NONCE: AtomicU64 = AtomicU64::new(0);
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {}", e))?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let name = crate::dragipc::pipe_name(std::process::id(), nanos ^ NONCE.fetch_add(1, Ordering::SeqCst));
        let helper = medium::launch_helper(&exe.to_string_lossy(), &name)?;
        let result = serve(shared, queue, &helper, &name);
        helper.terminate();
        result
    }

    fn open_pipe(name: &str, access: u32) -> Result<File, std::io::Error> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(wide.as_ptr(), access, 0, ptr::null(), OPEN_EXISTING, 0, ptr::null_mut())
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_handle(handle as _) })
    }

    /// Opens both pipes of `name` and checks that the helper process created them.
    fn connect_pipes(name: &str, helper: &Helper) -> Result<(File, File), String> {
        use std::os::windows::io::AsRawHandle;
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            if helper.exited() {
                return Err("the helper exited before it connected".into());
            }
            match (open_pipe(&format!("{}-a", name), GENERIC_WRITE), open_pipe(&format!("{}-b", name), GENERIC_READ)) {
                (Ok(to), Ok(from)) => {
                    for pipe in [&to, &from] {
                        let mut server = 0u32;
                        if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle() as _, &mut server) } == 0 {
                            return Err(format!("GetNamedPipeServerProcessId: {}", std::io::Error::last_os_error()));
                        }
                        if server != helper.pid {
                            return Err(format!("pipe server is process {}, not the helper ({})", server, helper.pid));
                        }
                    }
                    info!("Connected to the drag probe helper (pid {}, started with {}, pipes verified)", helper.pid, helper.how);
                    return Ok((to, from));
                }
                (a, b) => {
                    if Instant::now() >= deadline {
                        let last = a.err().or(b.err()).map(|e| e.to_string()).unwrap_or_default();
                        return Err(format!("the helper did not open its pipes in time: {}", last));
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn serve(shared: &Arc<Shared>, queue: &Receiver<ToHelper>, helper: &Helper, name: &str) -> Result<(), String> {
        let (mut to, mut from) = connect_pipes(name, helper)?;
        let session = shared.session.fetch_add(1, Ordering::SeqCst) + 1;
        let closed = Arc::new(AtomicBool::new(false));
        let reader_shared = shared.clone();
        let reader_closed = closed.clone();
        std::thread::Builder::new()
            .name("winbridge-drag-events".into())
            .spawn(move || {
                loop {
                    match read_message::<_, FromHelper>(&mut from) {
                        Ok(event) => {
                            if reader_shared.session.load(Ordering::SeqCst) == session {
                                handle_event(&reader_shared, event);
                            }
                        }
                        Err(e) => {
                            info!("Helper pipe closed: {}", e);
                            break;
                        }
                    }
                }
                reader_closed.store(true, Ordering::SeqCst);
            })
            .map_err(|e| e.to_string())?;
        loop {
            match queue.recv_timeout(Duration::from_millis(300)) {
                Ok(command) => write_message(&mut to, &command).map_err(|e| format!("writing to the helper: {}", e))?,
                Err(RecvTimeoutError::Timeout) => {
                    if helper.exited() {
                        return Err(format!("the helper (pid {}) exited", helper.pid));
                    }
                    if closed.load(Ordering::SeqCst) {
                        return Err("the helper closed its pipe".into());
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }

    fn handle_event(shared: &Shared, event: FromHelper) {
        match event {
            FromHelper::Ready { pid, integrity } => {
                info!(
                    "Drag probe helper ready: pid {}, integrity {:#x} ({})",
                    pid,
                    integrity,
                    if integrity == MEDIUM_RID { "medium" } else { "NOT medium: drags from Explorer may be blocked" }
                );
                *shared.status.lock().unwrap() = Some((pid, integrity));
                shared.connected.store(true, Ordering::SeqCst);
            }
            FromHelper::NoDrag => {
                info!("The helper saw no drag");
                shared.send(Message::GuestDragNone);
            }
            FromHelper::Offer { paths, text } => {
                let reported = paths.len();
                let paths = valid_offer_paths(paths);
                if paths.len() != reported {
                    warn!("{} of {} reported drag path(s) were not absolute existing paths and were dropped", reported - paths.len(), reported);
                }
                let scanned = if paths.is_empty() {
                    Ok((Vec::new(), 0))
                } else {
                    files::scan(&paths, DRAG_CAP_BYTES).map(|e| (files::top_names(&e), files::total_bytes(&e)))
                };
                let (names, total_bytes) = match scanned {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Dragged files not offered: {}", e);
                        (Vec::new(), 0)
                    }
                };
                if names.is_empty() && text.is_none() {
                    shared.send(Message::GuestDragNone);
                    return;
                }
                let drag_id = next_id();
                info!("Guest drag {}: {} file(s), {} bytes, text={}", drag_id, names.len(), total_bytes, text.is_some());
                *shared.offer.lock().unwrap() = Some(Offer { id: drag_id, paths });
                shared.send(Message::GuestDragOffer { drag_id, names, total_bytes, text });
            }
        }
    }

    fn kill_process(pid: u32) -> bool {
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle.is_null() {
                return false;
            }
            let ok = TerminateProcess(handle, 1) != 0;
            CloseHandle(handle);
            ok
        }
    }

    /// Diagnostics (`--self-test-drag <file>` / `--self-test-drag-hold <file>`): runs the real
    /// supervisor and helper without the host. The first mode checks that the helper is medium
    /// integrity, that a probe goes through the pipes and back, and that a killed helper is
    /// restarted; the hold mode writes the helper's pid to the file and waits (so a script can
    /// kill the agent and check that the helper dies with it). Results go to the file.
    pub fn self_test(out: &str, hold: bool) -> i32 {
        let (tx, rx) = std::sync::mpsc::channel::<Message>();
        let tx = Mutex::new(tx);
        let drag = DragOut::start(Arc::new(move |msg| {
            let _ = tx.lock().unwrap().send(msg);
            Ok(())
        }));
        let mut report = String::new();
        let mut ok = true;
        let mut check = |what: &str, passed: bool, detail: String| {
            report.push_str(&format!("{}: {} {}\n", if passed { "PASS" } else { "FAIL" }, what, detail));
            ok &= passed;
        };
        let wait_helper = |not: Option<u32>| {
            let deadline = Instant::now() + Duration::from_secs(25);
            while Instant::now() < deadline {
                if let Some((pid, rid)) = drag.helper_info() {
                    if Some(pid) != not {
                        return Some((pid, rid));
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            None
        };
        let first = wait_helper(None);
        check("helper starts and reports ready", first.is_some(), format!("{:?}", first));
        let Some((pid, rid)) = first else {
            let _ = std::fs::write(out, report);
            return 1;
        };
        check("helper runs at medium integrity", rid == MEDIUM_RID, format!("integrity {:#x}", rid));
        check(
            "agent integrity (informational)",
            true,
            format!("{:#x}, elevated: {}", medium::current_integrity().unwrap_or(0), medium::is_elevated()),
        );
        if hold {
            let _ = std::fs::write(out, format!("{}\n{}", pid, report));
            std::thread::sleep(Duration::from_secs(120));
            return 0;
        }
        // Probe round trip: no button is held, so the helper hides the window and says so.
        drag.probe(300, 300);
        let answered = rx.recv_timeout(Duration::from_secs(5));
        check("probe goes to the helper and its answer comes back", matches!(answered, Ok(Message::GuestDragNone)), format!("{:?}", answered));
        // Restart after the helper dies.
        let killed = kill_process(pid);
        let second = wait_helper(Some(pid));
        check(
            "a killed helper is restarted",
            killed && second.is_some(),
            format!("killed pid {} -> {:?}", pid, second),
        );
        if let Some((new_pid, new_rid)) = second {
            check("the restarted helper is medium integrity", new_rid == MEDIUM_RID, format!("pid {} integrity {:#x}", new_pid, new_rid));
        }
        let _ = std::fs::write(out, &report);
        if ok { 0 } else { 1 }
    }
}

#[cfg(not(windows))]
#[allow(dead_code)]
mod imp {
    use super::Sink;

    pub struct DragOut;

    impl DragOut {
        pub fn start(_sink: Sink) -> Self {
            Self
        }
        pub fn probe(&self, _x: u32, _y: u32) {}
        pub fn end(&self) {}
        pub fn fetch(&self, _drag_id: u32) {}
    }
}

#[cfg(not(windows))]
pub use imp::DragOut;
