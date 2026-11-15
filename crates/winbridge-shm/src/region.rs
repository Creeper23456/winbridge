// SPDX-License-Identifier: GPL-3.0-or-later

//! Region header page, lane directory and the `create` / `attach` entry points.

use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::fmt;
use std::sync::Arc;

use crate::mapping::Mapping;
use crate::ring::{Consumer, LaneError, Producer};

/// `"WBSHMEM\0"` read as a little-endian `u64`; written last by `create`.
pub const MAGIC: u64 = u64::from_le_bytes(*b"WBSHMEM\0");
/// Layout version. Peers with a different version must fall back to virtio-serial.
pub const VERSION: u32 = 1;
/// Size of the region header page.
pub const HEADER_LEN: usize = 4096;
/// Byte offset of the lane directory inside the header page.
pub const DIR_OFFSET: usize = 0x40;
/// Size of one directory entry.
pub const DIR_ENTRY_LEN: usize = 32;
/// Maximum number of lanes in a region (the page could hold 126).
pub const MAX_LANES: usize = 64;
/// Size of the per-ring header (head line + tail line).
pub const RING_HEADER_LEN: usize = 128;
/// Smallest ring data area.
pub const MIN_DATA_SIZE: usize = 64;
/// Largest ring data area (2^40); real limits come from the mapping length.
pub const MAX_DATA_SIZE: u64 = 1 << 40;
/// Required alignment of every lane offset.
pub const LANE_ALIGN: usize = 64;

// Header page field offsets (all little-endian).
const OFF_MAGIC: usize = 0x00;
const OFF_VERSION: usize = 0x08;
const OFF_HEADER_LEN: usize = 0x0C;
const OFF_CAPS: usize = 0x10;
const OFF_REGION_LEN: usize = 0x18;
const OFF_HOST_ROLE: usize = 0x20;
const OFF_GUEST_ROLE: usize = 0x24;
const OFF_LANE_COUNT: usize = 0x28;

/// Which end of the region a [`Region`] handle is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The Linux host (creates the region, treats everything as hostile).
    Host,
    /// The guest (agent or an unprivileged client).
    Guest,
}

/// Direction of a lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Direction {
    /// The host produces, the guest consumes.
    HostToGuest = 0,
    /// The guest produces, the host consumes.
    GuestToHost = 1,
}

impl Direction {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::HostToGuest),
            1 => Some(Self::GuestToHost),
            _ => None,
        }
    }

    /// The side that writes records into this lane.
    pub fn producer(self) -> Side {
        match self {
            Self::HostToGuest => Side::Host,
            Self::GuestToHost => Side::Guest,
        }
    }
}

/// Scheduling priority of a lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Priority {
    /// Control messages: strict priority.
    Control = 0,
    /// Commands and frames: high, round-robin among equals.
    High = 1,
    /// Bulk transfers: leftover bandwidth plus a guaranteed minimum.
    Bulk = 2,
}

impl Priority {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Control),
            1 => Some(Self::High),
            2 => Some(Self::Bulk),
            _ => None,
        }
    }
}

/// Role announced by each side in the header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Role {
    /// The Linux host process (only valid as the host role).
    Host = 1,
    /// The elevated guest agent.
    GuestAgent = 2,
    /// An unprivileged guest client (Vulkan ICD).
    GuestClient = 3,
}

impl Role {
    fn from_u32(v: u32) -> Option<Self> {
        match v {
            1 => Some(Self::Host),
            2 => Some(Self::GuestAgent),
            3 => Some(Self::GuestClient),
            _ => None,
        }
    }
}

/// One validated lane directory entry. `offset` and `size` have been checked
/// against the mapping; `size` is the power-of-two data area, the ring header
/// occupies the [`RING_HEADER_LEN`] bytes before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneInfo {
    /// Lane id (unique in the region).
    pub id: u16,
    /// Who produces.
    pub direction: Direction,
    /// Scheduling class.
    pub priority: Priority,
    /// Opaque lane flags (for example "newest wins").
    pub flags: u32,
    /// Offset of the ring header from the start of the region.
    pub offset: usize,
    /// Size of the data area (power of two).
    pub size: usize,
}

