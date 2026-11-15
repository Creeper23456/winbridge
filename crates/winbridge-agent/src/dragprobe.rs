// SPDX-License-Identifier: GPL-3.0-or-later

//! The drag probe helper process (`winbridge-agent.exe --drag-probe <pipe> <agent pid>`).
//!
//! Windows only lets a drag reach a window of the same or a lower integrity level than the drag
//! source. The agent is elevated, Explorer is not, so the nearly invisible `IDropTarget` window
//! that catches a guest-to-host drag lives here, in a process the agent starts at medium
//! integrity (see `medium`). The helper never opens the VirtIO port: it takes commands from the
//! agent over a private pipe (`dragipc`) and reports what a drag carries; the agent does the
//! rest. Nothing is injected: the window only waits, and it hides when the left button is
//! released, when no drag arrives within five seconds, or when the host says the drag is over.
//! It exits when the agent's pipe closes.

#[cfg(windows)]
pub use imp::run;

#[cfg(not(windows))]
pub fn run(_pipe: &str, _agent_pid: u32) -> i32 {
    1
}

#[cfg(windows)]
mod imp {
    use crate::clipboard::hdrop_paths;
    use crate::dragipc::{read_message, write_message, FromHelper, ToHelper};
    use crate::medium;
    use std::fs::File;
    use std::os::windows::io::FromRawHandle;
    use std::path::PathBuf;
    use std::ptr;
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};
    use tracing::{error, info, warn};
    use windows::core::{implement, Ref};
    use windows::Win32::Foundation::{HWND as WinHwnd, POINTL};
    use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
    use windows::Win32::System::Ole::{
        IDropTarget, IDropTarget_Impl, OleInitialize, RegisterDragDrop, ReleaseStgMedium, DROPEFFECT,
        DROPEFFECT_COPY, DROPEFFECT_NONE,
    };
    use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    use windows_sys::Win32::Foundation::{GetLastError, LocalFree, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, KillTimer, PostMessageW, RegisterClassW,
        SetLayeredWindowAttributes, SetTimer, SetWindowPos, ShowWindow, TranslateMessage, HWND_TOPMOST,
        LWA_ALPHA, MSG, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, WM_APP, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };

    const WM_PROBE: u32 = WM_APP + 1;
    const WM_END: u32 = WM_APP + 2;
    const POLL_TIMER: usize = 1;
    /// How often the probe window checks that the left button is still held.
    const POLL_MS: u32 = 30;
    /// The longest a probe window waits for a drag to show up while the button stays held.
    const PROBE_MAX_MS: u64 = 5000;
    /// Side of the probe window, centred on the cursor.
    const PROBE_SIZE: i32 = 120;
    const CF_UNICODETEXT: u16 = 13;
    const CF_HDROP: u16 = 15;
    const ERROR_PIPE_CONNECTED: u32 = 535;

    #[derive(Default)]
    struct State {
        probing: bool,
        /// The probe window is shown.
        visible: bool,
        /// When a probe without a drag gives up.
        deadline: Option<Instant>,
        /// A drag is over the probe window right now.
        active: bool,
        /// `DragOver` was logged for this drag.
        over_logged: bool,
    }

    /// Whether the left mouse button is held (the guest's own device state; nothing is injected).
    fn left_button_down() -> bool {
        unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 }
    }

    struct Shared {
        out: Mutex<File>,
        state: Mutex<State>,
        hwnd: AtomicIsize,
    }

    impl Shared {
        fn send(&self, message: FromHelper) {
            let result = match self.out.lock() {
                Ok(mut out) => write_message(&mut *out, &message),
                Err(_) => Err(std::io::Error::other("poisoned")),
            };
            if let Err(e) = result {
                error!("The agent pipe is gone ({}); the drag probe helper exits", e);
                std::process::exit(0);
            }
        }

        fn hwnd(&self) -> HWND {
            self.hwnd.load(Ordering::SeqCst) as HWND
        }

        fn hide(&self) {
            let hwnd = self.hwnd();
            self.state.lock().unwrap().visible = false;
            if !hwnd.is_null() {
                unsafe {
                    KillTimer(hwnd, POLL_TIMER);
                    ShowWindow(hwnd, SW_HIDE);
                }
            }
        }
    }

    static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Creates one end of the private pipes: only the user's own SID may open it.
    unsafe fn create_pipe(name: &str, inbound: bool) -> Result<HANDLE, String> {
        let sid = medium::user_sid_string().ok_or("cannot read the user SID")?;
        let sddl = wide(&format!("D:(A;;GA;;;{})", sid));
        let mut descriptor = ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut descriptor, ptr::null_mut()) == 0 {
            return Err(format!("security descriptor: {}", std::io::Error::last_os_error()));
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        const PIPE_ACCESS_INBOUND: u32 = 1;
        const PIPE_ACCESS_OUTBOUND: u32 = 2;
        const PIPE_TYPE_BYTE_WAIT_NO_REMOTE: u32 = 0x8; // PIPE_REJECT_REMOTE_CLIENTS; byte, wait are 0
        let handle = CreateNamedPipeW(
            wide(name).as_ptr(),
            if inbound { PIPE_ACCESS_INBOUND } else { PIPE_ACCESS_OUTBOUND },
            PIPE_TYPE_BYTE_WAIT_NO_REMOTE,
            1,
            64 * 1024,
            64 * 1024,
            0,
            &attributes,
        );
        let error = std::io::Error::last_os_error();
        LocalFree(descriptor);
        if handle as isize == -1 {
            return Err(format!("CreateNamedPipe {}: {}", name, error));
        }
        Ok(handle)
    }

    /// Waits for the agent to connect and checks that the client is the agent.
    unsafe fn accept_agent(pipe: HANDLE, agent_pid: u32) -> Result<(), String> {
        if ConnectNamedPipe(pipe, ptr::null_mut()) == 0 && GetLastError() != ERROR_PIPE_CONNECTED {
            return Err(format!("ConnectNamedPipe: {}", std::io::Error::last_os_error()));
        }
        let mut client = 0u32;
        if GetNamedPipeClientProcessId(pipe, &mut client) == 0 {
            return Err(format!("GetNamedPipeClientProcessId: {}", std::io::Error::last_os_error()));
        }
        if client != agent_pid {
            return Err(format!("pipe client is process {}, not the agent ({})", client, agent_pid));
        }
        Ok(())
    }

    /// Runs the helper; returns the process exit code.
    pub fn run(pipe: &str, agent_pid: u32) -> i32 {
        let integrity = medium::current_integrity().unwrap_or(0);
        info!(
            "Drag probe helper v{} starting: pid {}, integrity {:#x}, agent pid {}",
            env!("CARGO_PKG_VERSION"),
            unsafe { GetCurrentProcessId() },
            integrity,
            agent_pid
        );
        crate::display::init_dpi_awareness();
        unsafe {
            if let Err(e) = OleInitialize(None) {
                error!("OleInitialize failed: {}", e);
                return 2;
            }
            let class_name: Vec<u16> = "WinBridgeDragProbe\0".encode_utf16().collect();
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: ptr::null_mut(),
                hIcon: ptr::null_mut(),
                hCursor: ptr::null_mut(),
                hbrBackground: ptr::null_mut(),
                lpszMenuName: ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
                class_name.as_ptr(),
                ptr::null(),
                WS_POPUP,
                0,
                0,
                PROBE_SIZE,
                PROBE_SIZE,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            if hwnd.is_null() {
                error!("Could not create the drag probe window: {}", std::io::Error::last_os_error());
                return 3;
            }
            // Nearly transparent but still hit-testable.
            SetLayeredWindowAttributes(hwnd, 0, 1, LWA_ALPHA);

            // The agent opens `-a` for writing and `-b` for reading.
            let (inbound, outbound) = match (
                create_pipe(&format!("{}-a", pipe), true),
                create_pipe(&format!("{}-b", pipe), false),
            ) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), _) | (_, Err(e)) => {
                    error!("Could not create the agent pipes: {}", e);
                    return 4;
                }
            };
            info!("Waiting for the agent on {}", pipe);
            for (handle, what) in [(inbound, "-a"), (outbound, "-b")] {
                if let Err(e) = accept_agent(handle, agent_pid) {
                    error!("Pipe {}{} rejected: {}", pipe, what, e);
                    return 5;
                }
            }
            info!("The agent is connected (pid verified)");
            let mut commands = File::from_raw_handle(inbound as _);
            let shared = Arc::new(Shared {
                out: Mutex::new(File::from_raw_handle(outbound as _)),
                state: Mutex::new(State::default()),
                hwnd: AtomicIsize::new(hwnd as isize),
            });
            let _ = SHARED.set(shared.clone());

            let target: IDropTarget = Target { shared: shared.clone() }.into();
            if let Err(e) = RegisterDragDrop(WinHwnd(hwnd), &target) {
                error!("RegisterDragDrop failed: {}", e);
                return 6;
            }
            shared.send(FromHelper::Ready { pid: GetCurrentProcessId(), integrity });

            let hwnd_value = hwnd as isize;
            std::thread::Builder::new()
                .name("winbridge-probe-commands".into())
                .spawn(move || loop {
                    match read_message::<_, ToHelper>(&mut commands) {
                        Ok(ToHelper::Probe { x, y }) => {
                            PostMessageW(hwnd_value as HWND, WM_PROBE, x as WPARAM, y as LPARAM);
                        }
                        Ok(ToHelper::End) => {
                            PostMessageW(hwnd_value as HWND, WM_END, 0, 0);
                        }
                        Err(e) => {
                            info!("The agent pipe closed ({}); the drag probe helper exits", e);
                            std::process::exit(0);
                        }
                    }
                })
                .ok();

            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        0
    }

    unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let Some(shared) = SHARED.get() else {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        };
        match msg {
            WM_PROBE => {
                let (x, y) = (wparam as i32, lparam as i32);
                let mut state = shared.state.lock().unwrap();
                // A drag is already over the window: moving it would pull it from under the drag.
                if state.active {
                    return 0;
                }
                let first = !state.visible;
                state.probing = true;
                state.visible = true;
                if first {
                    state.deadline = Some(Instant::now() + Duration::from_millis(PROBE_MAX_MS));
                }
                drop(state);
                let left = x - PROBE_SIZE / 2;
                let top = y - PROBE_SIZE / 2;
                SetWindowPos(hwnd, HWND_TOPMOST, left, top, PROBE_SIZE, PROBE_SIZE, SWP_NOACTIVATE | SWP_SHOWWINDOW);
                info!(
                    "Guest drag probe at ({}, {}): window ({}, {})-({}, {}), left button down: {}{}",
                    x,
                    y,
                    left,
                    top,
                    left + PROBE_SIZE,
                    top + PROBE_SIZE,
                    left_button_down(),
                    if first { "" } else { " (repeat)" }
                );
                SetTimer(hwnd, POLL_TIMER, POLL_MS, None);
                0
            }
            WM_TIMER if wparam == POLL_TIMER => {
                let (active, probing, expired) = {
                    let state = shared.state.lock().unwrap();
                    (state.active, state.probing, state.deadline.is_some_and(|d| Instant::now() >= d))
                };
                // The window never outlives the button, and a drag-less probe never outlives its time.
                let reason = if !left_button_down() {
                    Some("the left button was released")
                } else if expired && !active {
                    Some("no drag arrived in time")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    {
                        let mut state = shared.state.lock().unwrap();
                        state.probing = false;
                        state.active = false;
                        state.over_logged = false;
                    }
                    shared.hide();
                    if probing {
                        info!("Guest drag probe over without a drag: {}", reason);
                        shared.send(FromHelper::NoDrag);
                    } else {
                        info!("Probe window hidden: {}", reason);
                    }
                }
                0
            }
            WM_END => {
                info!("Guest drag end requested by the host");
                {
                    let mut state = shared.state.lock().unwrap();
                    state.probing = false;
                    state.active = false;
                    state.over_logged = false;
                }
                shared.hide();
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    #[implement(IDropTarget)]
    struct Target {
        shared: Arc<Shared>,
    }

    impl Target {
        /// A drag entered the probe window: read what it carries and report it to the agent.
        fn on_enter(&self, data: &IDataObject) -> bool {
            // Accepted while the probe window is up, even when no probe is still pending: the
            // host re-sends probes and a drag may enter between two of them.
            {
                let mut state = self.shared.state.lock().unwrap();
                if !state.visible {
                    info!("DragEnter while the probe window is hidden: ignored");
                    return false;
                }
                state.probing = false;
                state.over_logged = false;
            }
            let (paths, text) = unsafe { extract(data) };
            if paths.is_empty() && text.is_none() {
                info!("The drag carries neither files nor text");
                self.shared.hide();
                self.shared.send(FromHelper::NoDrag);
                return false;
            }
            info!("The drag carries {} path(s), text={}", paths.len(), text.is_some());
            self.shared.state.lock().unwrap().active = true;
            self.shared.send(FromHelper::Offer { paths, text });
            true
        }

        fn leave(&self) {
            {
                let mut state = self.shared.state.lock().unwrap();
                state.active = false;
                state.over_logged = false;
            }
            self.shared.hide();
        }
    }

    impl IDropTarget_Impl for Target_Impl {
        fn DragEnter(
            &self,
            pdataobj: Ref<IDataObject>,
            keys: MODIFIERKEYS_FLAGS,
            pt: &POINTL,
            effect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            info!("DragEnter on the probe window at ({}, {}), keys {:#x}", pt.x, pt.y, keys.0);
            let accepted = pdataobj.as_ref().is_some_and(|data| self.on_enter(data));
            info!("DragEnter {}", if accepted { "accepted: the drag was reported to the agent" } else { "ignored" });
            unsafe { *effect = if accepted { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
            Ok(())
        }

        fn DragOver(&self, _keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
            let mut state = self.shared.state.lock().unwrap();
            if !std::mem::replace(&mut state.over_logged, true) {
                info!("Guest drag moves over the probe window at ({}, {})", pt.x, pt.y);
            }
            let active = state.active;
            unsafe { *effect = if active { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
            Ok(())
        }

        fn DragLeave(&self) -> windows::core::Result<()> {
            info!("Guest drag left the probe window");
            self.leave();
            Ok(())
        }

        fn Drop(
            &self,
            _data: Ref<IDataObject>,
            _keys: MODIFIERKEYS_FLAGS,
            _pt: &POINTL,
            effect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            // Copy, never move: the originals stay where they are.
            unsafe { *effect = DROPEFFECT_COPY };
            info!("Guest drag dropped on the probe window (effect COPY); the host decides what to fetch");
            self.leave();
            Ok(())
        }
    }

    /// Reads dragged file paths and text from an OLE data object.
    unsafe fn extract(data: &IDataObject) -> (Vec<PathBuf>, Option<String>) {
        let format = |cf: u16| FORMATETC {
            cfFormat: cf,
            ptd: ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0 as u32,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let mut paths = Vec::new();
        if let Ok(mut medium) = data.GetData(&format(CF_HDROP)) {
            if medium.tymed == TYMED_HGLOBAL.0 as u32 {
                paths = hdrop_paths(medium.u.hGlobal.0);
            }
            ReleaseStgMedium(&mut medium);
        }
        let mut text = None;
        if paths.is_empty() {
            if let Ok(mut medium) = data.GetData(&format(CF_UNICODETEXT)) {
                if medium.tymed == TYMED_HGLOBAL.0 as u32 {
                    let handle = medium.u.hGlobal.0;
                    let size = GlobalSize(handle);
                    let ptr = GlobalLock(handle) as *const u16;
                    if !ptr.is_null() {
                        let units = std::slice::from_raw_parts(ptr, size / 2);
                        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
                        text = Some(String::from_utf16_lossy(&units[..end]));
                        GlobalUnlock(handle);
                    }
                }
                ReleaseStgMedium(&mut medium);
            }
        }
        if paths.is_empty() && text.is_none() {
            warn!("The drag data object offered no CF_HDROP and no text");
        }
        (paths, text.filter(|t| !t.is_empty()))
    }
}
