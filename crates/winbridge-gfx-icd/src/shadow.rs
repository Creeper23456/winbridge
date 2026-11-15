// SPDX-License-Identifier: GPL-3.0-or-later

//! Guest RAM that backs `vkMapMemory` of a host-visible allocation.
//!
//! On Windows a shadow of a coherent memory type is allocated with `MEM_WRITE_WATCH`, so the
//! pages the application wrote since the last flush can be read back from the kernel
//! (`GetWriteWatch`) at no cost to the writer. Elsewhere (the Linux build used by the host
//! loopback tests) there is no write watch: the shadow keeps a clean copy and finds dirty
//! pages by comparing, which is correct but doubles the memory.

pub const PAGE: usize = 4096;
/// Not exported by windows-sys 0.59 (winnt.h values).
#[cfg(windows)]
const MEM_WRITE_WATCH: u32 = 0x0020_0000;
#[cfg(windows)]
const WRITE_WATCH_FLAG_RESET: u32 = 1;
/// Dirty runs separated by fewer than this many clean bytes are sent as one.
const MERGE_GAP: usize = 4 * PAGE;

pub struct ShadowMem {
    base: *mut u8,
    size: usize,
    watch: bool,
    #[cfg(not(windows))]
    clean: Vec<u8>,
}

// SAFETY: the pointer is owned by the value; users serialise access through the global mutex.
unsafe impl Send for ShadowMem {}

impl ShadowMem {
    /// Zeroed memory of `size` bytes. `watch`: track writes for `take_dirty`.
    pub fn new(size: usize, watch: bool) -> Option<ShadowMem> {
        let size = size.max(1);
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Memory::{VirtualAlloc, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE};
            let flags = MEM_RESERVE | MEM_COMMIT | if watch { MEM_WRITE_WATCH } else { 0 };
            // SAFETY: plain allocation; zero-filled by the OS.
            let base = unsafe { VirtualAlloc(std::ptr::null(), size, flags, PAGE_READWRITE) }.cast::<u8>();
            (!base.is_null()).then_some(ShadowMem { base, size, watch })
        }
        #[cfg(not(windows))]
        {
            let layout = std::alloc::Layout::from_size_align(size, PAGE).ok()?;
            // SAFETY: non-zero size.
            let base = unsafe { std::alloc::alloc_zeroed(layout) };
            (!base.is_null()).then(|| ShadowMem { base, size, watch, clean: if watch { vec![0; size] } else { Vec::new() } })
        }
    }

    pub fn base(&self) -> *mut u8 {
        self.base
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Byte ranges `(offset, len)` written since the previous call (or since creation), merged
    /// and clamped to the allocation. Resets the tracking.
    pub fn take_dirty(&mut self) -> Vec<(usize, usize)> {
        if !self.watch {
            return Vec::new();
        }
        let mut pages: Vec<usize> = Vec::new();
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Memory::GetWriteWatch;
            let mut start = 0usize;
            while start < self.size {
                let mut addrs = [std::ptr::null_mut::<std::ffi::c_void>(); 1024];
                let mut count = addrs.len();
                let mut gran = 0u32;
                // SAFETY: the range lies inside our MEM_WRITE_WATCH allocation.
                let rc = unsafe {
                    GetWriteWatch(WRITE_WATCH_FLAG_RESET, self.base.add(start).cast(), self.size - start, addrs.as_mut_ptr(), &mut count, &mut gran)
                };
                if rc != 0 {
                    // Cannot tell what changed: treat everything as dirty.
                    return vec![(0, self.size)];
                }
                pages.extend(addrs[..count].iter().map(|a| *a as usize - self.base as usize));
                if count < addrs.len() {
                    break;
                }
                start = pages.last().map_or(self.size, |p| p + gran as usize);
            }
        }
        #[cfg(not(windows))]
        {
            for off in (0..self.size).step_by(PAGE) {
                let end = (off + PAGE).min(self.size);
                // SAFETY: inside the allocation.
                let now = unsafe { std::slice::from_raw_parts(self.base.add(off), end - off) };
                if now != &self.clean[off..end] {
                    self.clean[off..end].copy_from_slice(now);
                    pages.push(off);
                }
            }
        }
        let mut out: Vec<(usize, usize)> = Vec::new();
        for p in pages {
            let end = (p + PAGE).min(self.size);
            match out.last_mut() {
                Some(r) if p <= r.0 + r.1 + MERGE_GAP => r.1 = end - r.0,
                _ => out.push((p, end - p)),
            }
        }
        out
    }
}

impl Drop for ShadowMem {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: `base` came from VirtualAlloc; MEM_RELEASE needs size 0.
        unsafe {
            windows_sys::Win32::System::Memory::VirtualFree(self.base.cast(), 0, windows_sys::Win32::System::Memory::MEM_RELEASE);
        }
        #[cfg(not(windows))]
        // SAFETY: allocated in `new` with this layout.
        unsafe {
            std::alloc::dealloc(self.base, std::alloc::Layout::from_size_align(self.size, PAGE).unwrap());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_pages_are_found_merged_and_reset() {
        let mut m = ShadowMem::new(64 * PAGE, true).unwrap();
        assert!(m.take_dirty().is_empty());
        unsafe {
            *m.base().add(5) = 1;
            *m.base().add(PAGE + 1) = 1;
            *m.base().add(40 * PAGE + 7) = 9;
            *m.base().add(64 * PAGE - 1) = 3;
        }
        let d = m.take_dirty();
        assert_eq!(d, vec![(0, 2 * PAGE), (40 * PAGE, PAGE), (63 * PAGE, PAGE)]);
        assert!(m.take_dirty().is_empty(), "tracking must reset");
        let mut plain = ShadowMem::new(PAGE, false).unwrap();
        unsafe { *plain.base() = 1 };
        assert!(plain.take_dirty().is_empty());
    }
}
