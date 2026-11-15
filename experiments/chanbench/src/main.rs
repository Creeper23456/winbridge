// SPDX-License-Identifier: GPL-3.0-or-later
// chanbench: bulk throughput and round-trip latency of a virtio-serial port.
//
//   host  (Linux): chanbench host --socket PATH [--mib N] [--iters N]
//          Connects to the QEMU chardev socket. QEMU is the listener (server=on).
//   guest (Windows, admin): chanbench guest
//          Opens \\.\Global\org.winbridge.gfx.0 overlapped and serves requests until killed.
//          Reopens the port if the host disconnects.
//
// Wire: 16-byte header {op: u32 LE, a: u32 LE, b: u64 LE}, followed by the payload its op defines.
// Only the host reads clocks; every measurement is host-side wall time.

use std::env;
use std::io::{self, Read, Write};
use std::time::Duration;

const HDR: usize = 16;
/// a = payload bytes (<= MAX_PING). Guest echoes header + payload verbatim.
const OP_PING: u32 = 1;
/// a = chunk bytes, b = total bytes. Host sends total bytes; guest replies OP_ACK.
const OP_H2G: u32 = 2;
/// a = chunk bytes, b = total bytes. Guest sends total bytes; host times the reads.
const OP_G2H: u32 = 3;
/// b = total bytes received in the OP_H2G that this acknowledges.
const OP_ACK: u32 = 4;
const MAX_CHUNK: usize = 4 << 20;
/// Largest echo payload; the guest's buffer is MAX_CHUNK bytes.
#[cfg(windows)]
const MAX_PING: usize = 64 << 10;
/// Bulk chunk sizes: the 16 B..64 KiB range the framing layer will see, then two large ones.
#[cfg(unix)]
const CHUNKS: [usize; 9] = [16, 64, 256, 1 << 10, 4 << 10, 16 << 10, 64 << 10, 1 << 20, 4 << 20];
/// One bulk transfer is at most this many chunks, so tiny chunks finish in bounded time.
#[cfg(unix)]
const MAX_OPS: usize = 1 << 18;
#[cfg(unix)]
const PING_SIZES: [usize; 8] = [0, 16, 64, 256, 1 << 10, 4 << 10, 16 << 10, 64 << 10];
#[cfg(windows)]
const PORT: &str = r"\\.\Global\org.winbridge.gfx.0";
const USAGE: &str = "usage: chanbench host --socket PATH [--mib N] [--iters N]\n       chanbench guest";

type Hdr = [u8; HDR];

fn header(op: u32, a: u32, b: u64) -> Hdr {
    let mut h = [0u8; HDR];
    h[0..4].copy_from_slice(&op.to_le_bytes());
    h[4..8].copy_from_slice(&a.to_le_bytes());
    h[8..16].copy_from_slice(&b.to_le_bytes());
    h
}

fn parse(h: &Hdr) -> (u32, u32, u64) {
    (
        u32::from_le_bytes(h[0..4].try_into().unwrap()),
        u32::from_le_bytes(h[4..8].try_into().unwrap()),
        u64::from_le_bytes(h[8..16].try_into().unwrap()),
    )
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(unix)]
mod host {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::thread;
    use std::time::Instant;

    const HANDSHAKE_SEQ: u64 = 0xC0DE;

