// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows clipboard synchronization service.

#[cfg(windows)]
mod imp {
    use std::ptr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    #[allow(unused_imports)]
    use tracing::{error, info, warn};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::DataExchange::{
        AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
        OpenClipboard, RemoveClipboardFormatListener, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GHND,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        PostMessageW, RegisterClassW, TranslateMessage, MSG, WM_CLIPBOARDUPDATE, WM_USER,
        WNDCLASSW,
    };

    const CF_UNICODETEXT: u32 = 13;
    const WM_STOP_CLIPBOARD: u32 = WM_USER + 101;

    /// Reads plain UTF-16 text from the Windows clipboard and converts to UTF-8 String.
    pub fn read_clipboard_text() -> Option<String> {
        unsafe {
            if OpenClipboard(ptr::null_mut()) == 0 {
                return None;
            }

            let handle = GetClipboardData(CF_UNICODETEXT);
            if handle.is_null() {
                CloseClipboard();
                return None;
            }

            let ptr = GlobalLock(handle) as *const u16;
            if ptr.is_null() {
                CloseClipboard();
                return None;
            }

            // Find null terminator
            let mut len = 0;
            while *ptr.add(len) != 0 {
                len += 1;
            }

            let slice = std::slice::from_raw_parts(ptr, len);
            let text = String::from_utf16_lossy(slice);

            GlobalUnlock(handle);
            CloseClipboard();

            Some(text)
        }
    }

    /// Writes a UTF-8 String into the Windows clipboard as CF_UNICODETEXT.
    pub fn write_clipboard_text(text: &str) -> bool {
        let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes_len = utf16.len() * 2;

        unsafe {
            if OpenClipboard(ptr::null_mut()) == 0 {
                return false;
            }

            EmptyClipboard();

            let h_mem = GlobalAlloc(GHND, bytes_len);
            if h_mem.is_null() {
                CloseClipboard();
                return false;
            }

            let ptr = GlobalLock(h_mem) as *mut u16;
            if ptr.is_null() {
                CloseClipboard();
                return false;
            }

            ptr::copy_nonoverlapping(utf16.as_ptr(), ptr, utf16.len());
            GlobalUnlock(h_mem);

            if SetClipboardData(CF_UNICODETEXT, h_mem).is_null() {
                CloseClipboard();
                return false;
            }

            CloseClipboard();
            true
        }
    }

    #[allow(dead_code)]
    pub struct ClipboardWatcher {
        hwnd: HWND,
        suppress_echo: Arc<AtomicBool>,
    }

    impl ClipboardWatcher {
        pub fn start<F>(on_change: F, suppress_echo: Arc<AtomicBool>) -> Self
        where
            F: Fn(String) + Send + 'static,
        {
            let (tx, rx) = std::sync::mpsc::channel::<isize>();
            let suppress_clone = suppress_echo.clone();

            std::thread::spawn(move || {
                let class_name: Vec<u16> = "WinBridgeClipboardClass\0".encode_utf16().collect();
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

                unsafe {
                    RegisterClassW(&wc);
                    let hwnd = CreateWindowExW(
                        0,
                        class_name.as_ptr(),
                        ptr::null(),
                        0,
                        0,
                        0,
                        0,
                        0,
                        ptr::null_mut(), // HWND_MESSAGE
                        ptr::null_mut(),
                        ptr::null_mut(),
                        ptr::null(),
                    );

                    if hwnd.is_null() {
                        error!("Failed to create clipboard message window");
                        let _ = tx.send(0);
                        return;
                    }

                    if AddClipboardFormatListener(hwnd) == 0 {
                        error!("Failed to register clipboard format listener");
                    }

                    let _ = tx.send(hwnd as isize);

                    let mut msg: MSG = std::mem::zeroed();
                    while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
                        if msg.message == WM_STOP_CLIPBOARD {
                            DestroyWindow(hwnd);
                            break;
                        }

                        if msg.message == WM_CLIPBOARDUPDATE {
                            if suppress_clone.swap(false, Ordering::SeqCst) {
                                // Echo event suppressed
                                continue;
                            }

                            if let Some(text) = read_clipboard_text() {
                                on_change(text);
                            }
                        }

                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }

                    RemoveClipboardFormatListener(hwnd);
                }
            });

            let hwnd = rx.recv().unwrap_or(0) as HWND;
            Self { hwnd, suppress_echo }
        }
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    impl Drop for ClipboardWatcher {
        fn drop(&mut self) {
            if !self.hwnd.is_null() {
                unsafe {
                    PostMessageW(self.hwnd, WM_STOP_CLIPBOARD, 0, 0);
                }
            }
        }
    }
}

#[cfg(not(windows))]
#[allow(dead_code, unused_imports)]
mod imp {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    pub fn read_clipboard_text() -> Option<String> {
        Some("Mock clipboard content".into())
    }

    pub fn write_clipboard_text(text: &str) -> bool {
        tracing::info!("[Mock] Wrote to clipboard: {}", text);
        true
    }

    pub struct ClipboardWatcher {
        pub suppress_echo: Arc<AtomicBool>,
    }

    impl ClipboardWatcher {
        pub fn start<F>(_on_change: F, suppress_echo: Arc<AtomicBool>) -> Self
        where
            F: Fn(String) + Send + 'static,
        {
            Self { suppress_echo }
        }
    }
}

pub use imp::*;
