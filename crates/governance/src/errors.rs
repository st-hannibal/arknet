//! Governance errors.

use thiserror::Error;

/// Result alias for this crate.
pub type Result<T, E = GovernanceError> = std::result::Result<T, E>;

/// Governance subsystem errors.
#[derive(Debug, Error)]
pub enum GovernanceError {
    /// Proposal not found.
    #[error("proposal {0} not found")]
    ProposalNotFound(u64),

    /// Proposal is not in the expected phase.
    #[error("proposal {id} is in {actual} phase, expected {expected}")]
    WrongPhase {
        /// Proposal id.
        id: u64,
        /// Current phase.
        actual: String,
        /// Required phase.
        expected: String,
    },

    /// Voter has already voted on this proposal.
    #[error("duplicate vote on proposal {0}")]
    DuplicateVote(u64),

    /// Deposit is below the required minimum.
    #[error("deposit {have} below minimum {need}")]
    DepositTooLow {
        /// Offered deposit.
        have: u128,
        /// Required deposit.
        need: u128,
    },

    /// Chain-layer error.
    #[error("chain: {0}")]
    Chain(String),
}

impl From<arknet_chain::ChainError> for GovernanceError {
    fn from(e: arknet_chain::ChainError) -> Self {
        GovernanceError::Chain(e.to_string())
    }
}
