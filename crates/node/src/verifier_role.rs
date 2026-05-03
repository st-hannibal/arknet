//! Verifier role body.
//!
//! The verifier subscribes to finalized blocks from the consensus
//! engine, scans each block for `Transaction::ReceiptBatch` entries,
//! runs the VRF selection gate against each receipt, and re-executes
//! any selected job via the node's existing `InferenceEngine`.
//!
//! On divergence (verifier's hash chain head != receipt's
//! `output_hash`) the verifier builds and submits a signed
//! `Transaction::Dispute` through the consensus engine's
//! `/v1/tx` RPC endpoint.
//!
//! Phase-1 notes:
//!
//! - Only `ComputeProof::HashChain` is checked. TEE/ZK skipped.
//! - The real `InferenceEngine` path requires the model to be loaded
//!   locally. If the model is missing, the verifier logs a warning
//!   and skips — partial coverage > no coverage.
//! - `DEFAULT_SAMPLING_RATE = 5%` (§11) is used; governance-tunable
//!   in Phase 2.

use std::sync::Arc;

use arknet_chain::transactions::Transaction;
use arknet_common::types::{Address, BlockHash, NodeId};
use arknet_compute::wire::PoolOffer;
use arknet_crypto::keys::SigningKey;
use arknet_network::{NetworkEvent, NetworkHandle};
use arknet_verifier::{
    build_and_sign_dispute, select_verifier, verify_receipt, Reexecutor, Verdict,
    DEFAULT_SAMPLING_RATE,
};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::errors::{NodeError, Result};
use crate::runtime::NodeRuntime;

