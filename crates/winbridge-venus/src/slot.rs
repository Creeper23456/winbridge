// SPDX-License-Identifier: GPL-3.0-or-later

//! One Venus client slot: an ivshmem region the guest maps for itself, plus the lifecycle around it.
//!
//! The ivshmem driver lets one process map a device, so a guest Vulkan process takes a whole
//! device (`/dev/shm/winbridge-venus<N>` on this side) and owns it until it exits. The region is
//! a `winbridge-shm` region with two lanes (guest to host, host to guest) whose records are
//! pieces of one WBVN byte stream. The slot block in the header page (offset [`BLOCK`], mirrored
//! by `venus/guest/wbvn_proto.h`) carries the lifecycle:
//!
//! ```text
//! 0x00 u64 magic "WBVNSLOT"   written last by the host: the guest waits for it
//! 0x08 u32 version            WBVN_SLOT_VERSION
//! 0x0C u32 slot id            names the serial port that carries the wake-ups
//! 0x10 u32 state              FREE -> CLAIMED (guest) -> CLOSED (guest) -> FREE (host, after reset)
//! 0x18 u64 nonce              chosen by the guest when it claims the slot
//! 0x20 u64 heartbeat          the guest increments it while it lives
//! ```
//!
//! A guest that dies without closing leaves the state at CLAIMED; the host notices that the
//! heartbeat stopped and resets the slot. The serial port is not a data path: it carries one byte
//! when the peer has said that it sleeps (the `sleeping` flag of the lane it reads).

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tracing::warn;
use winbridge_shm::file::{self, FileMapping};
use winbridge_shm::{Consumer, Direction, LaneSpec, Mapping, Priority, Producer, PushError, Region, Role};

use crate::channel::{poll_fds, Channel};

/// Size of a slot's backing file and of the guest's ivshmem device.
pub const SLOT_LEN: u64 = 64 << 20;
/// Lane carrying the guest's messages.
pub const LANE_G2H: u16 = 1;
/// Lane carrying the host's messages.
pub const LANE_H2G: u16 = 2;
/// Data size of each lane.
pub const LANE_SIZE: usize = 16 << 20;
/// Region capability bit: a WBVN slot.
pub const CAP_VENUS: u64 = 1;

/// Offset of the slot block in the header page.
pub const BLOCK: usize = 0x800;
const OFF_MAGIC: usize = 0x00;
const OFF_VERSION: usize = 0x08;
const OFF_SLOT_ID: usize = 0x0C;
const OFF_STATE: usize = 0x10;
const OFF_NONCE: usize = 0x18;
const OFF_HEARTBEAT: usize = 0x20;

/// `"WBVNSLOT"` as a little-endian `u64`.
pub const SLOT_MAGIC: u64 = u64::from_le_bytes(*b"WBVNSLOT");
pub const SLOT_VERSION: u32 = 1;

/// The slot is waiting for a guest.
pub const FREE: u32 = 0;
/// A guest owns the slot.
pub const CLAIMED: u32 = 1;
/// The guest has finished; the host resets the slot.
pub const CLOSED: u32 = 2;

/// A claimed slot whose heartbeat has not moved for this long belongs to a dead guest.
const STALE: Duration = Duration::from_secs(10);
/// A write that finds the lane full gives up after this long.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest record the host writes.
const CHUNK: usize = 1 << 20;
/// Waits up to this long are spent polling; longer ones sleep and ask for a wake-up.
const SPIN_LIMIT: Duration = Duration::from_micros(250);

/// The slot block of a mapped region.
#[derive(Clone)]
pub struct SlotBlock {
    map: FileMapping,
}

impl SlotBlock {
    fn u32_at(&self, off: usize) -> &AtomicU32 {
        // SAFETY: the block lies inside the header page of a mapping of at least `HEADER_LEN`
        // bytes (`Slot::open` checks the size), is 4/8-aligned, and the mapping outlives `self`.
        unsafe { &*(self.map.base().add(BLOCK + off) as *const AtomicU32) }
    }

