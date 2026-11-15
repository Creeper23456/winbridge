// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows-only behaviour checks of the driver: single mapping per device, what happens when the
//! owner exits without RELEASE_MMAP, and who may open the device at all.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::dev::Mode;
use crate::util::Args;
use crate::winimp::{interfaces, nt_name, Dev, IOCTL_MMAP};

fn status(st: i32) -> String {
    nt_name(st)
}

/// Print the number of interrupt vectors the driver reports (`IVSHMEM_MMAP.vectors`).
pub fn vectors(d: &Dev) {
    let (st, o) = d.request_mmap(Mode::Cached as u8);
    if st >= 0 {
        println!("  REQUEST_MMAP(cached): peer={} size={} MiB vectors={}", o.peer, o.size >> 20, o.vectors);
        d.release_mmap();
    } else {
        println!("  REQUEST_MMAP(cached): {}", status(st));
    }
}

fn child(idx: usize, extra: &[&str]) -> Command {
    let mut c = Command::new(std::env::current_exe().expect("current exe"));
    c.args(["guest", "trymap", "--dev", &idx.to_string()]).args(extra);
    c
}

fn run_child(idx: usize, extra: &[&str]) -> String {
    match child(idx, extra).output() {
        Ok(o) => format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)).trim().replace('\n', " | "),
        Err(e) => format!("spawn failed: {e}"),
    }
}

/// Child entry point: try to map once.
/// `--exit`: leave the process without RELEASE_MMAP. `--hold`: stay mapped until killed.
pub fn trymap(d: &Dev, a: &Args) -> Result<bool, String> {
    let (st, o) = d.request_mmap(Mode::Cached as u8);
    println!("TRYMAP pid={} REQUEST_MMAP -> {} ptr=0x{:x} size={} MiB", std::process::id(), status(st), o.ptr, o.size >> 20);
    if st < 0 {
        println!("TRYMAP pid={} RELEASE_MMAP by a non-owner -> {}", std::process::id(), status(d.release_mmap()));
        return Ok(true);
    }
    if a.flag("--exit") {
        println!("MAPPED");
        std::process::exit(0);
    }
    if a.flag("--hold") {
        println!("MAPPED");
        std::thread::sleep(Duration::from_secs(300));
        return Ok(true);
    }
    println!("TRYMAP pid={} RELEASE_MMAP -> {}", std::process::id(), status(d.release_mmap()));
    Ok(true)
}

/// Single-mapping behaviour, run as SYSTEM or an elevated admin.
pub fn single(d: &Dev, idx: usize) -> Result<bool, String> {
    let ok = std::cell::Cell::new(true);
    let expect = |what: &str, st: i32, want: i32| {
        let good = st == want;
        ok.set(ok.get() && good);
        println!("SINGLE {what}: {}{}", status(st), if good { "" } else { "   <-- UNEXPECTED" });
    };
    const OK: i32 = 0;
    const ALREADY: i32 = 0xC000_0038u32 as i32;
    const BAD_HANDLE: i32 = 0xC000_0008u32 as i32;

    let (st, o) = d.request_mmap(Mode::Cached as u8);
    expect("1. REQUEST_MMAP on handle H1", st, OK);
    println!("SINGLE    peer={} size={} MiB ptr=0x{:x} vectors={}", o.peer, o.size >> 20, o.ptr, o.vectors);
    // SAFETY: the mapping is live; offset 0x900 is inside it.
    unsafe { (o.ptr as *mut u64).add(0x900 / 8).write_volatile(0x5748_4245_5441_4747) };

    expect("2. second REQUEST_MMAP on the same handle H1", d.request_mmap(Mode::Cached as u8).0, ALREADY);
    expect("2b. second REQUEST_MMAP, other cache mode, same handle", d.request_mmap(Mode::WriteCombined as u8).0, ALREADY);

    let d2 = Dev::open(d.iface.clone()).map_err(|e| format!("second open: Win32 error {e}"))?;
    expect("3. REQUEST_MMAP on a second handle H2 (same process)", d2.request_mmap(Mode::Cached as u8).0, ALREADY);
    expect("3b. RELEASE_MMAP from non-owner handle H2", d2.release_mmap(), BAD_HANDLE);

    println!("SINGLE 4. second process while H1 holds the mapping: {}", run_child(idx, &[]));

    expect("5. RELEASE_MMAP by the owner H1", d.release_mmap(), OK);
    expect("5b. RELEASE_MMAP again (not mapped)", d.release_mmap(), 0xC000_0010u32 as i32);
    let (st, _) = d2.request_mmap(Mode::Cached as u8);
    expect("5c. REQUEST_MMAP on H2 after H1 released", st, OK);
    expect("5d. RELEASE_MMAP by H2 (new owner)", d2.release_mmap(), OK);

    println!("SINGLE 6. child maps and exits WITHOUT RELEASE_MMAP: {}", run_child(idx, &["--exit"]));
    let (st, o) = d.request_mmap(Mode::Cached as u8);
    expect("6b. REQUEST_MMAP in the parent after that child exited", st, OK);
    // SAFETY: live mapping again; read the marker the first mapping wrote.
    let marker = unsafe { (o.ptr as *const u64).add(0x900 / 8).read_volatile() };
    println!("SINGLE    marker written through the first mapping, read through the new one: 0x{marker:x} ({})", if marker == 0x5748_4245_5441_4747 { "persisted" } else { "LOST" });
    ok.set(ok.get() && marker == 0x5748_4245_5441_4747);
    d.release_mmap();

    let mut c = child(idx, &["--hold"]).stdout(Stdio::piped()).spawn().map_err(|e| format!("spawn: {e}"))?;
    let mut line = String::new();
    let mut r = BufReader::new(c.stdout.take().expect("piped"));
    let _ = r.read_line(&mut line);
    let _ = r.read_line(&mut line);
    println!("SINGLE 7. child holds the mapping: {}", line.trim().replace('\n', " | "));
    expect("7a. REQUEST_MMAP in the parent while the child holds it", d.request_mmap(Mode::Cached as u8).0, ALREADY);
    c.kill().map_err(|e| format!("kill: {e}"))?;
    let _ = c.wait();
    expect("7b. REQUEST_MMAP in the parent after the child was killed (TerminateProcess)", d.request_mmap(Mode::Cached as u8).0, OK);
    d.release_mmap();

    {
        let d3 = Dev::open(d.iface.clone()).map_err(|e| format!("third open: Win32 error {e}"))?;
        expect("8. REQUEST_MMAP on a short-lived handle H3", d3.request_mmap(Mode::Cached as u8).0, OK);
        // H3 is closed here without RELEASE_MMAP.
    }
    expect("8b. REQUEST_MMAP on H1 after H3 was closed without release", d.request_mmap(Mode::Cached as u8).0, OK);
    d.release_mmap();

    let mut bad = 0;
    for mode in Mode::ALL {
        for _ in 0..100 {
            let (st, o) = d.request_mmap(mode as u8);
            if st < 0 || o.ptr == 0 || d.release_mmap() < 0 {
                bad += 1;
            }
        }
    }
    println!("SINGLE 9. 300 map/release cycles (100 per cache mode): {bad} failures");
    ok.set(ok.get() && bad == 0);

    let mut bad_mode = Vec::new();
    for m in [3u8, 255] {
        let (st, _) = d.request_mmap(m);
        bad_mode.push(format!("mode {m}: {}", status(st)));
        if st >= 0 {
            d.release_mmap();
        }
    }
    println!("SINGLE 10. invalid cache mode values -> {}", bad_mode.join("; "));

    // Wrong buffer sizes are rejected without mapping anything.
    let mut out = [0u8; 16];
    let (st, _) = crate::winimp::ioctl(d.handle(), IOCTL_MMAP, &mut [1u8], &mut out);
    println!("SINGLE 11. REQUEST_MMAP with a 16-byte output buffer -> {}", status(st));
    Ok(ok.get())
}

