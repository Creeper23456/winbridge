// SPDX-License-Identifier: GPL-3.0-or-later

//! Who the agent works for.
//!
//! The agent runs as `NT AUTHORITY\SYSTEM`: the service ([`crate::service`]) starts it in the
//! console session, with the environment block of the logged-on user. What it handles (dropped
//! and pasted files, the Downloads folder, the HKCU settings) belongs to that user, so threads
//! that touch the user's data impersonate the user's token ([`impersonate`]): files they create
//! are the user's, `HKEY_CURRENT_USER` is the user's hive, known folders resolve to the user's.
//!
//! An agent that is not SYSTEM (started by hand, the build VM's self-tests) already is the user:
//! nothing is impersonated there and the user is the token's own.

#[cfg(windows)]
pub use imp::*;

#[cfg(windows)]
mod imp {
    use std::ptr;
    use std::sync::LazyLock;
    use tracing::{info, warn};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::Security::{
        EqualSid, GetLengthSid, GetTokenInformation, ImpersonateLoggedOnUser, RevertToSelf, TokenUser, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId, WTSQueryUserToken};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, OpenProcessToken};

    /// `S-1-5-18`, `NT AUTHORITY\SYSTEM`, in binary form.
    const SYSTEM_SID: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];

    /// No session is attached to the console (`WTSGetActiveConsoleSessionId`).
    const NO_SESSION: u32 = 0xFFFF_FFFF;

    /// The logged-on user's primary token and SID.
    pub struct User {
        token: HANDLE,
        sid: Vec<u8>,
    }

    // SAFETY: a token handle may be used from any thread; it is never closed while shared.
    unsafe impl Send for User {}
    unsafe impl Sync for User {}

    impl User {
        pub fn token(&self) -> HANDLE {
            self.token
        }

        pub fn sid(&self) -> &[u8] {
            &self.sid
        }
    }

    impl Drop for User {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.token) };
        }
    }

    fn sid_of_token(token: HANDLE) -> Option<Vec<u8>> {
        let mut len = 0u32;
        unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut len) };
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        if unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) } == 0 {
            return None;
        }
        unsafe {
            let sid = (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid;
            Some(std::slice::from_raw_parts(sid as *const u8, GetLengthSid(sid) as usize).to_vec())
        }
    }

    /// The session that owns the console (the one the viewer shows), if there is one.
    pub fn console_session() -> Option<u32> {
        let id = unsafe { WTSGetActiveConsoleSessionId() };
        (id != NO_SESSION).then_some(id)
    }

    /// The user logged on in `session`; `None` while nobody is (the logon screen).
    pub fn query_user(session: u32) -> Option<User> {
        let mut token: HANDLE = ptr::null_mut();
        if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
            return None;
        }
        match sid_of_token(token) {
            Some(sid) => Some(User { token, sid }),
            None => {
                unsafe { CloseHandle(token) };
                None
            }
        }
    }

    /// Whether this process runs as SYSTEM.
    pub fn is_system() -> bool {
        let mut token: HANDLE = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return false;
        }
        let own = sid_of_token(token);
        unsafe { CloseHandle(token) };
        own.is_some_and(|sid| unsafe { EqualSid(sid.as_ptr() as _, SYSTEM_SID.as_ptr() as _) } != 0)
    }

    /// The user the agent works for: the one logged on in the agent's own session while it runs
    /// as SYSTEM, the process's own user otherwise.
    struct Worker {
        /// Set only when the agent is SYSTEM and a user is logged on.
        user: Option<User>,
        /// The SID the files belong to.
        sid: Option<Vec<u8>>,
    }

    static WORKER: LazyLock<Worker> = LazyLock::new(|| {
        if !is_system() {
            let mut token: HANDLE = ptr::null_mut();
            let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } != 0;
            let own = if opened { sid_of_token(token) } else { None };
            if opened {
                unsafe { CloseHandle(token) };
            }
            return Worker { user: None, sid: own };
        }
        let mut session = 0u32;
        if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 {
            warn!("Identity: cannot read the agent's session: error {}", unsafe { GetLastError() });
            return Worker { user: None, sid: None };
        }
        match query_user(session) {
            Some(user) => {
                info!("Identity: running as SYSTEM in session {}, working for the logged-on user", session);
                let sid = Some(user.sid.clone());
                Worker { user: Some(user), sid }
            }
            None => {
                info!("Identity: running as SYSTEM in session {}, nobody is logged on", session);
                Worker { user: None, sid: None }
            }
        }
    });

    /// Resolves the user now rather than on first use. Call before the first thread is started.
    pub fn init() {
        LazyLock::force(&WORKER);
    }

    /// The SID of the user whose files these are (`None` at the logon screen).
    pub fn user_sid() -> Option<&'static [u8]> {
        WORKER.sid.as_deref()
    }

    /// The logged-on user's token, only while the agent is SYSTEM and impersonation is needed.
    pub fn user_token() -> Option<HANDLE> {
        WORKER.user.as_ref().map(User::token)
    }

    /// The calling thread acts as the user until this is dropped.
    pub struct Impersonation(());

    impl Drop for Impersonation {
        fn drop(&mut self) {
            unsafe { RevertToSelf() };
        }
    }

    /// Makes the calling thread act as the logged-on user. `None` when there is nothing to do
    /// (the agent already is the user) or nobody is logged on.
    pub fn impersonate() -> Option<Impersonation> {
        let token = user_token()?;
        if unsafe { ImpersonateLoggedOnUser(token) } == 0 {
            warn!("Identity: cannot impersonate the user: error {}", unsafe { GetLastError() });
            return None;
        }
        Some(Impersonation(()))
    }

    /// Starts a named thread that acts as the user for its whole life.
    pub fn spawn<F>(name: &str, f: F) -> std::io::Result<std::thread::JoinHandle<()>>
    where
        F: FnOnce() + Send + 'static,
    {
        std::thread::Builder::new().name(name.into()).spawn(move || {
            let _user = impersonate();
            f();
        })
    }
}

#[cfg(not(windows))]
mod imp {
    /// Nothing to resolve off Windows.
    pub fn init() {}

    /// Nothing to impersonate off Windows.
    pub fn impersonate() -> Option<()> {
        None
    }

    pub fn spawn<F>(name: &str, f: F) -> std::io::Result<std::thread::JoinHandle<()>>
    where
        F: FnOnce() + Send + 'static,
    {
        std::thread::Builder::new().name(name.into()).spawn(f)
    }
}

#[cfg(not(windows))]
pub use imp::*;
