//! Concrete [`malachitebft_core_types::Vote`] implementation.
//!
//! We keep vote payloads in the simplest shape possible — height, round,
//! voted value id, vote type, and issuer address. Phase 1 does not use
//! vote extensions (they are only needed for app-driven features like
//! ABCI++), so `Extension = ()` on the [`ArknetContext`].

use malachitebft_core_types::{NilOrVal, Round, SignedExtension, Vote as MalachiteVote, VoteType};

use crate::context::ArknetContext;
use crate::height::Height;
use crate::validators::ChainAddress;
use crate::value::BlockId;

/// Prevote / precommit vote issued by a validator for a specific round.
///
/// Malachite's trait keeps ordering-relevance inside votes (`Ord`
/// required), so we derive on `(height, round, type, value, validator)`
/// which is a stable tuple deterministic across every node.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChainVote {
    /// Height this vote targets.
    pub height: Height,
    /// Round inside the height.
    pub round: Round,
    /// `Nil` → voted for no value this round (timeout), `Val(id)` →
    /// voted for this block id.
    pub value: NilOrVal<BlockId>,
    /// Prevote or precommit.
    pub vote_type: VoteType,
    /// Operator address of the validator that cast this vote.
    pub validator_address: ChainAddress,
}

impl MalachiteVote<ArknetContext> for ChainVote {
    fn height(&self) -> Height {
        self.height
    }

    fn round(&self) -> Round {
        self.round
    }

    fn value(&self) -> &NilOrVal<BlockId> {
        &self.value
    }

    fn take_value(self) -> NilOrVal<BlockId> {
        self.value
    }

    fn vote_type(&self) -> VoteType {
        self.vote_type
    }

    fn validator_address(&self) -> &ChainAddress {
        &self.validator_address
    }

    // Vote extensions are unused in Phase 1 — we keep the Context's
    // `Extension = ()`, so these methods return / accept the unit type.
    fn extension(&self) -> Option<&SignedExtension<ArknetContext>> {
        None
    }

    fn take_extension(&mut self) -> Option<SignedExtension<ArknetContext>> {
        None
    }

    fn extend(self, _extension: SignedExtension<ArknetContext>) -> Self {
        // No-op: Phase 1 does not carry vote extensions. Swallowing is
        // safe because callers only attach extensions when
        // `ValuePayload` demands them, and our context will declare it
        // does not.
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use arknet_common::types::Address;

    fn sample(addr_byte: u8, vt: VoteType) -> ChainVote {
        ChainVote {
            height: Height(1),
            round: Round::new(0),
            value: NilOrVal::Nil,
            vote_type: vt,
            validator_address: ChainAddress(Address::new([addr_byte; 20])),
        }
    }

    #[test]
    fn extension_methods_are_no_ops() {
        let v = sample(1, VoteType::Prevote);
        assert!(v.extension().is_none());

        let mut v2 = sample(1, VoteType::Prevote);
        assert!(v2.take_extension().is_none());
    }

    #[test]
    fn ord_is_stable() {
        let a = sample(1, VoteType::Prevote);
        let b = sample(2, VoteType::Prevote);
        assert!(a < b);
    }

    #[test]
    fn accessors_return_expected_values() {
        let v = sample(7, VoteType::Precommit);
        assert_eq!(v.height(), Height(1));
        assert_eq!(v.round(), Round::new(0));
        assert!(matches!(v.value(), NilOrVal::Nil));
        assert_eq!(v.vote_type(), VoteType::Precommit);
        assert_eq!(v.validator_address(), &ChainAddress(Address::new([7; 20])));
    }
}
