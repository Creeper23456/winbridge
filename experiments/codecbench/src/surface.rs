// SPDX-License-Identifier: GPL-3.0-or-later
//! Output surface: a 1080p top-down BGRA DIB section on Windows (the real target), a plain
//! page-aligned allocation elsewhere.

use crate::util::{Dst, FRAME_BYTES};

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use windows::Win32::Graphics::Gdi::*;

    pub struct Surface {
        ptr: *mut u8,
        bitmap: HBITMAP,
    }

    impl Surface {
        pub fn new() -> Surface {
            Self::with_size(crate::util::W as i32, crate::util::H as i32)
        }

        pub fn with_size(w: i32, h: i32) -> Surface {
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            // SAFETY: valid BITMAPINFO and out-pointer; no HDC needed for DIB_RGB_COLORS.
            let bitmap = unsafe { CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }.expect("CreateDIBSection");
            assert!(!bits.is_null());
            Surface { ptr: bits as *mut u8, bitmap }
        }

        pub fn bitmap(&self) -> HBITMAP {
            self.bitmap
        }

        pub fn dst(&self) -> Dst {
            Dst(self.ptr)
        }

        pub fn kind() -> &'static str {
            "DIB section"
        }
    }

    impl Drop for Surface {
        fn drop(&mut self) {
            // SAFETY: the bitmap was created by us and is not selected into any DC at this point.
            unsafe {
                let _ = DeleteObject(self.bitmap.into());
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub struct Surface {
        buf: Vec<u8>,
    }

    impl Surface {
        pub fn new() -> Surface {
            Surface { buf: vec![0u8; FRAME_BYTES] }
        }

        pub fn dst(&self) -> Dst {
            Dst(self.buf.as_ptr() as *mut u8)
        }

        pub fn kind() -> &'static str {
            "Vec<u8>"
        }
    }
}

pub use imp::Surface;

impl Surface {
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the surface is FRAME_BYTES long and no decoder is running while the slice is alive.
        unsafe { std::slice::from_raw_parts(self.dst().0, FRAME_BYTES) }
    }
}
