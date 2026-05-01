//! Receipt batching + Merkle aggregation.
//!
//! A router collects [`InferenceReceipt`]s signed by the compute node
//! (and counter-signed by the user at settlement), bundles them into a
//! [`ReceiptBatch`] of up to [`RECEIPT_BATCH_MAX`] entries, and anchors
//! the batch into L1 via [`crate::anchor`].
//!
//! The batch's `merkle_root` is computed over each receipt's domain-
//! tagged `blake3` hash so a light client can verify inclusion without
//! pulling the full batch — important for Phase-3 DA offload.

use arknet_chain::{InferenceReceipt, ReceiptBatch, MAX_RECEIPT_BATCH_BYTES, RECEIPT_BATCH_MAX};
use arknet_common::types::{Hash256, NodeId, Signature};
use arknet_crypto::hash::{sha256, Sha256Digest};
use arknet_crypto::merkle::MerkleTree;

use crate::errors::{ReceiptError, Result};

/// Domain tag for receipt-hash leaves in the Merkle tree. Matches
/// `arknet_common::types::DOMAIN_RECEIPT_ROOT` (used by block-body
/// roots) prefixed with a type marker.
pub const DOMAIN_RECEIPT_LEAF: &[u8] = b"arknet-receipt-leaf-v1";

/// Hash a single receipt for Merkle leaf inclusion.
///
/// Shape: `sha256(DOMAIN_RECEIPT_LEAF || borsh(receipt))`.
/// Keeping this in the receipts crate (rather than on `InferenceReceipt`
/// itself) avoids widening the chain-layer `InferenceReceipt` API.
pub fn hash_receipt(r: &InferenceReceipt) -> Sha256Digest {
    let body = borsh::to_vec(r).expect("receipt borsh encoding is infallible");
    let mut buf = Vec::with_capacity(DOMAIN_RECEIPT_LEAF.len() + body.len());
    buf.extend_from_slice(DOMAIN_RECEIPT_LEAF);
    buf.extend_from_slice(&body);
    sha256(&buf)
}

/// Compute the Merkle root over `receipts` in the supplied order.
///
/// The order is part of the commitment — reordering receipts produces
/// a different root. Routers must preserve insertion order end-to-end.
pub fn compute_merkle_root(receipts: &[InferenceReceipt]) -> Result<Hash256> {
    if receipts.is_empty() {
        return Err(ReceiptError::Empty);
    }
    let leaves: Vec<[u8; 32]> = receipts
        .iter()
        .map(|r| *hash_receipt(r).as_bytes())
        .collect();
    let tree = MerkleTree::new(leaves.iter().map(|l| l.as_slice()))
        .map_err(|e| ReceiptError::Merkle(e.to_string()))?;
    Ok(*tree.root().as_bytes())
}

/// Compute the batch identifier — `blake3` over the concatenation of
/// every receipt's borsh body. Deterministic; unchanged by the
/// aggregator's identity.
pub fn compute_batch_id(receipts: &[InferenceReceipt]) -> Hash256 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arknet-receipt-batch-id-v1");
    for r in receipts {
        let body = borsh::to_vec(r).expect("receipt borsh encoding is infallible");
        hasher.update(&body);
    }
    *hasher.finalize().as_bytes()
}

/// Builder: accumulates receipts and emits a [`ReceiptBatch`].
///
/// Phase 1 is single-threaded — routers hold one builder per epoch,
/// seal it, and anchor. Phase 2 gains concurrent builders keyed on
/// model / pool when throughput pressure justifies the complexity.
pub struct ReceiptBatchBuilder {
    aggregator: NodeId,
    receipts: Vec<InferenceReceipt>,
}

impl ReceiptBatchBuilder {
    /// Build a new builder that tags the batch with `aggregator` (the
    /// router's node id).
    pub fn new(aggregator: NodeId) -> Self {
        Self {
            aggregator,
            receipts: Vec::new(),
        }
    }

    /// Push a receipt. Returns an error if the builder has already
    /// reached [`RECEIPT_BATCH_MAX`].
    pub fn push(&mut self, receipt: InferenceReceipt) -> Result<()> {
        if self.receipts.len() >= RECEIPT_BATCH_MAX {
            return Err(ReceiptError::TooManyReceipts {
                count: self.receipts.len() + 1,
                max: RECEIPT_BATCH_MAX,
            });
        }
        self.receipts.push(receipt);
        Ok(())
    }

