//! Pending-unbonding record.
//!
//! When `StakeOp::Withdraw` lands, we don't burn the stake immediately —
//! we move it from the live [`StakeEntry`] into an [`UnbondingEntry`]
//! that completes after `UNBONDING_PERIOD_BLOCKS` (§16 = 1,209,600).
//! Slashing during the unbonding window cuts both the live stake AND
//! any unbondings whose `completes_at` ≥ `slash_evidence_height` —
//! otherwise a validator could front-run an attack by starting an
//! unbond on the same block it double-signs.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use arknet_common::types::{Address, Amount, Height, NodeId, PoolId};

use crate::transactions::StakeRole;

/// A stake amount that has been withdrawn but not yet returned to the
/// holder's spendable balance.
///
/// Invariant: once `completes_at` is reached, any
/// `StakeOp::Complete { node_id, unbond_id }` returns `amount` to
/// `delegator.unwrap_or(operator)`.
#[derive(Clone, PartialEq, Eq, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct UnbondingEntry {
    /// Monotonic id (per-node). Stored alongside `node_id` in the key.
    pub unbond_id: u64,
    /// Target node the stake was locked under.
    pub node_id: NodeId,
    /// Role the stake backed.
    pub role: StakeRole,
    /// Pool the stake was pinned to, if any.
    pub pool_id: Option<PoolId>,
    /// Delegator address, or `None` if this was an operator self-stake.
    pub delegator: Option<Address>,
    /// Amount being unbonded, in `ark_atom`.
    pub amount: Amount,
    /// Height at which the unbonding started.
    pub started_at: Height,
    /// Height at which the holder may call `StakeOp::Complete`.
    pub completes_at: Height,
}

impl UnbondingEntry {
    /// `true` when the window has elapsed and the entry is eligible
    /// for completion.
    pub fn is_complete(&self, current_height: Height) -> bool {
        current_height >= self.completes_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> UnbondingEntry {
        UnbondingEntry {
            unbond_id: 7,
            node_id: NodeId::new([1; 32]),
            role: StakeRole::Validator,
            pool_id: None,
            delegator: Some(Address::new([2; 20])),
            amount: 1_000,
            started_at: 100,
            completes_at: 1_209_700,
        }
    }

    #[test]
    fn borsh_roundtrip() {
        let e = sample();
        let bytes = borsh::to_vec(&e).unwrap();
        let decoded: UnbondingEntry = borsh::from_slice(&bytes).unwrap();
        assert_eq!(e, decoded);
    }

    #[test]
    fn is_complete_gates_on_height() {
        let e = sample();
        assert!(!e.is_complete(e.completes_at - 1));
        assert!(e.is_complete(e.completes_at));
        assert!(e.is_complete(e.completes_at + 1));
    }
}
