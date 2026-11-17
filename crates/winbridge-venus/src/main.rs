// SPDX-License-Identifier: GPL-3.0-or-later

//! `winbridge-venus --shm FILE --socket PATH [--slot N] [--record FILE] [--stats] --virgl-lib LIB [--render-server PATH]`
//! `winbridge-venus --replay FILE --virgl-lib LIB [--render-server PATH] [--stats]`
//!
//! Serves one Venus client slot. `FILE` is the backing file of the slot's `ivshmem-plain`
//! device (64 MiB); the guest's Vulkan process maps that device and talks to this process
//! through the two lanes in it (`slot.rs`). `PATH` is the QEMU chardev socket of the slot's
//! `org.winbridge.venus.N` virtio-serial port (QEMU is the listener, `server=on`); it carries
//! only wake-up bytes. The guest process that claims the slot gets a Venus context on virglrenderer
//! for each of its Vulkan instances; when it closes the slot or dies the slot is reset and the next
//! one is served.
//!
//! `--record` keeps what the guest sent; `--replay` feeds such a recording to a session without
//! a guest, with the original timing, so host-side problems can be reproduced in seconds.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use winbridge_venus::channel::StreamChannel;
use winbridge_venus::session::Session;
use winbridge_venus::slot::{Arrival, ShmChannel, Slot};
use winbridge_venus::virgl::Virgl;

const USAGE: &str = "usage: winbridge-venus (--shm FILE --socket PATH [--slot N] [--record FILE] [--stats] | --replay FILE [--stats]) --virgl-lib LIB [--render-server PATH]";

struct Args {
    shm: Option<String>,
    socket: Option<String>,
    slot: u32,
    lib: Option<String>,
    server: Option<String>,
    record: Option<String>,
    replay: Option<String>,
    stats: bool,
}

fn parse_args() -> Args {
    let mut a = Args { shm: None, socket: None, slot: 0, lib: None, server: None, record: None, replay: None, stats: false };
    let mut args = std::env::args().skip(1);
    let usage = || -> ! {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--shm" => a.shm = args.next(),
            "--socket" => a.socket = args.next(),
            "--slot" => a.slot = args.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| usage()),
            "--virgl-lib" => a.lib = args.next(),
            "--render-server" => a.server = args.next(),
            "--record" => a.record = args.next(),
            "--replay" => a.replay = args.next(),
            "--stats" => a.stats = true,
            _ => usage(),
        }
    }
    let live = a.shm.is_some() && a.socket.is_some() && a.replay.is_none();
    let replay = a.shm.is_none() && a.socket.is_none() && a.replay.is_some();
    if a.lib.is_none() || live == replay {
        usage();
    }
    a
}

fn main() {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let args = parse_args();
    if let Some(server) = &args.server {
        // Read by virglrenderer when it starts the render server, before any other thread exists.
        std::env::set_var("RENDER_SERVER_EXEC_PATH", server);
    }
    let mut virgl = match Virgl::open(args.lib.as_deref().unwrap()) {
        Ok(v) => v,
        Err(e) => {
            error!("{e}");
            std::process::exit(1);
        }
    };
    if let Some(path) = &args.replay {
        replay_file(&mut virgl, path, args.stats);
        return;
    }
    serve(&mut virgl, &args);
}

/// True if the peer of `s` has closed it (a wake-up socket never carries data toward us that we
/// would lose by peeking).
fn socket_closed(s: &UnixStream) -> bool {
    let mut b = [0u8; 1];
    // SAFETY: `b` is valid for one byte.
    let n = unsafe { libc::recv(s.as_raw_fd(), b.as_mut_ptr().cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    n == 0
}

fn connect(path: &str) -> UnixStream {
    loop {
        match UnixStream::connect(path) {
            Ok(s) => {
                info!("connected to {path}");
                return s;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

/// Serves the slot for ever: wait for a guest, run its session, reset, repeat.
fn serve(virgl: &mut Virgl, args: &Args) -> ! {
    let (shm, socket) = (args.shm.as_deref().unwrap(), args.socket.as_deref().unwrap());
    let slot = match Slot::open(Path::new(shm), args.slot) {
        Ok(s) => s,
        Err(e) => {
            error!("{e}");
            std::process::exit(1);
        }
    };
    let mut kick: Option<UnixStream> = None;
    loop {
        let k = match kick.take() {
            Some(k) if !socket_closed(&k) => k,
            _ => connect(socket),
        };
        let region = match slot.reset() {
            Ok(r) => r,
            Err(e) => {
                error!("{e}");
                std::process::exit(1);
            }
        };
        let block = slot.block();
        info!("slot {} is free", slot.id());
        match block.wait_for_guest(|| socket_closed(&k)) {
            Arrival::Claimed => {}
            Arrival::Left => {
                info!("a guest claimed slot {} and left it before the host looked", slot.id());
                kick = Some(k);
                continue;
            }
            Arrival::Abandoned => {
                warn!("the wake-up socket closed while the slot was free");
                continue;
            }
        }
        info!("a guest claimed slot {} (nonce {:#x})", slot.id(), block.nonce());
        let channel = match k.try_clone().map_err(|e| e.to_string()).and_then(|k2| ShmChannel::new(&region, block.clone(), k2)) {
            Ok(c) => c,
            Err(e) => {
                error!("{e}");
                std::process::exit(1);
            }
        };
        let mut session = Session::new(&mut *virgl, channel);
        if args.stats {
            session.enable_stats();
        }
        if let Some(path) = &args.record {
            match std::fs::File::create(path) {
                Ok(f) => session.record_to(f),
                Err(e) => error!("cannot record to {path}: {e}"),
            }
        }
        match session.run() {
            Ok(()) => info!("the guest left slot {}", slot.id()),
            Err(e) => error!("session ended: {e}"),
        }
        drop(session);
        kick = Some(k);
    }
}

/// Plays a recording into a session: each record is written when its time has come, replies are
/// read and dropped, and the channel is closed a little after the last record.
fn replay_file(virgl: &mut Virgl, path: &str, stats: bool) {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            error!("cannot read {path}: {e}");
            std::process::exit(1);
        }
    };
    let (guest, host) = UnixStream::pair().expect("socketpair");
    let mut drain = guest.try_clone().expect("clone");
    std::thread::spawn(move || {
        let mut sink = [0u8; 65536];
        while drain.read(&mut sink).map(|n| n > 0).unwrap_or(false) {}
    });
    let mut guest = guest;
    std::thread::spawn(move || {
        let start = Instant::now();
        let mut at = 0;
        while at + 12 <= data.len() {
            let micros = u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
            let len = u32::from_le_bytes(data[at + 8..at + 12].try_into().unwrap()) as usize;
            let Some(chunk) = data.get(at + 12..at + 12 + len) else { break };
            if let Some(wait) = Duration::from_micros(micros).checked_sub(start.elapsed()) {
                std::thread::sleep(wait);
            }
            if guest.write_all(chunk).is_err() {
                return;
            }
            at += 12 + len;
        }
        // Let the host finish what is in flight before the channel closes.
        std::thread::sleep(Duration::from_secs(5));
        let _ = guest.shutdown(std::net::Shutdown::Both);
    });
    let mut session = Session::new(virgl, StreamChannel::new(host));
    if stats {
        session.enable_stats();
    }
    match session.run() {
        Ok(()) => info!("replay finished"),
        Err(e) => error!("replay ended: {e}"),
    }
}