    /// How many receipts are staged.
    pub fn len(&self) -> usize {
        self.receipts.len()
    }

    /// `true` if no receipts are staged.
    pub fn is_empty(&self) -> bool {
        self.receipts.is_empty()
    }

    /// Seal the builder into a [`ReceiptBatch`], attaching the
    /// aggregator's `signature` (caller signs `batch_id ||
    /// merkle_root`).
    ///
    /// Returns an error if the batch is empty or would exceed the
    /// borsh size cap.
    pub fn seal(self, signature: Signature) -> Result<ReceiptBatch> {
        if self.receipts.is_empty() {
            return Err(ReceiptError::Empty);
        }
        let merkle_root = compute_merkle_root(&self.receipts)?;
        let batch_id = compute_batch_id(&self.receipts);

        let batch = ReceiptBatch {
            batch_id,
            receipts: self.receipts,
            merkle_root,
            aggregator: self.aggregator,
            signature,
        };

        let encoded = borsh::to_vec(&batch).map_err(|e| ReceiptError::Encoding(e.to_string()))?;
        if encoded.len() > MAX_RECEIPT_BATCH_BYTES {
            return Err(ReceiptError::Oversize {
                actual: encoded.len(),
                max: MAX_RECEIPT_BATCH_BYTES,
            });
        }
        Ok(batch)
    }
}

/// Compute the digest that an aggregator signs before calling
/// [`ReceiptBatchBuilder::seal`]:
/// `sha256(DOMAIN_RECEIPT_BATCH_SIG || batch_id || merkle_root)`.
pub fn aggregator_signing_digest(batch_id: &Hash256, merkle_root: &Hash256) -> Sha256Digest {
    let mut buf = Vec::with_capacity(DOMAIN_RECEIPT_BATCH_SIG.len() + 64);
    buf.extend_from_slice(DOMAIN_RECEIPT_BATCH_SIG);
    buf.extend_from_slice(batch_id);
    buf.extend_from_slice(merkle_root);
    sha256(&buf)
}

