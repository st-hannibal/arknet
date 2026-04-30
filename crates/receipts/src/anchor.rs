//! Anchor a [`ReceiptBatch`] into L1 via a [`SignedTransaction`].
//!
//! The router (or any holder of the aggregator key) calls
//! [`build_anchor_tx`] to wrap the sealed batch in a
//! [`Transaction::ReceiptBatch`] and signs the transaction hash. The
//! result is ready to push through `/v1/tx` → mempool → consensus →
//! `apply_receipt_batch`.
//!
//! Keeping this helper in `arknet-receipts` (rather than the router)
//! lets the verifier crate reuse it when submitting dispute-driven
//! post-hoc anchors in Phase 2.

use arknet_chain::{ReceiptBatch, SignedTransaction, Transaction};
use arknet_common::types::{PubKey, Signature};

use crate::errors::{ReceiptError, Result};

/// Wrap `batch` in a [`SignedTransaction`].
///
/// The caller supplies the `signer_pubkey` (must match the address
/// derived by the chain's apply layer) and a function that produces a
/// signature over the transaction hash bytes. We accept a sign-fn
/// rather than a raw `SigningKey` so operators can hold the secret
/// behind whatever key-management layer they use (OS keychain, HSM,
/// future remote signer).
pub fn build_anchor_tx<F>(
    batch: ReceiptBatch,
    signer_pubkey: PubKey,
    sign_hash: F,
) -> Result<SignedTransaction>
where
    F: FnOnce(&[u8; 32]) -> Signature,
{
    // Validate once more that the batch meets the size cap — defense
    // in depth against callers that bypassed the builder.
    let encoded = borsh::to_vec(&batch).map_err(|e| ReceiptError::Encoding(e.to_string()))?;
    if encoded.len() > arknet_chain::MAX_RECEIPT_BATCH_BYTES {
        return Err(ReceiptError::Oversize {
            actual: encoded.len(),
            max: arknet_chain::MAX_RECEIPT_BATCH_BYTES,
        });
    }

    let tx = Transaction::ReceiptBatch(batch);
    let hash = tx.hash();
    let signature = sign_hash(hash.as_bytes());

    Ok(SignedTransaction {
        tx,
        signer: signer_pubkey,
        signature,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::ReceiptBatchBuilder;
    use arknet_chain::{ComputeProof, InferenceReceipt, Quantization, VerificationTier};
    use arknet_common::types::{
        Address, JobId, NodeId, PoolId, Signature as ApiSignature, SignatureScheme,
    };
    use arknet_crypto::keys::SigningKey;
    use arknet_crypto::signatures::sign;

    fn sample_receipt() -> InferenceReceipt {
        InferenceReceipt {
            job_id: JobId::new([1; 32]),
            pool_id: PoolId::new([1; 16]),
            model_id: "m".into(),
            model_hash: [1; 32],
            quantization: Quantization::F32,
            user_address: Address::new([1; 20]),
            router_node: NodeId::new([1; 32]),
            compute_node: NodeId::new([2; 32]),
            backup_node: None,
            input_hash: [1; 32],
            output_hash: [2; 32],
            da_reference: None,
            input_token_count: 1,
            output_token_count: 1,
            latency_ms: 1,
            total_time_ms: 1,
            seed: 0,
            compute_proof: ComputeProof::HashChain(vec![[0; 32]]),
            tee_attestation: None,
            verification_tier: VerificationTier::Optimistic,
            prompt_encrypted: false,
            timestamp_start: 1,
            timestamp_end: 2,
            compute_signature: ApiSignature::new(SignatureScheme::Ed25519, vec![0xaa; 64]).unwrap(),
            user_signature: ApiSignature::new(SignatureScheme::Ed25519, vec![0xbb; 64]).unwrap(),
        }
    }

    #[test]
    fn build_anchor_tx_produces_valid_signed_tx() {
        let sk = SigningKey::generate();
        let pk = sk.verifying_key().to_pubkey();
        let mut b = ReceiptBatchBuilder::new(NodeId::new([1; 32]));
        b.push(sample_receipt()).unwrap();
        let batch = b.seal(ApiSignature::ed25519([0xcc; 64])).unwrap();

        let stx = build_anchor_tx(batch, pk.clone(), |h| sign(&sk, h)).expect("wrap ok");
        assert!(matches!(stx.tx, Transaction::ReceiptBatch(_)));
        // Signature verifies against tx hash.
        let hash = stx.tx.hash();
        arknet_crypto::signatures::verify(&pk, hash.as_bytes(), &stx.signature)
            .expect("anchor sig verifies");
    }
}
