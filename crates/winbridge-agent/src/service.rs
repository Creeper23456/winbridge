// SPDX-License-Identifier: GPL-3.0-or-later

//! The service that keeps the agent running as SYSTEM in the console session.
//!
//! The service control manager starts `winbridge-agent.exe --service` at boot (`sc.exe create
//! WinBridgeAgent ... start= auto obj= LocalSystem`, see `scripts/install-agent.ps1`). The
//! service does nothing else than supervise one child: the same program without arguments, with
//! a copy of its own SYSTEM token moved into the console session and the environment block of
//! the logged-on user, on `winsta0\default`. The child is started as soon as the console has a
//! session (the logon screen included, so the host can set the resolution before anyone logs
//! on), and started anew when it ends, when the console moves to another session, or when
//! another user logs on (a new user has a new profile, temp folder and token).

/// What the console looks like: the session that owns it and the user logged on there.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Console {
    pub session: u32,
    /// SID of the logged-on user, `None` at the logon screen.
    pub user: Option<Vec<u8>>,
}

/// What the supervisor does with the child this second.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum Step {
    Wait,
    Start,
    /// The child was made for another console: end it, then start a new one.
    Restart,
}

/// `running`: the console the child was started for (`None` without a child). `exited`: the child
/// is gone. `wanted`: the console now (`None` while no session owns it).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn decide(running: Option<&Console>, exited: bool, wanted: Option<&Console>) -> Step {
    let Some(wanted) = wanted else {
        return Step::Wait;
    };
    match running {
        None => Step::Start,
        Some(_) if exited => Step::Start,
        Some(console) if console != wanted => Step::Restart,
        Some(_) => Step::Wait,
    }
}

#[cfg(windows)]
pub use imp::run;

#[cfg(windows)]
mod imp {
    use super::{decide, Console, Step};
    use crate::identity;
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tracing::{error, info, warn};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::Security::{
        DuplicateTokenEx, SetTokenInformation, TokenSessionId, TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY,
        TOKEN_DUPLICATE, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Services::{
        RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, SERVICE_ACCEPT_SHUTDOWN,
        SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
        SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STOPPED, SERVICE_STOP_PENDING,
        SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, ResumeThread, SetEvent,
        TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
        PROCESS_INFORMATION, STARTUPINFOW,
    };

    const SERVICE_NAME: &str = "WinBridgeAgent";
    const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
    const SECURITY_IMPERSONATION: i32 = 2;
    const TOKEN_PRIMARY: i32 = 1;
    /// The child is not started again sooner than this after the last start (a crash loop).
    const RESTART_GAP: Duration = Duration::from_secs(3);
    /// How often the console is looked at.
    const POLL: u32 = 1000;
    const ERROR_CALL_NOT_IMPLEMENTED: u32 = 120;

    static STATUS_HANDLE: AtomicUsize = AtomicUsize::new(0);
    static STOP_EVENT: AtomicUsize = AtomicUsize::new(0);

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn report(state: u32, accepted: u32) {
        let status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: accepted,
            dwWin32ExitCode: 0,
            dwServiceSpecificExitCode: 0,
            dwCheckPoint: 0,
            dwWaitHint: if state == SERVICE_RUNNING || state == SERVICE_STOPPED { 0 } else { 10_000 },
        };
        unsafe { SetServiceStatus(STATUS_HANDLE.load(Ordering::SeqCst) as HANDLE, &status) };
    }

