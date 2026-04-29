//! Transaction application: `SignedTransaction` → state mutation.
//!
//! Lenient rejection model (Cosmos-style): invalid txs are discarded as
//! `TxOutcome::Rejected(reason)` without poisoning the block. The proposer
//! chooses whether to include a tx; the state layer only answers "does it
//! apply cleanly?"
//!
//! # Week 3-4 coverage
//!
//! - [`Transaction::Transfer`] — full implementation (nonce, balance, fee burn).
//! - [`Transaction::StakeOp`] — `Deposit` wires through; other variants are
//!   stubbed with `Rejected(NotYetImplemented)` until Week 9.
//! - [`Transaction::ReceiptBatch`] — rejected (`NotYetImplemented`) until
//!   Weeks 10-11.
//! - [`Transaction::RegisterModel`] / `GovProposal` / `GovVote` — same, until
//!   Week 9+.
//!
//! # Fee model
//!
//! Per PROTOCOL_SPEC §7.2: the EIP-1559 base fee is **burned** (subtracted
//! from the sender's balance, credited to nobody). The validator tip is a
//! separate field that flows to the proposer, wired up when consensus lands
//! in Week 7-8. Week 3-4 implements the burn side only, using the tx's
//! `fee` field as the gas budget priced at 1 ark_atom/gas.

use arknet_common::types::{Address, Amount, Gas, Height, Nonce};

use crate::errors::{ChainError, Result};
use crate::state::BlockCtx;
use crate::transactions::{SignedTransaction, Transaction};

/// Outcome of a single `apply_tx` call. Lenient: rejection is a normal
/// return, not a [`ChainError`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TxOutcome {
    /// State mutated cleanly.
    Applied {
        /// Gas consumed (for block gas accounting).
        gas_used: Gas,
    },
    /// Tx was not applied; state is unchanged.
    Rejected(RejectReason),
}

/// Why a transaction was rejected. Used by mempool / proposer to filter bad
/// txs without halting consensus.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RejectReason {
    /// Sender account has insufficient balance for `amount + fee`.
    InsufficientBalance {
        /// Amount the sender owns.
        have: Amount,
        /// Amount required (amount + fee).
        need: Amount,
    },
    /// Nonce mismatch — sender replayed or skipped ahead.
    NonceMismatch {
        /// Nonce expected by state.
        expected: Nonce,
        /// Nonce the tx carried.
        got: Nonce,
    },
    /// Fee is below the protocol floor (must cover base transfer gas).
    FeeTooLow {
        /// Minimum fee required.
        min: Gas,
        /// Fee the tx offered.
        got: Gas,
    },
    /// Self-transfer (`from == to`) — disallowed to keep the transfer flow
    /// simple and avoid nonce-only traffic that mutates nothing.
    SelfTransfer,
    /// No stake entry exists for the (node, role, pool, delegator) tuple.
    /// Surfaced by Withdraw / Redelegate.
    StakeNotFound,
    /// Withdraw / Redelegate asked for more than the entry holds.
    StakeExceeded {
        /// Amount requested.
        requested: Amount,
        /// Amount available in the entry.
        available: Amount,
    },
    /// `StakeOp::Complete` called before the unbonding window elapsed.
    UnbondingNotComplete {
        /// Current block height.
        current: Height,
        /// Earliest height at which Complete may land.
        completes_at: Height,
    },
    /// `StakeOp::Complete` targets a non-existent unbonding id.
    UnbondingNotFound,
    /// Redelegate rejected during the 1-day cooldown.
    RedelegateCooldown {
        /// Blocks still to wait.
        blocks_remaining: Height,
    },
    /// Third-party delegation (delegator != sender) — reserved for Phase 2.
    ThirdPartyDelegation,
    /// Redelegate source and destination are the same node.
    RedelegateSameNode,
    /// Transaction variant is not yet live in this phase — see the phase
    /// plan.
    NotYetImplemented(&'static str),
}

/// Minimum gas cost of a `Transfer` transaction. Matches EVM's 21,000 base
/// gas — not binding beyond that reference.
pub const BASE_TRANSFER_GAS: Gas = 21_000;

/// Apply a signed transaction against the buffered block context.
///
/// Returns [`TxOutcome::Applied`] or [`TxOutcome::Rejected`] as appropriate.
/// Errors ([`ChainError`]) are reserved for unrecoverable issues (DB I/O,
/// encoding) — they abort the whole block.
pub fn apply_tx(ctx: &mut BlockCtx<'_>, tx: &SignedTransaction) -> Result<TxOutcome> {
    // Height is sourced from META; the stake handlers fall back to 0
    // on a fresh chain (block 0) which is correct — bootstrap-epoch
    // checks treat block 0 as inside the window.
    let height = ctx.current_height()?.unwrap_or(0);
    match &tx.tx {
        Transaction::Transfer {
            from,
            to,
            amount,
            nonce,
            fee,
        } => apply_transfer(ctx, from, to, *amount, *nonce, *fee),
        Transaction::StakeOp(op) => {
            let sender = derive_address_from_signer(&tx.signer);
            crate::stake_apply::apply_stake_op(ctx, op, &sender, height)
        }
        Transaction::ReceiptBatch(_) => Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
            "ReceiptBatch application (Week 10-11)",
        ))),
        Transaction::RegisterModel { .. } => Ok(TxOutcome::Rejected(
            RejectReason::NotYetImplemented("RegisterModel (Week 10+)"),
        )),
        Transaction::GovProposal(_) => Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
            "GovProposal (Week 10+)",
        ))),
        Transaction::GovVote { .. } => Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
            "GovVote (Week 10+)",
        ))),
    }
}

