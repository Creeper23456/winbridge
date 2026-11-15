// SPDX-License-Identifier: GPL-3.0-or-later
//! `tail` subcommand (Windows): the last links of the guest chain, measured separately from decode.
//!   memcpy      8 MiB BGRA (rotating cold sources) -> DIB section
//!   bitblt-dib  BitBlt DIB (in a memory DC) -> memory DC holding a second 32 bpp DIB
//!   bitblt-ddb  same, destination is a device-compatible bitmap (needs a screen DC)
//!   bitblt-win  same, destination is a visible top-level window DC (needs an interactive desktop)
//! Every BitBlt is followed by GdiFlush so the sample covers completion, not just queuing.

use crate::surface::Surface;
use crate::util::*;
use std::fs::OpenOptions;
use std::io::Write;
use std::time::Instant;
use windows::core::w;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::*;

const ITERS: usize = 300;
const WARMUP: usize = 20;

struct Sink {
    out: Option<std::fs::File>,
}

impl Sink {
    fn report(&mut self, name: &str, note: &str, ns: &mut [f64]) {
        let s = summarize(ns);
        let line = format!(
            "{{\"kind\":\"tail\",\"os\":\"windows\",\"test\":\"{name}\",\"note\":\"{}\",\"samples\":{},\"p50_ms\":{:.3},\"p95_ms\":{:.3},\"max_ms\":{:.3},\"mean_ms\":{:.3}}}",
            json_escape(note),
            ns.len(),
            s.p50 / 1e6,
            s.p95 / 1e6,
            s.max / 1e6,
            s.mean / 1e6
        );
        println!("{line}");
        if let Some(f) = &mut self.out {
            writeln!(f, "{line}").unwrap();
        }
    }
}

fn time_loop(mut f: impl FnMut()) -> Vec<f64> {
    let mut v = Vec::with_capacity(ITERS);
    for i in 0..ITERS + WARMUP {
        let t0 = Instant::now();
        f();
        if i >= WARMUP {
            v.push(t0.elapsed().as_nanos() as f64);
        }
    }
    v
}

/// One BitBlt + GdiFlush; false if GDI rejects the pair of DCs.
fn blit_once(dst: HDC, src: HDC, w: i32, h: i32) -> bool {
    // SAFETY: both DCs are live for the duration of the call.
    unsafe {
        let ok = BitBlt(dst, 0, 0, w, h, Some(src), 0, 0, SRCCOPY).is_ok();
        let _ = GdiFlush();
        ok
    }
}

fn measure(sink: &mut Sink, name: &str, note: &str, dst: HDC, src: HDC, w: i32, h: i32) {
    if !blit_once(dst, src, w, h) {
        println!("{name}: BitBlt rejected by GDI (no usable destination DC), skipped");
        return;
    }
    let mut ns = time_loop(|| {
        blit_once(dst, src, w, h);
    });
    sink.report(name, note, &mut ns);
}

pub fn run(args: &Args) {
    let mut sink = Sink { out: args.get("out").map(|p| OpenOptions::new().create(true).append(true).open(p).expect("open --out")) };
    let (w, h) = (W as i32, H as i32);
    let mut session = 0u32;
    // SAFETY: plain Win32 queries.
    unsafe {
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut session);
        println!(
            "session id {session}, screen {}x{}",
            GetSystemMetrics(SM_CXSCREEN),
            GetSystemMetrics(SM_CYSCREEN)
        );
    }

    // (i) memcpy into the DIB from 16 rotating 8 MiB sources (128 MiB working set, larger than any cache).
    let surface = Surface::new();
    let sources: Vec<Vec<u8>> = (0..16).map(|i| vec![i as u8; FRAME_BYTES]).collect();
    let mut k = 0;
    let mut ns = time_loop(|| {
        // SAFETY: the DIB is FRAME_BYTES long; sources are the same size.
        unsafe { surface.dst().rows(0, H).copy_from_slice(&sources[k % 16]) };
        k += 1;
    });
    sink.report("memcpy-to-dib", "8 MiB from 16 rotating cold sources", &mut ns);

    // (ii) BitBlt DIB -> memory DC with another DIB.
    // SAFETY: GDI calls on objects created here; bitmaps are deselected before deletion.
    unsafe {
        let src_dc = CreateCompatibleDC(None);
        let old_src = SelectObject(src_dc, surface.bitmap().into());
        let dst_surface = Surface::new();
        let dst_dc = CreateCompatibleDC(None);
        let old_dst = SelectObject(dst_dc, dst_surface.bitmap().into());
        measure(&mut sink, "bitblt-dib-to-dib", "memory DC -> memory DC, both 32 bpp DIB sections", dst_dc, src_dc, w, h);
        SelectObject(dst_dc, old_dst);
        let _ = DeleteDC(dst_dc);

        // (iii) device-compatible bitmap and a real window, if the desktop allows it.
        let screen = GetDC(None);
        if screen.is_invalid() {
            println!("no screen DC: bitblt-ddb and bitblt-win skipped");
        } else {
            let ddb = CreateCompatibleBitmap(screen, w, h);
            let ddb_dc = CreateCompatibleDC(Some(screen));
            let old = SelectObject(ddb_dc, ddb.into());
            measure(&mut sink, "bitblt-dib-to-ddb", "memory DC -> device-compatible bitmap in memory DC", ddb_dc, src_dc, w, h);
            SelectObject(ddb_dc, old);
            let _ = DeleteDC(ddb_dc);
            let _ = DeleteObject(ddb.into());

            match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("codecbench"),
                WS_POPUP | WS_VISIBLE,
                0,
                0,
                w,
                h,
                None::<HWND>,
                None,
                None,
                None,
            ) {
                Ok(hwnd) => {
                    let mut msg = MSG::default();
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = DispatchMessageW(&msg);
                    }
                    let wdc = GetDC(Some(hwnd));
                    let note = format!(
                        "DIB -> visible popup window DC (session {session}, screen {}x{}: the blit is clipped to the screen)",
                        GetSystemMetrics(SM_CXSCREEN),
                        GetSystemMetrics(SM_CYSCREEN)
                    );
                    measure(&mut sink, "bitblt-dib-to-window", &note, wdc, src_dc, w, h);
                    ReleaseDC(Some(hwnd), wdc);
                    let _ = DestroyWindow(hwnd);
                }
                Err(e) => println!("window creation failed ({e}): bitblt-win skipped"),
            }
            ReleaseDC(None, screen);
        }
        SelectObject(src_dc, old_src);
        let _ = DeleteDC(src_dc);
    }
}