/// Domain tag for the aggregator-signature payload. Prevents the same
/// signing key from being tricked into signing a semantically different
/// message.
pub const DOMAIN_RECEIPT_BATCH_SIG: &[u8] = b"arknet-receipt-batch-sig-v1";

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_chain::{ComputeProof, InferenceReceipt, Quantization, VerificationTier};
    use arknet_common::types::{
        Address, JobId, PoolId, Signature as ApiSignature, SignatureScheme,
    };

    fn sample_receipt(seed: u8) -> InferenceReceipt {
        InferenceReceipt {
            job_id: JobId::new([seed; 32]),
            pool_id: PoolId::new([seed; 16]),
            model_id: "local/stories260K".into(),
            model_hash: [seed; 32],
            quantization: Quantization::F32,
            user_address: Address::new([seed; 20]),
            router_node: NodeId::new([seed; 32]),
            compute_node: NodeId::new([seed.wrapping_add(1); 32]),
            backup_node: None,
            input_hash: [seed; 32],
            output_hash: [seed.wrapping_add(2); 32],
            da_reference: None,
            input_token_count: 4,
            output_token_count: 8,
            latency_ms: 100,
            total_time_ms: 500,
            seed: 42,
            compute_proof: ComputeProof::HashChain(vec![[seed; 32]]),
            tee_attestation: None,
            verification_tier: VerificationTier::Optimistic,
            prompt_encrypted: false,
            timestamp_start: 1_700_000_000_000,
            timestamp_end: 1_700_000_000_500,
            compute_signature: ApiSignature::new(SignatureScheme::Ed25519, vec![0xaa; 64]).unwrap(),
            user_signature: ApiSignature::new(SignatureScheme::Ed25519, vec![0xbb; 64]).unwrap(),
        }
    }

    fn sig() -> ApiSignature {
        ApiSignature::ed25519([0xcc; 64])
    }

    #[test]
    fn empty_builder_fails_seal() {
        let b = ReceiptBatchBuilder::new(NodeId::new([1; 32]));
        let err = b.seal(sig()).unwrap_err();
        assert!(matches!(err, ReceiptError::Empty));
    }

    #[test]
    fn single_receipt_seals_with_merkle_root() {
        let mut b = ReceiptBatchBuilder::new(NodeId::new([1; 32]));
        b.push(sample_receipt(0x10)).unwrap();
        let batch = b.seal(sig()).expect("seal ok");
        assert_eq!(batch.receipts.len(), 1);
        assert_ne!(batch.merkle_root, [0u8; 32], "root must be populated");
        assert_ne!(batch.batch_id, [0u8; 32]);
    }

    #[test]
    fn batch_id_stable_across_builds() {
        let mut b1 = ReceiptBatchBuilder::new(NodeId::new([1; 32]));
        let mut b2 = ReceiptBatchBuilder::new(NodeId::new([9; 32]));
        for seed in [1u8, 2, 3] {
            b1.push(sample_receipt(seed)).unwrap();
            b2.push(sample_receipt(seed)).unwrap();
        }
        let batch_a = b1.seal(sig()).unwrap();
        let batch_b = b2.seal(sig()).unwrap();
        assert_eq!(
            batch_a.batch_id, batch_b.batch_id,
            "batch_id ignores aggregator"
        );
        assert_eq!(batch_a.merkle_root, batch_b.merkle_root);
    }

    #[test]
    fn reordering_changes_merkle_root() {
        let mut b1 = ReceiptBatchBuilder::new(NodeId::new([1; 32]));
        let mut b2 = ReceiptBatchBuilder::new(NodeId::new([1; 32]));
        b1.push(sample_receipt(1)).unwrap();
        b1.push(sample_receipt(2)).unwrap();
        b2.push(sample_receipt(2)).unwrap();
        b2.push(sample_receipt(1)).unwrap();
        let a = b1.seal(sig()).unwrap();
        let b = b2.seal(sig()).unwrap();
        assert_ne!(a.merkle_root, b.merkle_root);
    }

    #[test]
    fn push_rejects_past_cap() {
        let mut b = ReceiptBatchBuilder::new(NodeId::new([1; 32]));
        for i in 0..RECEIPT_BATCH_MAX {
            b.push(sample_receipt((i % 255) as u8)).unwrap();
        }
        let err = b.push(sample_receipt(0)).unwrap_err();
        assert!(matches!(err, ReceiptError::TooManyReceipts { .. }));
    }

    #[test]
    fn hundred_receipts_fit_in_one_batch() {
        // Spec invariant: RECEIPT_BATCH_MAX = 1000, §16. 100 typical
        // receipts must encode well under MAX_RECEIPT_BATCH_BYTES.
        let mut b = ReceiptBatchBuilder::new(NodeId::new([1; 32]));
        for i in 0..100u8 {
            b.push(sample_receipt(i)).unwrap();
        }
        let batch = b.seal(sig()).expect("100 receipts seal");
        assert_eq!(batch.receipts.len(), 100);
        let encoded = borsh::to_vec(&batch).unwrap();
        assert!(
            encoded.len() < MAX_RECEIPT_BATCH_BYTES,
            "100-receipt batch should fit well under the cap, got {} bytes",
            encoded.len()
        );
    }

    #[test]
    fn signing_digest_is_domain_separated() {
        let batch_id = [0x11u8; 32];
        let root = [0x22u8; 32];
        let d = aggregator_signing_digest(&batch_id, &root);
        // Domain-tag collision check: a naive `sha256(batch_id || root)`
        // must differ from our digest.
        let mut naive = Vec::with_capacity(64);
        naive.extend_from_slice(&batch_id);
        naive.extend_from_slice(&root);
        let d_naive = arknet_crypto::hash::sha256(&naive);
        assert_ne!(d, d_naive, "must differ from untagged digest");
    }

    #[test]
    fn merkle_root_helper_matches_seal() {
        let r = sample_receipt(7);
        let mut b = ReceiptBatchBuilder::new(NodeId::new([1; 32]));
        b.push(r.clone()).unwrap();
        let batch = b.seal(sig()).unwrap();
        let direct = compute_merkle_root(&[r]).unwrap();
        assert_eq!(batch.merkle_root, direct);
    }
}
