//! Compute role body.
//!
//! Week-10 scope: attach a [`arknet_compute::ComputeJobRunner`] to the
//! runtime (the runtime's existing [`arknet_inference::InferenceEngine`]
//! is the inner engine) and park until shutdown. When a router and a
//! compute role run in the same binary, the scheduler also registers
//! a [`crate::l2_dispatch::LocalComputeDispatcher`] into the router's
//! candidate registry so jobs flow end-to-end in-process.

#![allow(dead_code)]

use std::sync::Arc;

use arknet_common::types::{Address, NodeId, PoolId};
use arknet_compute::wire::{InferenceJobRequest, PoolOffer};
use arknet_compute::ComputeJobRunner;
use arknet_model_manager::ModelRef;
use arknet_network::{InboundInferenceRequest, InferenceResponse, NetworkHandle};
use arknet_router::candidate::Candidate;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

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

/// Drive the compute role until shutdown. If `inference_requests` is
/// provided, the compute node handles incoming p2p inference requests
/// from remote routers.
/// Shared list of loaded models, updated by the RPC model-load handler
/// and read by the heartbeat to re-gossip PoolOffers.
pub type LoadedModels = Arc<parking_lot::Mutex<Vec<String>>>;

pub async fn run(
    rt: NodeRuntime,
    mut inference_requests: Option<mpsc::Receiver<InboundInferenceRequest>>,
    loaded_models: LoadedModels,
    shutdown: CancellationToken,
) -> Result<()> {
    let Some(runner) = rt.compute.clone() else {
        return Err(crate::errors::NodeError::Config(
            "compute role requires a ComputeJobRunner — not attached at boot".into(),
        ));
    };
    info!("compute role online — awaiting shutdown");

    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(60));
    heartbeat.tick().await;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("compute role shutting down cleanly");
                return Ok(());
            }
            _ = heartbeat.tick() => {
                let models = loaded_models.lock().clone();
                if !models.is_empty() {
                    if let Some(net) = &rt.network {
                        let operator = local_operator(&rt.data_dir);
                        let tee = rt.cfg.tee.enabled;
                        announce_models(net, models, operator, tee).await;
                    }
                }
            }
            Some(inbound) = async {
                match inference_requests.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let network = rt.network.clone();
                let runner = runner.clone();
                let inference = rt.inference.clone();
                tokio::spawn(async move {
                    handle_inference_request(inbound, runner, inference, network).await;
                });
            }
        }
    }
}

/// Handle a single inbound inference request from a remote router.
async fn handle_inference_request(
    inbound: InboundInferenceRequest,
    runner: ComputeJobRunner,
    _inference: arknet_inference::InferenceEngine,
    network: Option<NetworkHandle>,
) {
    let Some(net) = network else {
        warn!("received inference request but network handle is not available");
        return;
    };

    let req: InferenceJobRequest = match borsh::from_slice(&inbound.data) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "failed to decode inference request");
            return;
        }
    };

    let now_ms = arknet_router::failover::now_ms();
    if let Err(e) = arknet_router::intake::verify_request(&req, now_ms) {
        warn!(error = %e, "inference request verification failed");
        return;
    }

    let model_ref = match ModelRef::parse(&req.model_ref) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, model = %req.model_ref, "bad model ref in inference request");
            return;
        }
    };

    let pool_id = local_pool_id();
    let now = arknet_router::failover::now_ms();
    let job_id = {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"arknet-job-id-v1");
        hasher.update(&req.billing_address().0);
        hasher.update(&req.nonce.to_le_bytes());
        hasher.update(&now.to_le_bytes());
        let digest = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(digest.as_bytes());
        arknet_common::types::JobId::new(out)
    };

    let stream = match runner.run(req, &model_ref, pool_id, job_id, now).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "compute job failed");
            return;
        }
    };

    let mut pinned = std::pin::pin!(stream);
    let mut encoded_events = Vec::new();
    while let Some(event) = pinned.next().await {
        match borsh::to_vec(&event) {
            Ok(bytes) => encoded_events.push(bytes),
            Err(e) => {
                error!(error = %e, "failed to encode inference event");
                return;
            }
        }
    }

    let response = InferenceResponse::new(encoded_events);
    let wire_resp = match borsh::to_vec(&response) {
        Ok(b) => b,
        Err(e) => {
            error!(error = %e, "failed to encode inference response");
            return;
        }
    };

    if let Err(e) = net
        .send_inference_response(inbound.request_id, wire_resp)
        .await
    {
        warn!(error = %e, "failed to send inference response");
    }
}

/// Publish a `PoolOffer` on gossip so routers discover this compute node.
pub async fn announce_models(
    network: &NetworkHandle,
    model_refs: Vec<String>,
    operator: Address,
    supports_tee: bool,
) {
    let offer = PoolOffer {
        peer_id: network.local_peer_id().to_bytes(),
        model_refs,
        operator,
        total_stake: 1_000_000,
        supports_tee,
        timestamp_ms: arknet_router::failover::now_ms(),
        available_slots: 1,
    };
    let data = match borsh::to_vec(&offer) {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "failed to encode pool offer");
            return;
        }
    };
    let topic = arknet_network::gossip::pool_offer().to_string();
    if let Err(e) = network.publish(topic, data).await {
        warn!(error = %e, "failed to publish pool offer");
    } else {
        info!(models = ?offer.model_refs, "published pool offer");
    }
}
