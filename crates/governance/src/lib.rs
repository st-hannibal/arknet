//! On-chain governance for arknet.
//!
//! Proposals, voting, tally, and parameter-change execution.
//! §13 of PROTOCOL_SPEC.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod errors;
pub mod proposals;

pub use errors::{GovernanceError, Result};
pub use proposals::{
    phase_for_time, vote_key, ProposalPhase, ProposalRecord, Tally, TallyOutcome,
    APPROVAL_THRESHOLD_BPS, GOV_PROPOSAL_GAS, GOV_VOTE_GAS, PROPOSAL_DEPOSIT, QUORUM_BPS,
    VETO_THRESHOLD_BPS,
};