/// Parameters for one lane given to [`Region::create`].
#[derive(Clone, Copy, Debug)]
pub struct LaneSpec {
    /// Lane id.
    pub id: u16,
    /// Who produces.
    pub direction: Direction,
    /// Scheduling class.
    pub priority: Priority,
    /// Opaque lane flags.
    pub flags: u32,
    /// Data area size; must be a power of two of at least [`MIN_DATA_SIZE`].
    pub data_size: usize,
}

/// What is wrong with a directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirFault {
    /// Unknown direction value.
    BadDirection,
    /// Unknown priority value.
    BadPriority,
    /// Size is zero, not a power of two, or out of the allowed range.
    BadSize,
    /// Offset is not 64-byte aligned or lies inside the header page.
    BadOffset,
    /// The ring does not fit inside the region.
    OutOfBounds,
    /// The ring overlaps another ring.
    Overlap,
    /// The lane id appears twice.
    DuplicateId,
}

/// Errors from `create`, `attach` and endpoint acquisition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionError {
    /// The mapping is smaller than the header page.
    TooSmall,
    /// The mapping base is not 64-byte aligned.
    Misaligned,
    /// The host has not finished initialising (magic is still zero).
    NotInitialised,
    /// Wrong magic.
    BadMagic,
    /// Unsupported layout version.
    BadVersion(u32),
    /// Header page fields are inconsistent (header length, region length).
    BadHeader,
    /// Unknown or inconsistent roles.
    BadRole,
    /// More lanes than [`MAX_LANES`].
    TooManyLanes,
    /// A directory entry is invalid.
    Directory {
        /// Index of the entry.
        index: usize,
        /// What is wrong.
        fault: DirFault,
    },
    /// No lane with this id.
    UnknownLane(u16),
    /// This side may not produce/consume on that lane.
    WrongDirection,
    /// An endpoint for this lane is already alive in this handle.
    AlreadyTaken,
    /// The ring state found at endpoint creation is corrupt.
    Lane(LaneError),
}

impl fmt::Display for RegionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for RegionError {}

pub(crate) struct Inner<M: Mapping> {
    pub(crate) map: M,
    pub(crate) side: Side,
    pub(crate) caps: u64,
    pub(crate) host_role: Role,
    pub(crate) guest_role: Role,
    pub(crate) lanes: Vec<LaneInfo>,
    pub(crate) taken: Vec<AtomicBool>,
}

/// A validated shared region. The lane table is a private snapshot taken at
/// `create`/`attach`; the directory in shared memory is never read again.
pub struct Region<M: Mapping> {
    inner: Arc<Inner<M>>,
}

impl<M: Mapping> Clone for Region<M> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

// SAFETY (all `rd*`): callers pass an offset inside the 4096-byte header page of
// a mapping whose base is 64-byte aligned, so the access is in bounds and
// naturally aligned. Volatile because the peer may write concurrently.
unsafe fn rd32(base: *const u8, off: usize) -> u32 {
    u32::from_le(ptr::read_volatile(base.add(off) as *const u32))
}
unsafe fn rd64(base: *const u8, off: usize) -> u64 {
    u64::from_le(ptr::read_volatile(base.add(off) as *const u64))
}
unsafe fn wr32(base: *mut u8, off: usize, v: u32) {
    ptr::write_volatile(base.add(off) as *mut u32, v.to_le())
}
unsafe fn wr64(base: *mut u8, off: usize, v: u64) {
    ptr::write_volatile(base.add(off) as *mut u64, v.to_le())
}

