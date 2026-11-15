// SPDX-License-Identifier: GPL-3.0-or-later

//! Host side of the WinBridge GPU forwarding channel.

pub mod present;
pub mod server;
pub mod session;

pub use server::{serve, ServeError};
pub use session::{DeviceSelect, Session};
