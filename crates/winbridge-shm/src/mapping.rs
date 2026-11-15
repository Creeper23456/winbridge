// SPDX-License-Identifier: GPL-3.0-or-later

//! The memory a [`Region`](crate::Region) lives in: the ivshmem BAR in production,
//! a heap block or a memory-mapped file in tests.

use std::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};
use std::sync::Arc;

/// A block of memory shared with the peer.
///
/// # Safety
///
/// Implementors guarantee that, for the whole lifetime of the value, `base()`
/// points to `len()` bytes that are valid for reads and writes, and that the
/// pointer and length never change. The memory may be modified concurrently by
/// another party (that is the point); the crate only accesses it through
/// raw-pointer reads/writes and atomics.
pub unsafe trait Mapping: Send + Sync + 'static {
    /// First byte of the mapping.
    fn base(&self) -> *mut u8;
    /// Length of the mapping in bytes.
    fn len(&self) -> usize;
    /// True for an empty mapping.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct Block {
    ptr: *mut u8,
    layout: Layout,
}

// SAFETY: the block is plain memory owned by this value; access discipline is
// enforced by the ring protocol, not by the type.
unsafe impl Send for Block {}
unsafe impl Sync for Block {}

impl Drop for Block {
    fn drop(&mut self) {
        // SAFETY: allocated in `HeapMapping::new` with this layout.
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

/// Zero-initialised, 4096-aligned heap block. Stands in for the BAR in unit
/// tests that must also run under Miri (which cannot `mmap`). `clone()` yields
/// a second handle to the *same* memory, like a second mapping of one file.
#[derive(Clone)]
pub struct HeapMapping {
    block: Arc<Block>,
}

impl HeapMapping {
    /// Allocate `len` zeroed bytes aligned to 4096. Panics if `len == 0`.
    pub fn new(len: usize) -> Self {
        assert!(len > 0, "empty mapping");
        let layout = Layout::from_size_align(len, 4096).expect("mapping layout");
        // SAFETY: layout has non-zero size.
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        Self {
            block: Arc::new(Block { ptr, layout }),
        }
    }
}

// SAFETY: `ptr`/`layout` are fixed and the block lives as long as any handle.
unsafe impl Mapping for HeapMapping {
    fn base(&self) -> *mut u8 {
        self.block.ptr
    }
    fn len(&self) -> usize {
        self.block.layout.size()
    }
}
