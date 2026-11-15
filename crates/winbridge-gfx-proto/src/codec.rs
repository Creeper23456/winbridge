// SPDX-License-Identifier: GPL-3.0-or-later

//! Little-endian byte codec and bump arena used by the generated Vulkan serializers.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// The input ended before the value was complete.
    Truncated,
    /// A count or length cannot be satisfied by the remaining input.
    BadLength(u64),
    /// A handle id that the receiver does not know, or knows with another type.
    BadHandle { id: u64, kind: &'static str },
    /// A structure type or command id outside the generated set.
    BadTag(u32),
    /// The decoded value is inconsistent (for example a count that disagrees with its array).
    Invalid(&'static str),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("truncated payload"),
            Self::BadLength(n) => write!(f, "length {n} exceeds the remaining payload"),
            Self::BadHandle { id, kind } => write!(f, "unknown {kind} handle id {id}"),
            Self::BadTag(t) => write!(f, "unknown tag {t}"),
            Self::Invalid(s) => write!(f, "invalid payload: {s}"),
        }
    }
}

impl std::error::Error for CodecError {}

#[derive(Default)]
pub struct Enc {
    buf: Vec<u8>,
}

impl Enc {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(n: usize) -> Self {
        Self { buf: Vec::with_capacity(n) }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }

    /// Length-prefixed byte string.
    pub fn blob(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.bytes(v);
    }

    /// Raw bytes of a plain-old-data value.
    ///
    /// # Safety
    /// `T` must be `repr(C)` plain data without pointers or handles that need translation.
    pub unsafe fn pod<T>(&mut self, v: &T) {
        let p = (v as *const T).cast::<u8>();
        self.buf.extend_from_slice(std::slice::from_raw_parts(p, std::mem::size_of::<T>()));
    }

    /// Raw bytes of `n` consecutive plain-old-data values.
    ///
    /// # Safety
    /// As for `pod`; `p` must point to `n` valid values.
    pub unsafe fn pod_slice<T>(&mut self, p: *const T, n: usize) {
        if n == 0 {
            return;
        }
        self.buf.extend_from_slice(std::slice::from_raw_parts(p.cast::<u8>(), n * std::mem::size_of::<T>()));
    }
}

pub struct Dec<'a> {
    buf: &'a [u8],
}

impl<'a> Dec<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len()
    }

    pub fn finish(&self) -> Result<(), CodecError> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(CodecError::Invalid("trailing bytes"))
        }
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        if n > self.buf.len() {
            return Err(CodecError::Truncated);
        }
        let (a, b) = self.buf.split_at(n);
        self.buf = b;
        Ok(a)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        Ok(self.bytes(N)?.try_into().unwrap())
    }

    pub fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.array::<1>()?[0])
    }

    pub fn bool(&mut self) -> Result<bool, CodecError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(CodecError::Invalid("bool out of range")),
        }
    }

    pub fn u16(&mut self) -> Result<u16, CodecError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn i32(&mut self) -> Result<i32, CodecError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// Reads an element count and checks that at least `count * min_elem` bytes remain.
    pub fn count(&mut self, min_elem: usize) -> Result<usize, CodecError> {
        let n = self.u32()? as u64;
        if n.saturating_mul(min_elem.max(1) as u64) > self.buf.len() as u64 {
            return Err(CodecError::BadLength(n));
        }
        Ok(n as usize)
    }

    pub fn blob(&mut self) -> Result<&'a [u8], CodecError> {
        let n = self.u32()? as usize;
        self.bytes(n).map_err(|_| CodecError::BadLength(n as u64))
    }

    /// Fills `*out` with the next `size_of::<T>()` bytes.
    ///
    /// # Safety
    /// `out` must be valid for writes, and every bit pattern must be a valid `T`.
    pub unsafe fn pod_into<T>(&mut self, out: *mut T) -> Result<(), CodecError> {
        let b = self.bytes(std::mem::size_of::<T>())?;
        std::ptr::copy_nonoverlapping(b.as_ptr(), out.cast::<u8>(), b.len());
        Ok(())
    }

    /// Fills `n` consecutive values at `out`.
    ///
    /// # Safety
    /// As for `pod_into`; `out` must be valid for `n` values.
    pub unsafe fn pod_slice_into<T>(&mut self, out: *mut T, n: usize) -> Result<(), CodecError> {
        if n == 0 {
            return Ok(());
        }
        let len = n.checked_mul(std::mem::size_of::<T>()).ok_or(CodecError::BadLength(n as u64))?;
        let b = self.bytes(len).map_err(|_| CodecError::BadLength(n as u64))?;
        std::ptr::copy_nonoverlapping(b.as_ptr(), out.cast::<u8>(), len);
        Ok(())
    }
}

