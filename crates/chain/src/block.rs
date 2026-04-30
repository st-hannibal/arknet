//! Block and block-header types.
//!
//! A block is split into a header (fixed-size, cheap to hash, used for
//! consensus votes) and a body (full tx + receipt list, used for state
//! application). Block hashes are computed over the borsh-encoded header
//! only, with domain separation per [`DOMAIN_BLOCK`].

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use arknet_common::types::{
    Amount, BlockHash, Hash256, Height, NodeId, StateRoot, Timestamp, DOMAIN_BLOCK,
    DOMAIN_RECEIPT_ROOT, DOMAIN_TX_ROOT,
};
use arknet_crypto::hash::blake3;

use crate::errors::{ChainError, Result};
use crate::receipt::InferenceReceipt;
use crate::transactions::SignedTransaction;

/// Canonical block header. Hashing + gossip happens over this type only.
///
/// Field ordering is stable. Adding a field is a hard fork.
#[derive(Clone, PartialEq, Eq, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct BlockHeader {
    /// Protocol version. Bump on hard fork.
    pub version: u32,
    /// Chain identifier — e.g. `"arknet-devnet-1"`.
    pub chain_id: String,
    /// Height of this block. Genesis is 0.
    pub height: Height,
    /// Block timestamp in milliseconds since Unix epoch.
    pub timestamp_ms: Timestamp,
    /// Hash of the parent block's header. Genesis uses all-zeros.
    pub parent_hash: BlockHash,
    /// State root after applying this block's transactions.
    pub state_root: StateRoot,
    /// Merkle root of transaction hashes in the body.
    pub tx_root: Hash256,
    /// Merkle root of receipt hashes in the body.
    pub receipt_root: Hash256,
    /// Proposer node identifier.
    pub proposer: NodeId,
    /// Hash of the active validator set (for light-client verification).
    pub validator_set_hash: Hash256,
    /// Current EIP-1559 base fee in ark_atom/gas.
    pub base_fee: Amount,
    /// Genesis coinbase message. Non-empty only at height 0 — proves
    /// the chain could not have been pre-mined before the embedded date.
    /// Analogous to Bitcoin's "The Times 03/Jan/2009..." in block 0.
    #[serde(default)]
    pub genesis_message: String,
}

impl BlockHeader {
    /// Domain-separated header hash:
    /// `blake3(DOMAIN_BLOCK || borsh(header))`.
    pub fn hash(&self) -> BlockHash {
        let body = borsh::to_vec(self).expect("block header borsh encoding is infallible");
        let mut buf = Vec::with_capacity(DOMAIN_BLOCK.len() + body.len());
        buf.extend_from_slice(DOMAIN_BLOCK);
        buf.extend_from_slice(&body);
        BlockHash::new(*blake3(&buf).as_bytes())
    }
}

/// Full block: header + body.
///
/// The body carries signed transactions and anchored inference receipts.
/// `header.tx_root` / `header.receipt_root` commit to the body contents;
/// mismatch is a consensus-level error.
#[derive(Clone, PartialEq, Eq, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct Block {
    /// Consensus-relevant header.
    pub header: BlockHeader,
    /// Signed transactions included this block.
    pub txs: Vec<SignedTransaction>,
    /// Inference receipts anchored this block (distinct from
    /// `ReceiptBatch` transactions — receipts arrive here after dispute
    /// resolution; batches in `txs` carry the raw-submitted bundle).
    pub receipts: Vec<InferenceReceipt>,
}

impl Block {
    /// Shortcut for `self.header.hash()`.
    pub fn hash(&self) -> BlockHash {
        self.header.hash()
    }
}

/// Hard cap on the borsh size of a full block. Protects consensus gossip.
pub const MAX_BLOCK_BYTES: usize = 10 * 1024 * 1024;

/// Validate a block's size bound.
pub fn check_block_size(block: &Block) -> Result<()> {
    let len = borsh::to_vec(block)
        .map_err(|e| ChainError::Codec(format!("block encode: {e}")))?
        .len();
    if len > MAX_BLOCK_BYTES {
        return Err(ChainError::Oversize {
            what: "block",
            actual: len,
            max: MAX_BLOCK_BYTES,
        });
    }
    Ok(())
}