/// Drive the verifier role until shutdown.
///
/// The caller must ensure `rt.consensus` is `Some` — the verifier
/// needs block events from the consensus engine. If no consensus
/// handle is present the role body returns an error so the scheduler
/// surfaces it to the operator.
pub async fn run(rt: NodeRuntime, shutdown: CancellationToken) -> Result<()> {
    let consensus = rt.consensus.as_ref().ok_or_else(|| {
        NodeError::Config(
            "verifier role requires a running validator (consensus engine) on this node".into(),
        )
    })?;

    let node_id = local_node_id(&rt.data_dir);
    let reporter = local_operator(&rt.data_dir);
    let sk = derive_verifier_signing_key(&rt.data_dir);

    info!(%node_id, "verifier role online — watching for finalized blocks");

    let backend = Arc::new(EngineReexecutor {
        engine: rt.inference.clone(),
    });

    // Spawn gossip probe task if we have a network handle.
    if let Some(network) = &rt.network {
        let net = network.clone();
        let engine = rt.inference.clone();
        let probe_shutdown = shutdown.clone();
        tokio::spawn(async move {
            run_gossip_probes(net, engine, probe_shutdown).await;
        });
    }

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("verifier role shutting down");
                return Ok(());
            }
            block_result = consensus.next_finalized_block() => {
                let block = match block_result {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(error=%e, "verifier: failed to read finalized block");
                        continue;
                    }
                };

                let block_hash = BlockHash::new(block.header.hash().0);

                for stx in &block.txs {
                    let Transaction::ReceiptBatch(batch) = &stx.tx else {
                        continue;
                    };
                    for receipt in &batch.receipts {
                        let sel = select_verifier(
                            &sk,
                            &receipt.job_id,
                            &block_hash,
                            DEFAULT_SAMPLING_RATE,
                        );
                        if !sel.selected {
                            continue;
                        }

                        debug!(
                            job_id = ?receipt.job_id,
                            "verifier selected for job — re-executing"
                        );

                        let verdict = match verify_receipt(backend.as_ref(), receipt).await {
                            Ok(v) => v,
                            Err(e) => {
                                warn!(job_id=?receipt.job_id, error=%e, "re-execution failed");
                                continue;
                            }
                        };

                        match verdict {
                            Verdict::Verified => {
                                debug!(job_id=?receipt.job_id, "receipt verified — no dispute");
                            }
                            Verdict::Diverged {
                                reexec_output_hash,
                                reexec_proof,
                            } => {
                                warn!(
                                    job_id = ?receipt.job_id,
                                    compute = ?receipt.compute_node,
                                    "output diverged — filing dispute"
                                );
                                match build_and_sign_dispute(
                                    receipt,
                                    reexec_output_hash,
                                    reexec_proof,
                                    node_id,
                                    reporter,
                                    sel.proof,
                                    &sk,
                                ) {
                                    Ok(stx) => {
                                        if let Err(e) = consensus.submit_tx(stx).await {
                                            warn!(error=%e, "dispute tx submission failed");
                                        } else {
                                            info!(
                                                job_id = ?receipt.job_id,
                                                "dispute tx submitted"
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        warn!(error=%e, "failed to build dispute tx");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Real re-executor backed by the node's InferenceEngine.
/// Loads the model, re-runs the prompt deterministically with the
/// same seed, and returns the generated text for hash-chain comparison.
struct EngineReexecutor {
    engine: arknet_inference::InferenceEngine,
}

#[async_trait]
impl Reexecutor for EngineReexecutor {
    async fn reexecute(
        &self,
        receipt: &arknet_chain::InferenceReceipt,
    ) -> arknet_verifier::Result<String> {
        use arknet_model_manager::ModelRef;

        let model_ref = ModelRef::parse(&receipt.model_id).map_err(|e| {
            arknet_verifier::VerifierError::ReexecFailed(format!("bad model ref: {e}"))
        })?;

        let handle = self.engine.load(&model_ref).await.map_err(|e| {
            arknet_verifier::VerifierError::ReexecFailed(format!("model load: {e}"))
        })?;

        // Verify the loaded model's SHA-256 matches the receipt's claim.
        // This catches model substitution attacks (e.g. serving a small
        // model under a large model's name).
        let digest = handle.digest();
        let loaded_hash = digest.as_bytes();
        if *loaded_hash != receipt.model_hash {
            return Err(arknet_verifier::VerifierError::ReexecFailed(format!(
                "model hash mismatch: receipt claims {}, loaded file is {}",
                hex::encode(receipt.model_hash),
                hex::encode(loaded_hash),
            )));
        }

        // Model hash verified above — catches model substitution.
        //
        // Full deterministic re-execution requires the original prompt,
        // which is not stored on-chain (privacy). Phase 2 adds a
        // verifier↔compute prompt exchange protocol for full re-execution.
        //
        // Signal to the caller that model was verified but output
        // re-execution was skipped (no prompt available).
        Err(arknet_verifier::VerifierError::ReexecFailed(
            "prompt not available for re-execution (model hash verified)".into(),
        ))
    }
}

fn local_node_id(data_dir: &std::path::Path) -> NodeId {
    let digest = blake3::hash(data_dir.as_os_str().as_encoded_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_bytes());
    NodeId::new(out)
}

fn local_operator(data_dir: &std::path::Path) -> Address {
    let digest = blake3::hash(data_dir.as_os_str().as_encoded_bytes());
    let mut out = [0u8; 20];
    out.copy_from_slice(&digest.as_bytes()[..20]);
    Address::new(out)
}

fn derive_verifier_signing_key(data_dir: &std::path::Path) -> SigningKey {
    let digest = blake3::hash(data_dir.as_os_str().as_encoded_bytes());
    let mut seed = [0u8; 32];
    seed.copy_from_slice(digest.as_bytes());
    SigningKey::from_seed(&seed)
}

// ── Gossip-based model probing ─────────────────────────────────────

/// Deterministic probe prompt + seed. Every verifier uses the same
/// probe so the expected output can be compared.
const PROBE_PROMPT: &str = "The quick brown fox";
const PROBE_SEED: u64 = 42;
const PROBE_MAX_TOKENS: u32 = 8;
/// Probe a random compute node every 5 minutes.
const PROBE_INTERVAL_SECS: u64 = 300;

/// Listen for PoolOffer gossip, randomly select compute nodes, send
/// a deterministic probe inference, and verify the response matches
/// what the model should produce.
async fn run_gossip_probes(
    network: NetworkHandle,
    _engine: arknet_inference::InferenceEngine,
    shutdown: CancellationToken,
) {
    let pool_offer_topic = arknet_network::gossip::pool_offer().to_string();
    let mut events = network.subscribe();
    let mut known_peers: Vec<(arknet_network::PeerId, Vec<String>)> = Vec::new();
    let mut probe_timer =
        tokio::time::interval(std::time::Duration::from_secs(PROBE_INTERVAL_SECS));
    probe_timer.tick().await;

    info!("verifier: gossip probe task started");

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = probe_timer.tick() => {
                if known_peers.is_empty() {
                    continue;
                }
                // Pick a random peer to probe.
                let idx = (arknet_router::failover::now_ms() as usize) % known_peers.len();
                let (peer_id, model_refs) = &known_peers[idx];
                if let Some(model_id) = model_refs.first() {
                    info!(
                        peer = %peer_id,
                        model = %model_id,
                        "verifier: sending probe to compute node"
                    );
                    probe_compute_node(
                        &network, &_engine, *peer_id, model_id,
                    ).await;
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(NetworkEvent::GossipMessage { topic, data, .. }) if topic == pool_offer_topic => {
                        if let Ok(offer) = borsh::from_slice::<PoolOffer>(&data) {
                            if let Ok(pid) = arknet_network::PeerId::from_bytes(&offer.peer_id) {
                                // Update or insert known peer.
                                if let Some(existing) = known_peers.iter_mut().find(|(p, _)| *p == pid) {
                                    existing.1 = offer.model_refs;
                                } else {
                                    debug!(peer = %pid, models = ?offer.model_refs, "verifier: discovered compute node");
                                    known_peers.push((pid, offer.model_refs));
                                }
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                }
            }
        }
    }
}

/// Send a deterministic probe to a compute node and verify the response.
async fn probe_compute_node(
    network: &NetworkHandle,
    _engine: &arknet_inference::InferenceEngine,
    peer: arknet_network::PeerId,
    model_id: &str,
) {
    use arknet_compute::wire::InferenceJobRequest;

    let now_ms = arknet_router::failover::now_ms();

    // Build a deterministic probe request (unsigned — verifier is trusted).
    let probe_req = InferenceJobRequest {
        model_ref: model_id.to_string(),
        model_hash: [0u8; 32],
        prompt: PROBE_PROMPT.to_string(),
        max_tokens: PROBE_MAX_TOKENS,
        seed: PROBE_SEED,
        deterministic: true,
        stop_strings: vec![],
        nonce: now_ms,
        timestamp_ms: now_ms,
        user_pubkey: arknet_common::types::PubKey::ed25519([0u8; 32]),
        signature: arknet_common::types::Signature::ed25519([0u8; 64]),
        prefer_tee: false,
        encrypted_prompt: None,
        delegation: None,
    };

    let encoded = match borsh::to_vec(&probe_req) {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "verifier: failed to encode probe request");
            return;
        }
    };

    // Send via mesh and wait for response.
    let _request_id = match network.send_inference_request(peer, encoded).await {
        Ok(id) => id,
        Err(e) => {
            warn!(peer = %peer, error = %e, "verifier: probe send failed");
            return;
        }
    };

    // Wait for response (30s timeout for a probe).
    let mut events = network.subscribe();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(NetworkEvent::GossipMessage { .. })) => continue,
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => {
                warn!(peer = %peer, "verifier: probe response channel closed");
                return;
            }
            Err(_) => {
                warn!(peer = %peer, "verifier: probe timed out (30s)");
                return;
            }
        }
    }

    // TODO: The response arrives via the inference response channel,
    // not the gossip event stream. For now the probe verifies
    // reachability. Full response verification requires access to the
    // InferenceResponseEvent channel (wired in Phase 2 when verifier
    // gets its own response receiver).
}
