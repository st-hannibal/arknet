//! Composed libp2p `NetworkBehaviour` for arknet.
//!
//! Pulls together the four sub-behaviours the node relies on:
//!
//! - [`gossipsub`] for block / tx / receipt dissemination.
//! - [`kad`] (Kademlia DHT) for peer discovery.
//! - [`identify`] for peer-agent exchange (carries our handshake blob).
//! - [`ping`] for keepalive + liveness.
//!
//! Kept in its own module so Week 7-8 (consensus) and Week 10-11 (roles)
//! can add sub-behaviours here without touching the rest of the crate.

// The `#[derive(NetworkBehaviour)]` macro generates an event enum whose
// variants are undocumented. Accept that at the module level rather
// than weakening the crate-wide lint.
#![allow(missing_docs)]

use std::time::Duration;

use libp2p::{
    gossipsub, identify, identity::Keypair, kad, kad::store::MemoryStore, ping,
    swarm::NetworkBehaviour, StreamProtocol,
};

use crate::errors::NetworkError;
use crate::handshake::HandshakeInfo;

/// Combined `NetworkBehaviour` used by [`crate::Network`].
///
/// The `NetworkBehaviour` derive generates `ArknetBehaviourEvent` whose
/// variants are the individual sub-behaviour events, so the main task can
/// match on them with `match event { ArknetBehaviourEvent::Gossipsub(e) => … }`.
#[derive(NetworkBehaviour)]
pub struct ArknetBehaviour {
    /// Gossipsub — topic-based pub-sub.
    pub gossipsub: gossipsub::Behaviour,
    /// Kademlia DHT — peer discovery.
    pub kad: kad::Behaviour<MemoryStore>,
    /// Identify — agent-version + listen-addr exchange, carries our
    /// handshake payload in `agent_version`.
    pub identify: identify::Behaviour,
    /// Ping — periodic keepalive + RTT sample.
    pub ping: ping::Behaviour,
}

/// libp2p protocol identifier for our kademlia instance. Must match on
/// both sides of a connection; using a chain-specific protocol name
/// keeps devnet / testnet peers from accidentally cross-polluting DHTs.
pub const KAD_PROTOCOL: &str = "/arknet/kad/1";

/// libp2p identify protocol version string. Paired with the arknet
/// handshake payload stuffed into `agent_version`.
pub const IDENTIFY_PROTOCOL: &str = "/arknet/id/1";

impl ArknetBehaviour {
    /// Build the composed behaviour with sensible defaults for Phase 1.
    pub fn new(keypair: &Keypair, handshake: &HandshakeInfo) -> crate::errors::Result<Self> {
        let local_peer_id = keypair.public().to_peer_id();

        // Gossipsub — default heartbeat, strict-signing disabled because we
        // piggyback authenticity on the libp2p secure channel rather than
        // individual message signatures. Re-enable if we ever accept
        // off-channel gossip.
        let gs_config = gossipsub::ConfigBuilder::default()
            .heartbeat_interval(Duration::from_millis(700))
            .validation_mode(gossipsub::ValidationMode::Strict)
            .build()
            .map_err(|e| NetworkError::Behaviour(format!("gossipsub config: {e}")))?;
        let gossipsub = gossipsub::Behaviour::new(
            gossipsub::MessageAuthenticity::Signed(keypair.clone()),
            gs_config,
        )
        .map_err(|e| NetworkError::Behaviour(format!("gossipsub new: {e}")))?;

        // Kademlia — memory store is sufficient for Phase 1 devnet.
        // Persistence across restarts happens via the peer book, not the
        // DHT store.
        let kad_store = MemoryStore::new(local_peer_id);
        let kad_cfg = kad::Config::new(StreamProtocol::new(KAD_PROTOCOL));
        let kad = kad::Behaviour::with_config(local_peer_id, kad_store, kad_cfg);

        // Identify — agent_version carries our handshake payload.
        let identify_cfg = identify::Config::new(IDENTIFY_PROTOCOL.to_string(), keypair.public())
            .with_agent_version(handshake.to_agent_version())
            // Keep identify pushes rare — the payload is static after boot.
            .with_interval(Duration::from_secs(60));
        let identify = identify::Behaviour::new(identify_cfg);

        // Ping — 15 s interval, 20 s timeout. Mostly a liveness signal.
        let ping = ping::Behaviour::new(
            ping::Config::new()
                .with_interval(Duration::from_secs(15))
                .with_timeout(Duration::from_secs(20)),
        );

        Ok(Self {
            gossipsub,
            kad,
            identify,
            ping,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::identity::Keypair;

    fn sample_handshake() -> HandshakeInfo {
        HandshakeInfo {
            version: crate::handshake::HANDSHAKE_VERSION,
            network_id: "arknet-devnet-1".into(),
            software: "arknet/test".into(),
            roles: Default::default(),
        }
    }

    #[test]
    fn behaviour_constructs() {
        let kp = Keypair::generate_ed25519();
        let info = sample_handshake();
        let _ = ArknetBehaviour::new(&kp, &info).unwrap();
    }
}
