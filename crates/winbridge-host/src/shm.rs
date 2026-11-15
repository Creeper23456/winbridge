// SPDX-License-Identifier: GPL-3.0-or-later

//! Host side of the shared-memory negotiation (`docs/PROTOCOL.md`, "Shared-memory transport").
//!
//! Per serial session: the agent announces the capability in its `StatusReply`; if shared memory is
//! enabled and the backing file exists, the host creates a fresh region (a new generation),
//! sends `ShmOffer` and waits for `ShmReady`. `ok = true` switches both ends to the lanes; `ok =
//! false`, a timeout, a missing capability or any error leaves the session on the serial channel.
//! The state is per serial session: a serial reconnect resets it and the next session offers
//! again; an agent restart (new instance id in the announcement) re-offers inside the session.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{info, warn};
use winbridge_proto::caps;
use winbridge_proto::ring::{create_region, Endpoint, REGION_LEN};
use winbridge_proto::Message;
use winbridge_shm::{Mapping, Region};

/// How long the host waits for `ShmReady` before it stays on serial.
pub const OFFER_TIMEOUT: Duration = Duration::from_secs(5);

/// User settings for the shared-memory transport.
#[derive(Debug, Clone)]
pub struct ShmSettings {
    /// Opt-in (`shm = true` in `config.toml`, or `--shm`); off by default.
    pub enabled: bool,
    /// Backing file of the control region.
    pub path: std::path::PathBuf,
}

impl Default for ShmSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            path: std::path::PathBuf::from(crate::shmfile::DEFAULT_PATH),
        }
    }
}

enum State<M: Mapping> {
    /// Nothing offered yet in this session.
    Idle,
    /// `ShmOffer` sent, waiting for the answer.
    Offered {
        region: Region<M>,
        since: Instant,
        instance: u32,
    },
    /// The lanes are in use.
    Active { instance: u32 },
    /// Refused, timed out or failed: serial for the rest of the session.
    Failed { instance: u32 },
}

/// Opens a fresh mapping of the backing file.
pub type Opener<M> = Box<dyn Fn() -> Result<M, String> + Send + Sync>;

/// The host's negotiation state machine.
pub struct HostShm<M: Mapping> {
    enabled: bool,
    ep: Arc<Endpoint>,
    open: Opener<M>,
    state: State<M>,
    generation: u32,
    timeout: Duration,
}

impl<M: Mapping> HostShm<M> {
    /// `open` maps the backing file; it is only called when an offer is about to be made.
    pub fn new(enabled: bool, ep: Arc<Endpoint>, open: Opener<M>, timeout: Duration) -> Self {
        Self {
            enabled,
            ep,
            open,
            state: State::Idle,
            generation: 0,
            timeout,
        }
    }

    /// A new serial session starts (or the old one ended): leave the lanes, forget the state.
    pub fn new_session(&mut self) {
        self.ep.leave("serial session ended");
        self.state = State::Idle;
    }

    /// True while the lanes carry traffic (state machine and link agree).
    #[cfg(test)]
    pub fn is_active(&self) -> bool {
        matches!(self.state, State::Active { .. }) && self.ep.is_active()
    }

    fn instance(&self) -> Option<u32> {
        match self.state {
            State::Idle => None,
            State::Offered { instance, .. } | State::Active { instance } | State::Failed { instance } => Some(instance),
        }
    }

    /// The agent sent a `StatusReply`: offer shared memory if it announced the capability.
    pub fn on_status(&mut self, os_description: &str) {
        if !self.enabled {
            return;
        }
        let Some(caps) = caps::parse(os_description) else {
            // An old agent: it could not decode `ShmOffer`, so nothing is sent.
            return;
        };
        if caps.shm_protocol != caps::SHM_PROTOCOL {
            return;
        }
        match self.instance() {
            Some(i) if i == caps.instance => return,
            Some(_) => {
                info!("The agent restarted: shared memory is offered again");
                self.ep.leave("agent restarted");
                self.state = State::Idle;
            }
            None => {}
        }
        self.offer(caps.instance);
    }

    fn offer(&mut self, instance: u32) {
        self.generation = self.generation.wrapping_add(1);
        let map = match (self.open)() {
            Ok(m) => m,
            Err(e) => {
                info!("Shared memory not offered: {e}");
                self.state = State::Failed { instance };
                return;
            }
        };
        let len = map.len() as u64;
        if len != REGION_LEN {
            warn!("Shared memory not offered: region is {len} bytes, expected {REGION_LEN}");
            self.state = State::Failed { instance };
            return;
        }
        let region = match create_region(map, self.generation) {
            Ok(r) => r,
            Err(e) => {
                warn!("Shared memory not offered: cannot initialise the region: {e}");
                self.state = State::Failed { instance };
                return;
            }
        };
        info!("Offering shared memory (generation {})", self.generation);
        self.ep.send_serial(Message::ShmOffer {
            shm_version: winbridge_shm::VERSION,
            region_len: len,
        });
        self.state = State::Offered {
            region,
            since: Instant::now(),
            instance,
        };
    }

    /// The agent answered the offer.
    pub fn on_ready(&mut self, ok: bool, reason: &str) {
        let State::Offered { region, instance, .. } = std::mem::replace(&mut self.state, State::Idle) else {
            warn!("Ignoring an unexpected ShmReady");
            return;
        };
        if !ok {
            info!("The agent declined shared memory ({reason}); staying on serial");
            self.state = State::Failed { instance };
            return;
        }
        match self.ep.activate(&region) {
            Ok(()) => {
                info!("Shared memory active: control and bulk lanes in use");
                self.state = State::Active { instance };
            }
            Err(e) => {
                warn!("Shared memory could not start: {e}");
                self.ep.send_serial(Message::ShmFallback {
                    reason: format!("host could not start the link: {e}"),
                });
                self.state = State::Failed { instance };
            }
        }
    }

