// SPDX-License-Identifier: GPL-3.0-or-later

//! Starting the drag probe helper at medium integrity.
//!
//! The agent runs elevated (high integrity). Windows blocks OLE drags from a lower-integrity
//! source (Explorer, medium) into a higher-integrity window, so the window that receives guest
//! drags lives in a helper process with a medium token derived from the agent's own: the linked
//! (filtered) token when Windows hands out a usable one, otherwise a restricted token with the
//! integrity level lowered to medium. The helper is put in a kill-on-close job so it never
//! outlives the agent.

/// Integrity RID of the medium level (`S-1-16-8192`).
#[cfg_attr(not(windows), allow(dead_code))]
pub const MEDIUM_RID: u32 = 0x2000;

/// The medium mandatory label SID `S-1-16-8192` in binary form.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn medium_sid() -> [u8; 12] {
    let mut sid = [0u8; 12];
    sid[0] = 1; // revision
    sid[1] = 1; // one sub-authority
    sid[7] = 16; // identifier authority 16 (mandatory label)
    sid[8..12].copy_from_slice(&MEDIUM_RID.to_le_bytes());
    sid
}

/// The command line of the helper: `"<exe>" --drag-probe <pipe> <agent pid>`.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn helper_command_line(exe: &str, pipe: &str, agent_pid: u32) -> String {
    format!("\"{}\" --drag-probe {} {}", exe, pipe, agent_pid)
}

#[cfg(windows)]
pub use imp::*;

