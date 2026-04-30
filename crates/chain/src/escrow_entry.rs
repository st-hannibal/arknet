//! Escrow record stored in `CF_ESCROWS`.
//!
//! Kept in the chain crate (not payments) to avoid a chain ↔ payments
//! dependency cycle — same pattern as `stake_entry.rs` and
//! `unbonding.rs`.

use arknet_common::types::{Address, Amount, Height, JobId};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// On-chain escrow record.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct EscrowEntry {
    /// Job this escrow covers.
    pub job_id: JobId,
    /// User who locked the funds.
    pub user: Address,
    /// Amount locked (ark_atom).
    pub amount: Amount,
    /// Block at which the escrow was created.
    pub created_at: Height,
    /// Current state.
    pub state: EscrowState,
}

/// Escrow lifecycle states.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub enum EscrowState {
    /// Funds locked; waiting for settlement or timeout.
    Locked,
    /// Funds distributed to reward recipients.
    Settled,
    /// Funds returned to the user (timeout or cancellation).
    Refunded,
}

/// Blocks before an unsettled escrow is automatically refundable.
pub const ESCROW_TIMEOUT_BLOCKS: Height = 300;
