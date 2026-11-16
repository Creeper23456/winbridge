// SPDX-License-Identifier: GPL-3.0-or-later

//! `winbridge-venus --socket PATH --virgl-lib LIB [--render-server PATH] [--record FILE]`
//! `winbridge-venus --replay FILE --virgl-lib LIB [--render-server PATH]`
//!
//! Connects to the QEMU chardev socket of the `org.winbridge.venus.0` virtio-serial port (QEMU is
//! the listener, `server=on`) and runs the guest's Venus context on virglrenderer. Reconnects
//! when the guest side closes. `--record` keeps what the guest sent; `--replay` feeds such a
//! recording to a session without a guest, with the original timing, so host-side problems can be
//! reproduced in seconds.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use tracing::{error, info};
use winbridge_venus::session::Session;
use winbridge_venus::virgl::Virgl;

const USAGE: &str = "usage: winbridge-venus (--socket PATH [--record FILE] | --replay FILE) --virgl-lib LIB [--render-server PATH]";

fn main() {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let (mut socket, mut lib, mut server, mut record, mut replay) = (None, None, None, None, None);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next(),
            "--virgl-lib" => lib = args.next(),
            "--render-server" => server = args.next(),
            "--record" => record = args.next(),
            "--replay" => replay = args.next(),
            _ => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
        }
    }
    let Some(lib) = lib else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    if socket.is_none() == replay.is_none() {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    if let Some(server) = server {
        // Read by virglrenderer when it starts the render server, before any other thread exists.
        std::env::set_var("RENDER_SERVER_EXEC_PATH", server);
    }
    let mut virgl = match Virgl::open(&lib) {
        Ok(v) => v,
        Err(e) => {
            error!("{e}");
            std::process::exit(1);
        }
    };
    if let Some(path) = replay {
        replay_file(&mut virgl, &path);
        return;
    }
    let socket = socket.unwrap();
    loop {
        let stream = match UnixStream::connect(&socket) {
            Ok(s) => s,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        info!("connected to {socket}");
        let mut session = Session::new(&mut virgl, stream);
        if let Some(path) = &record {
            match std::fs::File::create(path) {
                Ok(f) => session.record_to(f),
                Err(e) => error!("cannot record to {path}: {e}"),
            }
        }
        match session.run() {
            Ok(()) => info!("guest closed the channel"),
            Err(e) => error!("session ended: {e}"),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Plays a recording into a session: each record is written when its time has come, replies are
/// read and dropped, and the channel is closed a little after the last record.
fn replay_file(virgl: &mut Virgl, path: &str) {
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
    match Session::new(virgl, host).run() {
        Ok(()) => info!("replay finished"),
        Err(e) => error!("replay ended: {e}"),
    }
}
