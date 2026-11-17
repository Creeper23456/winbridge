// SPDX-License-Identifier: GPL-3.0-or-later

//! WBVN wire format. Must stay identical to `venus/guest/wbvn_proto.h`.

pub const VERSION: u32 = 2;
pub const HDR_LEN: usize = 16;
pub const MAX_PAYLOAD: u32 = 64 << 20;
pub const REPLY: u32 = 0x8000;
/// The low half of a guest message's `kind` word is the message kind (with `REPLY` for an answer);
/// the high half names the venus context the message is for, 0 where none applies.
pub const KIND_MASK: u32 = 0xFFFF;
pub const CTX_SHIFT: u32 = 16;
/// The largest context id the kind word can carry.
pub const CTX_MAX: u32 = 0xFFFF;

/// `kind` for context `ctx`.
pub fn tag(kind: u32, ctx: u32) -> u32 {
    kind | ctx << CTX_SHIFT
}

/// Offset of the status word in a ring shmem.
pub const RING_STATUS: u32 = 128;
/// Offset of the head word in a ring shmem.
pub const RING_HEAD: u32 = 0;
/// Offset of the tail word in a ring shmem.
pub const RING_TAIL: u32 = 64;
/// `VK_RING_STATUS_IDLE_BIT_MESA`.
pub const RING_STATUS_IDLE: u32 = 1;
/// `VK_RING_STATUS_FATAL_BIT_MESA`: the renderer stopped executing the ring after a bad command.
pub const RING_STATUS_FATAL: u32 = 2;

pub const HELLO: u32 = 1;
pub const SHM_CREATE: u32 = 2;
pub const SHM_DESTROY: u32 = 3;
pub const PUSH: u32 = 4;
pub const SHM_ROLE: u32 = 5;
pub const SUBMIT: u32 = 6;
pub const SYNC_SIGNAL: u32 = 7;
pub const LOST: u32 = 8;
pub const STATUS_CLEAR: u32 = 9;
pub const BO_CREATE: u32 = 10;
pub const PING: u32 = 11;
pub const CTX_DESTROY: u32 = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hdr {
    pub len: u32,
    pub kind: u32,
    pub req: u32,
    pub arg: u32,
}

impl Hdr {
    pub fn encode(&self) -> [u8; HDR_LEN] {
        let mut b = [0u8; HDR_LEN];
        b[0..4].copy_from_slice(&self.len.to_le_bytes());
        b[4..8].copy_from_slice(&self.kind.to_le_bytes());
        b[8..12].copy_from_slice(&self.req.to_le_bytes());
        b[12..16].copy_from_slice(&self.arg.to_le_bytes());
        b
    }

    /// The context the message is for (0 for none).
    pub fn ctx(&self) -> u32 {
        self.kind >> CTX_SHIFT
    }

    /// The message kind without the context tag.
    pub fn id(&self) -> u32 {
        self.kind & KIND_MASK
    }

    pub fn decode(b: &[u8]) -> Hdr {
        Hdr { len: u32_at(b, 0), kind: u32_at(b, 4), req: u32_at(b, 8), arg: u32_at(b, 12) }
    }
}

pub fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

pub fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// One range of a `PUSH`: `data.len()` is a multiple of 8, as is `offset`.
#[derive(Debug, PartialEq, Eq)]
pub struct Range<'a> {
    pub res_id: u32,
    pub offset: u32,
    pub data: &'a [u8],
}

/// Splits a `PUSH` payload into its ranges. Stops at the first malformed range.
pub fn parse_push(p: &[u8]) -> Vec<Range<'_>> {
    let mut out = Vec::new();
    if p.len() < 8 {
        return out;
    }
    let count = u32_at(p, 0);
    let mut pos = 8usize;
    for _ in 0..count {
        if pos + 16 > p.len() {
            break;
        }
        let (res_id, offset, len) = (u32_at(p, pos), u32_at(p, pos + 4), u32_at(p, pos + 8) as usize);
        pos += 16;
        if len > p.len() - pos || offset % 8 != 0 || len % 8 != 0 {
            break;
        }
        out.push(Range { res_id, offset, data: &p[pos..pos + len] });
        pos += len;
    }
    out
}

