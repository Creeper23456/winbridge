// SPDX-License-Identifier: GPL-3.0-or-later

//! VirtIO serial channel communication abstraction.

use std::io::{self, Read, Write};
#[allow(unused_imports)]
use tracing::{error, info, warn};

#[cfg(windows)]
pub mod sys {
    use super::*;
    use std::ptr;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING,
    };

    pub struct VirtioSerialStream {
        handle: HANDLE,
    }

    unsafe impl Send for VirtioSerialStream {}
    unsafe impl Sync for VirtioSerialStream {}

    impl VirtioSerialStream {
        /// Opens the Windows VirtIO serial port, retrying until available.
        pub fn open(port_name: Option<&str>) -> Self {
            let primary = port_name.unwrap_or(r"\\.\Global\org.winbridge.0");
            let secondary = r"\\.\org.winbridge.0";

            let wide_primary: Vec<u16> = primary.encode_utf16().chain(std::iter::once(0)).collect();
            let wide_secondary: Vec<u16> = secondary.encode_utf16().chain(std::iter::once(0)).collect();

            loop {
                let handle = unsafe {
                    CreateFileW(
                        wide_primary.as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        0,
                        ptr::null(),
                        OPEN_EXISTING,
                        FILE_ATTRIBUTE_NORMAL,
                        ptr::null_mut(),
                    )
                };

                if handle != INVALID_HANDLE_VALUE {
                    info!("Successfully opened VirtIO serial port at {}", primary);
                    return Self { handle };
                }

                // Try secondary path
                let handle_sec = unsafe {
                    CreateFileW(
                        wide_secondary.as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        0,
                        ptr::null(),
                        OPEN_EXISTING,
                        FILE_ATTRIBUTE_NORMAL,
                        ptr::null_mut(),
                    )
                };

                if handle_sec != INVALID_HANDLE_VALUE {
                    info!("Successfully opened VirtIO serial port at {}", secondary);
                    return Self { handle: handle_sec };
                }

                warn!("VirtIO serial port not available yet, retrying in 1s...");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }

    impl Read for VirtioSerialStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut bytes_read = 0;
            let success = unsafe {
                ReadFile(
                    self.handle,
                    buf.as_mut_ptr() as _,
                    buf.len() as u32,
                    &mut bytes_read,
                    ptr::null_mut(),
                )
            };

            if success == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(bytes_read as usize)
            }
        }
    }

    impl Write for VirtioSerialStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut bytes_written = 0;
            let success = unsafe {
                WriteFile(
                    self.handle,
                    buf.as_ptr() as _,
                    buf.len() as u32,
                    &mut bytes_written,
                    ptr::null_mut(),
                )
            };

            if success == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(bytes_written as usize)
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for VirtioSerialStream {
        fn drop(&mut self) {
            if self.handle != INVALID_HANDLE_VALUE {
                unsafe {
                    CloseHandle(self.handle);
                }
            }
        }
    }
}

#[cfg(not(windows))]
pub mod sys {
    use super::*;

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
