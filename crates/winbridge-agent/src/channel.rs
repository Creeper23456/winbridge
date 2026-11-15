// SPDX-License-Identifier: GPL-3.0-or-later

//! VirtIO serial channel communication abstraction.
//!
//! The port handle is opened for overlapped I/O: a synchronous handle serialises every call per
//! file object, so a thread blocked in `ReadFile` would stall all writes (clipboard, replies).
//! Clones share one handle; each read/write uses its own event, so one reader thread and any
//! number of (externally serialised) writers run concurrently.

use std::io::{self, Read, Write};
#[allow(unused_imports)]
use tracing::{error, info, warn};

#[cfg(windows)]
pub mod sys {
    use super::*;
    use std::ptr;
    use std::sync::Arc;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Threading::CreateEventW;
    use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

    struct PortHandle(HANDLE);

    unsafe impl Send for PortHandle {}
    unsafe impl Sync for PortHandle {}

    impl Drop for PortHandle {
        fn drop(&mut self) {
            if self.0 != INVALID_HANDLE_VALUE {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    /// Cloneable handle to the VirtIO serial port.
    #[derive(Clone)]
    pub struct VirtioSerialStream {
        handle: Arc<PortHandle>,
    }

    impl VirtioSerialStream {
        /// Opens the Windows VirtIO serial port, retrying until available.
        pub fn open(port_name: Option<&str>) -> Self {
            let primary = port_name.unwrap_or(r"\\.\Global\org.winbridge.0");
            let secondary = r"\\.\org.winbridge.0";

            let wide_primary: Vec<u16> = primary.encode_utf16().chain(std::iter::once(0)).collect();
            let wide_secondary: Vec<u16> = secondary.encode_utf16().chain(std::iter::once(0)).collect();

            let open_path = |path: &Vec<u16>| unsafe {
                CreateFileW(
                    path.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    ptr::null_mut(),
                )
            };

            loop {
                let handle = open_path(&wide_primary);
                if handle != INVALID_HANDLE_VALUE {
                    info!("Successfully opened VirtIO serial port at {}", primary);
                    return Self {
                        handle: Arc::new(PortHandle(handle)),
                    };
                }

                let handle_sec = open_path(&wide_secondary);
                if handle_sec != INVALID_HANDLE_VALUE {
                    info!("Successfully opened VirtIO serial port at {}", secondary);
                    return Self {
                        handle: Arc::new(PortHandle(handle_sec)),
                    };
                }

                warn!("VirtIO serial port not available yet, retrying in 1s...");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }

    /// Runs one overlapped operation to completion with a private event.
    fn overlapped_io(
        handle: HANDLE,
        start: impl FnOnce(*mut OVERLAPPED) -> i32,
    ) -> io::Result<usize> {
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event;

        let started = start(&mut overlapped);
        let pending = started == 0 && unsafe { GetLastError() } == ERROR_IO_PENDING;

        let result = if started != 0 || pending {
            let mut transferred: u32 = 0;
            if unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 1) } != 0 {
                Ok(transferred as usize)
            } else {
                Err(io::Error::last_os_error())
            }
        } else {
            Err(io::Error::last_os_error())
        };

        unsafe {
            CloseHandle(event);
        }
        result
    }

    impl Read for VirtioSerialStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let handle = self.handle.0;
            overlapped_io(handle, |ov| unsafe {
                ReadFile(
                    handle,
                    buf.as_mut_ptr() as _,
                    buf.len() as u32,
                    ptr::null_mut(),
                    ov,
                )
            })
        }
    }

    impl Write for VirtioSerialStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let handle = self.handle.0;
            overlapped_io(handle, |ov| unsafe {
                WriteFile(
                    handle,
                    buf.as_ptr() as _,
                    buf.len() as u32,
                    ptr::null_mut(),
                    ov,
                )
            })
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(not(windows))]
pub mod sys {
    use super::*;

    #[derive(Clone)]
    pub struct VirtioSerialStream;

    impl VirtioSerialStream {
        pub fn open(_port_name: Option<&str>) -> Self {
            Self
        }
    }

    impl Read for VirtioSerialStream {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(std::time::Duration::from_millis(500));
            Ok(0)
        }
    }

    impl Write for VirtioSerialStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}

pub use sys::VirtioSerialStream;


// ---------------------------------------------------------------------------------------------
// Transport seam: serial port or shared-memory lanes (docs/PROTOCOL.md, "Shared-memory transport").
// ---------------------------------------------------------------------------------------------

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use winbridge_proto::ring::{answer_offer, Endpoint, Routed, SerialSend};
use winbridge_proto::{read_frame, write_frame, Message};

/// The serial port shared by every sender; the lock is held per message only.
pub type SharedSerial = Arc<Mutex<VirtioSerialStream>>;

/// Writes one message to the serial port. Used for everything that must travel on serial
/// (negotiation, kicks, the fallback path).
pub fn serial_sender(serial: SharedSerial) -> SerialSend {
    Arc::new(move |msg| {
        if let Ok(mut port) = serial.lock() {
            if let Err(e) = write_frame(&mut *port, &msg) {
                warn!("Serial write failed: {}", e);
            }
        }
    })
}

