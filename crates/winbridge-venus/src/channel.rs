// SPDX-License-Identifier: GPL-3.0-or-later

//! What a [`Session`](crate::session::Session) talks to the guest through.
//!
//! The session parses a byte stream of WBVN messages. How the bytes travel is the channel's
//! business: production uses the ivshmem lanes of one slot ([`crate::slot::ShmChannel`]); replay
//! and the tests use a Unix socket ([`StreamChannel`]).

use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// A bidirectional, ordered byte stream to the guest.
pub trait Channel {
    /// Appends whatever the guest has sent to `buf` without blocking. Returns `false` once the
    /// guest has closed the channel and everything it sent has been delivered.
    fn read_available(&mut self, buf: &mut Vec<u8>) -> io::Result<bool>;

    /// Sends all of `data`, waiting for room if necessary.
    fn write_all(&mut self, data: &[u8]) -> io::Result<()>;

    /// Returns when the guest may have sent something, when `extra_fd` is readable, or after
    /// `timeout`, whichever comes first. A short timeout asks for the lowest latency, a long one
    /// for the lowest CPU use.
    fn wait(&mut self, extra_fd: RawFd, timeout: Duration) -> io::Result<()>;
}

/// `ppoll`s `fds` (a negative descriptor is ignored). Returns whether any is ready.
pub(crate) fn poll_fds(fds: &mut [libc::pollfd], timeout: Duration) -> io::Result<bool> {
    let ts = libc::timespec { tv_sec: timeout.as_secs() as _, tv_nsec: timeout.subsec_nanos() as _ };
    // SAFETY: `fds` and `ts` are valid for the call.
    let r = unsafe { libc::ppoll(fds.as_mut_ptr(), fds.len() as _, &ts, std::ptr::null()) };
    if r < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted { Ok(false) } else { Err(e) };
    }
    Ok(r > 0)
}

/// The channel over a connected Unix socket.
pub struct StreamChannel {
    stream: UnixStream,
    chunk: Vec<u8>,
}

impl StreamChannel {
    pub fn new(stream: UnixStream) -> StreamChannel {
        StreamChannel { stream, chunk: vec![0u8; 256 << 10] }
    }
}

impl Channel for StreamChannel {
    fn read_available(&mut self, buf: &mut Vec<u8>) -> io::Result<bool> {
        loop {
            // SAFETY: `chunk` is valid for its length.
            let n = unsafe { libc::recv(self.stream.as_raw_fd(), self.chunk.as_mut_ptr().cast(), self.chunk.len(), libc::MSG_DONTWAIT) };
            if n == 0 {
                return Ok(false);
            }
            if n < 0 {
                let e = io::Error::last_os_error();
                return match e.kind() {
                    io::ErrorKind::WouldBlock => Ok(true),
                    io::ErrorKind::Interrupted => continue,
                    _ => Err(e),
                };
            }
            buf.extend_from_slice(&self.chunk[..n as usize]);
        }
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        (&self.stream).write_all(data)
    }

    fn wait(&mut self, extra_fd: RawFd, timeout: Duration) -> io::Result<()> {
        let mut fds = [
            libc::pollfd { fd: self.stream.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: extra_fd, events: libc::POLLIN, revents: 0 },
        ];
        poll_fds(&mut fds, timeout).map(|_| ())
    }
}
