//! Escrow, reward distribution, and emission schedule for arknet.
//!
//! The economic core: escrows lock user payment before inference,
//! the emission schedule budgets new ARK minting per epoch, and the
//! reward calculator splits the total value (user payment + block
//! reward) across six recipients.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod emission;
pub mod errors;
pub mod escrow;
pub mod rewards;

pub use emission::{
    epoch_budget, epoch_for_height, per_token_rate, year_for_height, EpochEmissionState,
    ATOMS_PER_ARK, EPOCHS_PER_YEAR, EPOCH_LENGTH, TOTAL_SUPPLY_CAP,
};
pub use errors::{PaymentError, Result};
pub use escrow::{
    create_escrow, refund_escrow, settle_escrow, EscrowEntry, EscrowLockParams, EscrowState,
    ESCROW_LOCK_GAS, ESCROW_REFUND_GAS, ESCROW_SETTLE_GAS, ESCROW_TIMEOUT_BLOCKS,
};
pub use rewards::{
    compute_block_reward, credit_rewards, distribute_reward, latency_mult, size_mult, uptime_mult,
    ModelCategory, RewardDistribution, BURN_PERCENT, COMPUTE_PERCENT, DELEGATOR_PERCENT,
    ROUTER_PERCENT, TREASURY_PERCENT, VERIFIER_PERCENT,
};
