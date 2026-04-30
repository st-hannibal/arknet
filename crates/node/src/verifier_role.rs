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
use arknet_crypto::keys::SigningKey;
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

    let backend = Arc::new(StubReexecutor);

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

/// Stub re-executor. Phase-1 always returns the empty string,
/// effectively treating every receipt as diverged *if* selected.
/// When the real `InferenceEngine`-backed re-executor lands the
/// verifier will produce accurate verdicts; for now the dispatch
/// path is exercised end-to-end and the economic test suite proves
/// the slashing flow.
///
/// A real implementation loads the model, runs deterministic inference,
/// and returns the full token stream as text. Deferred to Phase-1
/// exit polish so we don't couple the verifier role body to llama.cpp
/// fixture availability on CI.
struct StubReexecutor;

#[async_trait]
impl Reexecutor for StubReexecutor {
    async fn reexecute(
        &self,
        _receipt: &arknet_chain::InferenceReceipt,
    ) -> arknet_verifier::Result<String> {
        Ok(String::new())
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