/// Derive the 20-byte account [`Address`] from the signer's public key.
///
/// Matches the derivation used by the genesis loader +
/// [`crate::genesis::genesis_to_validator_info`]: `blake3(pubkey_bytes)[..20]`.
fn derive_address_from_signer(signer: &arknet_common::types::PubKey) -> Address {
    let digest = arknet_crypto::hash::blake3(&signer.bytes);
    let mut out = [0u8; 20];
    out.copy_from_slice(&digest.as_bytes()[..20]);
    Address::new(out)
}

fn apply_transfer(
    ctx: &mut BlockCtx<'_>,
    from: &Address,
    to: &Address,
    amount: Amount,
    nonce: Nonce,
    fee: Gas,
) -> Result<TxOutcome> {
    if from == to {
        return Ok(TxOutcome::Rejected(RejectReason::SelfTransfer));
    }
    if fee < BASE_TRANSFER_GAS {
        return Ok(TxOutcome::Rejected(RejectReason::FeeTooLow {
            min: BASE_TRANSFER_GAS,
            got: fee,
        }));
    }

    let mut from_acct = ctx.get_account(from)?.unwrap_or_default();
    if from_acct.nonce != nonce {
        return Ok(TxOutcome::Rejected(RejectReason::NonceMismatch {
            expected: from_acct.nonce,
            got: nonce,
        }));
    }

    // Fee is priced at 1 ark_atom per gas unit during Phase 1. The base fee
    // curve from `fee_market.rs` becomes the multiplier once the block
    // builder hands it in (Week 7-8).
    let total: Amount = match amount.checked_add(fee as Amount) {
        Some(v) => v,
        None => {
            return Ok(TxOutcome::Rejected(RejectReason::InsufficientBalance {
                have: from_acct.balance,
                need: Amount::MAX,
            }))
        }
    };
    if from_acct.balance < total {
        return Ok(TxOutcome::Rejected(RejectReason::InsufficientBalance {
            have: from_acct.balance,
            need: total,
        }));
    }

    from_acct.balance -= total;
    from_acct.nonce += 1;

    let mut to_acct = ctx.get_account(to)?.unwrap_or_default();
    to_acct.balance = to_acct.balance.saturating_add(amount);

    ctx.set_account(from, &from_acct)?;
    ctx.set_account(to, &to_acct)?;

    Ok(TxOutcome::Applied { gas_used: fee })
}

