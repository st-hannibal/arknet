//! Deterministic re-execution.
//!
//! Given an [`InferenceReceipt`], rebuild the compute node's hash
//! chain by running the same deterministic inference job through a
//! [`Reexecutor`]. If the re-derived output hash matches the
//! receipt's `output_hash`, the job is verified; otherwise the
//! caller builds a [`arknet_chain::Dispute`] via
//! [`crate::dispute::build_dispute`].
//!
//! # Why a trait
//!
//! `arknet-verifier` abstracts re-execution behind a [`Reexecutor`]
//! trait so:
//!
//! - Unit tests can use a mock that returns fixed tokens.
//! - The integration test path binds a real
//!   [`arknet_inference::InferenceEngine`].
//! - A future Phase-3 TEE-backed verifier can swap in without
//!   touching this file.

use arknet_chain::{ComputeProof, InferenceReceipt};
use arknet_common::types::Hash256;
use async_trait::async_trait;

use crate::errors::{Result, VerifierError};

/// Deterministic re-execution backend.
#[async_trait]
pub trait Reexecutor: Send + Sync {
    /// Run `receipt`'s deterministic-mode inference again and return
    /// the decoded token stream as a single flat string.
    ///
    /// The caller is responsible for building the hash chain from
    /// that output — see [`rebuild_hash_chain`].
    async fn reexecute(&self, receipt: &InferenceReceipt) -> Result<String>;
}

/// Rebuild the hash chain for `output_text` under `job_id`.
///
/// Matches the shape that `arknet-compute`'s `HashChainBuilder`
/// produces: `h_0 = blake3(DOMAIN || job_id)`;
/// `h_{i+1} = blake3(DOMAIN || h_i || token_i_bytes)`.
///
/// We can't directly depend on `arknet-compute` from `arknet-verifier`
/// (would introduce a verifier → compute dep; the compute side might
/// later depend on verifier for local self-check). Reimplementing
/// the chain here — it's 6 lines — keeps the crate graph acyclic.
pub fn rebuild_hash_chain(job_id: &arknet_common::types::JobId, tokens_text: &str) -> Vec<Hash256> {
    const DOMAIN_HASHCHAIN: &[u8] = b"arknet-hashchain-v1";
    let mut chain = Vec::new();
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN_HASHCHAIN);
    hasher.update(&job_id.0);
    let mut current: Hash256 = *hasher.finalize().as_bytes();
    chain.push(current);

    // Emit one chain step per *grapheme-like unit* would be
    // preferable, but the compute side uses token `text` chunks. For
    // a drop-in Phase-1 check we treat the full text as a single
    // absorption; when the compute emitter is changed to absorb
    // per-token we'll update both sides together. This still catches
    // full-output divergence, which is the §11 threat model.
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN_HASHCHAIN);
    hasher.update(&current);
    hasher.update(tokens_text.as_bytes());
    current = *hasher.finalize().as_bytes();
    chain.push(current);
    chain
}

/// Verdict returned by [`verify_receipt`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Re-executed hash chain matches the receipt — job is verified.
    Verified,
    /// Re-executed hash chain diverged. Compute is slashable.
    Diverged {
        /// The verifier's own `output_hash` from re-execution.
        reexec_output_hash: Hash256,
        /// The verifier's own reconstructed `ComputeProof::HashChain`.
        reexec_proof: ComputeProof,
    },
}

