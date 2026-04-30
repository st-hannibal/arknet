//! `malachitebft_core_types::{Address, Validator, ValidatorSet}`
//! adapters over our existing on-chain [`ValidatorInfo`].
//!
//! Phase 1 Week 7-8 keeps the validator set static between blocks — the
//! set is seeded at genesis and reshuffled at epoch boundaries (Week 9).
//! This module only owns the *wire representation* used by malachite; it
//! does NOT own the epoch-change logic.
//!
//! # Address shape
//!
//! We wrap our 20-byte [`arknet_common::Address`] in a local newtype
//! (`ChainAddress`) so we can implement malachite's foreign `Address`
//! trait without tripping the orphan rule. The newtype is zero-cost and
//! converts via `From` / `Into`.

use std::cmp::Ordering;
use std::fmt;

use malachitebft_core_types::{
    Address as MalachiteAddress, Validator as MalachiteValidator,
    ValidatorSet as MalachiteValidatorSet, VotingPower,
};
use malachitebft_signing_ed25519::PublicKey;

use arknet_chain::validator::ValidatorInfo;
use arknet_common::types::Address;

use crate::context::ArknetContext;
use crate::errors::{ConsensusError, Result};

// ───────── Address ────────────────────────────────────────────────────────

/// Consensus-layer address — newtype over [`arknet_common::Address`] so
/// malachite's foreign `Address` trait can be implemented here without
/// orphan-rule issues. Zero cost; convert freely via `From` / `Into`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChainAddress(pub Address);

impl fmt::Display for ChainAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        <Address as fmt::Display>::fmt(&self.0, f)
    }
}

impl From<Address> for ChainAddress {
    fn from(a: Address) -> Self {
        Self(a)
    }
}

impl From<ChainAddress> for Address {
    fn from(a: ChainAddress) -> Self {
        a.0
    }
}

impl MalachiteAddress for ChainAddress {}

// ───────── Validator ──────────────────────────────────────────────────────

/// Wraps our on-chain [`ValidatorInfo`] with the fields malachite's
/// `Validator` trait needs. We strip the bonded-stake / jailed bits
/// because they do not cross the consensus boundary — consensus only
/// cares about voting power.
///
/// Keeping the consensus key as the `PublicKey` type from malachite's
/// signing-ed25519 crate (which underlies the `SigningScheme` impl)
/// guarantees the key bytes we hand malachite are the same key malachite
/// uses to verify signatures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainValidator {
    /// Operator-facing address wrapped for the consensus layer.
    pub address: ChainAddress,
    /// Consensus-layer Ed25519 public key bytes.
    pub public_key: PublicKey,
    /// Voting power for this validator at the current height.
    pub voting_power: VotingPower,
}