    fn u64_at(&self, off: usize) -> &AtomicU64 {
        // SAFETY: as in `u32_at`.
        unsafe { &*(self.map.base().add(BLOCK + off) as *const AtomicU64) }
    }

    pub fn state(&self) -> u32 {
        self.u32_at(OFF_STATE).load(Ordering::Acquire)
    }

    pub fn heartbeat(&self) -> u64 {
        self.u64_at(OFF_HEARTBEAT).load(Ordering::Acquire)
    }

    pub fn nonce(&self) -> u64 {
        self.u64_at(OFF_NONCE).load(Ordering::Acquire)
    }

    /// The guest's side of the lifecycle, for tests and tools.
    pub fn set_state(&self, state: u32) {
        self.u32_at(OFF_STATE).store(state, Ordering::Release);
    }

    pub fn set_nonce(&self, nonce: u64) {
        self.u64_at(OFF_NONCE).store(nonce, Ordering::Release);
    }

    pub fn beat(&self) {
        self.u64_at(OFF_HEARTBEAT).fetch_add(1, Ordering::AcqRel);
    }
}

/// The backing file of one slot.
pub struct Slot {
    map: FileMapping,
    id: u32,
}

impl Slot {
    /// Opens the backing file, which must be exactly [`SLOT_LEN`] bytes.
    pub fn open(path: &Path, id: u32) -> Result<Slot, String> {
        Ok(Slot { map: file::open(path, SLOT_LEN)?, id })
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn block(&self) -> SlotBlock {
        SlotBlock { map: self.map.clone() }
    }

    /// Lays the region out afresh: both lanes empty, the slot free. Nothing that the previous
    /// guest left in the ring survives, so call it only when no guest owns the slot.
    pub fn reset(&self) -> Result<Region<FileMapping>, String> {
        let lane = |id, direction| LaneSpec { id, direction, priority: Priority::High, flags: 0, data_size: LANE_SIZE };
        let specs = [lane(LANE_G2H, Direction::GuestToHost), lane(LANE_H2G, Direction::HostToGuest)];
        let region = Region::create(self.map.clone(), CAP_VENUS, Role::GuestClient, &specs).map_err(|e| format!("cannot lay out the region: {e}"))?;
        let b = self.block();
        b.u32_at(OFF_VERSION).store(SLOT_VERSION, Ordering::Relaxed);
        b.u32_at(OFF_SLOT_ID).store(self.id, Ordering::Relaxed);
        b.u32_at(OFF_STATE).store(FREE, Ordering::Relaxed);
        b.u64_at(OFF_NONCE).store(0, Ordering::Relaxed);
        b.u64_at(OFF_HEARTBEAT).store(0, Ordering::Relaxed);
        b.u64_at(OFF_MAGIC).store(SLOT_MAGIC, Ordering::Release);
        Ok(region)
    }
}

/// The channel over a claimed slot, with the serial socket as the wake-up path.
pub struct ShmChannel {
    rx: Consumer<FileMapping>,
    tx: Producer<FileMapping>,
    block: SlotBlock,
    kick: UnixStream,
    scratch: Vec<u8>,
    beat: u64,
    beat_at: Instant,
    kicks_sent: u64,
    kicks_seen: u64,
}

impl ShmChannel {
    /// Attaches to the lanes of a region that a guest has claimed.
    pub fn new(region: &Region<FileMapping>, block: SlotBlock, kick: UnixStream) -> Result<ShmChannel, String> {
        let rx = region.consumer(LANE_G2H).map_err(|e| format!("guest lane: {e}"))?;
        let tx = region.producer(LANE_H2G).map_err(|e| format!("host lane: {e}"))?;
        kick.set_nonblocking(true).map_err(|e| format!("wake-up socket: {e}"))?;
        let beat = block.heartbeat();
        Ok(ShmChannel { rx, tx, block, kick, scratch: Vec::new(), beat, beat_at: Instant::now(), kicks_sent: 0, kicks_seen: 0 })
    }