/// Outgoing path of the agent: a lane while shared memory is active (control or bulk by
/// message class), the serial port otherwise. Cheap to clone; callable from any thread.
#[derive(Clone)]
pub struct Outgoing {
    ep: Arc<Endpoint>,
    serial: SharedSerial,
}

impl Outgoing {
    /// `serial` is the port the endpoint's serial path writes to.
    pub fn new(ep: Arc<Endpoint>, serial: SharedSerial) -> Self {
        Self { ep, serial }
    }

    /// Sends `msg`. Blocks while the chosen lane's queue is full (back-pressure), like a serial
    /// write blocks on a full port.
    pub fn send(&self, msg: Message) -> Result<(), String> {
        match self.ep.route(msg) {
            Routed::Sent => Ok(()),
            Routed::Serial(msg) => {
                let mut port = self.serial.lock().map_err(|e| e.to_string())?;
                write_frame(&mut *port, &msg).map_err(|e| e.to_string())
            }
        }
    }
}

/// A random id of this agent process, announced in `StatusReply` so the host can tell an agent
/// restart from a repeated status reply.
pub fn instance_id() -> u32 {
    let mut h = RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    h.finish() as u32
}

/// Reads the serial port on its own thread and feeds the endpoint: transport messages are
/// handled there, everything else goes in order into the dispatch inbox. A read error is
/// logged and retried after 500 ms, as the message loop did before. Never touches the lanes.
pub fn spawn_serial_reader(mut port: VirtioSerialStream, ep: Arc<Endpoint>) {
    let spawned = std::thread::Builder::new().name("winbridge-serial-rx".into()).spawn(move || loop {
        // Reads use their own clone of the port handle: they must never hold the write lock.
        match read_frame(&mut port) {
            Ok(msg) => {
                if let Some(negotiation) = ep.on_serial(msg) {
                    on_negotiation(&ep, negotiation);
                }
            }
            Err(e) => {
                warn!("Stream read error: {}, reconnecting...", e);
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    });
    if let Err(e) = spawned {
        error!("Could not start the serial reader thread: {}", e);
    }
}

/// One offer at a time: mapping the device must not overlap with the previous link's teardown.
static OFFER_LOCK: Mutex<()> = Mutex::new(());

/// `ShmOffer` arrived: map the device and attach on a separate thread (it can take a while and
/// the serial reader must keep reading), then answer with `ShmReady`.
fn on_negotiation(ep: &Arc<Endpoint>, msg: Message) {
    let Message::ShmOffer { shm_version, region_len } = msg else {
        warn!("Unexpected negotiation message from the host: {:?}", msg);
        return;
    };
    let ep = ep.clone();
    let spawned = std::thread::Builder::new().name("winbridge-shm-offer".into()).spawn({
        let ep = ep.clone();
        move || {
            let _one_at_a_time = OFFER_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let reply = answer_offer(&ep, shm_version, region_len, map_bar);
            if let Message::ShmReady { ok, reason } = &reply {
                if *ok {
                    info!("Shared memory active: control and bulk lanes in use");
                } else {
                    info!("Shared memory not used: {}", reason);
                }
            }
            ep.send_serial(reply);
        }
    });
    if let Err(e) = spawned {
        warn!("Could not start the shared-memory thread: {}", e);
        ep.send_serial(Message::ShmReady {
            ok: false,
            reason: format!("agent could not start a thread: {e}"),
        });
    }
}

/// The guest mapping of the ivshmem device (cached, hardened by `ivshmem::map_device`).
#[cfg(windows)]
fn map_bar(region_len: u64) -> Result<crate::ivshmem::Bar, String> {
    crate::ivshmem::map_device(region_len).map_err(|e| e.to_string())
}

/// Without Windows there is no device: the offer is declined (only the unit tests run here).
#[cfg(not(windows))]
fn map_bar(_region_len: u64) -> Result<winbridge_shm::HeapMapping, String> {
    Err("shared memory needs the Windows ivshmem driver".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use winbridge_proto::ring::{Inbox, LinkConfig, REGION_LEN};
    use winbridge_shm::HeapMapping;

    fn endpoint(log: Arc<Mutex<Vec<Message>>>) -> Arc<Endpoint> {
        Endpoint::new(Arc::new(move |m| log.lock().unwrap().push(m)), Inbox::new(8, 8, 4), LinkConfig::default())
    }

    #[test]
    fn announcement_carries_the_capability() {
        let text = winbridge_proto::caps::announce("Windows 11 x64", instance_id());
        let caps = winbridge_proto::caps::parse(&text).unwrap();
        assert_eq!(caps.shm_protocol, winbridge_proto::caps::SHM_PROTOCOL);
    }

    #[test]
    fn offer_is_declined_cleanly_when_the_device_cannot_be_mapped() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let ep = endpoint(log);
        let reply = answer_offer(&ep, winbridge_shm::VERSION, REGION_LEN, |_| -> Result<HeapMapping, String> {
            Err("no ivshmem device".to_string())
        });
        assert!(matches!(&reply, Message::ShmReady { ok: false, reason } if reason.contains("no ivshmem")));
        assert!(!ep.is_active());
    }
}