/// Build a domain-separated Merkle root of transaction hashes.
///
/// Input is already-hashed transactions; domain tag prevents collision
/// with receipt roots. Empty input yields `blake3(DOMAIN_TX_ROOT)`.
pub fn tx_root(tx_hashes: &[Hash256]) -> Hash256 {
    merkle_root(DOMAIN_TX_ROOT, tx_hashes)
}

/// Build a domain-separated Merkle root of receipt hashes.
pub fn receipt_root(receipt_hashes: &[Hash256]) -> Hash256 {
    merkle_root(DOMAIN_RECEIPT_ROOT, receipt_hashes)
}

/// Minimal domain-separated Merkle root helper.
///
/// For Phase 1 Week 1-2 we use a linear blake3 over concatenated leaves
/// behind a domain tag. The full RFC-6962 binary Merkle tree from
/// `arknet-crypto` lands when receipts need inclusion proofs (later
/// week-blocks).
fn merkle_root(domain: &[u8], leaves: &[Hash256]) -> Hash256 {
    let mut buf = Vec::with_capacity(domain.len() + leaves.len() * 32);
    buf.extend_from_slice(domain);
    for leaf in leaves {
        buf.extend_from_slice(leaf);
    }
    *blake3(&buf).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transactions::Transaction;
    use arknet_common::types::{Address, SignatureScheme};
    use arknet_common::types::{PubKey, Signature};

    fn sample_header() -> BlockHeader {
        BlockHeader {
            version: 1,
            chain_id: "arknet-devnet-1".to_string(),
            height: 42,
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

    fn sample_signed_tx() -> SignedTransaction {
        SignedTransaction {
            tx: Transaction::Transfer {
                from: Address::new([0xaa; 20]),
                to: Address::new([0xbb; 20]),
                amount: 1_000_000_000u128,
                nonce: 1,
                fee: 21_000,
            },
            signer: PubKey::ed25519([0x11; 32]),
            signature: Signature::new(SignatureScheme::Ed25519, vec![0x22; 64]).unwrap(),
        }
    }

    #[test]
    fn header_borsh_roundtrip() {
        let h = sample_header();
        let bytes = borsh::to_vec(&h).unwrap();
        let decoded: BlockHeader = borsh::from_slice(&bytes).unwrap();
        assert_eq!(h, decoded);
    }

    #[test]
    fn header_hash_is_deterministic() {
        let h = sample_header();
        assert_eq!(h.hash(), h.hash());
    }

    #[test]
    fn header_hash_differs_on_any_field_change() {
        let base = sample_header();
        let mut bumped = base.clone();
        bumped.height = 43;
        assert_ne!(base.hash(), bumped.hash());
    }

    #[test]
    fn block_hash_equals_header_hash() {
        let block = Block {
            header: sample_header(),
            txs: vec![sample_signed_tx()],
            receipts: Vec::new(),
        };
        assert_eq!(block.hash(), block.header.hash());
    }

    #[test]
    fn block_borsh_roundtrip() {
        let block = Block {
            header: sample_header(),
            txs: vec![sample_signed_tx(), sample_signed_tx()],
            receipts: Vec::new(),
        };
        let bytes = borsh::to_vec(&block).unwrap();
        let decoded: Block = borsh::from_slice(&bytes).unwrap();
        assert_eq!(block, decoded);
    }

    #[test]
    fn block_size_check_accepts_small_block() {
        let block = Block {
            header: sample_header(),
            txs: vec![sample_signed_tx()],
            receipts: Vec::new(),
        };
        assert!(check_block_size(&block).is_ok());
    }

    #[test]
    fn tx_root_and_receipt_root_differ_on_same_leaves() {
        // Same input, different domain → different root.
        let leaves = vec![[0x11; 32], [0x22; 32]];
        let tx = tx_root(&leaves);
        let rc = receipt_root(&leaves);
        assert_ne!(tx, rc);
    }

    #[test]
    fn merkle_root_empty_is_stable() {
        // Empty-leaves root should still be deterministic and differ
        // between tx and receipt domains.
        assert_eq!(tx_root(&[]), tx_root(&[]));
        assert_ne!(tx_root(&[]), receipt_root(&[]));
    }

    #[test]
    fn merkle_root_differs_on_leaf_reorder() {
        // Order-sensitive: swapping leaves must change the root.
        let a = tx_root(&[[1; 32], [2; 32]]);
        let b = tx_root(&[[2; 32], [1; 32]]);
        assert_ne!(a, b);
    }
}
