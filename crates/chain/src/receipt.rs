//! Inference receipts — the on-chain artifact of a verified L2 inference job.
//!
//! Shape is the authoritative spec in
//! [`docs/PROTOCOL_SPEC.md`](../../../docs/PROTOCOL_SPEC.md) §6. In Phase 1
//! Week 1-2 we define the types + encoding; the construction paths (compute
//! proof generation, verifier signatures, batching into blocks) land in
//! later week-blocks.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use arknet_common::types::{Address, Hash256, JobId, NodeId, PoolId, Signature, Timestamp};

/// Quantization level recorded in a receipt. Mirrors `model-manager`'s
/// `GgufQuant` without introducing a cross-crate dep — chain is a primitive
/// layer below `model-manager`.
///
/// Variant names intentionally use llama.cpp's canonical `Q{bits}_K_{mix}`
/// convention (e.g. `Q4_K_M`). Renaming to upper-camel-case would break
/// the recognizable identifier used across the GGUF ecosystem.
#[allow(non_camel_case_types)]
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Debug,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
#[borsh(use_discriminant = true)]
#[repr(u8)]
pub enum Quantization {
    /// Full 32-bit float.
    F32 = 0x01,
    /// 16-bit float.
    F16 = 0x02,
    /// bfloat16.
    BF16 = 0x03,
    /// 8-bit integer.
    Q8_0 = 0x04,
    /// 6-bit integer (llama.cpp).
    Q6_K = 0x05,
    /// 5-bit integer (llama.cpp).
    Q5_K_M = 0x06,
    /// 4-bit integer (llama.cpp).
    Q4_K_M = 0x07,
    /// 3-bit integer (llama.cpp).
    Q3_K_M = 0x08,
    /// 2-bit integer (llama.cpp).
    Q2_K = 0x09,
}

/// The compute node's proof-of-work witness for a given job.
///
/// Variants map to PROTOCOL_SPEC §6 `ComputeProof`. Today only `HashChain`
/// is produced; `TeeQuote` lands in Phase 3, `ZkProof` in Phase 4+.
#[derive(
    Clone, PartialEq, Eq, Hash, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub enum ComputeProof {
    /// Streaming blake3-chain of generated token IDs. Cheap; verifier
    /// re-executes deterministic jobs to check.
    HashChain(Vec<Hash256>),
    /// Raw TEE quote bytes (Intel TDX / AMD SEV-SNP). Phase 3.
    TeeQuote(Vec<u8>),
    /// Succinct zero-knowledge proof. Phase 4+.
    ZkProof(Vec<u8>),
}

/// Pointer to an off-L1 data-availability blob (Celestia, EigenDA) or the
/// inline receipt body. Phase 3+ feature — stubbed here so the receipt
/// shape is stable.
#[derive(
    Clone, PartialEq, Eq, Hash, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub enum DaLayer {
    /// Stored inline in the receipt (Phase 1-2 default).
    Inline,
    /// Celestia blob.
    Celestia,
    /// EigenDA blob.
    EigenDa,
}

/// Data-availability commitment attached to a receipt.
#[derive(
    Clone, PartialEq, Eq, Hash, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub struct DaReference {
    /// Which DA layer holds the blob.
    pub layer: DaLayer,
    /// Content-addressed commitment.
    pub commitment: Hash256,
    /// Height / slot in the DA layer.
    pub height: u64,
}

/// Optional TEE attestation body. Stubbed in Phase 1.
#[derive(
    Clone, PartialEq, Eq, Hash, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub struct TeeAttestation {
    /// Raw quote bytes.
    pub quote: Vec<u8>,
}

/// Which verification path was used for a job. Recorded on every
/// receipt so the chain has an auditable trail of verification quality.
///
/// Discriminant values are protocol-level — changing them is a hard fork.
#[repr(u8)]
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Debug,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
#[borsh(use_discriminant = true)]
pub enum VerificationTier {
    /// Default: 5% spot-check, no re-execution unless selected.
    Optimistic = 0x01,
    /// Verifier re-executed the job deterministically.
    Deterministic = 0x02,
    /// Compute node ran inside a TEE; receipt carries attestation.
    Tee = 0x03,
}

/// The on-chain record of a single inference job.
///
/// Canonical shape: `docs/PROTOCOL_SPEC.md §6`. Signed by the compute node
/// (and counter-signed by the user at settlement). Verification and reward
/// minting consult this structure.
///
/// Size bound: ~1 KB typical, capped at [`MAX_RECEIPT_BYTES`] after borsh.
#[derive(
    Clone, PartialEq, Eq, Hash, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub struct InferenceReceipt {
    // --- Identity ---
    /// Unique job identifier.
    pub job_id: JobId,
    /// Pool the job ran in.
    pub pool_id: PoolId,
    /// Canonical model identifier (e.g. `"meta-llama/Llama-3-8B-Instruct"`).
    pub model_id: String,
    /// Content-addressed model digest.
    pub model_hash: Hash256,
    /// Quantization level served.
    pub quantization: Quantization,

    // --- Participants ---
    /// Ephemeral, one-time user address.
    pub user_address: Address,
    /// Router that dispatched.
    pub router_node: NodeId,
    /// Compute node that served.
    pub compute_node: NodeId,
    /// Pre-warmed backup (optional).
    pub backup_node: Option<NodeId>,

    // --- I/O fingerprints ---
    /// `blake3(encrypted_prompt)`.
    pub input_hash: Hash256,
    /// `blake3(generated_output)`.
    pub output_hash: Hash256,
    /// Optional DA reference (Phase 3+).
    pub da_reference: Option<DaReference>,

    // --- Metering ---
    /// Input token count (post-tokenization).
    pub input_token_count: u32,
    /// Output token count.
    pub output_token_count: u32,
    /// Time-to-first-token in milliseconds.
    pub latency_ms: u32,
    /// Total wall-clock milliseconds.
    pub total_time_ms: u32,

    // --- Proof ---
    /// Deterministic-mode seed.
    pub seed: u64,
    /// Compute-proof body (hash chain / TEE / ZK).
    pub compute_proof: ComputeProof,
    /// Optional TEE attestation.
    pub tee_attestation: Option<TeeAttestation>,
    /// Which verification path was used.
    #[serde(default = "default_verification_tier")]
    pub verification_tier: VerificationTier,
    /// `true` if the prompt was encrypted to the enclave's pubkey
    /// (confidential inference).
    #[serde(default)]
    pub prompt_encrypted: bool,

    // --- Timing ---
    /// Job start timestamp (ms since epoch).
    pub timestamp_start: Timestamp,
    /// Job end timestamp (ms since epoch).
    pub timestamp_end: Timestamp,

    // --- Signatures ---
    /// Compute node signs all prior fields.
    pub compute_signature: Signature,
    /// User counter-signature (collected at settlement).
    pub user_signature: Signature,
}

