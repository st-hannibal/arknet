//! `malachitebft_core_types::Value` adapter for our [`Block`].
//!
//! Malachite treats the proposed value as opaque except for its
//! [`Value::Id`] (typically a hash). We wrap our [`Block`] in a
//! newtype so we can:
//!
//! 1. Derive `Ord` via the block hash (Malachite's trait bound requires
//!    total ordering even though the state machine only uses `==`).
//! 2. Expose [`BlockId`] with the right `Display` impl for malachite's
//!    logs.

use std::cmp::Ordering;

use malachitebft_core_types::Value as MalachiteValue;

use arknet_chain::block::Block;
use arknet_common::types::BlockHash;

/// Newtype wrapper around a committed-in-consensus [`Block`].
///
/// Clones are cheap as far as the consensus inner loop is concerned:
/// a block's hot path is hash + roundtrip, and we only build it once
/// per round (not per message). Real optimization (Arc-sharing) lands
/// when blocks get bulky in later phases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainValue {
    /// Inner block body + header.
    pub block: Block,
}

impl ChainValue {
    /// Build a [`ChainValue`] from an owned block.
    pub fn new(block: Block) -> Self {
        Self { block }
    }

    /// Short-hand for `self.block.header.hash()`.
    pub fn id(&self) -> BlockId {
        BlockId(self.block.header.hash())
    }
}

impl PartialOrd for ChainValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ChainValue {
    fn cmp(&self, other: &Self) -> Ordering {
        // Hash order is total and deterministic — malachite only uses
        // Ord for deterministic tie-breaking (e.g. sorted evidence),
        // so hash ordering is safe even though it isn't "semantic".
        self.id().0.as_bytes().cmp(other.id().0.as_bytes())
    }
}

impl MalachiteValue for ChainValue {
    type Id = BlockId;

    fn id(&self) -> Self::Id {
        self.id()
    }
}

/// Compact identifier malachite stamps on votes.
///
/// Wraps our existing [`BlockHash`]. The [`Display`] impl uses `block:`
/// prefix + 8-char hex so debug logs stay scannable even when dozens of
/// values are in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub BlockHash);

impl std::fmt::Display for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let full = hex::encode(self.0.as_bytes());
        write!(f, "block:{}", &full[..full.len().min(16)])
    }
}

impl From<BlockHash> for BlockId {
    fn from(h: BlockHash) -> Self {
        Self(h)
    }
}

impl From<BlockId> for BlockHash {
    fn from(id: BlockId) -> Self {
        id.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_chain::block::BlockHeader;
    use arknet_common::types::{NodeId, StateRoot};

    fn sample_block(height: u64, salt: u8) -> Block {
        let header = BlockHeader {
            version: 1,
            chain_id: "arknet-test".into(),
            height,
            timestamp_ms: 0,
            parent_hash: BlockHash::new([salt; 32]),
            state_root: StateRoot::new([0; 32]),
            tx_root: [0; 32],
            receipt_root: [0; 32],
            proposer: NodeId::new([0; 32]),
            validator_set_hash: [0; 32],
            base_fee: 1_000_000_000,
            genesis_message: String::new(),
        };
        Block {
            header,
            txs: Vec::new(),
            receipts: Vec::new(),
        }
    }

    #[test]
    fn id_uses_block_hash() {
        let v = ChainValue::new(sample_block(1, 1));
        let expected = BlockId(v.block.header.hash());
        assert_eq!(v.id(), expected);
    }

    #[test]
    fn ord_is_deterministic_by_hash() {
        let a = ChainValue::new(sample_block(1, 1));
        let b = ChainValue::new(sample_block(1, 2));
        // Different previous_hash → different hash → stable ordering.
        assert_ne!(a.cmp(&b), Ordering::Equal);
        assert_eq!(a.cmp(&b), a.cmp(&b));
    }

    #[test]
    fn display_is_truncated_and_prefixed() {
        let id = BlockId(BlockHash::new([0xAB; 32]));
        let s = id.to_string();
        assert!(s.starts_with("block:"), "got {s:?}");
        assert_eq!(s.len(), "block:".len() + 16);
    }
}
