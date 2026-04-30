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

use arknet_compute::free_tier::{FreeTierConfig, FreeTierTracker};
use arknet_router::{CandidateRegistry, Router};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::errors::Result;
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