/// Replace the DACL of every ivshmem device object (default: SYSTEM and Administrators only).
pub fn harden(a: &Args) -> Result<bool, String> {
    const WRITE_DAC: u32 = 0x0004_0000;
    let sddl = a.opt("--sddl").unwrap_or_else(|| "D:P(A;;GA;;;SY)(A;;GA;;;BA)".into());
    let mut ok = true;
    for (i, f) in interfaces()?.into_iter().enumerate() {
        match Dev::open_access(f, 0x8000_0000 | 0x4000_0000 | WRITE_DAC) {
            Ok(d) => match d.set_dacl(&sddl) {
                Ok(()) => println!("HARDEN iface {i}: DACL set to {sddl}; now: {}", d.sddl()),
                Err(e) => {
                    ok = false;
                    println!("HARDEN iface {i}: SetKernelObjectSecurity failed, Win32 error {e}");
                }
            },
            Err(e) => {
                ok = false;
                println!("HARDEN iface {i}: open with WRITE_DAC failed, Win32 error {e}");
            }
        }
    }
    Ok(ok)
}

/// Can this user open the ivshmem devices? Run it as each account of interest.
pub fn open_test() -> Result<bool, String> {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "?".into());
    let list = interfaces()?;
    println!("OPENTEST user={user} interfaces={}", list.len());
    for (i, f) in list.into_iter().enumerate() {
        println!("OPENTEST iface {i}: {}", f.location);
        match Dev::open(f.clone()) {
            Ok(d) => {
                println!("OPENTEST iface {i}: open(GENERIC_READ|GENERIC_WRITE) OK, {}", d.describe());
                println!("OPENTEST iface {i}: device object security: {}", d.sddl());
                let (st, o) = d.request_mmap(Mode::Cached as u8);
                println!("OPENTEST iface {i}: REQUEST_MMAP -> {} ptr=0x{:x}", status(st), o.ptr);
                if st >= 0 {
                    d.release_mmap();
                }
            }
            Err(e) => println!("OPENTEST iface {i}: open(GENERIC_READ|GENERIC_WRITE) failed, Win32 error {e}"),
        }
        match Dev::open_access(f, 0) {
            Ok(d) => {
                let (st, o) = d.request_mmap(Mode::Cached as u8);
                println!("OPENTEST iface {i}: open(access 0) OK; REQUEST_MMAP -> {} ptr=0x{:x} (FILE_ANY_ACCESS)", status(st), o.ptr);
                if st >= 0 {
                    d.release_mmap();
                }
            }
            Err(e) => println!("OPENTEST iface {i}: open(access 0) failed, Win32 error {e}"),
        }
    }
    Ok(true)
}
