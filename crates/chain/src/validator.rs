//! On-chain validator record.
//!
//! Kept minimal for Phase 1 Week 3-4: identity + voting power + bootstrap
//! flag. DPoS election, jailing, slashing, and commission rates land in
//! Week 9.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use arknet_common::types::{Address, Amount, NodeId, PubKey};

/// Information recorded for every active validator.
#[derive(Clone, PartialEq, Eq, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct ValidatorInfo {
    /// Validator identifier derived from the consensus public key.
    pub node_id: NodeId,
    /// Consensus public key (scheme-tagged).
    pub consensus_key: PubKey,
    /// Operator address (controls withdrawals / governance).
    pub operator: Address,
    /// Total bonded stake (self + delegations) in `ark_atom`. Zero while in
    /// the bootstrap epoch (see PROTOCOL_SPEC §9.4).
    pub bonded_stake: Amount,
    /// Voting power in consensus. Currently derived from `bonded_stake`,
    /// with an override during bootstrap when all validators get equal
    /// weight.
    pub voting_power: u64,
    /// `true` if this validator was in the genesis set. Retains its
    /// zero-stake exemption until bootstrap ends.
    pub is_genesis: bool,
    /// `true` if the validator is jailed (excluded from the active set).
    pub jailed: bool,
}

impl ValidatorInfo {
    /// `true` when the validator is active (not jailed, non-zero voting
    /// power).
    pub fn is_active(&self) -> bool {
        !self.jailed && self.voting_power > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_common::types::SignatureScheme;

    fn sample() -> ValidatorInfo {
        ValidatorInfo {
            node_id: NodeId::new([1; 32]),
            consensus_key: PubKey::ed25519([2; 32]),
            operator: Address::new([3; 20]),
            bonded_stake: 0,
            voting_power: 1,
            is_genesis: true,
            jailed: false,
        }
    }

    #[test]
    fn borsh_roundtrip() {
        let v = sample();
        let bytes = borsh::to_vec(&v).unwrap();
        let decoded: ValidatorInfo = borsh::from_slice(&bytes).unwrap();
        assert_eq!(v, decoded);
    }

    #[test]
    fn genesis_validator_is_active_with_zero_stake() {
        let v = sample();
        assert_eq!(v.bonded_stake, 0);
        assert!(v.is_active());
    }

    #[test]
    fn jailed_validator_is_inactive() {
        let mut v = sample();
        v.jailed = true;
        assert!(!v.is_active());
    }

    #[test]
    fn zero_voting_power_is_inactive() {
        let mut v = sample();
        v.voting_power = 0;
        assert!(!v.is_active());
    }

    #[test]
    fn consensus_key_scheme_is_ed25519() {
        let v = sample();
        assert_eq!(v.consensus_key.scheme, SignatureScheme::Ed25519);
    }
}
