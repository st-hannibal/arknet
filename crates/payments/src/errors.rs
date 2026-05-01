//! Payment-layer errors.

use arknet_common::types::JobId;
use thiserror::Error;

/// Result alias for this crate.
pub type Result<T, E = PaymentError> = std::result::Result<T, E>;

/// Payment subsystem errors.
#[derive(Debug, Error)]
pub enum PaymentError {
    /// Escrow for this job already exists.
    #[error("escrow already exists for job {job_id:?}")]
    EscrowAlreadyExists {
        /// The duplicate job id.
        job_id: JobId,
    },

    /// Escrow is not in the Locked state.
    #[error("escrow for job {job_id:?} is {state}, not Locked")]
    EscrowNotLocked {
        /// Job id.
        job_id: JobId,
        /// Current state.
        state: String,
    },

    /// No escrow found for this job.
    #[error("no escrow for job {job_id:?}")]
    EscrowNotFound {
        /// Job id.
        job_id: JobId,
    },

    /// User balance insufficient for the escrow amount.
    #[error("insufficient balance: have {have}, need {need}")]
    InsufficientBalance {
        /// Available balance.
        have: u128,
        /// Required amount.
        need: u128,
    },

    /// Reward computation overflow or invalid parameters.
    #[error("reward computation: {0}")]
    RewardComputation(String),

    /// Emission budget exhausted for this epoch.
    #[error("emission budget exhausted for epoch {epoch}")]
    EmissionExhausted {
        /// The epoch that ran out.
        epoch: u64,
    },

    /// Chain-layer error propagated from state operations.
    #[error("chain: {0}")]
    Chain(String),
}

impl From<arknet_chain::ChainError> for PaymentError {
    fn from(e: arknet_chain::ChainError) -> Self {
        PaymentError::Chain(e.to_string())
    }
}