    /// Time-driven part: gives up on an unanswered offer, notices a link that ended by itself.
    pub fn tick(&mut self, now: Instant) {
        match &self.state {
            State::Offered { since, instance, .. } if now.duration_since(*since) >= self.timeout => {
                warn!("The agent did not answer the shared-memory offer in time; staying on serial");
                let instance = *instance;
                // A late-attaching agent must leave again.
                self.ep.send_serial(Message::ShmFallback {
                    reason: "offer timed out".to_string(),
                });
                self.state = State::Failed { instance };
            }
            State::Active { instance } if !self.ep.is_active() => {
                info!("The shared-memory link ended; serial for the rest of this session");
                self.state = State::Failed { instance: *instance };
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use winbridge_proto::ring::{answer_offer, Inbox, LinkConfig};
    use winbridge_shm::HeapMapping;

    struct Fixture {
        shm: HostShm<HeapMapping>,
        ep: Arc<Endpoint>,
        sent: Arc<Mutex<Vec<Message>>>,
        map: HeapMapping,
    }

    fn fixture(enabled: bool) -> Fixture {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let log = sent.clone();
        let ep = Endpoint::new(
            Arc::new(move |m| log.lock().unwrap().push(m)),
            Inbox::new(16, 16, 4),
            LinkConfig::default(),
        );
        let map = HeapMapping::new(REGION_LEN as usize);
        let m2 = map.clone();
        let shm = HostShm::new(enabled, ep.clone(), Box::new(move || Ok(m2.clone())), Duration::from_secs(5));
        Fixture { shm, ep, sent, map }
    }

    fn offers(f: &Fixture) -> usize {
        f.sent.lock().unwrap().iter().filter(|m| matches!(m, Message::ShmOffer { .. })).count()
    }

    #[test]
    fn old_agent_without_capability_gets_no_offer() {
        let mut f = fixture(true);
        f.shm.on_status("Windows 11 x64");
        assert_eq!(offers(&f), 0);
        assert!(f.sent.lock().unwrap().is_empty(), "nothing at all is sent to an old agent");
        assert!(!f.shm.is_active());
    }

    #[test]
    fn disabled_host_never_offers() {
        let mut f = fixture(false);
        f.shm.on_status(&caps::announce("Windows 11 x64", 1));
        assert_eq!(offers(&f), 0);
    }

    #[test]
    fn declined_offer_stays_on_serial_for_the_session() {
        let mut f = fixture(true);
        let text = caps::announce("Windows 11 x64", 7);
        f.shm.on_status(&text);
        assert_eq!(offers(&f), 1);
        assert!(matches!(
            f.sent.lock().unwrap()[0],
            Message::ShmOffer { shm_version, region_len } if shm_version == winbridge_shm::VERSION && region_len == REGION_LEN
        ));
        f.shm.on_ready(false, "no ivshmem device");
        assert!(!f.shm.is_active() && !f.ep.is_active());
        // Later status replies of the same agent do not re-offer; a new session does.
        f.shm.on_status(&text);
        assert_eq!(offers(&f), 1);
        f.shm.new_session();
        f.shm.on_status(&text);
        assert_eq!(offers(&f), 2);
    }

    #[test]
    fn unanswered_offer_times_out_to_serial() {
        let mut f = fixture(true);
        f.shm.on_status(&caps::announce("Windows 11 x64", 7));
        f.shm.tick(Instant::now() + Duration::from_secs(4));
        assert!(f.sent.lock().unwrap().iter().all(|m| !matches!(m, Message::ShmFallback { .. })));
        f.shm.tick(Instant::now() + Duration::from_secs(6));
        assert!(f.sent.lock().unwrap().iter().any(|m| matches!(m, Message::ShmFallback { .. })));
        // A late ShmReady is ignored.
        f.shm.on_ready(true, "");
        assert!(!f.ep.is_active());
    }

    #[test]
    fn accepted_offer_switches_and_an_agent_restart_offers_again() {
        let mut f = fixture(true);
        f.shm.on_status(&caps::announce("Windows 11 x64", 1));
        // The agent side, on the same memory.
        let guest = Endpoint::new(Arc::new(|_| {}), Inbox::new(16, 16, 4), LinkConfig::default());
        let m = f.map.clone();
        let reply = answer_offer(&guest, winbridge_shm::VERSION, REGION_LEN, move |_| Ok(m));
        assert!(matches!(reply, Message::ShmReady { ok: true, .. }));
        f.shm.on_ready(true, "");
        assert!(f.shm.is_active());
        // Same instance: nothing happens. New instance: the host leaves and offers again.
        f.shm.on_status(&caps::announce("Windows 11 x64", 1));
        assert_eq!(offers(&f), 1);
        f.shm.on_status(&caps::announce("Windows 11 x64", 2));
        assert_eq!(offers(&f), 2);
        assert!(!f.ep.is_active());
        assert!(f.sent.lock().unwrap().iter().any(|m| matches!(m, Message::ShmFallback { .. })));
    }

    #[test]
    fn missing_backing_file_means_no_offer() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let log = sent.clone();
        let ep = Endpoint::new(
            Arc::new(move |m| log.lock().unwrap().push(m)),
            Inbox::new(16, 16, 4),
            LinkConfig::default(),
        );
        let mut shm: HostShm<HeapMapping> =
            HostShm::new(true, ep, Box::new(|| Err("no such file".to_string())), OFFER_TIMEOUT);
        shm.on_status(&caps::announce("Windows 11 x64", 1));
        assert!(sent.lock().unwrap().is_empty());
    }
}