impl ChainValidator {
    /// Build from the on-chain record. Returns an error if the record's
    /// consensus key is not Ed25519 (other schemes are reserved per
    /// SECURITY.md §12 but not active yet).
    pub fn from_info(info: &ValidatorInfo) -> Result<Self> {
        let pk_bytes = info.consensus_key.bytes.as_slice();
        if pk_bytes.len() != 32 {
            return Err(ConsensusError::Config(format!(
                "validator {} has {}-byte pubkey, expected 32 (Ed25519)",
                info.node_id,
                pk_bytes.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(pk_bytes);
        let public_key = PublicKey::from_bytes(arr);
        Ok(Self {
            address: ChainAddress(info.operator),
            public_key,
            voting_power: info.voting_power,
        })
    }
}

impl MalachiteValidator<ArknetContext> for ChainValidator {
    fn address(&self) -> &ChainAddress {
        &self.address
    }

    fn public_key(&self) -> &PublicKey {
        &self.public_key
    }

    fn voting_power(&self) -> VotingPower {
        self.voting_power
    }
}

// ───────── Validator set ──────────────────────────────────────────────────

/// Static set of validators active at the current height.
///
/// Invariant: validators are sorted first by voting power descending,
/// then lexicographically by address (per malachite's documented
/// CometBFT-style ordering). `from_infos` enforces this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainValidatorSet {
    validators: Vec<ChainValidator>,
}

impl ChainValidatorSet {
    /// Build from the on-chain validator records. Empty sets are
    /// rejected — consensus cannot make progress with zero stake.
    pub fn from_infos(infos: &[ValidatorInfo]) -> Result<Self> {
        if infos.is_empty() {
            return Err(ConsensusError::Config("validator set is empty".into()));
        }
        let mut validators = Vec::with_capacity(infos.len());
        for info in infos {
            if info.jailed || !info.is_active() {
                continue;
            }
            validators.push(ChainValidator::from_info(info)?);
        }
        if validators.is_empty() {
            return Err(ConsensusError::Config(
                "validator set has no active validators (all jailed or zero power)".into(),
            ));
        }
        validators.sort_by(|a, b| match b.voting_power.cmp(&a.voting_power) {
            Ordering::Equal => a.address.0.cmp(&b.address.0),
            other => other,
        });
        Ok(Self { validators })
    }
}

impl MalachiteValidatorSet<ArknetContext> for ChainValidatorSet {
    fn count(&self) -> usize {
        self.validators.len()
    }

    fn total_voting_power(&self) -> VotingPower {
        self.validators
            .iter()
            .map(|v| v.voting_power)
            .fold(0u64, |acc, p| acc.saturating_add(p))
    }

    fn get_by_address(&self, address: &ChainAddress) -> Option<&ChainValidator> {
        self.validators.iter().find(|v| v.address == *address)
    }

    fn get_by_index(&self, index: usize) -> Option<&ChainValidator> {
        self.validators.get(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_common::types::{NodeId, PubKey};
    use malachitebft_signing_ed25519::Ed25519;

    /// Generate a sample validator with a real ed25519 public key so
    /// `malachite`'s `PublicKey::from_bytes` (which panics on bad
    /// curve points) does not reject the bytes.
    fn sample(addr_byte: u8, power: u64, jailed: bool) -> ValidatorInfo {
        let sk = Ed25519::generate_keypair(rand::rngs::OsRng);
        let pk_bytes = *sk.public_key().as_bytes();
        ValidatorInfo {
            node_id: NodeId::new([addr_byte; 32]),
            consensus_key: PubKey::ed25519(pk_bytes),
            operator: Address::new([addr_byte; 20]),
            bonded_stake: 0,
            voting_power: power,
            is_genesis: true,
            jailed,
        }
    }

    #[test]
    fn empty_set_rejected() {
        let err = ChainValidatorSet::from_infos(&[]).unwrap_err();
        assert!(matches!(err, ConsensusError::Config(_)));
    }

    #[test]
    fn jailed_validators_dropped() {
        let vs =
            ChainValidatorSet::from_infos(&[sample(1, 10, true), sample(2, 5, false)]).unwrap();
        assert_eq!(vs.count(), 1);
        assert_eq!(vs.total_voting_power(), 5);
    }

    #[test]
    fn all_jailed_rejected() {
        let err =
            ChainValidatorSet::from_infos(&[sample(1, 10, true), sample(2, 5, true)]).unwrap_err();
        assert!(matches!(err, ConsensusError::Config(_)));
    }

    #[test]
    fn order_is_power_desc_then_address_asc() {
        let vs = ChainValidatorSet::from_infos(&[
            sample(3, 5, false),
            sample(2, 10, false),
            sample(1, 10, false),
        ])
        .unwrap();
        let addr_first_byte: Vec<u8> = (0..vs.count())
            .map(|i| vs.get_by_index(i).unwrap().address.0.as_bytes()[0])
            .collect();
        assert_eq!(addr_first_byte, vec![1, 2, 3]);
    }

    #[test]
    fn total_voting_power_sums() {
        let vs = ChainValidatorSet::from_infos(&[
            sample(1, 5, false),
            sample(2, 7, false),
            sample(3, 11, false),
        ])
        .unwrap();
        assert_eq!(vs.total_voting_power(), 23);
    }

    #[test]
    fn get_by_address_finds_member() {
        let vs = ChainValidatorSet::from_infos(&[sample(9, 3, false)]).unwrap();
        let addr = ChainAddress(Address::new([9u8; 20]));
        assert!(vs.get_by_address(&addr).is_some());
        let other = ChainAddress(Address::new([8u8; 20]));
        assert!(vs.get_by_address(&other).is_none());
    }

    #[test]
    fn non_ed25519_pubkey_rejected() {
        // Start from a real-keyed sample and tamper with the inner
        // `Vec<u8>` to trigger our wrong-length branch. Using a sample
        // with a valid 32-byte key first ensures we are testing our
        // own length check rather than the upstream curve-point check.
        let mut bad = sample(1, 1, false);
        bad.consensus_key.bytes = vec![0; 33];
        assert!(ChainValidator::from_info(&bad).is_err());
    }
}