// Dead-code guard: the unused `ChainError` import would trip clippy if no
// path currently surfaces one. Keep the import reachable via a trivial
// `From` to prepare for Week 9's stake ops.
#[allow(dead_code)]
fn _chain_error_is_reachable(_: ChainError) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::Account;
    use crate::state::State;
    use arknet_common::types::{PubKey, Signature, SignatureScheme};

    fn tmp_state() -> (tempfile::TempDir, State) {
        let tmp = tempfile::tempdir().unwrap();
        let state = State::open(tmp.path()).unwrap();
        (tmp, state)
    }

    fn sign(tx: Transaction) -> SignedTransaction {
        SignedTransaction {
            tx,
            signer: PubKey::ed25519([1; 32]),
            signature: Signature::new(SignatureScheme::Ed25519, vec![2; 64]).unwrap(),
        }
    }

    fn fund(ctx: &mut BlockCtx<'_>, addr: &Address, balance: Amount) {
        ctx.set_account(addr, &Account { balance, nonce: 0 })
            .unwrap();
    }

    #[test]
    fn transfer_happy_path() {
        let (_tmp, state) = tmp_state();
        let alice = Address::new([1; 20]);
        let bob = Address::new([2; 20]);

        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &alice, 1_000_000);
            ctx.commit().unwrap();
        }

        let mut ctx = state.begin_block();
        let stx = sign(Transaction::Transfer {
            from: alice,
            to: bob,
            amount: 500,
            nonce: 0,
            fee: BASE_TRANSFER_GAS,
        });
        let outcome = apply_tx(&mut ctx, &stx).unwrap();
        assert_eq!(
            outcome,
            TxOutcome::Applied {
                gas_used: BASE_TRANSFER_GAS
            }
        );
        ctx.commit().unwrap();

        let a = state.get_account(&alice).unwrap().unwrap();
        let b = state.get_account(&bob).unwrap().unwrap();
        assert_eq!(a.balance, 1_000_000 - 500 - BASE_TRANSFER_GAS as Amount);
        assert_eq!(a.nonce, 1);
        assert_eq!(b.balance, 500);
    }

    #[test]
    fn transfer_rejects_wrong_nonce() {
        let (_tmp, state) = tmp_state();
        let alice = Address::new([1; 20]);
        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &alice, 1_000_000);
            ctx.commit().unwrap();
        }

        let mut ctx = state.begin_block();
        let stx = sign(Transaction::Transfer {
            from: alice,
            to: Address::new([2; 20]),
            amount: 1,
            nonce: 42, // sender has nonce 0
            fee: BASE_TRANSFER_GAS,
        });
        match apply_tx(&mut ctx, &stx).unwrap() {
            TxOutcome::Rejected(RejectReason::NonceMismatch {
                expected: 0,
                got: 42,
            }) => {}
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn transfer_rejects_over_balance() {
        let (_tmp, state) = tmp_state();
        let alice = Address::new([1; 20]);
        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &alice, 100);
            ctx.commit().unwrap();
        }

        let mut ctx = state.begin_block();
        let stx = sign(Transaction::Transfer {
            from: alice,
            to: Address::new([2; 20]),
            amount: 1_000_000,
            nonce: 0,
            fee: BASE_TRANSFER_GAS,
        });
        match apply_tx(&mut ctx, &stx).unwrap() {
            TxOutcome::Rejected(RejectReason::InsufficientBalance { .. }) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn transfer_rejects_below_base_fee() {
        let (_tmp, state) = tmp_state();
        let alice = Address::new([1; 20]);
        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &alice, 1_000_000);
            ctx.commit().unwrap();
        }

        let mut ctx = state.begin_block();
        let stx = sign(Transaction::Transfer {
            from: alice,
            to: Address::new([2; 20]),
            amount: 1,
            nonce: 0,
            fee: 100, // below BASE_TRANSFER_GAS
        });
        match apply_tx(&mut ctx, &stx).unwrap() {
            TxOutcome::Rejected(RejectReason::FeeTooLow { min, got: 100 }) => {
                assert_eq!(min, BASE_TRANSFER_GAS);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn transfer_rejects_self_transfer() {
        let (_tmp, state) = tmp_state();
        let alice = Address::new([1; 20]);
        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &alice, 1_000_000);
            ctx.commit().unwrap();
        }

        let mut ctx = state.begin_block();
        let stx = sign(Transaction::Transfer {
            from: alice,
            to: alice,
            amount: 1,
            nonce: 0,
            fee: BASE_TRANSFER_GAS,
        });
        assert_eq!(
            apply_tx(&mut ctx, &stx).unwrap(),
            TxOutcome::Rejected(RejectReason::SelfTransfer)
        );
    }

    #[test]
    fn rejected_tx_does_not_mutate_state() {
        let (_tmp, state) = tmp_state();
        let alice = Address::new([1; 20]);
        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &alice, 1_000_000);
            ctx.commit().unwrap();
        }
        let root_before = state.state_root();

        let mut ctx = state.begin_block();
        let stx = sign(Transaction::Transfer {
            from: alice,
            to: Address::new([2; 20]),
            amount: 1,
            nonce: 999, // bogus
            fee: BASE_TRANSFER_GAS,
        });
        let _ = apply_tx(&mut ctx, &stx).unwrap();
        ctx.commit().unwrap();

        assert_eq!(state.state_root(), root_before);
    }

    #[test]
    fn stake_deposit_happy_path_applies() {
        use crate::transactions::{StakeOp, StakeRole};

        let (_tmp, state) = tmp_state();
        // Derive the sender address from the public key bytes used by `sign`
        // so the deposit debits the correct account.
        let signer_pubkey: [u8; 32] = [1; 32];
        let sender = {
            let d = arknet_crypto::hash::blake3(&signer_pubkey);
            let mut a = [0u8; 20];
            a.copy_from_slice(&d.as_bytes()[..20]);
            Address::new(a)
        };
        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &sender, 10_000_000);
            ctx.commit().unwrap();
        }

        let mut ctx = state.begin_block();
        let stx = sign(Transaction::StakeOp(StakeOp::Deposit {
            node_id: arknet_common::types::NodeId::new([9; 32]),
            role: StakeRole::Validator,
            pool_id: None,
            amount: 2_500,
            delegator: None,
        }));
        match apply_tx(&mut ctx, &stx).unwrap() {
            TxOutcome::Applied { gas_used } => assert!(gas_used > 0),
            other => panic!("unexpected: {other:?}"),
        }
        ctx.commit().unwrap();

        let e = state
            .get_stake(
                &arknet_common::types::NodeId::new([9; 32]),
                crate::transactions::StakeRole::Validator,
                None,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(e.amount, 2_500);
    }
}
