// SPDX-License-Identifier: GPL-3.0-or-later

//! Copy/scan kernels measured against the BAR. `bar` is the shared-memory side, `local` ordinary
//! private memory. `len` is a multiple of 64 and both pointers are 64-byte aligned.
//! Every kernel returns a sink value so that pure reads are not optimised away.

use std::arch::asm;
use std::arch::x86_64::*;
use std::ptr;

pub type Kernel = unsafe fn(bar: *mut u8, local: *mut u8, len: usize) -> u64;

/// A named kernel; the `w_`/`r_` prefix says whether it writes or reads the BAR.
pub struct Op {
    pub name: &'static str,
    pub feature: &'static str,
    pub run: Kernel,
}

impl Op {
    pub fn available(&self) -> bool {
        match self.feature {
            "" => true,
            "sse4.1" => is_x86_feature_detected!("sse4.1"),
            "avx2" => is_x86_feature_detected!("avx2"),
            _ => false,
        }
    }
}

pub const OPS: &[Op] = &[
    Op { name: "w_scalar64", feature: "", run: w_scalar64 },
    Op { name: "w_movsb", feature: "", run: w_movsb },
    Op { name: "w_memcpy", feature: "", run: w_memcpy },
    Op { name: "w_sse2", feature: "", run: w_sse2 },
    Op { name: "w_avx2", feature: "avx2", run: w_avx2 },
    Op { name: "w_nt_sse2", feature: "", run: w_nt_sse2 },
    Op { name: "w_nt_avx2", feature: "avx2", run: w_nt_avx2 },
    Op { name: "r_scalar64", feature: "", run: r_scalar64 },
    Op { name: "r_sse2", feature: "", run: r_sse2 },
    Op { name: "r_avx2", feature: "avx2", run: r_avx2 },
    Op { name: "r_movntdqa", feature: "sse4.1", run: r_movntdqa },
    Op { name: "r_movntdqa_avx2", feature: "avx2", run: r_movntdqa_avx2 },
    Op { name: "r_movsb", feature: "", run: r_movsb },
    Op { name: "r_memcpy", feature: "", run: r_memcpy },
];

pub fn by_name(name: &str) -> Option<&'static Op> {
    OPS.iter().find(|o| o.name == name)
}

// ---- writes -------------------------------------------------------------------------------

unsafe fn w_scalar64(bar: *mut u8, local: *mut u8, len: usize) -> u64 {
    let (d, s) = (bar as *mut u64, local as *const u64);
    for i in 0..len / 8 {
        ptr::write_volatile(d.add(i), ptr::read(s.add(i)));
    }
    0
}

unsafe fn w_movsb(bar: *mut u8, local: *mut u8, len: usize) -> u64 {
    asm!("rep movsb", inout("rcx") len => _, inout("rdi") bar => _, inout("rsi") local => _,
         options(nostack, preserves_flags));
    0
}

unsafe fn w_memcpy(bar: *mut u8, local: *mut u8, len: usize) -> u64 {
    ptr::copy_nonoverlapping(local, bar, len);
    0
}

unsafe fn w_sse2(bar: *mut u8, local: *mut u8, len: usize) -> u64 {
    let mut i = 0;
    while i < len {
        let s = local.add(i) as *const __m128i;
        let d = bar.add(i) as *mut __m128i;
        let (a, b, c, e) = (_mm_load_si128(s), _mm_load_si128(s.add(1)), _mm_load_si128(s.add(2)), _mm_load_si128(s.add(3)));
        _mm_store_si128(d, a);
        _mm_store_si128(d.add(1), b);
        _mm_store_si128(d.add(2), c);
        _mm_store_si128(d.add(3), e);
        i += 64;
    }
    0
}

#[target_feature(enable = "avx2")]
unsafe fn w_avx2(bar: *mut u8, local: *mut u8, len: usize) -> u64 {
    let mut i = 0;
    while i < len {
        let s = local.add(i) as *const __m256i;
        let d = bar.add(i) as *mut __m256i;
        let (a, b) = (_mm256_load_si256(s), _mm256_load_si256(s.add(1)));
        _mm256_store_si256(d, a);
        _mm256_store_si256(d.add(1), b);
        i += 64;
    }
    0
}

unsafe fn w_nt_sse2(bar: *mut u8, local: *mut u8, len: usize) -> u64 {
    let mut i = 0;
    while i < len {
        let s = local.add(i) as *const __m128i;
        let d = bar.add(i) as *mut __m128i;
        let (a, b, c, e) = (_mm_load_si128(s), _mm_load_si128(s.add(1)), _mm_load_si128(s.add(2)), _mm_load_si128(s.add(3)));
        _mm_stream_si128(d, a);
        _mm_stream_si128(d.add(1), b);
        _mm_stream_si128(d.add(2), c);
        _mm_stream_si128(d.add(3), e);
        i += 64;
    }
    _mm_sfence();
    0
}

#[target_feature(enable = "avx2")]
unsafe fn w_nt_avx2(bar: *mut u8, local: *mut u8, len: usize) -> u64 {
    let mut i = 0;
    while i < len {
        let s = local.add(i) as *const __m256i;
        let d = bar.add(i) as *mut __m256i;
        let (a, b) = (_mm256_load_si256(s), _mm256_load_si256(s.add(1)));
        _mm256_stream_si256(d, a);
        _mm256_stream_si256(d.add(1), b);
        i += 64;
    }
    _mm_sfence();
    0
}

