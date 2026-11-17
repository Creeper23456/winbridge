// SPDX-License-Identifier: GPL-3.0-or-later

//! Console and log setup for the agent.
//!
//! The Windows build is a GUI-subsystem program so that starting it at logon does not open a
//! console window. When it is started from a terminal it attaches to that terminal and logs
//! there; otherwise it appends to `%LOCALAPPDATA%\WinBridge\agent.log`.

use tracing_subscriber::EnvFilter;

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Attaches to the parent's console (if any) so `println!` works from a terminal.
/// Returns whether a console is available.
#[cfg(windows)]
pub fn attach_console() -> bool {
    use std::fs::OpenOptions;
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{
        AttachConsole, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } == 0 {
        return false;
    }
    // A GUI-subsystem process starts without standard handles: point them at the console.
    if let Ok(out) = OpenOptions::new().write(true).open("CONOUT$") {
        let handle = out.into_raw_handle();
        unsafe {
            SetStdHandle(STD_OUTPUT_HANDLE, handle as _);
            SetStdHandle(STD_ERROR_HANDLE, handle as _);
        }
        return true;
    }
    false
}

#[cfg(not(windows))]
pub fn attach_console() -> bool {
    true
}

/// Installs the global tracing subscriber.
pub fn init(has_console: bool) {
    init_as(has_console, "agent.log");
}

/// Like [`init`], logging to `file_name` below `%LOCALAPPDATA%\WinBridge` when there is no console.
pub fn init_as(has_console: bool, file_name: &str) {
    if has_console {
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter(env_filter())
            .init();
        return;
    }

    let dir = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("WinBridge");
    init_file(&dir, file_name);
}

/// Logs to `file_name` below `%ProgramData%\WinBridge`: for the service, which is nobody's
/// program and has no profile worth writing to.
pub fn init_service(file_name: &str) {
    let dir = std::env::var_os("ProgramData")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("WinBridge");
    init_file(&dir, file_name);
}

fn init_file(dir: &std::path::Path, file_name: &str) {
    let _ = std::fs::create_dir_all(dir);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(file_name))
    {
        Ok(file) => tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter(env_filter())
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        Err(_) => tracing_subscriber::fmt()
            .with_writer(std::io::sink)
            .with_env_filter(env_filter())
            .init(),
    }
}
