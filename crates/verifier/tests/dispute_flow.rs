//! End-to-end verifier dispute flow.
//!
//! 1. Router anchors a (tampered) receipt onto L1.
//! 2. Verifier runs the VRF gate + re-execution.
//! 3. On divergence, verifier builds a signed Dispute transaction.
//! 4. Chain's `apply_tx` accepts the dispute (→ `TxOutcome::Applied`).
//!
//! A matching "honest receipt → Verdict::Verified → no dispute"
//! path is asserted too, so the verifier cannot false-positive the
//! honest flow.

use arknet_chain::apply::{apply_tx, RejectReason, TxOutcome};
use arknet_chain::state::State;
use arknet_chain::{
    ComputeProof, InferenceReceipt, Quantization, SignedTransaction, Transaction, VerificationTier,
};
use arknet_common::types::{
    Address, BlockHash, JobId, NodeId, PoolId, PubKey, Signature, SignatureScheme,
};
use arknet_crypto::keys::SigningKey;
use arknet_verifier::{
    rebuild_hash_chain, select_verifier, verify_receipt, Reexecutor, Result as VerifierResult,
    Verdict,
};
use async_trait::async_trait;

fn tmp_state() -> (tempfile::TempDir, State) {
    let tmp = tempfile::tempdir().unwrap();
    let state = State::open(tmp.path()).unwrap();
    (tmp, state)
}

struct FixedOutputBackend {
    output_text: String,
}

#[async_trait]
impl Reexecutor for FixedOutputBackend {
    async fn reexecute(&self, _receipt: &InferenceReceipt) -> VerifierResult<String> {
        Ok(self.output_text.clone())
    }
}

fn stub_sign(_hash: &[u8; 32]) -> Signature {
    Signature::ed25519([0x22; 64])
}

fn build_receipt(job: JobId, claimed_output_hash: [u8; 32]) -> InferenceReceipt {
    InferenceReceipt {
        job_id: job,
        pool_id: PoolId::new([1; 16]),
        model_id: "local/stories260K".into(),
        model_hash: [1; 32],
        quantization: Quantization::F32,
        user_address: Address::new([1; 20]),
        router_node: NodeId::new([1; 32]),
        compute_node: NodeId::new([2; 32]),
        backup_node: None,
        input_hash: [1; 32],
        output_hash: claimed_output_hash,
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
        compute_signature: Signature::new(SignatureScheme::Ed25519, vec![0xaa; 64]).unwrap(),
        user_signature: Signature::new(SignatureScheme::Ed25519, vec![0xbb; 64]).unwrap(),
    }
}

fn anchor_receipt(state: &State, receipt: InferenceReceipt) {
    use arknet_receipts::{build_anchor_tx, ReceiptBatchBuilder};
    let mut b = ReceiptBatchBuilder::new(NodeId::new([1; 32]));
    b.push(receipt).unwrap();
    let batch = b.seal(Signature::ed25519([0; 64])).unwrap();
    let stx: SignedTransaction =
        build_anchor_tx(batch, PubKey::ed25519([0x33; 32]), stub_sign).unwrap();
    let mut ctx = state.begin_block();
    let out = apply_tx(&mut ctx, &stx).unwrap();
    assert!(matches!(out, TxOutcome::Applied { .. }));
    ctx.commit().unwrap();
}

#[tokio::test]
async fn honest_receipt_verifies_no_dispute() {
    let (_tmp, state) = tmp_state();

    // Truth is "correct-output". Receipt claims the matching hash.
    let job = JobId::new([9; 32]);
    let chain = rebuild_hash_chain(&job, "correct-output");
    let truth_hash = *chain.last().unwrap();
    let receipt = build_receipt(job, truth_hash);
    anchor_receipt(&state, receipt.clone());

    let backend = FixedOutputBackend {
        output_text: "correct-output".into(),
    };
    let verdict = verify_receipt(&backend, &receipt).await.expect("verdict");
    assert_eq!(verdict, Verdict::Verified);
}

