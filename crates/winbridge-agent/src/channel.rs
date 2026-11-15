// SPDX-License-Identifier: GPL-3.0-or-later

//! VirtIO serial channel communication abstraction.
//!
//! The port handle is opened for overlapped I/O: a synchronous handle serialises every call per
//! file object, so a thread blocked in `ReadFile` would stall all writes (clipboard, replies).
//! Clones share one handle; each read/write uses its own event, so one reader thread and any
//! number of (externally serialised) writers run concurrently.

use std::io::{self, Read, Write};
#[allow(unused_imports)]
use tracing::{error, info, warn};

#[cfg(windows)]
pub mod sys {
    use super::*;
    use std::ptr;
    use std::sync::Arc;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Threading::CreateEventW;
    use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

    struct PortHandle(HANDLE);

    unsafe impl Send for PortHandle {}
    unsafe impl Sync for PortHandle {}

    impl Drop for PortHandle {
        fn drop(&mut self) {
            if self.0 != INVALID_HANDLE_VALUE {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    /// Cloneable handle to the VirtIO serial port.
    #[derive(Clone)]
    pub struct VirtioSerialStream {
        handle: Arc<PortHandle>,
    }

    impl VirtioSerialStream {
        /// Opens the Windows VirtIO serial port, retrying until available.
        pub fn open(port_name: Option<&str>) -> Self {
            let primary = port_name.unwrap_or(r"\\.\Global\org.winbridge.0");
            let secondary = r"\\.\org.winbridge.0";

            let wide_primary: Vec<u16> = primary.encode_utf16().chain(std::iter::once(0)).collect();
            let wide_secondary: Vec<u16> = secondary.encode_utf16().chain(std::iter::once(0)).collect();

            let open_path = |path: &Vec<u16>| unsafe {
                CreateFileW(
                    path.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    ptr::null_mut(),
                )
            };

            loop {
                let handle = open_path(&wide_primary);
                if handle != INVALID_HANDLE_VALUE {
                    info!("Successfully opened VirtIO serial port at {}", primary);
                    return Self {
                        handle: Arc::new(PortHandle(handle)),
                    };
                }

                let handle_sec = open_path(&wide_secondary);
                if handle_sec != INVALID_HANDLE_VALUE {
                    info!("Successfully opened VirtIO serial port at {}", secondary);
                    return Self {
                        handle: Arc::new(PortHandle(handle_sec)),
                    };
                }

                warn!("VirtIO serial port not available yet, retrying in 1s...");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }

    /// Runs one overlapped operation to completion with a private event.
    fn overlapped_io(
        handle: HANDLE,
        start: impl FnOnce(*mut OVERLAPPED) -> i32,
    ) -> io::Result<usize> {
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event;

        let started = start(&mut overlapped);
        let pending = started == 0 && unsafe { GetLastError() } == ERROR_IO_PENDING;

        let result = if started != 0 || pending {
            let mut transferred: u32 = 0;
            if unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 1) } != 0 {
                Ok(transferred as usize)
            } else {
                Err(io::Error::last_os_error())
            }
        } else {
            Err(io::Error::last_os_error())
        };

        unsafe {
            CloseHandle(event);
        }
        result
    }

    impl Read for VirtioSerialStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let handle = self.handle.0;
            overlapped_io(handle, |ov| unsafe {
                ReadFile(
                    handle,
                    buf.as_mut_ptr() as _,
                    buf.len() as u32,
                    ptr::null_mut(),
                    ov,
                )
            })
        }
    }

    impl Write for VirtioSerialStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let handle = self.handle.0;
            overlapped_io(handle, |ov| unsafe {
                WriteFile(
                    handle,
                    buf.as_ptr() as _,
                    buf.len() as u32,
                    ptr::null_mut(),
                    ov,
                )
            })
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(not(windows))]
pub mod sys {
    use super::*;

    #[derive(Clone)]
    pub struct VirtioSerialStream;

    impl VirtioSerialStream {
        pub fn open(_port_name: Option<&str>) -> Self {
            Self
        }
    }

    impl Read for VirtioSerialStream {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(std::time::Duration::from_millis(500));
            Ok(0)
        }
    }

    impl Write for VirtioSerialStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}

pub use sys::VirtioSerialStream;
