//! On-chain record of a single stake position.
//!
//! A `(node_id, role, pool_id?, delegator?)` tuple can have at most one
//! `StakeEntry` at any moment. The state layer keys stakes by the tuple
//! hash; full slashing / delegation semantics land in Week 9.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use arknet_common::types::{Address, Amount, Height, NodeId, PoolId};

use crate::transactions::StakeRole;

/// A stake locked toward a specific node + role (+ optional pool / delegator).
#[derive(Clone, PartialEq, Eq, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct StakeEntry {
    /// Target node receiving the stake.
    pub node_id: NodeId,
    /// Role the stake applies to.
    pub role: StakeRole,
    /// Optional pool the stake is pinned to (compute roles only).
    pub pool_id: Option<PoolId>,
    /// Address delegating to this node (`None` when self-staked by the node's
    /// operator).
    pub delegator: Option<Address>,
    /// Amount locked in `ark_atom`.
    pub amount: Amount,
    /// Height at which the stake was first locked. Used for unbonding math
    /// in Week 9.
    pub bonded_at: Height,
}

impl StakeEntry {
    /// `true` when the stake has been fully withdrawn (`amount == 0`). Such
    /// entries should be removed from the state trie.
    pub fn is_empty(&self) -> bool {
        self.amount == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StakeEntry {
        StakeEntry {
            node_id: NodeId::new([0x11; 32]),
            role: StakeRole::Compute,
            pool_id: Some(PoolId::new([0x22; 16])),
            delegator: None,
            amount: 2_500,
            bonded_at: 100,
        }
    }

    #[test]
    fn borsh_roundtrip() {
        let s = sample();
        let bytes = borsh::to_vec(&s).unwrap();
        let decoded: StakeEntry = borsh::from_slice(&bytes).unwrap();
        assert_eq!(s, decoded);
    }

    #[test]
    fn is_empty_respects_zero_amount() {
        let mut s = sample();
        assert!(!s.is_empty());
        s.amount = 0;
        assert!(s.is_empty());
    }
}
