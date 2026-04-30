//! Compute role body.
//!
//! Week-10 scope: attach a [`arknet_compute::ComputeJobRunner`] to the
//! runtime (the runtime's existing [`arknet_inference::InferenceEngine`]
//! is the inner engine) and park until shutdown. When a router and a
//! compute role run in the same binary, the scheduler also registers
//! a [`crate::l2_dispatch::LocalComputeDispatcher`] into the router's
//! candidate registry so jobs flow end-to-end in-process.

// Helpers in this module are exercised by node-level integration
// tests only; mark them `allow(dead_code)` so clippy doesn't reject
// the currently thin boot path. Week 11 wires these into the
// multi-role boot sequence when verifier + L2 mesh land.
#![allow(dead_code)]

use std::sync::Arc;

use arknet_common::types::{Address, NodeId, PoolId};
use arknet_compute::ComputeJobRunner;
use arknet_router::candidate::Candidate;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::errors::Result;
use crate::l2_dispatch::LocalComputeDispatcher;
use crate::runtime::NodeRuntime;

/// Synthetic pool id for the Phase-1 node: `blake3("arknet-local-pool")[..16]`.
/// Real pool ids come from the on-chain pool registry (Week 11+); this
/// keeps a stable placeholder so receipts + quota buckets line up
/// between the local router and the local compute during tests.
pub fn local_pool_id() -> PoolId {
    let digest = blake3::hash(b"arknet-local-pool");
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest.as_bytes()[..16]);
    PoolId::new(out)
}

/// Operator address used when a node plays compute + router in one
/// process. Derived deterministically from the data-dir path so tests
/// get repeatable addresses without any on-disk keystore dependency.
pub fn local_operator(data_dir: &std::path::Path) -> Address {
    let digest = blake3::hash(data_dir.as_os_str().as_encoded_bytes());
    let mut out = [0u8; 20];
    out.copy_from_slice(&digest.as_bytes()[..20]);
    Address::new(out)
}

/// Node id companion to [`local_operator`].
pub fn local_node_id(data_dir: &std::path::Path) -> NodeId {
    let digest = blake3::hash(data_dir.as_os_str().as_encoded_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_bytes());
    NodeId::new(out)
}

/// Register the local compute runner as a candidate in the router's
/// registry (co-located router + compute case). `model_refs` are the
/// canonical model refs this compute node advertises.
pub fn register_self_as_candidate(
    rt: &NodeRuntime,
    runner: ComputeJobRunner,
    model_refs: Vec<String>,
) {
    let Some(router) = rt.router.as_ref() else {
        return;
    };
    let Some(first_model) = model_refs.first() else {
        // Router would never pick us anyway; skip.
        return;
    };
    let model_ref = match arknet_model_manager::ModelRef::parse(first_model) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(model=%first_model, error=%e, "skipping self-registration — bad model ref");
            return;
        }
    };
    let pool_id = local_pool_id();
    let dispatcher = Arc::new(LocalComputeDispatcher::new(runner, pool_id, model_ref));
    let candidate = Candidate {
        node_id: local_node_id(&rt.data_dir),
        operator: local_operator(&rt.data_dir),
        total_stake: 1_000_000,
        model_refs,
        last_seen_ms: arknet_router::failover::now_ms(),
        dispatcher,
        supports_tee: rt.cfg.tee.enabled,
    };
    router.registry().upsert(candidate);
}

/// Drive the compute role until shutdown.
pub async fn run(rt: NodeRuntime, shutdown: CancellationToken) -> Result<()> {
    let Some(_runner) = rt.compute.clone() else {
        return Err(crate::errors::NodeError::Config(
            "compute role requires a ComputeJobRunner — not attached at boot".into(),
        ));
    };
    info!("compute role online — awaiting shutdown");
    shutdown.cancelled().await;
    info!("compute role shutting down cleanly");
    Ok(())
}
