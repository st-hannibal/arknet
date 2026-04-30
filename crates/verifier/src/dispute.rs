//! Dispute-transaction construction.
//!
//! Wraps a [`crate::reexec::Verdict::Diverged`] into an
//! on-chain [`arknet_chain::Transaction::Dispute`] signed by the
//! verifier. The transaction — once accepted by a block — triggers
//! the Week-9 slashing pathway for the offending compute node.

use arknet_chain::{ComputeProof, Dispute, InferenceReceipt, SignedTransaction, Transaction};
use arknet_common::types::{Address, Hash256, NodeId, PubKey, Signature};
use arknet_crypto::signatures::sign;
use arknet_crypto::vrf::VrfProof;

use crate::errors::{Result, VerifierError};

/// Build a signed dispute transaction.
///
/// Parameters:
///
/// - `receipt` — the anchored receipt under dispute.
/// - `reexec_output_hash` — verifier's re-derived `output_hash`
///   (must differ from `receipt.output_hash`).
/// - `reexec_proof` — verifier's re-built `ComputeProof::HashChain`.
/// - `verifier_node` — verifier's node id.
/// - `reporter` — where the reporter cut of the slash goes (§10).
/// - `vrf_proof` — proves the verifier was selected for this job.
/// - `verifier_pubkey` — pubkey the tx signature is under.
/// - `sign_hash` — sign function closure over tx hash bytes.
#[allow(clippy::too_many_arguments)]
pub fn build_dispute<F>(
    receipt: &InferenceReceipt,
    reexec_output_hash: Hash256,
    reexec_proof: ComputeProof,
    verifier_node: NodeId,
    reporter: Address,
    vrf_proof: VrfProof,
    verifier_pubkey: PubKey,
    sign_hash: F,
) -> Result<SignedTransaction>
where
    F: FnOnce(&[u8; 32]) -> Signature,
{
    if receipt.output_hash == reexec_output_hash {
        return Err(VerifierError::Internal(
            "cannot build dispute when output hashes match".into(),
        ));
    }
    let dispute = Dispute {
        job_id: receipt.job_id,
        compute_node: receipt.compute_node,
        claimed_output_hash: receipt.output_hash,
        reexec_output_hash,
        verifier: verifier_node,
        reporter,
        vrf_proof: vrf_proof.0.bytes.clone(),
        reexec_proof,
    };
    let tx = Transaction::Dispute(dispute);
    let hash = tx.hash();
    let signature = sign_hash(hash.as_bytes());
    Ok(SignedTransaction {
        tx,
        signer: verifier_pubkey,
        signature,
    })
}

/// Convenience: sign with a local [`arknet_crypto::keys::SigningKey`].
/// Saves every caller from repeating the closure.
pub fn build_and_sign_dispute(
    receipt: &InferenceReceipt,
    reexec_output_hash: Hash256,
    reexec_proof: ComputeProof,
    verifier_node: NodeId,
    reporter: Address,
    vrf_proof: VrfProof,
    sk: &arknet_crypto::keys::SigningKey,
) -> Result<SignedTransaction> {
    let pubkey = sk.verifying_key().to_pubkey();
    build_dispute(
        receipt,
        reexec_output_hash,
        reexec_proof,
        verifier_node,
        reporter,
        vrf_proof,
        pubkey,
        |h| sign(sk, h),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_chain::{InferenceReceipt, Quantization};
    use arknet_common::types::{
        Address, JobId, NodeId, PoolId, Signature as ApiSignature, SignatureScheme,
    };
    use arknet_crypto::keys::SigningKey;
    use arknet_crypto::vrf::prove;

    fn receipt(output_hash: Hash256) -> InferenceReceipt {
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
            output_hash,
            da_reference: None,
            input_token_count: 1,
            output_token_count: 1,
            latency_ms: 1,
            total_time_ms: 1,
            seed: 0,
            compute_proof: ComputeProof::HashChain(vec![[9; 32]]),
            tee_attestation: None,
            timestamp_start: 1,
            timestamp_end: 2,
            compute_signature: ApiSignature::new(SignatureScheme::Ed25519, vec![0xaa; 64]).unwrap(),
            user_signature: ApiSignature::new(SignatureScheme::Ed25519, vec![0xbb; 64]).unwrap(),
        }
    }

    #[test]
    fn build_and_sign_roundtrip() {
        let sk = SigningKey::generate();
        let rec = receipt([0x11; 32]);
        let (vrf, _out) = prove(&sk, b"some-input");
        let stx = build_and_sign_dispute(
            &rec,
            [0x22; 32],
            ComputeProof::HashChain(vec![[0x22; 32]]),
            NodeId::new([9; 32]),
            Address::new([9; 20]),
            vrf,
            &sk,
        )
        .expect("build ok");
        // Signature verifies.
        let hash = stx.tx.hash();
        let pk = sk.verifying_key().to_pubkey();
        arknet_crypto::signatures::verify(&pk, hash.as_bytes(), &stx.signature)
            .expect("sig verifies");
        assert!(matches!(stx.tx, Transaction::Dispute(_)));
    }

    #[test]
    fn refuses_when_hashes_match() {
        let sk = SigningKey::generate();
        let rec = receipt([0x33; 32]);
        let (vrf, _out) = prove(&sk, b"x");
        let err = build_and_sign_dispute(
            &rec,
            [0x33; 32], // same as claimed
            ComputeProof::HashChain(vec![[0x33; 32]]),
            NodeId::new([9; 32]),
            Address::new([9; 20]),
            vrf,
            &sk,
        )
        .unwrap_err();
        assert!(matches!(err, VerifierError::Internal(_)));
    }
}