/// Validate one entry against `bound` (the usable region length).
fn check_entry(
    index: usize,
    id: u16,
    dir: u8,
    prio: u8,
    flags: u32,
    offset: u64,
    size: u64,
    bound: u64,
) -> Result<LaneInfo, RegionError> {
    let err = |fault| RegionError::Directory { index, fault };
    let direction = Direction::from_u8(dir).ok_or_else(|| err(DirFault::BadDirection))?;
    let priority = Priority::from_u8(prio).ok_or_else(|| err(DirFault::BadPriority))?;
    if size < MIN_DATA_SIZE as u64 || size > MAX_DATA_SIZE || !size.is_power_of_two() {
        return Err(err(DirFault::BadSize));
    }
    if offset % LANE_ALIGN as u64 != 0 || offset < HEADER_LEN as u64 {
        return Err(err(DirFault::BadOffset));
    }
    let end = offset
        .checked_add(RING_HEADER_LEN as u64)
        .and_then(|v| v.checked_add(size))
        .ok_or_else(|| err(DirFault::OutOfBounds))?;
    if end > bound {
        return Err(err(DirFault::OutOfBounds));
    }
    // `end <= bound <= usize::MAX`, so these conversions cannot fail.
    Ok(LaneInfo {
        id,
        direction,
        priority,
        flags,
        offset: offset as usize,
        size: size as usize,
    })
}

/// Reject duplicate ids and overlapping rings.
fn check_set(lanes: &[LaneInfo]) -> Result<(), RegionError> {
    for (i, a) in lanes.iter().enumerate() {
        for (j, b) in lanes.iter().enumerate().skip(i + 1) {
            if a.id == b.id {
                return Err(RegionError::Directory {
                    index: j,
                    fault: DirFault::DuplicateId,
                });
            }
            let a_end = a.offset + RING_HEADER_LEN + a.size;
            let b_end = b.offset + RING_HEADER_LEN + b.size;
            if a.offset < b_end && b.offset < a_end {
                return Err(RegionError::Directory {
                    index: j,
                    fault: DirFault::Overlap,
                });
            }
        }
    }
    Ok(())
}

impl<M: Mapping> Region<M> {
    fn precheck(map: &M) -> Result<(), RegionError> {
        if map.len() < HEADER_LEN {
            return Err(RegionError::TooSmall);
        }
        if (map.base() as usize) % LANE_ALIGN != 0 {
            return Err(RegionError::Misaligned);
        }
        Ok(())
    }

    fn build(
        map: M,
        side: Side,
        caps: u64,
        host_role: Role,
        guest_role: Role,
        lanes: Vec<LaneInfo>,
    ) -> Self {
        let taken = lanes.iter().map(|_| AtomicBool::new(false)).collect();
        Self {
            inner: Arc::new(Inner {
                map,
                side,
                caps,
                host_role,
                guest_role,
                lanes,
                taken,
            }),
        }
    }

