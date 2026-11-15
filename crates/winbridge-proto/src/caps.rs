// SPDX-License-Identifier: GPL-3.0-or-later

//! Capability announcement of the guest agent.
//!
//! The wire format is positional (`bincode`), so a host must never send a message variant an old
//! agent does not know: the old agent could not decode it. The agent therefore announces the
//! shared-memory capability inside the free-text `os_description` of its `StatusReply` (the first
//! reply after start, and every reply to `QueryStatus`), as a trailing token:
//!
//! ```text
//! Windows 11 x64 [wb-shm=1:0badcafe]
//! ```
//!
//! `1` is the shared-memory protocol generation the agent speaks, `0badcafe` a random id of the
//! agent process (a changed id means the agent restarted). Old agents never emit the token and old
//! hosts only display the text, so both directions stay compatible and the host sends `ShmOffer`
//! only to an agent that announced the capability.

/// Shared-memory protocol generation announced and required (`ShmOffer::shm_version` is the
/// *region layout* version of `winbridge-shm`; this one gates the message set).
pub const SHM_PROTOCOL: u32 = 1;

const PREFIX: &str = " [wb-shm=";

/// What a `StatusReply` announced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentCaps {
    /// Shared-memory protocol generation of the agent.
    pub shm_protocol: u32,
    /// Random id of the agent process.
    pub instance: u32,
}

/// `base` plus the capability token for an agent process identified by `instance`.
pub fn announce(base: &str, instance: u32) -> String {
    format!("{base}{PREFIX}{SHM_PROTOCOL}:{instance:08x}]")
}

/// Extracts the capability token from an `os_description`; `None` for an agent without one.
pub fn parse(os_description: &str) -> Option<AgentCaps> {
    let start = os_description.rfind(PREFIX)? + PREFIX.len();
    let rest = os_description[start..].strip_suffix(']')?;
    let (proto, instance) = rest.split_once(':')?;
    Some(AgentCaps {
        shm_protocol: proto.parse().ok()?,
        instance: u32::from_str_radix(instance, 16).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let text = announce("Windows 11 x64", 0xbeef);
        assert_eq!(text, "Windows 11 x64 [wb-shm=1:0000beef]");
        assert_eq!(
            parse(&text),
            Some(AgentCaps { shm_protocol: 1, instance: 0xbeef })
        );
    }

    #[test]
    fn old_agent_has_no_token() {
        assert_eq!(parse("Windows 11 x64"), None);
        assert_eq!(parse("Windows [wb-shm=x:1]"), None);
        assert_eq!(parse("Windows [wb-shm=1:zz]"), None);
        assert_eq!(parse("Windows [wb-shm=1:0000beef] trailing"), None);
    }
}
