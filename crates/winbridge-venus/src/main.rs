// SPDX-License-Identifier: GPL-3.0-or-later

//! `winbridge-venus --socket PATH --virgl-lib LIB [--render-server PATH]`
//!
//! Connects to the QEMU chardev socket of the `org.winbridge.venus.0` virtio-serial port (QEMU is
//! the listener, `server=on`) and runs the guest's Venus context on virglrenderer. Reconnects
//! when the guest side closes.

use std::os::unix::net::UnixStream;
use std::time::Duration;
use tracing::{error, info};
use winbridge_venus::session::Session;
use winbridge_venus::virgl::Virgl;

const USAGE: &str = "usage: winbridge-venus --socket PATH --virgl-lib LIB [--render-server PATH]";

fn main() {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let (mut socket, mut lib, mut server) = (None, None, None);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next(),
            "--virgl-lib" => lib = args.next(),
            "--render-server" => server = args.next(),
            _ => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
        }
    }
    let (Some(socket), Some(lib)) = (socket, lib) else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
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
    loop {
        let stream = match UnixStream::connect(&socket) {
            Ok(s) => s,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        info!("connected to {socket}");
        match Session::new(&mut virgl, stream).run() {
            Ok(()) => info!("guest closed the channel"),
            Err(e) => error!("session ended: {e}"),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