/// Batched receipts anchored into a single L1 transaction.
#[derive(Clone, PartialEq, Eq, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct ReceiptBatch {
    /// Identifier — `blake3` over concatenated receipt borsh bodies.
    pub batch_id: Hash256,
    /// Receipts included.
    pub receipts: Vec<InferenceReceipt>,
    /// Merkle root of receipt hashes.
    pub merkle_root: Hash256,
    /// Router that aggregated.
    pub aggregator: NodeId,
    /// Aggregator signature over the batch.
    pub signature: Signature,
}

/// Default verification tier for backward-compatible deserialization.
fn default_verification_tier() -> VerificationTier {
    VerificationTier::Optimistic
}

/// Hard cap on receipt borsh size — anything larger is rejected before
/// consensus.
pub const MAX_RECEIPT_BYTES: usize = 64 * 1024;

/// Hard cap on receipt-batch borsh size.
pub const MAX_RECEIPT_BATCH_BYTES: usize = 16 * 1024 * 1024;

/// Maximum receipts per batch (matches `RECEIPT_BATCH_MAX` in
/// PROTOCOL_SPEC §16).
pub const RECEIPT_BATCH_MAX: usize = 1000;

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_common::types::SignatureScheme;

    fn sample_receipt() -> InferenceReceipt {
        InferenceReceipt {
            job_id: JobId::new([1; 32]),
            pool_id: PoolId::new([2; 16]),
            model_id: "test/model".to_string(),
            model_hash: [3; 32],
            quantization: Quantization::Q4_K_M,
            user_address: Address::new([4; 20]),
            router_node: NodeId::new([5; 32]),
            compute_node: NodeId::new([6; 32]),
            backup_node: None,
            input_hash: [7; 32],
            output_hash: [8; 32],
            da_reference: None,
            input_token_count: 100,
            output_token_count: 200,
            latency_ms: 500,
            total_time_ms: 2500,
            seed: 42,
            compute_proof: ComputeProof::HashChain(vec![[9; 32], [10; 32]]),
            tee_attestation: None,
            verification_tier: VerificationTier::Optimistic,
            prompt_encrypted: false,
            timestamp_start: 1_700_000_000_000,
            timestamp_end: 1_700_000_002_500,
            compute_signature: Signature::new(SignatureScheme::Ed25519, vec![0xaa; 64]).unwrap(),
            user_signature: Signature::new(SignatureScheme::Ed25519, vec![0xbb; 64]).unwrap(),
        }
    }

    #[test]
    fn receipt_borsh_roundtrip() {
        let r = sample_receipt();
        let bytes = borsh::to_vec(&r).unwrap();
        let decoded: InferenceReceipt = borsh::from_slice(&bytes).unwrap();
        assert_eq!(r, decoded);
    }

    #[test]
    fn receipt_size_under_cap() {
        let r = sample_receipt();
        let bytes = borsh::to_vec(&r).unwrap();
        assert!(
            bytes.len() < MAX_RECEIPT_BYTES,
            "typical receipt should be well under the cap, got {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn receipt_batch_borsh_roundtrip() {
        let batch = ReceiptBatch {
            batch_id: [1; 32],
            receipts: vec![sample_receipt(), sample_receipt()],
            merkle_root: [2; 32],
            aggregator: NodeId::new([3; 32]),
            signature: Signature::new(SignatureScheme::Ed25519, vec![0xcc; 64]).unwrap(),
        };
        let bytes = borsh::to_vec(&batch).unwrap();
        let decoded: ReceiptBatch = borsh::from_slice(&bytes).unwrap();
        assert_eq!(batch, decoded);
    }

    #[test]
    fn verification_tier_discriminants_are_stable() {
        assert_eq!(VerificationTier::Optimistic as u8, 0x01);
        assert_eq!(VerificationTier::Deterministic as u8, 0x02);
        assert_eq!(VerificationTier::Tee as u8, 0x03);
    }

    #[test]
    fn quantization_discriminants_are_stable() {
        // These values are protocol-level. Changing them is a hard fork.
        assert_eq!(Quantization::F32 as u8, 0x01);
        assert_eq!(Quantization::F16 as u8, 0x02);
        assert_eq!(Quantization::Q4_K_M as u8, 0x07);
    }
}
