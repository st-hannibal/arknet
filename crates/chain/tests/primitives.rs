//! Integration tests for chain primitives — exercise the public surface
//! the way downstream crates (consensus, staking, L2 roles) will. These
//! overlap with unit tests but catch regressions when the module layout
//! changes.

use arknet_chain::{
    check_block_size, check_signed_tx_size, next_base_fee, receipt_root, tx_root, Block,
    BlockHeader, SignedTransaction, StakeOp, StakeRole, Transaction,
};
use arknet_common::types::{
    Address, Amount, BlockHash, NodeId, PoolId, PubKey, Signature, SignatureScheme, StateRoot,
    ATOMS_PER_ARK,
};

fn sample_signer() -> (PubKey, Signature) {
    (
        PubKey::ed25519([0x11; 32]),
        Signature::new(SignatureScheme::Ed25519, vec![0x22; 64]).unwrap(),
    )
}

fn transfer_tx(amount: Amount, nonce: u64) -> SignedTransaction {
    let (signer, signature) = sample_signer();
    SignedTransaction {
        tx: Transaction::Transfer {
            from: Address::new([0xaa; 20]),
            to: Address::new([0xbb; 20]),
            amount,
            nonce,
            fee: 21_000,
        },
        signer,
        signature,
    }
}

fn sample_header(height: u64) -> BlockHeader {
    BlockHeader {
        version: 1,
        chain_id: "arknet-devnet-1".to_string(),
        height,
        timestamp_ms: 1_700_000_000_000,
        parent_hash: BlockHash::new([9; 32]),
        state_root: StateRoot::new([1; 32]),
        tx_root: [2; 32],
        receipt_root: [3; 32],
        proposer: NodeId::new([4; 32]),
        validator_set_hash: [5; 32],
        base_fee: 1_000_000_000,
        genesis_message: String::new(),
    }
}

#[test]
fn block_with_mixed_txs_roundtrips() {
    let block = Block {
        header: sample_header(1),
        txs: vec![
            transfer_tx(ATOMS_PER_ARK, 1),
            SignedTransaction {
                tx: Transaction::StakeOp(StakeOp::Deposit {
                    node_id: NodeId::new([7; 32]),
                    role: StakeRole::Compute,
                    pool_id: Some(PoolId::new([8; 16])),
                    amount: 2_500 * ATOMS_PER_ARK,
                    delegator: None,
                }),
                signer: PubKey::ed25519([0x33; 32]),
                signature: Signature::new(SignatureScheme::Ed25519, vec![0x44; 64]).unwrap(),
            },
        ],
        receipts: Vec::new(),
    };

    let bytes = borsh::to_vec(&block).unwrap();
    let decoded: Block = borsh::from_slice(&bytes).unwrap();
    assert_eq!(block, decoded);
    assert_eq!(block.hash(), decoded.hash());
}

#[test]
fn size_check_accepts_normal_workload() {
    let block = Block {
        header: sample_header(1),
        txs: (0..100).map(|n| transfer_tx(ATOMS_PER_ARK, n)).collect(),
        receipts: Vec::new(),
    };
    check_block_size(&block).expect("100-tx block fits");

    for stx in &block.txs {
        check_signed_tx_size(stx).expect("per-tx size is small");
    }
}

#[test]
fn tx_and_receipt_roots_differ_on_identical_leaves() {
    let leaves = vec![[0x11; 32], [0x22; 32], [0x33; 32]];
    assert_ne!(tx_root(&leaves), receipt_root(&leaves));
}

#[test]
fn fee_market_sequence_is_deterministic() {
    // A short history where the fee goes over target, then under, then
    // returns to target.
    let f0 = 1_000_000_000;
    let f1 = next_base_fee(f0, 30_000_000, 15_000_000).unwrap(); // +12.5%
    let f2 = next_base_fee(f1, 7_500_000, 15_000_000).unwrap(); // -6.25%
    let f3 = next_base_fee(f2, 15_000_000, 15_000_000).unwrap(); // stable
    assert_eq!(f1, 1_125_000_000);
    assert!(f2 < f1 && f2 > f0); // didn't drop all the way back
    assert_eq!(f3, f2);
}

#[test]
fn header_hash_is_sensitive_to_every_field() {
    let base = sample_header(1);
    let bumped_height = {
        let mut h = base.clone();
        h.height = 2;
        h
    };
    let bumped_version = {
        let mut h = base.clone();
        h.version = 2;
        h
    };
    let bumped_chain = {
        let mut h = base.clone();
        h.chain_id = "arknet-mainnet".to_string();
        h
    };
    let bumped_timestamp = {
        let mut h = base.clone();
        h.timestamp_ms += 1;
        h
    };

    let base_hash = base.hash();
    assert_ne!(base_hash, bumped_height.hash());
    assert_ne!(base_hash, bumped_version.hash());
    assert_ne!(base_hash, bumped_chain.hash());
    assert_ne!(base_hash, bumped_timestamp.hash());
}