#[tokio::test]
async fn tampered_receipt_yields_dispute_that_chain_applies() {
    let (_tmp, state) = tmp_state();

    // Truth output is "truth", but the compute node signed a receipt
    // claiming a different hash.
    let job = JobId::new([42; 32]);
    let truth_chain = rebuild_hash_chain(&job, "truth");
    let truth_hash = *truth_chain.last().unwrap();
    // The compute node lied: claimed a completely different hash.
    let claimed = [0xbb; 32];
    assert_ne!(truth_hash, claimed);
    let receipt = build_receipt(job, claimed);
    anchor_receipt(&state, receipt.clone());

    let backend = FixedOutputBackend {
        output_text: "truth".into(),
    };
    let verdict = verify_receipt(&backend, &receipt).await.expect("verdict");
    let (reexec_output_hash, reexec_proof) = match verdict {
        Verdict::Diverged {
            reexec_output_hash,
            reexec_proof,
        } => (reexec_output_hash, reexec_proof),
        other => panic!("expected Diverged, got {other:?}"),
    };
    assert_eq!(reexec_output_hash, truth_hash);

    // Verifier builds + signs a dispute tx. Use rate=1.0 so the
    // selection gate always passes; the VRF proof still attaches.
    let sk = SigningKey::from_seed(&[0x77; 32]);
    let sel = select_verifier(&sk, &job, &BlockHash::new([0; 32]), 1.0);
    assert!(sel.selected);
    let stx = arknet_verifier::build_and_sign_dispute(
        &receipt,
        reexec_output_hash,
        reexec_proof,
        NodeId::new([7; 32]),
        Address::new([7; 20]),
        sel.proof,
        &sk,
    )
    .expect("dispute built");
    assert!(matches!(stx.tx, Transaction::Dispute(_)));

    // Chain accepts the dispute.
    let mut ctx = state.begin_block();
    let out = apply_tx(&mut ctx, &stx).expect("apply dispute");
    match out {
        TxOutcome::Applied { gas_used } => {
            assert_eq!(gas_used, arknet_chain::apply::DISPUTE_GAS);
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    ctx.commit().unwrap();
}

#[tokio::test]
async fn dispute_for_non_anchored_receipt_is_rejected() {
    let (_tmp, state) = tmp_state();

    let job = JobId::new([99; 32]);
    let receipt = build_receipt(job, [0x11; 32]);
    // Deliberately skip anchoring.

    let sk = SigningKey::from_seed(&[0x88; 32]);
    let sel = select_verifier(&sk, &job, &BlockHash::new([0; 32]), 1.0);
    let stx = arknet_verifier::build_and_sign_dispute(
        &receipt,
        [0x22; 32],
        ComputeProof::HashChain(vec![[0x22; 32]]),
        NodeId::new([7; 32]),
        Address::new([7; 20]),
        sel.proof,
        &sk,
    )
    .expect("dispute built");

    let mut ctx = state.begin_block();
    let out = apply_tx(&mut ctx, &stx).unwrap();
    assert!(matches!(
        out,
        TxOutcome::Rejected(RejectReason::DisputeReceiptNotFound)
    ));
}

#[tokio::test]
async fn dispute_with_matching_hashes_is_rejected() {
    // If the verifier mistakenly submits a dispute where the
    // reexec_output_hash == claimed_output_hash the builder
    // refuses. But a tampered client could construct one manually —
    // the chain must also reject.
    let (_tmp, state) = tmp_state();
    let job = JobId::new([77; 32]);
    let claimed = [0x55; 32];
    let receipt = build_receipt(job, claimed);
    anchor_receipt(&state, receipt.clone());

    let vrf_bytes = vec![0u8; 64];
    let dispute = arknet_chain::Dispute {
        job_id: job,
        compute_node: receipt.compute_node,
        claimed_output_hash: claimed,
        reexec_output_hash: claimed, // match
        verifier: NodeId::new([7; 32]),
        reporter: Address::new([7; 20]),
        vrf_proof: vrf_bytes,
        reexec_proof: ComputeProof::HashChain(vec![claimed]),
    };
    let stx = SignedTransaction {
        tx: Transaction::Dispute(dispute),
        signer: PubKey::ed25519([0x33; 32]),
        signature: Signature::ed25519([0x44; 64]),
    };
    let mut ctx = state.begin_block();
    let out = apply_tx(&mut ctx, &stx).unwrap();
    assert!(matches!(
        out,
        TxOutcome::Rejected(RejectReason::DisputeOutputMatches)
    ));
}