    unsafe extern "system" fn control_handler(control: u32, _event: u32, _data: *mut c_void, _context: *mut c_void) -> u32 {
        match control {
            SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
                report(SERVICE_STOP_PENDING, 0);
                SetEvent(STOP_EVENT.load(Ordering::SeqCst) as HANDLE);
                0
            }
            SERVICE_CONTROL_INTERROGATE => 0,
            _ => ERROR_CALL_NOT_IMPLEMENTED,
        }
    }

    unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
        let name = wide(SERVICE_NAME);
        let handle = RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control_handler), ptr::null());
        if handle.is_null() {
            error!("RegisterServiceCtrlHandlerEx failed: {}", std::io::Error::last_os_error());
            return;
        }
        STATUS_HANDLE.store(handle as usize, Ordering::SeqCst);
        report(SERVICE_START_PENDING, 0);
        let stop = CreateEventW(ptr::null(), 1, 0, ptr::null());
        STOP_EVENT.store(stop as usize, Ordering::SeqCst);
        report(SERVICE_RUNNING, SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN);
        info!("Service started");
        supervise(stop);
        info!("Service stopped");
        report(SERVICE_STOPPED, 0);
        CloseHandle(stop);
    }

    /// Hands the thread to the service control manager. Returns the process exit code: it
    /// returns when the service has stopped, and fails at once when not started by the manager.
    pub fn run() -> i32 {
        let mut name = wide(SERVICE_NAME);
        let table = [
            SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) },
            SERVICE_TABLE_ENTRYW { lpServiceName: ptr::null_mut(), lpServiceProc: None },
        ];
        if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
            error!(
                "StartServiceCtrlDispatcher failed: {} (--service is for the service control manager)",
                std::io::Error::last_os_error()
            );
            return 1;
        }
        0
    }

    struct Child {
        process: HANDLE,
        console: Console,
        started: Instant,
    }

    impl Child {
        fn exited(&self) -> bool {
            unsafe { WaitForSingleObject(self.process, 0) == WAIT_OBJECT_0 }
        }

        fn end(self) {
            unsafe {
                TerminateProcess(self.process, 1);
                CloseHandle(self.process);
            }
        }
    }

    fn console_now() -> Option<Console> {
        let session = identity::console_session()?;
        Some(Console { session, user: identity::query_user(session).map(|u| u.sid().to_vec()) })
    }

    /// A job that ends its processes when the service goes (however it goes).
    fn kill_on_close_job() -> Option<HANDLE> {
        unsafe {
            let job = CreateJobObjectW(ptr::null(), ptr::null());
            if job.is_null() {
                return None;
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(job);
                return None;
            }
            Some(job)
        }
    }

    /// Starts the agent as SYSTEM in `console.session`, in the logged-on user's environment.
    fn start(console: &Console, job: Option<HANDLE>) -> Result<Child, String> {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {}", e))?;
        let exe_w = wide(&exe.to_string_lossy());
        let mut command = wide(&format!("\"{}\"", exe.display()));
        let mut desktop = wide("winsta0\\default");
        unsafe {
            let mut own: HANDLE = ptr::null_mut();
            if OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_SESSIONID,
                &mut own,
            ) == 0
            {
                return Err(format!("OpenProcessToken: {}", std::io::Error::last_os_error()));
            }
            let mut token: HANDLE = ptr::null_mut();
            let duplicated = DuplicateTokenEx(own, MAXIMUM_ALLOWED, ptr::null(), SECURITY_IMPERSONATION, TOKEN_PRIMARY, &mut token);
            CloseHandle(own);
            if duplicated == 0 {
                return Err(format!("DuplicateTokenEx: {}", std::io::Error::last_os_error()));
            }
            let session = console.session;
            if SetTokenInformation(token, TokenSessionId, (&session as *const u32).cast(), 4) == 0 {
                let e = std::io::Error::last_os_error();
                CloseHandle(token);
                return Err(format!("SetTokenInformation(TokenSessionId {}): {}", session, e));
            }

            // The user's environment: temp folder, LOCALAPPDATA and the rest are the user's.
            let user = identity::query_user(session);
            let mut environment: *mut c_void = ptr::null_mut();
            if let Some(user) = &user {
                if CreateEnvironmentBlock(&mut environment, user.token(), 0) == 0 {
                    warn!("CreateEnvironmentBlock failed: {}; the agent gets SYSTEM's environment", std::io::Error::last_os_error());
                    environment = ptr::null_mut();
                }
            }

            let mut startup: STARTUPINFOW = std::mem::zeroed();
            startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            startup.lpDesktop = desktop.as_mut_ptr();
            let mut info: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = CreateProcessAsUserW(
                token,
                exe_w.as_ptr(),
                command.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                0,
                CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | CREATE_SUSPENDED,
                environment,
                ptr::null(),
                &startup,
                &mut info,
            );
            let error = std::io::Error::last_os_error();
            if !environment.is_null() {
                DestroyEnvironmentBlock(environment);
            }
            CloseHandle(token);
            if ok == 0 {
                return Err(format!("CreateProcessAsUser: {}", error));
            }
            match job {
                Some(job) if AssignProcessToJobObject(job, info.hProcess) != 0 => {}
                _ => warn!("The agent could not be put in a kill-on-close job: {}", std::io::Error::last_os_error()),
            }
            ResumeThread(info.hThread);
            CloseHandle(info.hThread);
            info!(
                "Agent started as SYSTEM in session {} for {} (pid {})",
                session,
                if console.user.is_some() { "the logged-on user" } else { "the logon screen" },
                info.dwProcessId
            );
            Ok(Child { process: info.hProcess, console: console.clone(), started: Instant::now() })
        }
    }

    /// Keeps one agent running for the console until `stop` is signalled.
    fn supervise(stop: HANDLE) {
        let job = kill_on_close_job();
        let mut child: Option<Child> = None;
        loop {
            let wanted = console_now();
            let exited = child.as_ref().is_some_and(Child::exited);
            match decide(child.as_ref().map(|c| &c.console), exited, wanted.as_ref()) {
                Step::Wait => {}
                step => {
                    if let Some(old) = child.take() {
                        if exited {
                            warn!("The agent ended; starting it again");
                        } else {
                            info!("The console changed (session or user); restarting the agent");
                        }
                        let started = old.started;
                        old.end();
                        // A crash loop must not spin: wait out the rest of the gap.
                        if step == Step::Start && started.elapsed() < RESTART_GAP {
                            unsafe { WaitForSingleObject(stop, RESTART_GAP.as_millis() as u32) };
                        }
                    }
                    if let Some(wanted) = &wanted {
                        match start(wanted, job) {
                            Ok(started) => child = Some(started),
                            Err(e) => error!("Cannot start the agent: {}", e),
                        }
                    }
                }
            }
            if unsafe { WaitForSingleObject(stop, POLL) } == WAIT_OBJECT_0 {
                break;
            }
        }
        if let Some(child) = child {
            child.end();
        }
        if let Some(job) = job {
            unsafe { CloseHandle(job) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn console(session: u32, user: Option<u8>) -> Console {
        Console { session, user: user.map(|n| vec![1, 1, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, n]) }
    }

    #[test]
    fn no_console_session_means_nothing_to_do() {
        assert_eq!(decide(None, false, None), Step::Wait);
        assert_eq!(decide(Some(&console(1, Some(1))), false, None), Step::Wait);
    }

    #[test]
    fn a_console_without_an_agent_starts_one_even_at_the_logon_screen() {
        assert_eq!(decide(None, false, Some(&console(1, None))), Step::Start);
    }

    #[test]
    fn an_agent_that_ended_is_started_again() {
        let now = console(1, Some(1));
        assert_eq!(decide(Some(&now), true, Some(&now)), Step::Start);
    }

    #[test]
    fn another_session_or_another_user_restarts_the_agent() {
        let now = console(1, Some(1));
        assert_eq!(decide(Some(&now), false, Some(&now)), Step::Wait);
        assert_eq!(decide(Some(&now), false, Some(&console(2, Some(1)))), Step::Restart);
        assert_eq!(decide(Some(&now), false, Some(&console(1, Some(2)))), Step::Restart);
        // The user logged on after the agent started at the logon screen: its profile is new.
        assert_eq!(decide(Some(&console(1, None)), false, Some(&now)), Step::Restart);
        assert_eq!(decide(Some(&now), false, Some(&console(1, None))), Step::Restart);
    }
}