/// Bump arena for decoded structures. Memory is zeroed, 16-byte aligned, and stays at a fixed
/// address until the arena is dropped.
#[derive(Default)]
pub struct Arena {
    chunks: Vec<Box<[u128]>>,
}

impl Arena {
    pub fn new() -> Self {
        Self::default()
    }

    /// Zeroed storage for `n` values of `T` (`T` must tolerate all-zero bytes and align to at most 16).
    pub fn alloc<T>(&mut self, n: usize) -> *mut T {
        assert!(std::mem::align_of::<T>() <= 16);
        let bytes = n.saturating_mul(std::mem::size_of::<T>()).max(1);
        let chunk = vec![0u128; bytes.div_ceil(16)].into_boxed_slice();
        self.chunks.push(chunk);
        self.chunks.last_mut().unwrap().as_mut_ptr().cast::<T>()
    }

    /// NUL-terminated copy of `s`.
    pub fn cstr(&mut self, s: &[u8]) -> *const std::ffi::c_char {
        let p = self.alloc::<u8>(s.len() + 1);
        // SAFETY: `p` has `s.len() + 1` zeroed bytes.
        unsafe { std::ptr::copy_nonoverlapping(s.as_ptr(), p, s.len()) };
        p.cast()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_round_trip() {
        let mut e = Enc::new();
        e.u8(7);
        e.bool(true);
        e.u16(0xBEEF);
        e.u32(0xDEAD_BEEF);
        e.i32(-5);
        e.u64(u64::MAX - 1);
        e.blob(b"abc");
        let mut d = Dec::new(e.as_slice());
        assert_eq!(d.u8().unwrap(), 7);
        assert!(d.bool().unwrap());
        assert_eq!(d.u16().unwrap(), 0xBEEF);
        assert_eq!(d.u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(d.i32().unwrap(), -5);
        assert_eq!(d.u64().unwrap(), u64::MAX - 1);
        assert_eq!(d.blob().unwrap(), b"abc");
        d.finish().unwrap();
    }

    #[test]
    fn truncation_and_bad_counts_are_errors() {
        assert_eq!(Dec::new(&[1, 2]).u32(), Err(CodecError::Truncated));
        let mut e = Enc::new();
        e.u32(1_000_000);
        assert_eq!(Dec::new(e.as_slice()).count(4), Err(CodecError::BadLength(1_000_000)));
        let mut e = Enc::new();
        e.u32(10);
        e.bytes(b"xy");
        assert_eq!(Dec::new(e.as_slice()).blob(), Err(CodecError::BadLength(10)));
        assert!(Dec::new(&[2]).bool().is_err());
        assert!(Dec::new(&[0, 0]).finish().is_err());
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Pair {
        a: u64,
        b: u32,
    }

    #[test]
    fn pod_round_trip_and_arena() {
        let v = [Pair { a: 1, b: 2 }, Pair { a: u64::MAX, b: 9 }];
        let mut e = Enc::new();
        // SAFETY: Pair is repr(C) plain data.
        unsafe { e.pod_slice(v.as_ptr(), 2) };
        let mut arena = Arena::new();
        let p = arena.alloc::<Pair>(2);
        assert_eq!(p as usize % 16, 0);
        let mut d = Dec::new(e.as_slice());
        // SAFETY: p holds two zeroed Pairs.
        unsafe { d.pod_slice_into(p, 2).unwrap() };
        let out = unsafe { std::slice::from_raw_parts(p, 2) };
        assert_eq!(out[0].a, 1);
        assert_eq!(out[1], v[1]);
        let s = arena.cstr(b"hi");
        assert_eq!(unsafe { std::ffi::CStr::from_ptr(s) }.to_bytes(), b"hi");
    }
}