/// Builds a `PUSH` payload from ranges.
#[derive(Default)]
pub struct PushBuilder {
    body: Vec<u8>,
    count: u32,
}

impl PushBuilder {
    pub fn add(&mut self, res_id: u32, offset: u32, data: &[u8]) {
        debug_assert!(offset % 8 == 0 && data.len() % 8 == 0);
        self.body.extend_from_slice(&res_id.to_le_bytes());
        self.body.extend_from_slice(&offset.to_le_bytes());
        self.body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.body.extend_from_slice(&0u32.to_le_bytes());
        self.body.extend_from_slice(data);
        self.count += 1;
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn len(&self) -> u32 {
        self.count
    }

    pub fn finish(self) -> Vec<u8> {
        let mut p = Vec::with_capacity(8 + self.body.len());
        p.extend_from_slice(&self.count.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes());
        p.extend_from_slice(&self.body);
        p
    }
}

/// A parsed `SUBMIT` payload.
pub struct Submit<'a> {
    pub ring_idx: u32,
    pub syncs: Vec<(u32, u64)>,
    pub cs: &'a [u8],
}

pub fn parse_submit(p: &[u8]) -> Option<Submit<'_>> {
    if p.len() < 16 {
        return None;
    }
    let (ring_idx, nsync, cs_size) = (u32_at(p, 0), u32_at(p, 4) as usize, u32_at(p, 8) as usize);
    let cs_at = 16usize.checked_add(nsync.checked_mul(16)?)?;
    if cs_at.checked_add(cs_size)? != p.len() {
        return None;
    }
    let syncs = (0..nsync).map(|i| (u32_at(p, 16 + i * 16), u64_at(p, 24 + i * 16))).collect();
    Some(Submit { ring_idx, syncs, cs: &p[cs_at..] })
}

/// `VK_COMMAND_TYPE_vkNotifyRingMESA_EXT` of venus-protocol 1.1.3 (`vn_protocol_driver_defines.h`).
const CMD_NOTIFY_RING: u32 = 190;

/// `vkNotifyRingMESA(ring, seqno, flags = 0)` as the guest's encoder writes it:
/// command type, command flags, ring id, seqno, notify flags.
pub fn notify_ring_cs(ring_id: u64, seqno: u32) -> [u8; 24] {
    let mut cs = [0u8; 24];
    cs[0..4].copy_from_slice(&CMD_NOTIFY_RING.to_le_bytes());
    cs[8..16].copy_from_slice(&ring_id.to_le_bytes());
    cs[16..20].copy_from_slice(&seqno.to_le_bytes());
    cs
}

pub fn sync_signal_payload(points: &[(u32, u64)]) -> Vec<u8> {
    let mut p = Vec::with_capacity(8 + points.len() * 16);
    p.extend_from_slice(&(points.len() as u32).to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    for (id, v) in points {
        p.extend_from_slice(&id.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes());
        p.extend_from_slice(&v.to_le_bytes());
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_round_trips() {
        let mut b = PushBuilder::default();
        b.add(3, 64, &[1; 16]);
        b.add(4, 0, &[2; 8]);
        let p = b.finish();
        let r = parse_push(&p);
        assert_eq!(r, vec![Range { res_id: 3, offset: 64, data: &[1; 16] }, Range { res_id: 4, offset: 0, data: &[2; 8] }]);
    }

    #[test]
    fn push_stops_at_a_truncated_range() {
        let mut b = PushBuilder::default();
        b.add(3, 0, &[1; 16]);
        let mut p = b.finish();
        p.truncate(p.len() - 1);
        assert!(parse_push(&p).is_empty());
    }

    #[test]
    fn submit_parses_syncs_and_cs() {
        let mut p = Vec::new();
        for v in [2u32, 1, 8, 0] {
            p.extend_from_slice(&v.to_le_bytes());
        }
        p.extend_from_slice(&7u32.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes());
        p.extend_from_slice(&9u64.to_le_bytes());
        p.extend_from_slice(&[5; 8]);
        let s = parse_submit(&p).unwrap();
        assert_eq!((s.ring_idx, s.syncs.clone(), s.cs), (2, vec![(7, 9)], &[5u8; 8][..]));
        p.push(0);
        assert!(parse_submit(&p).is_none());
    }
}
