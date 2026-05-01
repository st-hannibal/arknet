//! End-to-end test: router builds a receipt batch, anchors it
//! through [`arknet_chain::apply_tx`], then a second anchor of the
//! same batch is rejected by the `CF_RECEIPTS_SEEN` dedup gate.
//!
//! This is the crate's cross-boundary test for the chain-side
//! settlement path. Smaller Merkle / sign-bytes tests live in the
//! unit-test module.

use arknet_chain::apply::{apply_tx, RejectReason, TxOutcome};
use arknet_chain::state::State;
use arknet_chain::{
    ComputeProof, InferenceReceipt, Quantization, SignedTransaction, Transaction, VerificationTier,
};
use arknet_common::types::{Address, JobId, NodeId, PoolId, PubKey, Signature, SignatureScheme};
use arknet_receipts::{build_anchor_tx, ReceiptBatchBuilder};

fn tmp_state() -> (tempfile::TempDir, State) {
    let tmp = tempfile::tempdir().unwrap();
    let state = State::open(tmp.path()).unwrap();
    (tmp, state)
}

fn sample_receipt(seed: u8) -> InferenceReceipt {
    InferenceReceipt {
        job_id: JobId::new([seed; 32]),
        pool_id: PoolId::new([seed; 16]),
        model_id: "m".into(),
        model_hash: [seed; 32],
        quantization: Quantization::F32,
        user_address: Address::new([seed; 20]),
        router_node: NodeId::new([seed; 32]),
        compute_node: NodeId::new([seed.wrapping_add(1); 32]),
        backup_node: None,
        input_hash: [seed; 32],
        output_hash: [seed.wrapping_add(2); 32],
        da_reference: None,
        input_token_count: 1,
        output_token_count: 1,
        latency_ms: 1,
        total_time_ms: 1,
        seed: 0,
        compute_proof: ComputeProof::HashChain(vec![[seed; 32]]),
        tee_attestation: None,
        verification_tier: VerificationTier::Optimistic,
        prompt_encrypted: false,
        timestamp_start: 1,
        timestamp_end: 2,
        compute_signature: Signature::new(SignatureScheme::Ed25519, vec![0xaa; 64]).unwrap(),
        user_signature: Signature::new(SignatureScheme::Ed25519, vec![0xbb; 64]).unwrap(),
    }
}

fn stub_pubkey() -> PubKey {
    PubKey::ed25519([0x11; 32])
}

fn stub_sign(_hash: &[u8; 32]) -> Signature {
    Signature::ed25519([0x22; 64])
}

#[test]
fn batch_anchors_and_marks_seen() {
    let (_tmp, state) = tmp_state();

    let mut b = ReceiptBatchBuilder::new(NodeId::new([1; 32]));
    let r1 = sample_receipt(0x10);
    let r2 = sample_receipt(0x20);
    let r3 = sample_receipt(0x30);
    b.push(r1.clone()).unwrap();
    b.push(r2.clone()).unwrap();
    b.push(r3.clone()).unwrap();

    let batch = b.seal(stub_sign(&[0u8; 32])).expect("seal ok");
    let stx: SignedTransaction = build_anchor_tx(batch, stub_pubkey(), stub_sign).expect("wrap ok");

    // Apply to a fresh block.
    {
        let mut ctx = state.begin_block();
        let out = apply_tx(&mut ctx, &stx).expect("apply succeeds");
        assert!(matches!(out, TxOutcome::Applied { .. }), "got {out:?}");
        ctx.commit().unwrap();
    }

    // Every receipt is now `is_receipt_seen == true`.
    assert!(state.is_receipt_seen(&r1.job_id).unwrap());
    assert!(state.is_receipt_seen(&r2.job_id).unwrap());
    assert!(state.is_receipt_seen(&r3.job_id).unwrap());
    // A non-anchored id is still `false`.
    assert!(!state.is_receipt_seen(&JobId::new([0xff; 32])).unwrap());
}

#[test]
fn double_anchor_is_rejected() {
    let (_tmp, state) = tmp_state();

    // First anchor — accepted.
    let mut b1 = ReceiptBatchBuilder::new(NodeId::new([1; 32]));
    let receipt = sample_receipt(0x42);
    b1.push(receipt.clone()).unwrap();
    let batch_a = b1.seal(stub_sign(&[0; 32])).unwrap();
    let stx_a = build_anchor_tx(batch_a, stub_pubkey(), stub_sign).unwrap();
    {
        let mut ctx = state.begin_block();
        let out = apply_tx(&mut ctx, &stx_a).unwrap();
        assert!(matches!(out, TxOutcome::Applied { .. }));
        ctx.commit().unwrap();
    }

    // Second anchor with the same `job_id` — rejected.
    let mut b2 = ReceiptBatchBuilder::new(NodeId::new([2; 32]));
    b2.push(receipt).unwrap();
    let batch_b = b2.seal(stub_sign(&[0; 32])).unwrap();
    let stx_b = build_anchor_tx(batch_b, stub_pubkey(), stub_sign).unwrap();
    {
        let mut ctx = state.begin_block();
        let out = apply_tx(&mut ctx, &stx_b).unwrap();
        match out {
            TxOutcome::Rejected(RejectReason::ReceiptDoubleAnchor { .. }) => {}
            other => panic!("expected ReceiptDoubleAnchor, got {other:?}"),
        }
    }
}

#[test]
fn merkle_mismatch_rejects_tampered_batch() {
    let (_tmp, state) = tmp_state();

    let mut b = ReceiptBatchBuilder::new(NodeId::new([1; 32]));
    b.push(sample_receipt(1)).unwrap();
    b.push(sample_receipt(2)).unwrap();
    let mut batch = b.seal(stub_sign(&[0; 32])).unwrap();

    // Tamper the root.
    batch.merkle_root = [0xff; 32];

    let stx = SignedTransaction {
        tx: Transaction::ReceiptBatch(batch),
        signer: stub_pubkey(),
        signature: stub_sign(&[0; 32]),
    };

    let mut ctx = state.begin_block();
    let out = apply_tx(&mut ctx, &stx).unwrap();
    assert!(matches!(
        out,
        TxOutcome::Rejected(RejectReason::ReceiptMerkleMismatch)
    ));
}

#[test]
fn duplicate_job_in_same_batch_rejects() {
    let (_tmp, state) = tmp_state();

    // Two copies of the same receipt → double-anchor within one
    // batch. Chain rejects before any side effect lands.
    let mut b = ReceiptBatchBuilder::new(NodeId::new([1; 32]));
    let r = sample_receipt(0x55);
    b.push(r.clone()).unwrap();
    b.push(r).unwrap();
    let batch = b.seal(stub_sign(&[0; 32])).unwrap();
    let stx = build_anchor_tx(batch, stub_pubkey(), stub_sign).unwrap();

    let mut ctx = state.begin_block();
    let out = apply_tx(&mut ctx, &stx).unwrap();
    // Two cases are acceptable: overlay may or may not catch the
    // in-batch dup depending on ordering. Both are correct
    // rejections.
    match out {
        TxOutcome::Rejected(RejectReason::ReceiptDoubleAnchor { .. }) => {}
        TxOutcome::Applied { .. } => {
            // If the apply path allowed this case, surface it —
            // `apply_receipt_batch` calls `is_receipt_seen` over the
            // overlay too, so duplicates within the batch should
            // always reject.
            panic!("expected reject on in-batch duplicate job_id");
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}
