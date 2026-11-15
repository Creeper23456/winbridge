// SPDX-License-Identifier: GPL-3.0-or-later

//! The private channel between the agent and its drag probe helper.
//!
//! Two unidirectional byte pipes (a synchronous pipe handle serialises reads and writes, so one
//! duplex handle could deadlock) carry length-prefixed bincode messages. Nothing here touches the
//! host protocol: the agent alone talks to the host.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

/// Largest message either side accepts.
const MAX_MESSAGE: usize = 8 * 1024 * 1024;

/// Agent -> helper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToHelper {
    /// The host's pointer left the canvas with the button held at guest pixels `x`, `y`.
    Probe { x: u32, y: u32 },
    /// The host's drag is over.
    End,
}

/// Helper -> agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FromHelper {
    /// The helper is up: its process id and integrity level RID.
    Ready { pid: u32, integrity: u32 },
    /// A drag entered the probe window: the paths (CF_HDROP) and/or text it carries.
    Offer { paths: Vec<PathBuf>, text: Option<String> },
    /// The probe window ended without a usable drag.
    NoDrag,
}

pub fn write_message<W: Write, T: Serialize>(out: &mut W, message: &T) -> io::Result<()> {
    let body = bincode::serialize(message).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if body.len() > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    out.write_all(&frame)?;
    out.flush()
}

pub fn read_message<R: Read, T: DeserializeOwned>(input: &mut R) -> io::Result<T> {
    let mut len = [0u8; 4];
    input.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut body = vec![0u8; len];
    input.read_exact(&mut body)?;
    bincode::deserialize(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The paths of an offer the agent is willing to stream: absolute and existing. The helper runs
/// with fewer rights than the agent and is not trusted to name arbitrary things.
pub fn valid_offer_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.into_iter().filter(|p| p.is_absolute() && p.exists()).collect()
}

/// `\\.\pipe\winbridge-drag-<agent pid>-<nonce>`; the helper adds `-a` / `-b`.
pub fn pipe_name(agent_pid: u32, nonce: u64) -> String {
    format!(r"\\.\pipe\winbridge-drag-{}-{:x}", agent_pid, nonce)
}

/// Restart delays for a helper that keeps dying: 1 s, 2 s, ... capped at 30 s.
#[derive(Debug)]
pub struct Backoff {
    next: Duration,
}

impl Backoff {
    const START: Duration = Duration::from_secs(1);
    const CAP: Duration = Duration::from_secs(30);

    pub fn new() -> Self {
        Self { next: Self::START }
    }

    pub fn next_delay(&mut self) -> Duration {
        let now = self.next;
        self.next = (self.next * 2).min(Self::CAP);
        now
    }

    pub fn reset(&mut self) {
        self.next = Self::START;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_and_reject_oversized_frames() {
        let offer = FromHelper::Offer { paths: vec![PathBuf::from("C:\\a\\b.txt")], text: Some("t".into()) };
        let mut pipe = Vec::new();
        write_message(&mut pipe, &offer).unwrap();
        write_message(&mut pipe, &FromHelper::NoDrag).unwrap();
        write_message(&mut pipe, &ToHelper::Probe { x: 5, y: 6 }).unwrap();
        let mut input = io::Cursor::new(pipe);
        assert_eq!(read_message::<_, FromHelper>(&mut input).unwrap(), offer);
        assert_eq!(read_message::<_, FromHelper>(&mut input).unwrap(), FromHelper::NoDrag);
        assert_eq!(read_message::<_, ToHelper>(&mut input).unwrap(), ToHelper::Probe { x: 5, y: 6 });
        assert!(read_message::<_, FromHelper>(&mut input).is_err(), "end of stream");

        let huge = ((MAX_MESSAGE + 1) as u32).to_le_bytes();
        assert!(read_message::<_, FromHelper>(&mut io::Cursor::new(huge.to_vec())).is_err());
    }

    #[test]
    fn only_absolute_existing_paths_are_streamed() {
        let here = std::env::current_dir().unwrap();
        let kept = valid_offer_paths(vec![
            here.clone(),
            PathBuf::from("relative/file.txt"),
            here.join("definitely-not-here-4242"),
        ]);
        assert_eq!(kept, vec![here]);
    }

    #[test]
    fn restart_delays_grow_to_a_cap_and_reset() {
        let mut backoff = Backoff::new();
        let delays: Vec<u64> = (0..7).map(|_| backoff.next_delay().as_secs()).collect();
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 30, 30]);
        backoff.reset();
        assert_eq!(backoff.next_delay().as_secs(), 1);
    }

    #[test]
    fn pipe_names_are_per_agent_and_nonce() {
        assert_ne!(pipe_name(1, 1), pipe_name(1, 2));
        assert!(pipe_name(7, 0xab).starts_with(r"\\.\pipe\winbridge-drag-7-"));
    }
}
