//! Pure p2p discovery for the SDK.
//!
//! Boots a lightweight libp2p swarm that joins the arknet gossip mesh,
//! listens for `PoolOffer` messages, and populates a [`CandidateTable`].
//! Also provides `publish_tx` for broadcasting escrow transactions.

use std::time::Duration;

use arknet_compute::wire::PoolOffer;
use arknet_network::gossip;
use arknet_network::{
    HandshakeInfo, Keypair, Multiaddr, NetworkConfig, NetworkEvent, NetworkHandle, PeerRoles,
    HANDSHAKE_VERSION,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::candidate_table::{CandidateEntry, CandidateTable};
use crate::errors::{Result, SdkError};

/// Configuration for the SDK swarm.
pub struct SdkConfig {
    /// Network id (must match the chain, e.g. `"mainnet"`).
    pub network_id: String,
    /// Bootstrap peer multiaddrs (typically validator seed nodes).
    pub bootstrap_peers: Vec<Multiaddr>,
    /// How long to wait for the first PoolOffer before returning an error.
    pub discovery_timeout: Duration,
}

impl Default for SdkConfig {
    fn default() -> Self {
        Self {
            network_id: "mainnet".into(),
            bootstrap_peers: Vec::new(),
            discovery_timeout: Duration::from_secs(30),
        }
    }
}

/// Handle to the SDK swarm. Cheap to clone.
#[derive(Clone)]
pub struct SdkSwarmHandle {
    network: NetworkHandle,
    candidates: CandidateTable,
    shutdown: CancellationToken,
}

impl SdkSwarmHandle {
    /// The local candidate table populated from gossip.
    pub fn candidates(&self) -> &CandidateTable {
        &self.candidates
    }

    /// Publish a signed transaction on the mempool gossip topic.
    pub async fn publish_tx(&self, signed_tx_bytes: Vec<u8>) -> Result<()> {
        let topic = gossip::tx_mempool().to_string();
        self.network
            .publish(topic, signed_tx_bytes)
            .await
            .map_err(|e| SdkError::P2p(format!("publish tx: {e}")))
    }

    /// The local peer id.
    pub fn local_peer_id(&self) -> libp2p::PeerId {
        self.network.local_peer_id()
    }

    /// Request shutdown of the swarm.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }
}

/// Boot the SDK swarm. Returns a handle and the background task join handle.
///
/// The swarm connects to bootstrap peers, subscribes to gossip, and
/// waits until at least one `PoolOffer` is received (or timeout).
pub async fn start(config: SdkConfig) -> Result<(SdkSwarmHandle, JoinHandle<()>)> {
    if config.bootstrap_peers.is_empty() {
        return Err(SdkError::Discovery("no bootstrap peers provided".into()));
    }

    let keypair = Keypair::generate_ed25519();
    let handshake = HandshakeInfo {
        version: HANDSHAKE_VERSION,
        network_id: config.network_id.clone(),
        software: format!("arknet-sdk/{}", env!("CARGO_PKG_VERSION")),
        roles: PeerRoles {
            validator: false,
            router: false,
            compute: false,
            verifier: false,
        },
    };

    let net_config = NetworkConfig::sdk_defaults(&config.network_id, config.bootstrap_peers);
    let shutdown = CancellationToken::new();

    let (handle, _inference_channels, net_join) =
        arknet_network::Network::start(net_config, keypair, handshake, shutdown.clone())
            .await
            .map_err(|e| SdkError::P2p(format!("network start: {e}")))?;

    let candidates = CandidateTable::new();
    let candidates_clone = candidates.clone();
    let pool_offer_topic = gossip::pool_offer().to_string();
    let mut events = handle.subscribe();
    let shutdown_clone = shutdown.clone();

    let gossip_join = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_clone.cancelled() => break,
                ev = events.recv() => {
                    match ev {
                        Ok(NetworkEvent::GossipMessage { topic, data, .. }) => {
                            if topic == pool_offer_topic {
                                match borsh::from_slice::<PoolOffer>(&data) {
                                    Ok(offer) => {
                                        debug!(
                                            models = ?offer.model_refs,
                                            slots = offer.available_slots,
                                            "sdk: received pool offer"
                                        );
                                        candidates_clone.upsert(CandidateEntry {
                                            peer_id_bytes: offer.peer_id,
                                            model_refs: offer.model_refs,
                                            operator: offer.operator,
                                            total_stake: offer.total_stake,
                                            supports_tee: offer.supports_tee,
                                            timestamp_ms: offer.timestamp_ms,
                                            available_slots: offer.available_slots,
                                            multiaddrs: vec![],
                                        });
                                    }
                                    Err(e) => {
                                        warn!(error = %e, "sdk: malformed pool offer");
                                    }
                                }
                            }
                        }
                        Ok(NetworkEvent::PeerConnected { peer, info }) => {
                            debug!(peer = %peer, roles = ?info.roles, "sdk: peer connected");
                        }
                        Ok(NetworkEvent::PeerDisconnected { peer }) => {
                            debug!(peer = %peer, "sdk: peer disconnected");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(missed = n, "sdk: event stream lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    });

    let join = tokio::spawn(async move {
        tokio::select! {
            _ = net_join => {},
            _ = gossip_join => {},
        }
    });

    let swarm_handle = SdkSwarmHandle {
        network: handle,
        candidates,
        shutdown,
    };

    // Wait for at least one candidate or timeout.
    let deadline = tokio::time::Instant::now() + config.discovery_timeout;
    loop {
        if !swarm_handle.candidates.is_empty() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            warn!("sdk: no pool offers received within timeout — continuing with empty candidates");
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Ok((swarm_handle, join))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_config_defaults_are_sane() {
        let cfg = SdkConfig::default();
        assert_eq!(cfg.network_id, "mainnet");
        assert!(cfg.bootstrap_peers.is_empty());
        assert_eq!(cfg.discovery_timeout, Duration::from_secs(30));
    }

    #[test]
    fn no_bootstrap_peers_returns_error() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(start(SdkConfig::default()));
        match result {
            Err(e) => assert!(e.to_string().contains("no bootstrap peers")),
            Ok(_) => panic!("expected error for empty bootstrap peers"),
        }
    }
}
