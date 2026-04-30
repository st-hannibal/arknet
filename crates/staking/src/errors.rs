//! Error types for staking operations.

use arknet_common::types::Amount;
use thiserror::Error;

/// Fallible result type for the staking crate.
pub type Result<T> = std::result::Result<T, StakingError>;

/// Everything the staking lifecycle can fail on.
///
/// Recoverable errors (nonce mismatches, insufficient balance, bad
/// pool) surface as [`StakingError`]; the `apply_tx` dispatcher maps
/// them to `TxOutcome::Rejected(reason)` so consensus isn't blocked
/// by a bad user-submitted tx.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StakingError {
    /// Sender has insufficient balance for the stake deposit (or
    /// can't pay the fee before the deposit).
    #[error("insufficient balance: have {have}, need {need}")]
    InsufficientBalance {
        /// Current balance in ark_atom.
        have: Amount,
        /// Required (deposit + fee).
        need: Amount,
    },
    /// Stake entry doesn't exist for the withdraw / complete.
    #[error("stake entry not found")]
    StakeNotFound,
    /// Tried to withdraw more than is currently staked.
    #[error("withdraw exceeds stake: requested {requested}, available {available}")]
    WithdrawExceedsStake {
        /// Amount asked to withdraw.
        requested: Amount,
        /// Currently staked amount.
        available: Amount,
    },
    /// The unbonding window hasn't elapsed yet.
    #[error("unbonding not yet complete: current height {current}, completes at {completes_at}")]
    UnbondingNotComplete {
        /// Current chain height.
        current: u64,
        /// Height at which the unbonding may be completed.
        completes_at: u64,
    },
    /// No pending unbonding with the given id (or wrong node).
    #[error("unbonding entry not found")]
    UnbondingNotFound,
    /// Proposed stake falls below `min_stake(role, pool, height)`.
    #[error("stake below minimum: proposed {proposed}, minimum {minimum}")]
    BelowMinimum {
        /// Proposed staked amount after the op.
        proposed: Amount,
        /// Minimum required at this height for this role/pool.
        minimum: Amount,
    },
    /// Redelegate rejected during cooldown.
    #[error("redelegate cooldown active: {blocks_remaining} blocks remaining")]
    RedelegateCooldown {
        /// Blocks remaining on the 1-day cooldown.
        blocks_remaining: u64,
    },
    /// Chain state I/O failed.
    #[error("chain state: {0}")]
    ChainState(String),
}

impl From<arknet_chain::errors::ChainError> for StakingError {
    fn from(e: arknet_chain::errors::ChainError) -> Self {
        StakingError::ChainState(e.to_string())
    }
}
