// SPDX-License-Identifier: GPL-3.0-or-later

//! # winbridge-shm: shared-memory lane transport core
//!
//! Implements `docs/IVSHMEM-TRANSPORT.md` sections 3-5 and 7 independent of
//! QEMU and Windows. The memory is any [`Mapping`]: the ivshmem BAR in
//! production, a [`HeapMapping`] or a memory-mapped file in tests.
//!
//! ## Layout (all little-endian)
//!
//! Header page (4096 bytes at offset 0):
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0x00 | 8 | magic `"WBSHMEM\0"` (written last by `create`) |
//! | 0x08 | 4 | version (1) |
//! | 0x0C | 4 | header length (4096) |
//! | 0x10 | 8 | capability bits |
//! | 0x18 | 8 | region length (must not exceed the mapping) |
//! | 0x20 | 4 | host role |
//! | 0x24 | 4 | guest role |
//! | 0x28 | 4 | lane count (max 64) |
//! | 0x40 | 32 x n | lane directory |
//!
//! Directory entry (32 bytes): `u16 id`, `u8 direction` (0 host-to-guest,
//! 1 guest-to-host), `u8 priority` (0 control, 1 high, 2 bulk), `u32 flags`,
//! `u64 offset` (of the ring header, 64-aligned), `u64 size` (data area,
//! power of two, >= 64), 8 reserved bytes.
//!
//! Ring (at `offset`): 128-byte header, then `size` data bytes.
//!
//! | Offset | Field | Written by |
//! |---|---|---|
//! | +0 | `head` u64, free-running byte count | producer (cache line 0) |
//! | +8 | producer flags (bit 0 closed) | producer |
//! | +64 | `tail` u64, free-running byte count | consumer (cache line 1) |
//! | +72 | consumer flags (bit 0 sleeping, bit 1 closed) | consumer |
//!
//! Record: `u32 payload length`, `u16 tag`, `u16 flags`, payload, zero padding
//! to a multiple of 8. A record never wraps; flag `0x8000` marks a skip
//! marker covering the rest of the data area.
//!
//! ## Invariants
//!
//! * Each index has exactly one writer; both are multiples of 8 and
//!   `tail <= head <= tail + size`. Positions are `index & (size - 1)`.
//! * The producer writes payload, calls [`publish_fence`] (`sfence` on x86-64,
//!   needed if the BAR mapping is write-combining), then stores `head` with
//!   release ordering. The consumer loads `head` with acquire, copies out,
//!   then stores `tail` with release.
//! * Nothing in shared memory is trusted. `attach` validates magic, version,
//!   every directory entry (known enums, power-of-two size, aligned offset,
//!   inside the declared and mapped length, no overlap, unique ids) and keeps
//!   a private copy of the lane table; the directory is never re-read.
//!   Each shared field is read once into a local before use.
//! * The consumer **copies out, then hands the copy to the caller**; no API
//!   exposes references into shared memory. Record length is checked against
//!   the lane cap, the published bytes and the data-area end *before* any
//!   allocation. A backwards index, an index distance above the data size, a
//!   misaligned index, an over-long/over-running record or a bad skip closes
//!   the lane with [`LaneError::Corrupt`]; the closed state is sticky and the
//!   peer is told through the consumer/producer closed flag.
//! * The producer applies the same checks to the consumer's `tail`.
//! * Wake-up: see [`wake`]. Scheduling: see [`sched`].
//!
//! ## API sketch
//!
//! ```
//! use winbridge_shm::*;
//! let bar = HeapMapping::new(1 << 20); // stands in for the BAR
//! let host = Region::create(
//!     bar.clone(),
//!     0,
//!     Role::GuestAgent,
//!     &[LaneSpec { id: 1, direction: Direction::HostToGuest, priority: Priority::Control, flags: 0, data_size: 4096 }],
//! ).unwrap();
//! let guest = Region::attach(bar, Side::Guest).unwrap();
//! let mut tx = host.producer(1).unwrap();
//! let mut rx = guest.consumer(1).unwrap();
//! let _ = tx.try_push(b"ping").unwrap();
//! let mut buf = Vec::new();
//! let meta = rx.try_pop(&mut buf).unwrap().unwrap();
//! assert_eq!((&buf[..], meta.len), (&b"ping"[..], 4));
//! ```
//!
//! Real use: one process calls `Region::create` on the mapping it owns, the
//! peer `Region::attach` on its own mapping of the same memory; each takes
//! `producer(id)` / `consumer(id)` for the lanes it may use.

mod mapping;
mod region;
mod ring;
pub mod sched;
pub mod wake;

pub use mapping::{HeapMapping, Mapping};
pub use region::{
    DirFault, Direction, LaneInfo, LaneSpec, Priority, Region, RegionError, Role, Side, DIR_ENTRY_LEN,
    DIR_OFFSET, HEADER_LEN, LANE_ALIGN, MAGIC, MAX_DATA_SIZE, MAX_LANES, MIN_DATA_SIZE,
    RING_HEADER_LEN, VERSION,
};
pub use ring::{
    publish_fence, Consumer, Corruption, LaneError, PopError, Producer, PushError, PushOk,
    RecordMeta, DEFAULT_MAX_RECORD, FLAG_SKIP, REC_HEADER_LEN,
};
pub use sched::{Class, NewestWins, Pick, SchedConfig, Scheduler};
pub use wake::{Idle, IdleConfig, Kick, Waker};