// ---- reads --------------------------------------------------------------------------------

unsafe fn r_scalar64(bar: *mut u8, _local: *mut u8, len: usize) -> u64 {
    let p = bar as *const u64;
    let mut acc = [0u64; 4];
    let mut i = 0;
    while i < len / 8 {
        acc[0] = acc[0].wrapping_add(ptr::read_volatile(p.add(i)));
        acc[1] = acc[1].wrapping_add(ptr::read_volatile(p.add(i + 1)));
        acc[2] = acc[2].wrapping_add(ptr::read_volatile(p.add(i + 2)));
        acc[3] = acc[3].wrapping_add(ptr::read_volatile(p.add(i + 3)));
        i += 4;
    }
    acc[0] ^ acc[1] ^ acc[2] ^ acc[3]
}

unsafe fn r_sse2(bar: *mut u8, _local: *mut u8, len: usize) -> u64 {
    let mut acc = _mm_setzero_si128();
    let mut i = 0;
    while i < len {
        let s = bar.add(i) as *const __m128i;
        acc = _mm_xor_si128(acc, _mm_load_si128(s));
        acc = _mm_xor_si128(acc, _mm_load_si128(s.add(1)));
        acc = _mm_xor_si128(acc, _mm_load_si128(s.add(2)));
        acc = _mm_xor_si128(acc, _mm_load_si128(s.add(3)));
        i += 64;
    }
    _mm_cvtsi128_si64(acc) as u64
}

#[target_feature(enable = "avx2")]
unsafe fn r_avx2(bar: *mut u8, _local: *mut u8, len: usize) -> u64 {
    let mut acc = _mm256_setzero_si256();
    let mut i = 0;
    while i < len {
        let s = bar.add(i) as *const __m256i;
        acc = _mm256_xor_si256(acc, _mm256_load_si256(s));
        acc = _mm256_xor_si256(acc, _mm256_load_si256(s.add(1)));
        i += 64;
    }
    _mm256_extract_epi64(acc, 0) as u64
}

#[target_feature(enable = "sse4.1")]
unsafe fn r_movntdqa(bar: *mut u8, _local: *mut u8, len: usize) -> u64 {
    let mut acc = _mm_setzero_si128();
    let mut i = 0;
    while i < len {
        let s = bar.add(i) as *const __m128i;
        acc = _mm_xor_si128(acc, _mm_stream_load_si128(s));
        acc = _mm_xor_si128(acc, _mm_stream_load_si128(s.add(1)));
        acc = _mm_xor_si128(acc, _mm_stream_load_si128(s.add(2)));
        acc = _mm_xor_si128(acc, _mm_stream_load_si128(s.add(3)));
        i += 64;
    }
    _mm_cvtsi128_si64(acc) as u64
}

#[target_feature(enable = "avx2")]
unsafe fn r_movntdqa_avx2(bar: *mut u8, _local: *mut u8, len: usize) -> u64 {
    let mut acc = _mm256_setzero_si256();
    let mut i = 0;
    while i < len {
        let s = bar.add(i) as *const __m256i;
        acc = _mm256_xor_si256(acc, _mm256_stream_load_si256(s));
        acc = _mm256_xor_si256(acc, _mm256_stream_load_si256(s.add(1)));
        i += 64;
    }
    _mm256_extract_epi64(acc, 0) as u64
}

unsafe fn r_movsb(bar: *mut u8, local: *mut u8, len: usize) -> u64 {
    asm!("rep movsb", inout("rcx") len => _, inout("rdi") local => _, inout("rsi") bar => _,
         options(nostack, preserves_flags));
    0
}

unsafe fn r_memcpy(bar: *mut u8, local: *mut u8, len: usize) -> u64 {
    ptr::copy_nonoverlapping(bar, local, len);
    0
}

/// CPU brand string from CPUID.
#[allow(unused_unsafe)]
pub fn cpu_brand() -> String {
    // SAFETY: CPUID is always available on x86-64.
    unsafe {
        let max = __cpuid(0x8000_0000).eax;
        if max < 0x8000_0004 {
            return "unknown".into();
        }
        let mut b = Vec::with_capacity(48);
        for leaf in 0x8000_0002..=0x8000_0004u32 {
            let r = __cpuid(leaf);
            for w in [r.eax, r.ebx, r.ecx, r.edx] {
                b.extend_from_slice(&w.to_le_bytes());
            }
        }
        String::from_utf8_lossy(&b).trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string()
    }
}

pub fn cpu_features() -> String {
    let mut f = Vec::new();
    if is_x86_feature_detected!("sse2") {
        f.push("sse2");
    }
    if is_x86_feature_detected!("sse4.1") {
        f.push("sse4.1");
    }
    if is_x86_feature_detected!("avx2") {
        f.push("avx2");
    }
    if is_x86_feature_detected!("ermsb") {
        f.push("ermsb");
    }
    if is_x86_feature_detected!("avx512f") {
        f.push("avx512f");
    }
    f.join(",")
}
