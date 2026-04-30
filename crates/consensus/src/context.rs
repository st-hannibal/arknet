//! `malachitebft_core_types::Context` binding.
//!
//! [`ArknetContext`] is the single type parameter every other malachite
//! type is generic over in our crate. It declares the concrete types we
//! use for height, address, value, vote, proposal, and signing scheme.
//!
//! Because the `Context` trait requires `Sized + Clone + Send + Sync +
//! 'static`, and the state machine only uses it to dispatch trait
//! methods, we make [`ArknetContext`] a zero-sized marker carrying no
//! data. All the useful state (validator set, signing provider) lives
//! outside the context and is threaded into the engine explicitly.

use malachitebft_core_types::{Context, NilOrVal, Round, ValidatorSet, ValueId};
use malachitebft_signing_ed25519::Ed25519;

use crate::height::Height;
use crate::proposal::{ChainProposal, ChainProposalPart};
use crate::validators::{ChainAddress, ChainValidator, ChainValidatorSet};
use crate::value::ChainValue;
use crate::vote::ChainVote;

/// Zero-sized marker type binding all of malachite's associated types to
/// arknet's concrete implementations.
///
/// Every function in the `malachite-core-*` crates that takes a context
/// takes an `&ArknetContext` or a `Ctx: Context = ArknetContext`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ArknetContext;

impl Context for ArknetContext {
    type Address = ChainAddress;
    type Height = Height;
    type ProposalPart = ChainProposalPart;
    type Proposal = ChainProposal;
    type Validator = ChainValidator;
    type ValidatorSet = ChainValidatorSet;
    type Value = ChainValue;
    type Vote = ChainVote;
    type Extension = ();
    type SigningScheme = Ed25519;

    fn select_proposer<'a>(
        &self,
        validator_set: &'a Self::ValidatorSet,
        _height: Self::Height,
        round: Round,
    ) -> &'a Self::Validator {
        // Phase 1 Week 7-8: deterministic round-robin over the
        // current (static) validator set. VRF-based proposer selection
        // lands in Week 9 when DPoS is live.
        //
        // `Round` may be `Nil` at startup before the state machine
        // advances; treat that as round 0 so the call is still total.
        let r = round.as_i64().max(0) as usize;
        let idx = r % validator_set.count().max(1);
        validator_set
            .get_by_index(idx)
            .expect("validator set is non-empty by invariant")
    }

    fn new_proposal(
        &self,
        height: Self::Height,
        round: Round,
        value: Self::Value,
        pol_round: Round,
        address: Self::Address,
    ) -> Self::Proposal {
        ChainProposal {
            height,
            round,
            value,
            pol_round,
            validator_address: address,
        }
    }

    fn new_prevote(
        &self,
        height: Self::Height,
        round: Round,
        value_id: NilOrVal<ValueId<Self>>,
        address: Self::Address,
    ) -> Self::Vote {
        ChainVote {
            height,
            round,
            value: value_id,
            vote_type: malachitebft_core_types::VoteType::Prevote,
            validator_address: address,
        }
    }

    fn new_precommit(
        &self,
        height: Self::Height,
        round: Round,
        value_id: NilOrVal<ValueId<Self>>,
        address: Self::Address,
    ) -> Self::Vote {
        ChainVote {
            height,
            round,
            value: value_id,
            vote_type: malachitebft_core_types::VoteType::Precommit,
            validator_address: address,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_chain::validator::ValidatorInfo;
    use arknet_common::types::{Address, NodeId, PubKey};
    use malachitebft_core_types::Validator as _;

    fn three_validators() -> ChainValidatorSet {
        // Generate real ed25519 keys — malachite's `PublicKey::from_bytes`
        // panics on malformed curve points so hard-coded byte patterns
        // would break the adapter before our assertions run.
        let mk = |addr_byte: u8, power: u64| {
            use malachitebft_signing_ed25519::Ed25519;
            let sk = Ed25519::generate_keypair(rand::rngs::OsRng);
            let pk_bytes = *sk.public_key().as_bytes();
            ValidatorInfo {
                node_id: NodeId::new([addr_byte; 32]),
                consensus_key: PubKey::ed25519(pk_bytes),
                operator: Address::new([addr_byte; 20]),
                bonded_stake: 0,
                voting_power: power,
                is_genesis: true,
                jailed: false,
            }
        };
        ChainValidatorSet::from_infos(&[mk(1, 1), mk(2, 1), mk(3, 1)]).unwrap()
    }

    #[test]
    fn proposer_cycles_with_round() {
        let ctx = ArknetContext;
        let vs = three_validators();
        let pr_0 = ctx.select_proposer(&vs, Height(1), Round::new(0));
        let pr_1 = ctx.select_proposer(&vs, Height(1), Round::new(1));
        let pr_2 = ctx.select_proposer(&vs, Height(1), Round::new(2));
        let pr_3 = ctx.select_proposer(&vs, Height(1), Round::new(3));

        assert_ne!(pr_0.address, pr_1.address);
        assert_ne!(pr_1.address, pr_2.address);
        assert_eq!(pr_3.address, pr_0.address); // wraps at count
    }

    #[test]
    fn proposer_nil_round_falls_back_to_first() {
        let ctx = ArknetContext;
        let vs = three_validators();
        let pr = ctx.select_proposer(&vs, Height(1), Round::Nil);
        // Round::Nil → round 0 → first validator
        assert_eq!(&pr.address, vs.get_by_index(0).unwrap().address());
    }

    #[test]
    fn new_prevote_has_correct_fields() {
        let ctx = ArknetContext;
        let v = ctx.new_prevote(
            Height(5),
            Round::new(2),
            NilOrVal::Nil,
            ChainAddress(Address::new([9; 20])),
        );
        assert_eq!(v.height, Height(5));
        assert_eq!(v.round, Round::new(2));
        assert_eq!(v.vote_type, malachitebft_core_types::VoteType::Prevote);
    }
}
