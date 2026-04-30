//! Account model: the per-address state the chain tracks.
//!
//! Phase 1 Week 3-4 keeps this intentionally small: balance + nonce. Rewards
//! accounting, pending unbondings, and delegation bookkeeping land in later
//! week-blocks as separate columns in `state.rs`.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use arknet_common::types::{Amount, Nonce};

/// On-chain account state for a single address.
#[derive(
    Clone, PartialEq, Eq, Debug, Default, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub struct Account {
    /// Spendable balance in `ark_atom`.
    pub balance: Amount,
    /// Next expected transaction nonce. Incremented on every accepted tx.
    pub nonce: Nonce,
}

impl Account {
    /// Empty account — zero balance, nonce 0.
    pub const ZERO: Account = Account {
        balance: 0,
        nonce: 0,
    };

    /// `true` when the account has no balance and no activity yet.
    /// These can be omitted from the state root (trie leaves default to zero).
    pub fn is_empty(&self) -> bool {
        self.balance == 0 && self.nonce == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty() {
        assert!(Account::default().is_empty());
        assert!(Account::ZERO.is_empty());
    }

    #[test]
    fn not_empty_after_balance() {
        let a = Account {
            balance: 1,
            nonce: 0,
        };
        assert!(!a.is_empty());
    }

    #[test]
    fn not_empty_after_nonce() {
        let a = Account {
            balance: 0,
            nonce: 1,
        };
        assert!(!a.is_empty());
    }

    #[test]
    fn borsh_roundtrip() {
        let a = Account {
            balance: 1_000_000,
            nonce: 42,
        };
        let bytes = borsh::to_vec(&a).unwrap();
        let decoded: Account = borsh::from_slice(&bytes).unwrap();
        assert_eq!(a, decoded);
    }
}