    /// Host side: lay out `specs` after the header page and initialise the
    /// header. The magic is published last, so a peer that attaches early sees
    /// [`RegionError::NotInitialised`]. Ring data areas are not cleared (fresh
    /// shm and BAR memory is zero); ring headers are.
    pub fn create(
        map: M,
        capabilities: u64,
        guest_role: Role,
        specs: &[LaneSpec],
    ) -> Result<Self, RegionError> {
        Self::precheck(&map)?;
        if specs.len() > MAX_LANES {
            return Err(RegionError::TooManyLanes);
        }
        if guest_role == Role::Host {
            return Err(RegionError::BadRole);
        }
        let bound = map.len() as u64;
        let mut lanes = Vec::with_capacity(specs.len());
        let mut cursor = HEADER_LEN as u64;
        for (i, s) in specs.iter().enumerate() {
            let info = check_entry(
                i,
                s.id,
                s.direction as u8,
                s.priority as u8,
                s.flags,
                cursor,
                s.data_size as u64,
                bound,
            )?;
            cursor += (RING_HEADER_LEN + info.size) as u64;
            lanes.push(info);
        }
        check_set(&lanes)?;

        let base = map.base();
        // SAFETY: `precheck` guarantees `len >= HEADER_LEN` and alignment; every
        // ring header lies inside the mapping (`check_entry`).
        unsafe {
            ptr::write_bytes(base, 0, HEADER_LEN);
            for l in &lanes {
                ptr::write_bytes(base.add(l.offset), 0, RING_HEADER_LEN);
            }
            wr32(base, OFF_VERSION, VERSION);
            wr32(base, OFF_HEADER_LEN, HEADER_LEN as u32);
            wr64(base, OFF_CAPS, capabilities);
            wr64(base, OFF_REGION_LEN, bound);
            wr32(base, OFF_HOST_ROLE, Role::Host as u32);
            wr32(base, OFF_GUEST_ROLE, guest_role as u32);
            wr32(base, OFF_LANE_COUNT, lanes.len() as u32);
            for (i, l) in lanes.iter().enumerate() {
                let e = DIR_OFFSET + i * DIR_ENTRY_LEN;
                ptr::write_volatile(base.add(e) as *mut u16, l.id.to_le());
                ptr::write_volatile(base.add(e + 2), l.direction as u8);
                ptr::write_volatile(base.add(e + 3), l.priority as u8);
                wr32(base, e + 4, l.flags);
                wr64(base, e + 8, l.offset as u64);
                wr64(base, e + 16, l.size as u64);
            }
            (&*(base.add(OFF_MAGIC) as *const AtomicU64)).store(MAGIC.to_le(), Ordering::Release);
        }
        Ok(Self::build(
            map,
            Side::Host,
            capabilities,
            Role::Host,
            guest_role,
            lanes,
        ))
    }

    /// Validate the header and directory found in `map` and take a private
    /// snapshot of the lane table. Every field is untrusted: the magic,
    /// version, declared length (must not exceed the mapping), roles, lane
    /// count and each entry (known direction/priority, power-of-two size,
    /// aligned offset, ring inside the region, no overlap, unique ids) are
    /// checked, each shared field being read exactly once.
    pub fn attach(map: M, side: Side) -> Result<Self, RegionError> {
        Self::precheck(&map)?;
        let base = map.base();
        // SAFETY: header page is in bounds and aligned (`precheck`).
        let (caps, host_role, guest_role, lanes) = unsafe {
            let magic = (&*(base.add(OFF_MAGIC) as *const AtomicU64)).load(Ordering::Acquire);
            let magic = u64::from_le(magic);
            if magic == 0 {
                return Err(RegionError::NotInitialised);
            }
            if magic != MAGIC {
                return Err(RegionError::BadMagic);
            }
            let version = rd32(base, OFF_VERSION);
            if version != VERSION {
                return Err(RegionError::BadVersion(version));
            }
            if rd32(base, OFF_HEADER_LEN) != HEADER_LEN as u32 {
                return Err(RegionError::BadHeader);
            }
            let caps = rd64(base, OFF_CAPS);
            let declared = rd64(base, OFF_REGION_LEN);
            if declared < HEADER_LEN as u64 || declared > map.len() as u64 {
                return Err(RegionError::BadHeader);
            }
            let host_role = Role::from_u32(rd32(base, OFF_HOST_ROLE));
            let guest_role = Role::from_u32(rd32(base, OFF_GUEST_ROLE));
            let (host_role, guest_role) = match (host_role, guest_role) {
                (Some(Role::Host), Some(g)) if g != Role::Host => (Role::Host, g),
                _ => return Err(RegionError::BadRole),
            };
            let count = rd32(base, OFF_LANE_COUNT) as usize;
            if count > MAX_LANES {
                return Err(RegionError::TooManyLanes);
            }
            let mut lanes = Vec::with_capacity(count);
            for i in 0..count {
                let e = DIR_OFFSET + i * DIR_ENTRY_LEN;
                let id = u16::from_le(ptr::read_volatile(base.add(e) as *const u16));
                let dir = ptr::read_volatile(base.add(e + 2));
                let prio = ptr::read_volatile(base.add(e + 3));
                let flags = rd32(base, e + 4);
                let offset = rd64(base, e + 8);
                let size = rd64(base, e + 16);
                lanes.push(check_entry(i, id, dir, prio, flags, offset, size, declared)?);
            }
            (caps, host_role, guest_role, lanes)
        };
        check_set(&lanes)?;
        Ok(Self::build(map, side, caps, host_role, guest_role, lanes))
    }

