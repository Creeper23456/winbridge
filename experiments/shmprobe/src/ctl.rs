// SPDX-License-Identifier: GPL-3.0-or-later

//! Fixed-offset mailbox in the first page of a device, used to lock-step the host and guest
//! processes of the non-ring tests (pattern, ordering). Not part of the ring format.

use std::sync::atomic::{AtomicU64, Ordering};

/// Written by the host once the mailbox is zeroed and it is serving.
pub const READY_MAGIC: u64 = 0x5742_5052_4f42_4531;

// pattern test
pub const G_EPOCH: usize = 0x000;
pub const G_MODE: usize = 0x008;
pub const H_ACK: usize = 0x010;
pub const H_BAD: usize = 0x018;
pub const H_EPOCH: usize = 0x020;
pub const G_ACK: usize = 0x028;
pub const G_BAD: usize = 0x030;
pub const G_DONE: usize = 0x038;
pub const HOST_READY: usize = 0x040;

// ordering test
pub const O_RUN: usize = 0x080;
pub const O_LABEL: usize = 0x088;
pub const O_SEEN: usize = 0x090;
pub const O_DONE: usize = 0x098;
pub const O_HDONE: usize = 0x0A0;
pub const O_VIOL: usize = 0x0A8;
pub const O_SAMPLES: usize = 0x0B0;
pub const O_TORN: usize = 0x0B8;
pub const O_ROUNDS: usize = 0x0C0;
pub const O_ALL: usize = 0x0C8;
pub const O_LAG: usize = 0x0D0;
pub const O_FINAL_BAD: usize = 0x0D8;

/// Mailbox accessor over the first page of a mapping.
#[derive(Clone, Copy)]
pub struct Ctl(*mut u8);

// SAFETY: only atomic accesses through the pointer; the mapping outlives users by construction.
unsafe impl Send for Ctl {}
unsafe impl Sync for Ctl {}

impl Ctl {
    /// # Safety
    /// `base` must point to at least one page of mapped shared memory that outlives the value.
    pub unsafe fn new(base: *mut u8) -> Self {
        Self(base)
    }

    fn at(&self, off: usize) -> &AtomicU64 {
        // SAFETY: `off` is one of the 8-aligned constants above, inside the first page.
        unsafe { &*(self.0.add(off) as *const AtomicU64) }
    }

    pub fn get(&self, off: usize) -> u64 {
        self.at(off).load(Ordering::Acquire)
    }

    pub fn set(&self, off: usize, v: u64) {
        self.at(off).store(v, Ordering::Release)
    }
}