    fn connect(path: &str, wait: Duration) -> io::Result<UnixStream> {
        let deadline = Instant::now() + wait;
        eprintln!("chanbench: connecting to {path}");
        loop {
            match UnixStream::connect(path) {
                Ok(s) => return Ok(s),
                Err(_) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(250));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Wait until the guest's port is open and echoing. Stale replies are skipped.
    fn handshake(s: &mut UnixStream) -> io::Result<()> {
        let mut h = [0u8; HDR];
        let want = (OP_PING, 0, HANDSHAKE_SEQ);
        for _ in 0..120 {
            s.write_all(&header(OP_PING, 0, HANDSHAKE_SEQ))?;
            loop {
                match s.read_exact(&mut h) {
                    Ok(()) if parse(&h) == want => return Ok(()),
                    Ok(()) => continue,
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => break,
                    Err(e) => return Err(e),
                }
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "guest did not answer the handshake (is chanbench guest running?)"))
    }

    fn mib_per_s(total: usize, d: Duration) -> f64 {
        total as f64 / (1 << 20) as f64 / d.as_secs_f64()
    }

    fn h2g(s: &mut UnixStream, buf: &[u8], chunk: usize, total: usize) -> io::Result<Duration> {
        let t = Instant::now();
        s.write_all(&header(OP_H2G, chunk as u32, total as u64))?;
        for _ in 0..total / chunk {
            s.write_all(&buf[..chunk])?;
        }
        let mut h = [0u8; HDR];
        s.read_exact(&mut h)?;
        let (op, _, b) = parse(&h);
        if op != OP_ACK || b != total as u64 {
            return Err(bad("expected ACK for host->guest transfer"));
        }
        Ok(t.elapsed())
    }

    fn g2h(s: &mut UnixStream, buf: &mut [u8], chunk: usize, total: usize) -> io::Result<Duration> {
        // The guest sends the same `i mod 256` pattern in every chunk, whatever the chunk size.
        let pat: Vec<u8> = (0..chunk).map(|i| i as u8).collect();
        let t = Instant::now();
        s.write_all(&header(OP_G2H, chunk as u32, total as u64))?;
        for _ in 0..total / chunk {
            s.read_exact(&mut buf[..chunk])?;
            if buf[..chunk] != pat[..] {
                return Err(bad("guest->host payload mismatch"));
            }
        }
        Ok(t.elapsed())
    }

    fn rtt_samples_us(s: &mut UnixStream, payload_len: usize, iters: usize) -> io::Result<Vec<f64>> {
        let mut frame = vec![0u8; HDR + payload_len];
        let mut back = vec![0u8; HDR + payload_len];
        let mut out = Vec::with_capacity(iters);
        for seq in 0..iters as u64 {
            frame[..HDR].copy_from_slice(&header(OP_PING, payload_len as u32, seq));
            let t = Instant::now();
            s.write_all(&frame)?;
            s.read_exact(&mut back)?;
            out.push(t.elapsed().as_secs_f64() * 1e6);
            if back != frame {
                return Err(bad("ping echo mismatch"));
            }
        }
        Ok(out)
    }

    fn percentile(sorted: &[f64], q: f64) -> f64 {
        sorted[((sorted.len() - 1) as f64 * q).round() as usize]
    }

    fn xfer_bytes(chunk: usize, total: usize) -> usize {
        chunk * (total / chunk).min(MAX_OPS)
    }

    pub fn run(path: &str, mib: usize, iters: usize) -> io::Result<()> {
        let total = mib << 20;
        let mut s = connect(path, Duration::from_secs(120))?;
        s.set_read_timeout(Some(Duration::from_secs(2)))?;
        handshake(&mut s)?;
        s.set_read_timeout(Some(Duration::from_secs(120)))?;
        s.set_write_timeout(Some(Duration::from_secs(120)))?;

        let payload = vec![0x5au8; MAX_CHUNK];
        let mut sink = vec![0u8; MAX_CHUNK];
        // Warm-up: 16 MiB each way, not reported.
        h2g(&mut s, &payload, 1 << 20, 16 << 20)?;
        g2h(&mut s, &mut sink, 1 << 20, 16 << 20)?;

        println!("# chanbench virtio-serial: up to {mib} MiB or {MAX_OPS} chunks per transfer, host-side clock");
        println!("direction,chunk_bytes,total_bytes,seconds,MiB_per_s,chunks_per_s");
        for &c in &CHUNKS {
            let t = xfer_bytes(c, total);
            let d = h2g(&mut s, &payload, c, t)?;
            println!("host_to_guest,{c},{t},{:.4},{:.2},{:.0}", d.as_secs_f64(), mib_per_s(t, d), (t / c) as f64 / d.as_secs_f64());
        }
        for &c in &CHUNKS {
            let t = xfer_bytes(c, total);
            let d = g2h(&mut s, &mut sink, c, t)?;
            println!("guest_to_host,{c},{t},{:.4},{:.2},{:.0}", d.as_secs_f64(), mib_per_s(t, d), (t / c) as f64 / d.as_secs_f64());
        }

        println!("# round trip (host write -> guest echo -> host read), {iters} iterations");
        println!("payload_bytes,p50_us,p90_us,p99_us,p99.9_us,min_us,max_us,mean_us");
        for &p in &PING_SIZES {
            let mut v = rtt_samples_us(&mut s, p, iters)?;
            v.sort_by(f64::total_cmp);
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            println!(
                "{p},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1}",
                percentile(&v, 0.50),
                percentile(&v, 0.90),
                percentile(&v, 0.99),
                percentile(&v, 0.999),
                v[0],
                v[v.len() - 1],
                mean
            );
        }
        Ok(())
    }
}

#[cfg(windows)]
mod guest {
    use super::*;
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use std::thread;

    type Handle = *mut c_void;
    const INVALID_HANDLE: isize = -1;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
    const ERROR_IO_PENDING: i32 = 997;

    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: Handle,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(name: *const u16, access: u32, share: u32, sec: *mut c_void, disposition: u32, flags: u32, template: Handle) -> Handle;
        fn CreateEventW(sec: *mut c_void, manual_reset: i32, initial: i32, name: *const u16) -> Handle;
        fn ReadFile(h: Handle, buf: *mut c_void, len: u32, read: *mut u32, ov: *mut Overlapped) -> i32;
        fn WriteFile(h: Handle, buf: *const c_void, len: u32, written: *mut u32, ov: *mut Overlapped) -> i32;
        fn GetOverlappedResult(h: Handle, ov: *mut Overlapped, transferred: *mut u32, wait: i32) -> i32;
        fn CloseHandle(h: Handle) -> i32;
    }

    struct Port {
        h: Handle,
        rd: Handle,
        wr: Handle,
    }

    impl Port {
        fn open() -> io::Result<Port> {
            let wide: Vec<u16> = PORT.encode_utf16().chain(Some(0)).collect();
            let h = unsafe {
                CreateFileW(wide.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, null_mut(), OPEN_EXISTING, FILE_FLAG_OVERLAPPED, null_mut())
            };
            if h as isize == INVALID_HANDLE {
                return Err(io::Error::last_os_error());
            }
            // Each direction gets its own manual-reset event, so a pending read never blocks a write.
            let mut p = Port { h, rd: null_mut(), wr: null_mut() };
            p.rd = unsafe { CreateEventW(null_mut(), 1, 0, null()) };
            p.wr = unsafe { CreateEventW(null_mut(), 1, 0, null()) };
            if p.rd.is_null() || p.wr.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(p)
        }

        /// Overlapped transfer; `len` is capped at u32::MAX (callers use <= 4 MiB).
        fn xfer(&self, ev: Handle, write: bool, buf: *mut c_void, len: usize) -> io::Result<usize> {
            let len = len.min(u32::MAX as usize) as u32;
            let mut ov: Overlapped = unsafe { std::mem::zeroed() };
            ov.event = ev;
            let mut n: u32 = 0;
            let ok = unsafe {
                if write { WriteFile(self.h, buf, len, &mut n, &mut ov) } else { ReadFile(self.h, buf, len, &mut n, &mut ov) }
            };
            if ok == 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() != Some(ERROR_IO_PENDING) {
                    return Err(e);
                }
            }
            if unsafe { GetOverlappedResult(self.h, &mut ov, &mut n, 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(n as usize)
        }
    }

    impl Drop for Port {
        fn drop(&mut self) {
            for x in [self.h, self.rd, self.wr] {
                if !x.is_null() && x as isize != INVALID_HANDLE {
                    unsafe { CloseHandle(x) };
                }
            }
        }
    }

    impl Read for Port {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.xfer(self.rd, false, buf.as_mut_ptr().cast(), buf.len())
        }
    }

    impl Write for Port {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.xfer(self.wr, true, buf.as_ptr() as *mut c_void, buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn bulk_args(a: u32, b: u64) -> io::Result<(usize, usize)> {
        let (c, t) = (a as usize, b as usize);
        if c == 0 || c > MAX_CHUNK || t % c != 0 {
            return Err(bad("bad bulk chunk/total"));
        }
        Ok((c, t))
    }

    fn serve<S: Read + Write>(s: &mut S, buf: &mut [u8]) -> io::Result<()> {
        loop {
            let mut h = [0u8; HDR];
            s.read_exact(&mut h)?;
            let (op, a, b) = parse(&h);
            match op {
                OP_PING => {
                    let n = a as usize;
                    if n > MAX_PING {
                        return Err(bad("ping payload too large"));
                    }
                    s.read_exact(&mut buf[..n])?;
                    s.write_all(&h)?;
                    s.write_all(&buf[..n])?;
                }
                OP_H2G => {
                    let (chunk, total) = bulk_args(a, b)?;
                    for _ in 0..total / chunk {
                        s.read_exact(&mut buf[..chunk])?;
                    }
                    s.write_all(&header(OP_ACK, 0, b))?;
                }
                OP_G2H => {
                    let (chunk, total) = bulk_args(a, b)?;
                    for (i, x) in buf[..chunk].iter_mut().enumerate() {
                        *x = i as u8;
                    }
                    for _ in 0..total / chunk {
                        s.write_all(&buf[..chunk])?;
                    }
                }
                _ => return Err(bad("unknown opcode")),
            }
        }
    }

    pub fn run() -> ! {
        let mut buf = vec![0u8; MAX_CHUNK];
        let mut last_err = String::new();
        loop {
            match Port::open() {
                Ok(mut port) => {
                    last_err.clear();
                    println!("chanbench guest: {PORT} open, serving");
                    match Port::open() {
                        Ok(_second) => println!("chanbench guest: second open of the port while the first is open: SUCCEEDED"),
                        Err(e) => println!("chanbench guest: second open of the port while the first is open: failed: {e}"),
                    }
                    let e = serve(&mut port, &mut buf).err().unwrap_or_else(|| io::Error::other("session ended"));
                    println!("chanbench guest: session ended: {e}");
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg != last_err {
                        println!("chanbench guest: open {PORT}: {msg}; retrying");
                        last_err = msg;
                    }
                    thread::sleep(Duration::from_secs(1));
                }
            }
            thread::sleep(Duration::from_secs(1));
        }
    }
}

#[cfg(unix)]
fn host_cli(args: &[String]) -> Result<(), String> {
    let mut socket = None;
    let mut mib: usize = 256;
    let mut iters: usize = 5000;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--socket" => socket = Some(it.next().ok_or("--socket needs a path")?.clone()),
            "--mib" => mib = it.next().ok_or("--mib needs a value")?.parse().map_err(|_| "invalid --mib")?,
            "--iters" => iters = it.next().ok_or("--iters needs a value")?.parse().map_err(|_| "invalid --iters")?,
            other => return Err(format!("unknown argument: {other}\n{USAGE}")),
        }
    }
    let socket = socket.ok_or("--socket PATH is required")?;
    if mib == 0 || mib % 4 != 0 {
        return Err("--mib must be a positive multiple of 4 (so every chunk size divides the total)".into());
    }
    if iters == 0 {
        return Err("--iters must be positive".into());
    }
    host::run(&socket, mib, iters).map_err(|e| e.to_string())
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let result: Result<(), String> = match args.get(1).map(String::as_str) {
        #[cfg(unix)]
        Some("host") => host_cli(&args[2..]),
        #[cfg(windows)]
        Some("guest") => guest::run(),
        _ => Err(USAGE.to_string()),
    };
    if let Err(e) = result {
        eprintln!("chanbench: {e}");
        std::process::exit(1);
    }
}
