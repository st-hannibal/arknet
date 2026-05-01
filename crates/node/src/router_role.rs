//! Router role body.
//!
//! Week-10 scope: instantiate a [`arknet_router::Router`] backed by
//! the runtime's candidate registry + free-tier tracker, subscribe to
//! the `arknet/quota/tick/1` gossip topic (future-proof — merging is
//! wired up even though production routers haven't started gossipping
//! yet), and park until shutdown.
//!
//! The actual entrypoint from clients is the `/v1/inference` RPC; when
//! the router role is active the [`crate::rpc`] layer forwards jobs
//! into the shared [`Router`] instead of running inference locally.

use std::sync::Arc;

use arknet_common::types::NodeId;
use arknet_compute::free_tier::{FreeTierConfig, FreeTierTracker};
use arknet_compute::wire::PoolOffer;
use arknet_network::{InferenceResponseEvent, NetworkEvent, NetworkHandle, PeerId};
use arknet_router::candidate::Candidate;
use arknet_router::{CandidateRegistry, Router};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::errors::Result;
use crate::l2_dispatch::RemoteComputeDispatcher;
use crate::runtime::NodeRuntime;

/// Drive the router role until shutdown.
///
/// Assumes the runtime is already holding a [`Router`] handle — the
/// scheduler attaches one at boot via [`attach_router`]. This body is
/// intentionally thin: the gossip subscription lives in the network
/// task and the RPC proxy lives in the rpc task; we mostly log and
/// wait here so `arknet status` can report "router role online".
pub async fn run(rt: NodeRuntime, shutdown: CancellationToken) -> Result<()> {
    let Some(router) = rt.router.clone() else {
        return Err(crate::errors::NodeError::Config(
            "router role requires a Router handle — not attached at boot".into(),
        ));
    };
    info!(
        candidates = router.registry().len(),
        "router role online — awaiting shutdown"
    );
    shutdown.cancelled().await;
    info!("router role shutting down cleanly");
    Ok(())
}

/// Build a fresh [`Router`] with an empty candidate registry and the
/// default free-tier config. The caller (scheduler) stores the result
/// on the runtime.
pub fn build_router() -> Router {
    let registry = CandidateRegistry::new();
    let tracker = FreeTierTracker::new(FreeTierConfig::default());
    Router::new(registry, tracker)
}

/// Spawn a background task that listens for `pool/offer` gossip messages
/// and upserts remote compute nodes into the candidate registry.
pub fn start_gossip_listener(
    network: NetworkHandle,
    registry: CandidateRegistry,
    response_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<InferenceResponseEvent>>>,
    shutdown: CancellationToken,
) {
    let pool_offer_topic = arknet_network::gossip::pool_offer().to_string();
    let mut events = network.subscribe();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                result = events.recv() => {
                    let Ok(event) = result else { continue };
                    let NetworkEvent::GossipMessage { topic, data, .. } = event else {
                        continue;
                    };
                    if topic != pool_offer_topic {
                        continue;
                    }
                    let offer: PoolOffer = match borsh::from_slice(&data) {
                        Ok(o) => o,
                        Err(e) => {
                            warn!(error = %e, "malformed pool offer");
                            continue;
                        }
                    };
                    let peer_id = match PeerId::from_bytes(&offer.peer_id) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!(error = %e, "bad peer_id in pool offer");
                            continue;
                        }
                    };
                    let node_id = {
                        let digest = blake3::hash(&offer.peer_id);
                        let mut out = [0u8; 32];
                        out.copy_from_slice(digest.as_bytes());
                        NodeId::new(out)
                    };
                    let dispatcher = Arc::new(RemoteComputeDispatcher::new(
                        network.clone(),
                        peer_id,
                        response_rx.clone(),
                    ));
                    let candidate = Candidate {
                        node_id,
                        operator: offer.operator,
                        total_stake: offer.total_stake,
                        model_refs: offer.model_refs.clone(),
                        last_seen_ms: offer.timestamp_ms,
                        dispatcher,
                        supports_tee: offer.supports_tee,
                    };
                    debug!(peer = %peer_id, models = ?offer.model_refs, "registered remote compute candidate");
                    registry.upsert(candidate);
                }
            }
        }
    });
}
