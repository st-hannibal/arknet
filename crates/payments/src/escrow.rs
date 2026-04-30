//! Escrow — pre-lock user payment before inference.
//!
//! §11 lifecycle: `CREATED → ESCROWED → … → SETTLED | REFUNDED`.
//!
//! An escrow subtracts `amount` from the user's spendable balance and
//! parks it in a per-job hold. The hold is released to the reward
//! distribution on settlement or returned to the user on timeout.
//!
//! # State representation
//!
//! Escrows live in `CF_ESCROWS` (key: `job_id`, value: borsh
//! `EscrowEntry`). The column family is added to `chain/state.rs` by
//! the wiring commit that accompanies this module.
//!
//! # Security
//!
//! - Double-escrow for the same `job_id` is rejected.
//! - Only the user who created the escrow (or the settlement path)
//!   can release it.
//! - Refund is automatic after `ESCROW_TIMEOUT_BLOCKS` if no
//!   settlement tx lands.

use arknet_common::types::{Address, Amount, Height, JobId};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::errors::{PaymentError, Result};

/// Blocks before an unsettled escrow is automatically refundable
/// (§16: 5 minutes at 1s blocks = 300 blocks).
pub const ESCROW_TIMEOUT_BLOCKS: Height = 300;

/// Gas charged for an escrow lock operation.
pub const ESCROW_LOCK_GAS: u64 = 50_000;

/// Gas charged for an escrow settle operation.
pub const ESCROW_SETTLE_GAS: u64 = 50_000;

/// Gas charged for an escrow refund operation.
pub const ESCROW_REFUND_GAS: u64 = 30_000;

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

impl EscrowEntry {
    /// `true` if the escrow is past the timeout window and still locked.
    pub fn is_refundable(&self, current_height: Height) -> bool {
        self.state == EscrowState::Locked
            && current_height >= self.created_at.saturating_add(ESCROW_TIMEOUT_BLOCKS)
    }

    /// `true` if the escrow is in a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(self.state, EscrowState::Settled | EscrowState::Refunded)
    }
}

/// Parameters for creating a new escrow.
pub struct EscrowLockParams {
    /// Job id.
    pub job_id: JobId,
    /// User address.
    pub user: Address,
    /// Amount to lock.
    pub amount: Amount,
    /// Current block height.
    pub height: Height,
}

/// Build a new locked escrow entry. The caller is responsible for
/// debiting the user's balance before calling this.
pub fn create_escrow(params: EscrowLockParams) -> EscrowEntry {
    EscrowEntry {
        job_id: params.job_id,
        user: params.user,
        amount: params.amount,
        created_at: params.height,
        state: EscrowState::Locked,
    }
}

/// Settle an escrow — transition from Locked → Settled. Returns the
/// amount that should be distributed to reward recipients.
///
/// Returns an error if the escrow is not in the Locked state.
pub fn settle_escrow(entry: &mut EscrowEntry) -> Result<Amount> {
    if entry.state != EscrowState::Locked {
        return Err(PaymentError::EscrowNotLocked {
            job_id: entry.job_id,
            state: format!("{:?}", entry.state),
        });
    }
    entry.state = EscrowState::Settled;
    Ok(entry.amount)
}

/// Refund an escrow — transition from Locked → Refunded. Returns the
/// amount that should be credited back to the user.
///
/// Returns an error if the escrow is not in the Locked state.
pub fn refund_escrow(entry: &mut EscrowEntry) -> Result<Amount> {
    if entry.state != EscrowState::Locked {
        return Err(PaymentError::EscrowNotLocked {
            job_id: entry.job_id,
            state: format!("{:?}", entry.state),
        });
    }
    entry.state = EscrowState::Refunded;
    Ok(entry.amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(b: u8) -> JobId {
        JobId::new([b; 32])
    }

    fn addr(b: u8) -> Address {
        Address::new([b; 20])
    }

    #[test]
    fn create_and_settle() {
        let mut e = create_escrow(EscrowLockParams {
            job_id: job(1),
            user: addr(1),
            amount: 1_000_000,
            height: 100,
        });
        assert_eq!(e.state, EscrowState::Locked);
        assert!(!e.is_terminal());

        let amt = settle_escrow(&mut e).unwrap();
        assert_eq!(amt, 1_000_000);
        assert_eq!(e.state, EscrowState::Settled);
        assert!(e.is_terminal());
    }

    #[test]
    fn create_and_refund() {
        let mut e = create_escrow(EscrowLockParams {
            job_id: job(2),
            user: addr(2),
            amount: 500,
            height: 50,
        });
        let amt = refund_escrow(&mut e).unwrap();
        assert_eq!(amt, 500);
        assert_eq!(e.state, EscrowState::Refunded);
    }

    #[test]
    fn double_settle_fails() {
        let mut e = create_escrow(EscrowLockParams {
            job_id: job(3),
            user: addr(3),
            amount: 100,
            height: 0,
        });
        settle_escrow(&mut e).unwrap();
        let err = settle_escrow(&mut e).unwrap_err();
        assert!(matches!(err, PaymentError::EscrowNotLocked { .. }));
    }

    #[test]
    fn refund_after_settle_fails() {
        let mut e = create_escrow(EscrowLockParams {
            job_id: job(4),
            user: addr(4),
            amount: 100,
            height: 0,
        });
        settle_escrow(&mut e).unwrap();
        let err = refund_escrow(&mut e).unwrap_err();
        assert!(matches!(err, PaymentError::EscrowNotLocked { .. }));
    }

    #[test]
    fn is_refundable_respects_timeout() {
        let e = create_escrow(EscrowLockParams {
            job_id: job(5),
            user: addr(5),
            amount: 100,
            height: 100,
        });
        assert!(!e.is_refundable(100));
        assert!(!e.is_refundable(399));
        assert!(e.is_refundable(400));
        assert!(e.is_refundable(999));
    }

    #[test]
    fn settled_escrow_not_refundable() {
        let mut e = create_escrow(EscrowLockParams {
            job_id: job(6),
            user: addr(6),
            amount: 100,
            height: 0,
        });
        settle_escrow(&mut e).unwrap();
        assert!(!e.is_refundable(9999));
    }
}
