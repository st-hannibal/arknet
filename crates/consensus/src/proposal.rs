//! Concrete [`malachitebft_core_types::Proposal`] implementation.
//!
//! A proposal carries the full [`ChainValue`] (block) plus round
//! metadata. We use malachite's `ValuePayload::ProposalOnly` mode, so
//! there is no separate [`ProposalPart`] streaming channel — the whole
//! block fits inside the proposal message.

use malachitebft_core_types::{
    Proposal as MalachiteProposal, ProposalPart as MalachiteProposalPart, Round,
};

use crate::context::ArknetContext;
use crate::height::Height;
use crate::validators::ChainAddress;
use crate::value::ChainValue;

/// Consensus proposal wrapping a fully-formed block.
///
/// `Eq` is derived but malachite's trait bounds do NOT require `Ord` on
/// proposals (only `Eq + Clone + Debug + Send + Sync`), so we skip it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainProposal {
    /// Height this proposal is for.
    pub height: Height,
    /// Round inside the height.
    pub round: Round,
    /// The full block body + header.
    pub value: ChainValue,
    /// Proof-of-lock round. `Round::new(-1)` (i.e. `Round::Nil`) when
    /// this is a first proposal with no prior polka.
    pub pol_round: Round,
    /// Proposer's operator address.
    pub validator_address: ChainAddress,
}

impl MalachiteProposal<ArknetContext> for ChainProposal {
    fn height(&self) -> Height {
        self.height
    }

    fn round(&self) -> Round {
        self.round
    }

    fn value(&self) -> &ChainValue {
        &self.value
    }

    fn take_value(self) -> ChainValue {
        self.value
    }

    fn pol_round(&self) -> Round {
        self.pol_round
    }

    fn validator_address(&self) -> &ChainAddress {
        &self.validator_address
    }
}

/// We run in [`ValuePayload::ProposalOnly`] mode — no part-streaming.
/// Malachite's `Context::ProposalPart` associated type still needs a
/// concrete implementor even when unused, so `ChainProposalPart` is
/// a newtype over `()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainProposalPart;

impl MalachiteProposalPart<ArknetContext> for ChainProposalPart {
    fn is_first(&self) -> bool {
        true
    }

    fn is_last(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_chain::block::{Block, BlockHeader};
    use arknet_common::types::{Address, BlockHash, NodeId, StateRoot};

    fn sample_block() -> Block {
        let header = BlockHeader {
            version: 1,
            chain_id: "arknet-test".into(),
            height: 1,
            timestamp_ms: 0,
            parent_hash: BlockHash::new([0; 32]),
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
    fn proposal_exposes_inner_block() {
        let prop = ChainProposal {
            height: Height(1),
            round: Round::new(0),
            value: ChainValue::new(sample_block()),
            pol_round: Round::Nil,
            validator_address: ChainAddress(Address::new([7; 20])),
        };

        assert_eq!(prop.height(), Height(1));
        assert_eq!(prop.round(), Round::new(0));
        assert_eq!(prop.pol_round(), Round::Nil);
        assert_eq!(
            prop.validator_address(),
            &ChainAddress(Address::new([7; 20]))
        );

        let value_hash = prop.value().block.header.hash();
        let taken_hash = prop.take_value().block.header.hash();
        assert_eq!(value_hash, taken_hash);
    }

    #[test]
    fn proposal_part_is_only_part() {
        let part = ChainProposalPart;
        assert!(part.is_first());
        assert!(part.is_last());
    }
}