    /// Which side this handle is.
    pub fn side(&self) -> Side {
        self.inner.side
    }

    /// Capability bits announced by the host.
    pub fn capabilities(&self) -> u64 {
        self.inner.caps
    }

    /// Role of the host side.
    pub fn host_role(&self) -> Role {
        self.inner.host_role
    }

    /// Role of the guest side.
    pub fn guest_role(&self) -> Role {
        self.inner.guest_role
    }

    /// The validated lane table.
    pub fn lanes(&self) -> &[LaneInfo] {
        &self.inner.lanes
    }

    fn lane_index(&self, id: u16) -> Result<usize, RegionError> {
        self.inner
            .lanes
            .iter()
            .position(|l| l.id == id)
            .ok_or(RegionError::UnknownLane(id))
    }

    fn take(&self, idx: usize) -> Result<(), RegionError> {
        if self.inner.taken[idx].swap(true, Ordering::AcqRel) {
            return Err(RegionError::AlreadyTaken);
        }
        Ok(())
    }

    /// Writer endpoint of lane `id`; only valid on the producing side.
    pub fn producer(&self, id: u16) -> Result<Producer<M>, RegionError> {
        let idx = self.lane_index(id)?;
        if self.inner.lanes[idx].direction.producer() != self.inner.side {
            return Err(RegionError::WrongDirection);
        }
        self.take(idx)?;
        Producer::new(self.inner.clone(), idx).map_err(|e| {
            self.inner.taken[idx].store(false, Ordering::Release);
            RegionError::Lane(e)
        })
    }

    /// Reader endpoint of lane `id`; only valid on the consuming side.
    pub fn consumer(&self, id: u16) -> Result<Consumer<M>, RegionError> {
        let idx = self.lane_index(id)?;
        if self.inner.lanes[idx].direction.producer() == self.inner.side {
            return Err(RegionError::WrongDirection);
        }
        self.take(idx)?;
        Consumer::new(self.inner.clone(), idx).map_err(|e| {
            self.inner.taken[idx].store(false, Ordering::Release);
            RegionError::Lane(e)
        })
    }
}

/// Pointers into one ring, derived from a validated [`LaneInfo`].
#[derive(Clone, Copy)]
pub(crate) struct RingPtr {
    hdr: *mut u8,
    pub(crate) data: *mut u8,
    pub(crate) size: usize,
}

impl RingPtr {
    /// # Safety
    /// `info` must have been validated against the mapping behind `base`.
    pub(crate) unsafe fn new(base: *mut u8, info: &LaneInfo) -> Self {
        let hdr = base.add(info.offset);
        Self {
            hdr,
            data: hdr.add(RING_HEADER_LEN),
            size: info.size,
        }
    }

    // Line 0 (producer-owned): head u64 @0, producer flags u32 @8.
    // Line 1 (consumer-owned): tail u64 @64, consumer flags u32 @72.
    // SAFETY (all four): header is 64-aligned and in bounds by construction.
    pub(crate) fn head(&self) -> &AtomicU64 {
        unsafe { &*(self.hdr as *const AtomicU64) }
    }
    pub(crate) fn prod_flags(&self) -> &AtomicU32 {
        unsafe { &*(self.hdr.add(8) as *const AtomicU32) }
    }
    pub(crate) fn tail(&self) -> &AtomicU64 {
        unsafe { &*(self.hdr.add(64) as *const AtomicU64) }
    }
    pub(crate) fn cons_flags(&self) -> &AtomicU32 {
        unsafe { &*(self.hdr.add(72) as *const AtomicU32) }
    }
}
