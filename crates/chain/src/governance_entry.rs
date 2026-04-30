//! Governance records stored in `CF_PROPOSALS`.
//!
//! Kept in the chain crate to avoid a chain ↔ governance dependency
//! cycle.

use arknet_common::types::Height;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// Lifecycle phase of a proposal.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub enum ProposalPhase {
    /// Discussion period — no voting allowed yet.
    Discussion,
    /// Voting period — votes accepted.
    Voting,
    /// Tally complete — proposal passed.
    Passed,
    /// Tally complete — proposal rejected.
    Rejected,
    /// Tally complete — rejected with veto (deposit burned).
    RejectedWithVeto,
    /// Passed + activation height reached — changes applied.
    Executed,
}

/// On-chain proposal record.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct ProposalRecord {
    /// The original proposal body from the transaction.
    pub proposal: crate::transactions::Proposal,
    /// Current lifecycle phase.
    pub phase: ProposalPhase,
    /// Block height at which the proposal was submitted.
    pub submitted_at: Height,
}
