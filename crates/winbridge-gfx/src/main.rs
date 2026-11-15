// SPDX-License-Identifier: GPL-3.0-or-later

//! `winbridge-gfx --socket PATH [--device auto|INDEX|NAME]`
//!
//! Connects to the QEMU chardev socket of the `org.winbridge.gfx.0` virtio-serial port (QEMU is
//! the listener, `server=on`) and executes the guest's forwarded Vulkan calls on the host
//! driver. Reconnects when the guest side closes.

use std::os::unix::net::UnixStream;
use std::time::Duration;
use tracing::{error, info};
use winbridge_gfx::{serve, DeviceSelect, Session};
use winbridge_gfx_proto::frame::StreamTransport;

const USAGE: &str = "usage: winbridge-gfx --socket PATH [--device auto|INDEX|NAME]";

fn main() {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let mut socket = None;
    let mut select = DeviceSelect::Auto;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next(),
            "--device" => select = DeviceSelect::parse(&args.next().unwrap_or_default()),
            _ => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
        }
    }
    let Some(socket) = socket else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    // SAFETY: loads the system Vulkan loader once.
    let entry = match unsafe { ash::Entry::load() } {
        Ok(e) => e,
        Err(e) => {
            error!("cannot load the Vulkan loader: {e}");
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
        let reader = stream.try_clone().expect("clone socket");
        let mut transport = StreamTransport::new(reader, stream);
        let mut session = Session::new(entry.clone(), select.clone());
        match serve(&mut transport, &mut session) {
            Ok(()) => info!("guest closed the channel"),
            Err(e) => error!("session ended: {e}"),
        }
        drop(session);
        std::thread::sleep(Duration::from_millis(500));
    }
}
