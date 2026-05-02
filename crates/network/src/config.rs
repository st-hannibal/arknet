//! Network configuration wrapper.
//!
//! Mirrors [`arknet_common::config::NetworkSection`] but adds a few fields
//! that don't belong in the operator-facing TOML: the network id, the peer-book
//! path, and the per-role gossip topic subscription list (populated by the
//! node binary at construction time based on the role bitmap).
//!
//! The operator only touches the TOML. Role wiring happens in
//! [`arknet-node`].

use std::path::PathBuf;

use libp2p::Multiaddr;

use crate::errors::{NetworkError, Result};

/// Logical network identifier (`"arknet-devnet-1"`, etc.).
///
/// Peers carrying a different network id are rejected during the handshake —
/// this is what keeps a devnet node from accidentally joining a testnet swarm
/// even if their libp2p layers are otherwise compatible.
pub type NetworkId = String;

/// Full configuration for [`crate::Network`].
#[derive(Clone, Debug)]
pub struct NetworkConfig {
    /// Chain / network identifier. Peers must agree on this value or the
    /// handshake is rejected.
    pub network_id: NetworkId,

    /// P2P listen multiaddrs. At least one is required. Typical devnet
    /// config: `"/ip4/0.0.0.0/udp/26656/quic-v1"` and
    /// `"/ip4/0.0.0.0/tcp/26656"`.
    pub listen_addrs: Vec<Multiaddr>,

    /// Optional advertise address (for NAT traversal / hairpinning). Phase 1
    /// doesn't do AutoNAT — operators set this manually when behind NAT.
    pub external_addr: Option<Multiaddr>,

    /// Persistent peer addresses the node dials on start.
    pub bootstrap_peers: Vec<Multiaddr>,

    /// Path to the peer-book JSON file. Written after every
    /// successful outbound connection so the node can re-dial known peers
    /// after a restart.
    pub peer_book_path: PathBuf,

    /// Cap on inbound connections. 0 disables inbound entirely (useful for
    /// clients behind NAT that only dial out).
    pub max_inbound_peers: u32,

    /// Cap on outbound connections.
    pub max_outbound_peers: u32,
}

impl NetworkConfig {
    /// Validate the configuration. Returns the first failure encountered.
    pub fn validate(&self) -> Result<()> {
        if self.network_id.is_empty() {
            return Err(NetworkError::Config("network_id is empty".into()));
        }
        if self.listen_addrs.is_empty() {
            return Err(NetworkError::Config(
                "at least one listen_addr is required".into(),
            ));
        }
        if self.max_outbound_peers == 0 && self.bootstrap_peers.is_empty() {
            return Err(NetworkError::Config(
                "max_outbound_peers is 0 and no bootstrap peers — node would be offline".into(),
            ));
        }
        Ok(())
    }

    /// SDK-friendly defaults: random listen port, no inbound, no peer book.
    pub fn sdk_defaults(network_id: &str, bootstrap_peers: Vec<Multiaddr>) -> Self {
        Self {
            network_id: network_id.into(),
            listen_addrs: vec!["/ip4/0.0.0.0/udp/0/quic-v1"
                .parse()
                .expect("valid multiaddr")],
            external_addr: None,
            bootstrap_peers,
            peer_book_path: std::env::temp_dir()
                .join(format!("arknet-sdk-peers-{}.json", std::process::id())),
            max_inbound_peers: 0,
            max_outbound_peers: 20,
        }
    }
}

/// Parse a `Vec<String>` of multiaddrs into a `Vec<Multiaddr>`.
///
/// Each failure is reported with its index so operators can fix the right
/// line in their TOML.
pub fn parse_multiaddrs(raw: &[String]) -> Result<Vec<Multiaddr>> {
    raw.iter()
        .enumerate()
        .map(|(i, s)| {
            s.parse::<Multiaddr>()
                .map_err(|e| NetworkError::Config(format!("multiaddr[{i}] {s:?}: {e}")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> NetworkConfig {
        NetworkConfig {
            network_id: "arknet-devnet-1".into(),
            listen_addrs: vec!["/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap()],
            external_addr: None,
            bootstrap_peers: vec![],
            peer_book_path: PathBuf::from("/tmp/arknet-peers.json"),
            max_inbound_peers: 60,
            max_outbound_peers: 20,
        }
    }

    #[test]
    fn validate_accepts_sample() {
        sample().validate().unwrap();
    }

    #[test]
    fn rejects_empty_network_id() {
        let mut c = sample();
        c.network_id.clear();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_no_listen_addrs() {
        let mut c = sample();
        c.listen_addrs.clear();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_fully_offline_config() {
        let mut c = sample();
        c.max_outbound_peers = 0;
        c.bootstrap_peers.clear();
        assert!(c.validate().is_err());
    }

    #[test]
    fn parse_multiaddrs_reports_bad_index() {
        let raw = vec![
            "/ip4/127.0.0.1/tcp/1234".to_string(),
            "not-a-multiaddr".to_string(),
        ];
        let err = parse_multiaddrs(&raw).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("multiaddr[1]"), "got: {msg}");
    }
}
