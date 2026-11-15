// SPDX-License-Identifier: GPL-3.0-or-later

//! Byte transport of the ICD: the `org.winbridge.gfx.0` virtio-serial port on Windows, a
//! Unix socket named by `WINBRIDGE_GFX_SOCKET` elsewhere (the host-side loopback tests).

use winbridge_gfx_proto::frame::{MessageTransport, StreamTransport};

/// Longest wait for the host to answer or accept bytes.
pub const IO_TIMEOUT_SECS: u64 = 30;


#[cfg(unix)]
pub fn open() -> std::io::Result<Box<dyn MessageTransport>> {
    use std::io;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let path = std::env::var("WINBRIDGE_GFX_SOCKET")
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "WINBRIDGE_GFX_SOCKET is not set"))?;
    let s = UnixStream::connect(path)?;
    s.set_read_timeout(Some(Duration::from_secs(IO_TIMEOUT_SECS)))?;
    s.set_write_timeout(Some(Duration::from_secs(IO_TIMEOUT_SECS)))?;
    let r = s.try_clone()?;
    Ok(Box::new(StreamTransport::new(r, s)))
}

#[cfg(windows)]
pub use win::open;

#[cfg(windows)]
mod win {
    use super::{MessageTransport, StreamTransport, IO_TIMEOUT_SECS};
    use std::ffi::c_void;
    use std::io::{self, Read, Write};
    use std::ptr::{null, null_mut};
    use std::sync::Arc;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
        WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, OPEN_EXISTING};
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    const PORT: &str = r"\\.\Global\org.winbridge.gfx.0";
    /// One ReadFile/WriteFile moves at most this much (the vioserial driver locks the buffer).
    const MAX_IO: usize = 1 << 20;

    struct Port(HANDLE);
    // SAFETY: a file handle may be used from any thread.
    unsafe impl Send for Port {}
    unsafe impl Sync for Port {}

    impl Drop for Port {
        fn drop(&mut self) {
            // SAFETY: the handle was opened by `open` and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// One direction of the port with its own completion event, so a pending read never blocks a write.
    struct Half {
        port: Arc<Port>,
        event: HANDLE,
    }
    // SAFETY: see `Port`.
    unsafe impl Send for Half {}

    impl Drop for Half {
        fn drop(&mut self) {
            // SAFETY: the event was created in `Half::new`.
            unsafe { CloseHandle(self.event) };
        }
    }

    impl Half {
        fn new(port: Arc<Port>) -> io::Result<Half> {
            // SAFETY: plain event creation.
            let event = unsafe { CreateEventW(null(), 1, 0, null()) };
            if event.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Half { port, event })
        }

        fn xfer(&self, write: bool, buf: *mut c_void, len: usize) -> io::Result<usize> {
            let len = len.min(MAX_IO) as u32;
            // SAFETY: OVERLAPPED is plain data; `buf` is valid for `len` bytes for the whole call,
            // which does not return until the operation completed or was cancelled.
            unsafe {
                let mut ov: OVERLAPPED = std::mem::zeroed();
                ov.hEvent = self.event;
                let mut n = 0u32;
                let ok = if write {
                    WriteFile(self.port.0, buf as *const u8, len, &mut n, &mut ov)
                } else {
                    ReadFile(self.port.0, buf as *mut u8, len, &mut n, &mut ov)
                };
                if ok == 0 {
                    if GetLastError() != ERROR_IO_PENDING {
                        return Err(io::Error::last_os_error());
                    }
                    if WaitForSingleObject(self.event, (IO_TIMEOUT_SECS * 1000) as u32) != WAIT_OBJECT_0 {
                        CancelIoEx(self.port.0, &ov);
                        GetOverlappedResult(self.port.0, &ov, &mut n, 1);
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "host did not answer in time"));
                    }
                    if GetOverlappedResult(self.port.0, &ov, &mut n, 0) == 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(n as usize)
            }
        }
    }

    impl Read for Half {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.xfer(false, buf.as_mut_ptr().cast(), buf.len())
        }
    }

    impl Write for Half {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.xfer(true, buf.as_ptr() as *mut c_void, buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    pub fn open() -> io::Result<Box<dyn MessageTransport>> {
        let wide: Vec<u16> = PORT.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated.
        let h = unsafe { CreateFileW(wide.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, null(), OPEN_EXISTING, FILE_FLAG_OVERLAPPED, null_mut()) };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let port = Arc::new(Port(h));
        Ok(Box::new(StreamTransport::new(Half::new(port.clone())?, Half::new(port)?)))
    }
}
