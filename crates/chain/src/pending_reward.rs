//! Pending reward entries stored in `CF_PENDING_REWARDS`.
//!
//! Two-phase settlement: receipts land in epoch N, rewards are
//! computed at the start of epoch N+1 once the total output tokens
//! for the epoch are known. This eliminates the early-bird advantage
//! where jobs settled early in an epoch get a disproportionate share
//! of the emission budget.

use arknet_common::types::{Address, Amount, JobId};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// A receipt whose escrow has been settled but whose block reward
/// hasn't been minted yet. Queued until the next epoch boundary.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct PendingReward {
    /// Job id.
    pub job_id: JobId,
    /// Output tokens produced by this job.
    pub output_tokens: u32,
    /// User payment amount (from escrow).
    pub user_payment: Amount,
    /// Epoch in which the receipt settled.
    pub epoch: u64,
    /// Compute node operator address.
    pub compute_addr: Address,
    /// Verifier address.
    pub verifier_addr: Address,
    /// Router address.
    pub router_addr: Address,
    /// Treasury address.
    pub treasury_addr: Address,
}
