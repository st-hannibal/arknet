//! Arknet handshake payload.
//!
//! Phase 1 piggybacks on the libp2p [`identify`] protocol. Each peer
//! advertises an [`HandshakeInfo`] JSON blob as its `agent_version`
//! string; both sides parse the remote's blob on the first
//! `identify::Event::Received` and disconnect if the declared network or
//! chain id doesn't match our own.
//!
//! The blob is intentionally small (under 200 bytes) so it fits well
//! within libp2p's agent-version field and doesn't force a separate
//! request-response protocol just for handshake data. If the payload
//! grows past ~512 bytes we'll switch to a dedicated
//! `/arknet/handshake/1` protocol — but that's out of scope for Week 5-6.
//!
//! The envelope carries an explicit `version` field so future
//! breaking changes can be rejected without ambiguity. The current
//! wire version is [`HANDSHAKE_VERSION`].
//!
//! [`identify`]: libp2p::identify

use serde::{Deserialize, Serialize};

use crate::errors::{NetworkError, Result};

/// Current handshake wire-format version. Bump this when the schema
/// changes in a non-backwards-compatible way.
pub const HANDSHAKE_VERSION: u32 = 1;

/// Prefix applied to the JSON payload before it's stuffed into
/// `agent_version`. Lets us distinguish an arknet peer from some other
/// libp2p app that happens to connect on the same port.
pub const AGENT_PREFIX: &str = "arknet/";

/// Peer capability bits. Operators advertise which roles they serve so
/// routers can skip non-compute peers when looking for inference
/// capacity, etc.
///
/// Mirrors [`arknet_common::types::RoleBitmap`] but is duplicated here
/// to keep the network crate free of any implicit cycle through
/// `arknet-common`'s role-aware code. Convert explicitly at the node
/// boundary.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct PeerRoles {
    /// Exposes L1 consensus (validator).
    #[serde(default)]
    pub validator: bool,
    /// Exposes an L2 router endpoint.
    #[serde(default)]
    pub router: bool,
    /// Exposes L2 compute (inference) capacity.
    #[serde(default)]
    pub compute: bool,
    /// Exposes L2 verifier (receipt validation).
    #[serde(default)]
    pub verifier: bool,
}

/// Handshake payload — serialized into the libp2p identify
/// `agent_version` field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeInfo {
    /// Wire-format version. See [`HANDSHAKE_VERSION`].
    pub version: u32,
    /// Logical network id (`"arknet-devnet-1"`, etc.).
    pub network_id: String,
    /// Node software version string (CARGO_PKG_VERSION + a build hash in
    /// the future). Informational only.
    pub software: String,
    /// Declared roles.
    pub roles: PeerRoles,
}

impl HandshakeInfo {
    /// Build the peer-facing string placed in libp2p identify's
    /// `agent_version`. Format: `arknet/<json>`.
    pub fn to_agent_version(&self) -> String {
        format!(
            "{AGENT_PREFIX}{}",
            serde_json::to_string(self).expect("handshake encode infallible")
        )
    }

    /// Parse a remote peer's agent-version string back into
    /// [`HandshakeInfo`]. Returns `None` if the prefix is missing (the
    /// peer is not an arknet node) and `Err` if the prefix is present but
    /// the JSON is malformed (the peer is a broken arknet node).
    pub fn from_agent_version(raw: &str) -> Result<Option<Self>> {
        let Some(payload) = raw.strip_prefix(AGENT_PREFIX) else {
            return Ok(None);
        };
        let info: Self = serde_json::from_str(payload)?;
        Ok(Some(info))
    }

    /// Check whether `other` is compatible with us. Currently just a
    /// network-id + version match; chain-id is implicitly part of
    /// `network_id`.
    pub fn check_compatible(&self, other: &HandshakeInfo) -> Result<()> {
        if other.version != self.version {
            return Err(NetworkError::Handshake {
                peer: "unknown".into(),
                reason: format!(
                    "handshake version mismatch: ours={} theirs={}",
                    self.version, other.version
                ),
            });
        }
        if other.network_id != self.network_id {
            return Err(NetworkError::Handshake {
                peer: "unknown".into(),
                reason: format!(
                    "network id mismatch: ours={:?} theirs={:?}",
                    self.network_id, other.network_id
                ),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HandshakeInfo {
        HandshakeInfo {
            version: HANDSHAKE_VERSION,
            network_id: "arknet-devnet-1".into(),
            software: "arknet/0.1.0".into(),
            roles: PeerRoles {
                validator: true,
                ..Default::default()
            },
        }
    }

    #[test]
    fn roundtrip_agent_version() {
        let info = sample();
        let wire = info.to_agent_version();
        assert!(wire.starts_with(AGENT_PREFIX));
        let decoded = HandshakeInfo::from_agent_version(&wire).unwrap().unwrap();
        assert_eq!(info, decoded);
    }

    #[test]
    fn non_arknet_agent_returns_none() {
        let got = HandshakeInfo::from_agent_version("rust-libp2p/0.54").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn malformed_arknet_agent_is_err() {
        let got = HandshakeInfo::from_agent_version("arknet/{not json");
        assert!(got.is_err());
    }

    #[test]
    fn compatible_when_same_network() {
        let a = sample();
        let b = sample();
        a.check_compatible(&b).unwrap();
    }

    #[test]
    fn incompatible_when_network_differs() {
        let a = sample();
        let mut b = sample();
        b.network_id = "arknet-testnet-1".into();
        let err = a.check_compatible(&b).unwrap_err();
        assert!(format!("{err}").contains("network id mismatch"));
    }

    #[test]
    fn incompatible_when_version_differs() {
        let a = sample();
        let mut b = sample();
        b.version = HANDSHAKE_VERSION + 1;
        let err = a.check_compatible(&b).unwrap_err();
        assert!(format!("{err}").contains("handshake version mismatch"));
    }
}