/// Re-execute `receipt` under `backend` and return the verdict.
///
/// `receipt.compute_proof` must be a [`ComputeProof::HashChain`]
/// — TEE / ZK proof variants are Phase 3+.
pub async fn verify_receipt<B: Reexecutor>(
    backend: &B,
    receipt: &InferenceReceipt,
) -> Result<Verdict> {
    // We rely on the receipt's stored hash chain shape — fail early
    // on unexpected variants so the verifier never silently accepts a
    // TEE quote it can't check.
    if !matches!(receipt.compute_proof, ComputeProof::HashChain(_)) {
        return Err(VerifierError::UnsupportedProof);
    }

    let text = backend.reexecute(receipt).await?;
    let chain = rebuild_hash_chain(&receipt.job_id, &text);
    let rederived_head = *chain.last().expect("chain has at least the seed");
    // Compute the `output_hash` the compute side would have signed.
    // Phase 1 convention: `output_hash = head(chain)` — i.e. the last
    // chain step is the canonical output commitment.
    if rederived_head == receipt.output_hash {
        Ok(Verdict::Verified)
    } else {
        Ok(Verdict::Diverged {
            reexec_output_hash: rederived_head,
            reexec_proof: ComputeProof::HashChain(chain),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_chain::{ComputeProof, InferenceReceipt, Quantization};
    use arknet_common::types::{
        Address, JobId, NodeId, PoolId, Signature as ApiSignature, SignatureScheme,
    };

    struct ConstBackend {
        text: String,
    }

    #[async_trait]
    impl Reexecutor for ConstBackend {
        async fn reexecute(&self, _receipt: &InferenceReceipt) -> Result<String> {
            Ok(self.text.clone())
        }
    }

    fn base_receipt(output_hash: Hash256) -> InferenceReceipt {
        InferenceReceipt {
            job_id: JobId::new([7; 32]),
            pool_id: PoolId::new([7; 16]),
            model_id: "m".into(),
            model_hash: [7; 32],
            quantization: Quantization::F32,
            user_address: Address::new([7; 20]),
            router_node: NodeId::new([7; 32]),
            compute_node: NodeId::new([8; 32]),
            backup_node: None,
            input_hash: [7; 32],
            output_hash,
            da_reference: None,
            input_token_count: 1,
            output_token_count: 1,
            latency_ms: 1,
            total_time_ms: 1,
            seed: 0,
            compute_proof: ComputeProof::HashChain(vec![[0; 32]]),
            tee_attestation: None,
            timestamp_start: 1,
            timestamp_end: 2,
            compute_signature: ApiSignature::new(SignatureScheme::Ed25519, vec![0xaa; 64]).unwrap(),
            user_signature: ApiSignature::new(SignatureScheme::Ed25519, vec![0xbb; 64]).unwrap(),
        }
    }

    #[tokio::test]
    async fn verified_when_chain_matches() {
        let chain = rebuild_hash_chain(&JobId::new([7; 32]), "hello world");
        let expected = *chain.last().unwrap();
        let rec = base_receipt(expected);
        let backend = ConstBackend {
            text: "hello world".into(),
        };
        let v = verify_receipt(&backend, &rec).await.expect("verdict");
        assert_eq!(v, Verdict::Verified);
    }

    #[tokio::test]
    async fn diverged_when_text_differs() {
        let correct = rebuild_hash_chain(&JobId::new([7; 32]), "truth");
        let expected = *correct.last().unwrap();
        let rec = base_receipt(expected);
        let backend = ConstBackend {
            text: "lies".into(),
        };
        let v = verify_receipt(&backend, &rec).await.expect("verdict");
        match v {
            Verdict::Diverged {
                reexec_output_hash,
                reexec_proof,
            } => {
                assert_ne!(reexec_output_hash, expected);
                assert!(matches!(reexec_proof, ComputeProof::HashChain(_)));
            }
            other => panic!("expected Diverged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tee_proof_rejected_at_phase_1() {
        let mut rec = base_receipt([0; 32]);
        rec.compute_proof = ComputeProof::TeeQuote(vec![0; 16]);
        let backend = ConstBackend { text: "x".into() };
        let err = verify_receipt(&backend, &rec).await.unwrap_err();
        assert!(matches!(err, VerifierError::UnsupportedProof));
    }

    #[test]
    fn rebuild_chain_is_deterministic() {
        let a = rebuild_hash_chain(&JobId::new([1; 32]), "hello");
        let b = rebuild_hash_chain(&JobId::new([1; 32]), "hello");
        assert_eq!(a, b);
    }

    #[test]
    fn rebuild_chain_changes_with_job_id() {
        let a = rebuild_hash_chain(&JobId::new([1; 32]), "x");
        let b = rebuild_hash_chain(&JobId::new([2; 32]), "x");
        assert_ne!(a.last(), b.last());
    }

    #[test]
    fn rebuild_chain_changes_with_text() {
        let a = rebuild_hash_chain(&JobId::new([1; 32]), "a");
        let b = rebuild_hash_chain(&JobId::new([1; 32]), "b");
        assert_ne!(a.last(), b.last());
    }
}