#[cfg(windows)]
mod imp {
    use super::{helper_command_line, medium_sid, MEDIUM_RID};
    use std::ptr;
    use std::sync::OnceLock;
    use tracing::{info, warn};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LocalFree, HANDLE, LUID};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, CreateRestrictedToken, DuplicateTokenEx, GetSidSubAuthority,
        GetSidSubAuthorityCount, GetTokenInformation, LookupPrivilegeValueW, SetTokenInformation,
        TokenElevation, TokenIntegrityLevel, TokenLinkedToken, TokenUser, DISABLE_MAX_PRIVILEGE, LUA_TOKEN,
        LUID_AND_ATTRIBUTES, PSID, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_PRIVILEGES,
        TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_LINKED_TOKEN, TOKEN_MANDATORY_LABEL,
        TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessAsUserW, CreateProcessW, GetCurrentProcess, GetCurrentProcessId, OpenProcessToken,
        ResumeThread, TerminateProcess, WaitForSingleObject, PROCESS_INFORMATION, STARTUPINFOW,
    };

    const CREATE_SUSPENDED: u32 = 0x4;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
    const SE_GROUP_INTEGRITY: u32 = 0x20;
    const SECURITY_IMPERSONATION: i32 = 2;
    const TOKEN_PRIMARY: i32 = 1;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// An owned token handle.
    struct Token(HANDLE);

    impl Drop for Token {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    fn open_own_token(access: u32) -> Option<Token> {
        let mut token: HANDLE = ptr::null_mut();
        (unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut token) } != 0).then_some(Token(token))
    }

    /// Reads a variable-size token information class.
    fn token_info(token: HANDLE, class: i32) -> Option<Vec<u8>> {
        let mut len = 0u32;
        unsafe { GetTokenInformation(token, class, ptr::null_mut(), 0, &mut len) };
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        (unsafe { GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) } != 0).then_some(buf)
    }

    /// Integrity RID of `token` (0x2000 medium, 0x3000 high, 0x4000 system).
    fn token_integrity(token: HANDLE) -> Option<u32> {
        let buf = token_info(token, TokenIntegrityLevel)?;
        unsafe {
            let sid = (*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL)).Label.Sid;
            let count = *GetSidSubAuthorityCount(sid) as u32;
            (count > 0).then(|| *GetSidSubAuthority(sid, count - 1))
        }
    }

    /// Integrity RID of this process.
    pub fn current_integrity() -> Option<u32> {
        let token = open_own_token(TOKEN_QUERY)?;
        token_integrity(token.0)
    }

    /// Whether this process runs with an elevated token.
    pub fn is_elevated() -> bool {
        let Some(token) = open_own_token(TOKEN_QUERY) else {
            return false;
        };
        token_info(token.0, TokenElevation)
            .map(|b| unsafe { (*(b.as_ptr() as *const TOKEN_ELEVATION)).TokenIsElevated != 0 })
            .unwrap_or(false)
    }

    /// The user SID of this process as `S-1-5-21-...`.
    pub fn user_sid_string() -> Option<String> {
        let token = open_own_token(TOKEN_QUERY)?;
        let buf = token_info(token.0, TokenUser)?;
        unsafe {
            let sid: PSID = (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid;
            let mut text: *mut u16 = ptr::null_mut();
            if ConvertSidToStringSidW(sid, &mut text) == 0 {
                return None;
            }
            let mut len = 0;
            while *text.add(len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            Some(s)
        }
    }

    /// Enables `name` in the agent's own token; the result says whether it is now enabled.
    fn enable_privilege(name: &str) -> bool {
        let Some(token) = open_own_token(TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY) else {
            return false;
        };
        unsafe {
            let mut luid: LUID = std::mem::zeroed();
            if LookupPrivilegeValueW(ptr::null(), wide(name).as_ptr(), &mut luid) == 0 {
                return false;
            }
            let privileges = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
            };
            AdjustTokenPrivileges(token.0, 0, &privileges, 0, ptr::null_mut(), ptr::null_mut()) != 0
                && GetLastError() == 0
        }
    }

    /// Lowers the integrity level of `token` to medium.
    fn set_medium(token: HANDLE) -> bool {
        let mut sid = medium_sid();
        let label = TOKEN_MANDATORY_LABEL {
            Label: windows_sys::Win32::Security::SID_AND_ATTRIBUTES {
                Sid: sid.as_mut_ptr().cast(),
                Attributes: SE_GROUP_INTEGRITY,
            },
        };
        unsafe {
            SetTokenInformation(
                token,
                TokenIntegrityLevel,
                (&label as *const TOKEN_MANDATORY_LABEL).cast(),
                std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + sid.len() as u32,
            ) != 0
        }
    }

    /// The linked (filtered) token of an elevated admin as a primary token. Windows normally
    /// hands out an identification-level copy that cannot start processes: then this is `None`.
    fn linked_token(own: &Token) -> Option<Token> {
        let buf = token_info(own.0, TokenLinkedToken)?;
        let linked = Token(unsafe { (*(buf.as_ptr() as *const TOKEN_LINKED_TOKEN)).LinkedToken });
        let mut primary: HANDLE = ptr::null_mut();
        let ok = unsafe {
            DuplicateTokenEx(linked.0, MAXIMUM_ALLOWED, ptr::null(), SECURITY_IMPERSONATION, TOKEN_PRIMARY, &mut primary)
        };
        (ok != 0).then_some(Token(primary))
    }

    /// A restricted copy of the agent's token (admin groups deny-only, privileges removed) at
    /// medium integrity.
    fn restricted_token(own: &Token) -> Option<Token> {
        let mut restricted: HANDLE = ptr::null_mut();
        let ok = unsafe {
            CreateRestrictedToken(
                own.0,
                DISABLE_MAX_PRIVILEGE | LUA_TOKEN,
                0,
                ptr::null(),
                0,
                ptr::null(),
                0,
                ptr::null(),
                &mut restricted,
            )
        };
        (ok != 0).then_some(Token(restricted))
    }

    /// The medium-integrity token candidates, best first, each with its description.
    fn medium_tokens() -> Vec<(&'static str, Token)> {
        // The agent is SYSTEM: the helper is the logged-on user's, at the user's own integrity
        // level (medium; the linked token when Windows hands out the elevated one).
        if let Some(user) = crate::identity::user_token() {
            let mut copy: HANDLE = ptr::null_mut();
            let ok = unsafe {
                DuplicateTokenEx(user, MAXIMUM_ALLOWED, ptr::null(), SECURITY_IMPERSONATION, TOKEN_PRIMARY, &mut copy)
            };
            if ok == 0 {
                warn!("Drag helper: cannot copy the user's token: {}", std::io::Error::last_os_error());
                return Vec::new();
            }
            let copy = Token(copy);
            if token_integrity(copy.0) == Some(MEDIUM_RID) {
                return vec![("the logged-on user's token", copy)];
            }
            return match linked_token(&copy) {
                Some(t) if token_integrity(t.0) == Some(MEDIUM_RID) => vec![("the user's linked token", t)],
                _ => {
                    warn!("Drag helper: the user's token is not medium integrity and has no medium linked token");
                    Vec::new()
                }
            };
        }
        let Some(own) = open_own_token(TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT) else {
            warn!("Drag helper: cannot open the agent token: {}", std::io::Error::last_os_error());
            return Vec::new();
        };
        let mut out = Vec::new();
        match linked_token(&own) {
            Some(t) if token_integrity(t.0) == Some(MEDIUM_RID) => out.push(("linked token", t)),
            Some(_) => info!("Drag helper: the linked token is not medium integrity; not used"),
            None => info!("Drag helper: no usable linked token"),
        }
        match restricted_token(&own) {
            Some(t) if set_medium(t.0) || token_integrity(t.0) == Some(MEDIUM_RID) => {
                out.push(("restricted token", t))
            }
            Some(_) => warn!("Drag helper: cannot lower the restricted token to medium integrity"),
            None => warn!("Drag helper: CreateRestrictedToken failed: {}", std::io::Error::last_os_error()),
        }
        out
    }

    static JOB: OnceLock<usize> = OnceLock::new();

    /// The job every helper joins; closing it (the agent exiting for any reason) kills them.
    fn job() -> Option<HANDLE> {
        let handle = *JOB.get_or_init(|| unsafe {
            let job = CreateJobObjectW(ptr::null(), ptr::null());
            if job.is_null() {
                return 0;
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
                return 0;
            }
            job as usize
        });
        (handle != 0).then_some(handle as HANDLE)
    }

    /// A running helper process.
    pub struct Helper {
        process: HANDLE,
        pub pid: u32,
        /// How it was started, for the logs.
        pub how: &'static str,
    }

    unsafe impl Send for Helper {}

    impl Helper {
        pub fn exited(&self) -> bool {
            unsafe { WaitForSingleObject(self.process, 0) != 258 }
        }

        pub fn terminate(&self) {
            unsafe { TerminateProcess(self.process, 1) };
        }
    }

    impl Drop for Helper {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.process) };
        }
    }

    /// Starts `exe --drag-probe <pipe> <agent pid>` in the agent's session, on the interactive
    /// desktop, without a window or console. An elevated agent uses a medium-integrity token;
    /// a non-elevated one is medium already and simply starts the process.
    pub fn launch_helper(exe: &str, pipe: &str) -> Result<Helper, String> {
        launch(exe, &helper_command_line(exe, pipe, unsafe { GetCurrentProcessId() }))
    }

    /// Starts `exe` with `command_line` the way [`launch_helper`] does.
    pub fn launch(exe: &str, command_line: &str) -> Result<Helper, String> {
        let elevated = is_elevated();
        let mut command = wide(command_line);
        let exe_w = wide(exe);
        let mut desktop = wide("winsta0\\default");
        let mut attempts: Vec<(&'static str, Option<Token>)> = Vec::new();
        if elevated {
            info!("Drag helper: the agent is elevated; starting the helper at medium integrity");
            for name in ["SeIncreaseQuotaPrivilege", "SeAssignPrimaryTokenPrivilege"] {
                if !enable_privilege(name) {
                    info!("Drag helper: {} is not available to the agent", name);
                }
            }
            attempts.extend(medium_tokens().into_iter().map(|(how, t)| (how, Some(t))));
            if attempts.is_empty() {
                return Err("no medium-integrity token could be derived from the agent's token".into());
            }
        } else {
            attempts.push(("the agent's own (non-elevated) token", None));
        }

        let mut last_error = String::new();
        for (how, token) in attempts {
            let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
            startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            startup.lpDesktop = desktop.as_mut_ptr();
            let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            let flags = CREATE_NO_WINDOW | CREATE_SUSPENDED;
            let ok = unsafe {
                match &token {
                    Some(t) => CreateProcessAsUserW(
                        t.0,
                        exe_w.as_ptr(),
                        command.as_mut_ptr(),
                        ptr::null(),
                        ptr::null(),
                        0,
                        flags,
                        ptr::null(),
                        ptr::null(),
                        &startup,
                        &mut info,
                    ),
                    None => CreateProcessW(
                        exe_w.as_ptr(),
                        command.as_mut_ptr(),
                        ptr::null(),
                        ptr::null(),
                        0,
                        flags,
                        ptr::null(),
                        ptr::null(),
                        &startup,
                        &mut info,
                    ),
                }
            };
            if ok == 0 {
                last_error = format!("{}: {}", how, std::io::Error::last_os_error());
                warn!("Drag helper: starting it with {} failed: {}", how, last_error);
                continue;
            }
            unsafe {
                match job() {
                    Some(job) if AssignProcessToJobObject(job, info.hProcess) != 0 => {}
                    _ => warn!(
                        "Drag helper: it could not be put in a kill-on-close job ({}); it still exits when the agent's pipe closes",
                        std::io::Error::last_os_error()
                    ),
                }
                ResumeThread(info.hThread);
                CloseHandle(info.hThread);
            }
            info!("Drag helper started with {} (pid {})", how, info.dwProcessId);
            return Ok(Helper { process: info.hProcess, pid: info.dwProcessId, how });
        }
        Err(last_error)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Starts `cmd` through the production launcher and reads what `whoami /groups` says
        /// about its integrity level.
        #[test]
        fn the_helper_token_is_medium_integrity() {
            let out = std::path::PathBuf::from(r"C:\Users\Public").join(format!("wb-medium-{}.txt", std::process::id()));
            let _ = std::fs::remove_file(&out);
            let exe = r"C:\Windows\System32\cmd.exe";
            let helper = launch(exe, &format!("\"{}\" /c whoami /groups > \"{}\"", exe, out.display())).expect("launch");
            for _ in 0..100 {
                if helper.exited() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let text = std::fs::read_to_string(&out).unwrap_or_default();
            println!("elevated agent: {}, started with {}\n{}", is_elevated(), helper.how, text);
            assert!(text.contains("Medium Mandatory Level"), "the helper is not medium integrity:\n{}", text);
            assert!(!text.contains("High Mandatory Level"));
            let _ = std::fs::remove_file(&out);
        }
    }
}