    /// Wake-up bytes sent to the guest and received from it.
    pub fn kicks(&self) -> (u64, u64) {
        (self.kicks_sent, self.kicks_seen)
    }

    /// Whether the guest is still there: it has not closed the slot and its heartbeat moves.
    fn guest_gone(&mut self) -> bool {
        if self.block.state() != CLAIMED {
            return true;
        }
        let beat = self.block.heartbeat();
        if beat != self.beat {
            self.beat = beat;
            self.beat_at = Instant::now();
            return false;
        }
        if self.beat_at.elapsed() >= STALE {
            warn!("the guest has not shown a sign of life for {STALE:?}; closing the slot");
            return true;
        }
        false
    }

    fn rx_ready(&mut self) -> bool {
        // An error also ends the wait: `read_available` reports it.
        self.rx.poll_ready().unwrap_or(true)
    }

    /// Reads and discards wake-up bytes. Returns false when the socket is closed.
    fn drain_kick(&mut self) -> bool {
        let mut buf = [0u8; 64];
        loop {
            // SAFETY: `buf` is valid for its length.
            let n = unsafe { libc::recv(self.kick.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
            if n == 0 {
                return false;
            }
            if n < 0 {
                return io::Error::last_os_error().kind() != io::ErrorKind::ConnectionReset;
            }
            self.kicks_seen += n as u64;
        }
    }

    fn send_kick(&mut self) {
        // A full socket means a wake-up is already on its way.
        // SAFETY: the buffer is one valid byte.
        let n = unsafe { libc::send(self.kick.as_raw_fd(), [1u8].as_ptr().cast(), 1, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
        if n == 1 {
            self.kicks_sent += 1;
        }
    }
}

impl Channel for ShmChannel {
    fn read_available(&mut self, buf: &mut Vec<u8>) -> io::Result<bool> {
        let start = buf.len();
        while buf.len() - start < 4 * CHUNK {
            match self.rx.try_pop(&mut self.scratch) {
                Ok(Some(_)) => buf.extend_from_slice(&self.scratch),
                Ok(None) => break,
                Err(e) => {
                    warn!("the guest lane is unusable: {e}");
                    return Ok(false);
                }
            }
        }
        if buf.len() > start {
            return Ok(true);
        }
        if !self.drain_kick() {
            return Ok(false);
        }
        Ok(!self.guest_gone())
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        for chunk in data.chunks(CHUNK) {
            let started = Instant::now();
            let mut spins = 0u32;
            loop {
                match self.tx.try_push(chunk) {
                    Ok(ok) => {
                        if ok.kick {
                            self.send_kick();
                        }
                        break;
                    }
                    Err(PushError::Full) => {
                        spins += 1;
                        if spins % 256 == 0 {
                            if started.elapsed() >= WRITE_TIMEOUT || self.guest_gone() {
                                return Err(io::Error::new(io::ErrorKind::TimedOut, "the guest does not read its lane"));
                            }
                            std::thread::sleep(Duration::from_micros(50));
                        } else {
                            std::hint::spin_loop();
                        }
                    }
                    Err(e) => return Err(io::Error::new(io::ErrorKind::BrokenPipe, e.to_string())),
                }
            }
        }
        Ok(())
    }

    fn wait(&mut self, extra_fd: RawFd, timeout: Duration) -> io::Result<()> {
        let kick_fd = self.kick.as_raw_fd();
        let mut fds = [
            libc::pollfd { fd: kick_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: extra_fd, events: libc::POLLIN, revents: 0 },
        ];
        if timeout <= SPIN_LIMIT {
            let deadline = Instant::now() + timeout;
            loop {
                if self.rx_ready() || poll_fds(&mut fds, Duration::ZERO)? || Instant::now() >= deadline {
                    return Ok(());
                }
                std::hint::spin_loop();
            }
        }
        // Sleep, and have the guest wake us for a message: flag, re-check, then wait.
        self.rx.set_sleeping(true);
        if !self.rx_ready() {
            poll_fds(&mut fds, timeout)?;
        }
        self.rx.set_sleeping(false);
        Ok(())
    }
}
