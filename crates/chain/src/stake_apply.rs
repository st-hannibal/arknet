//! Stake lifecycle handlers dispatched from [`crate::apply::apply_tx`].
//!
//! Kept inside `arknet-chain` (not `arknet-staking`) to avoid a chain
//! ↔ staking dependency cycle: staking's pure helpers
//! (`min_stake`, `validator_set`, `slashing`) read & mutate the same
//! `BlockCtx`, but the *apply-layer entry point* belongs where
//! `apply_tx` lives. Phase 2 may split this out once the staking
//! crate has its own host crate (e.g. `arknet-l1-exec`).
//!
//! Handlers return `Result<TxOutcome, ChainError>`:
//! - `Ok(TxOutcome::Applied { gas_used })` on success,
//! - `Ok(TxOutcome::Rejected(reason))` on user-level failure
//!   (insufficient balance, bad unbond id, etc.),
//! - `Err(ChainError)` on DB / encoding failures (fatal).

use arknet_common::types::{Address, Amount, Gas, Height, NodeId, PoolId};

use crate::apply::{RejectReason, TxOutcome};
use crate::errors::{ChainError, Result};
use crate::stake_entry::StakeEntry;
use crate::state::BlockCtx;
use crate::transactions::{StakeOp, StakeRole};
use crate::unbonding::UnbondingEntry;

/// §16: unbonding window in blocks (14d × 86 400 s / 1 s per block).
pub const UNBONDING_PERIOD_BLOCKS: Height = 1_209_600;

/// Flat gas price per stake op in Phase 1 (matches EVM SSTORE + a
/// couple of signature-check slots).
pub const STAKE_OP_GAS: Gas = 50_000;

/// Redelegate cooldown window (1 day at 1s blocks, §9.3).
pub const REDELEGATE_COOLDOWN_BLOCKS: Height = 86_400;

/// Apply `op` against `ctx`. `sender` is the address recovered from
/// the signed transaction; `height` is the block this tx is landing
/// in.
pub fn apply_stake_op(
    ctx: &mut BlockCtx<'_>,
    op: &StakeOp,
    sender: &Address,
    height: Height,
) -> Result<TxOutcome> {
    match op {
        StakeOp::Deposit {
            node_id,
            role,
            pool_id,
            amount,
            delegator,
        } => apply_deposit(
            ctx, sender, node_id, *role, *pool_id, *amount, *delegator, height,
        ),
        StakeOp::Withdraw {
            node_id,
            role,
            pool_id,
            amount,
        } => apply_withdraw(ctx, sender, node_id, *role, *pool_id, *amount, height),
        StakeOp::Complete {
            node_id,
            role: _,
            pool_id: _,
            unbond_id,
        } => apply_complete(ctx, sender, node_id, *unbond_id, height),
        StakeOp::Redelegate {
            from,
            to,
            role,
            amount,
        } => apply_redelegate(ctx, sender, from, to, *role, *amount, height),
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_deposit(
    ctx: &mut BlockCtx<'_>,
    sender: &Address,
    node_id: &NodeId,
    role: StakeRole,
    pool_id: Option<PoolId>,
    amount: Amount,
    delegator: Option<Address>,
    height: Height,
) -> Result<TxOutcome> {
    // Third parties can't delegate on someone else's behalf.
    if let Some(d) = delegator {
        if d != *sender {
            return Ok(TxOutcome::Rejected(RejectReason::ThirdPartyDelegation));
        }
    }

    // Enforce min_stake after bootstrap (§9.1 + §9.4).
    // During bootstrap both gates are open and min_stake returns 0,
    // so this is a no-op. After bootstrap, the base stake per role
    // applies. Model-specific multipliers (Compute role) are enforced
    // at the staking crate level during epoch recomputation; here we
    // check only the role base minimum.
    //
    // The check considers the *resulting* stake (existing + deposit)
    // so top-ups to an existing position can be any size as long as
    // the total meets the floor.
    let active_count = ctx
        .state()
        .iter_validators()
        .map(|v| v.len() as u32)
        .unwrap_or(0);
    if !crate::bootstrap::in_bootstrap_epoch(height, active_count) {
        let base_min = inline_base_min_stake(role);
        let pool_bytes_check = pool_id.map(|p| p.0);
        let existing_amount = ctx
            .get_stake(node_id, role, pool_bytes_check, delegator.as_ref())?
            .map(|e| e.amount)
            .unwrap_or(0);
        let resulting = existing_amount.saturating_add(amount);
        if resulting < base_min {
            return Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
                "stake below minimum for this role",
            )));
        }
    }

    let mut acct = ctx.get_account(sender)?.unwrap_or_default();
    if acct.balance < amount {
        return Ok(TxOutcome::Rejected(RejectReason::InsufficientBalance {
            have: acct.balance,
            need: amount,
        }));
    }
    acct.balance -= amount;
    ctx.set_account(sender, &acct)?;

    let pool_bytes = pool_id.map(|p| p.0);
    let stake_holder = delegator;
    let existing = ctx.get_stake(node_id, role, pool_bytes, stake_holder.as_ref())?;
    let entry = match existing {
        Some(mut e) => {
            e.amount = e.amount.saturating_add(amount);
            e
        }
        None => StakeEntry {
            node_id: *node_id,
            role,
            pool_id,
            delegator: stake_holder,
            amount,
            bonded_at: height,
        },
    };
    ctx.set_stake(node_id, role, pool_bytes, stake_holder.as_ref(), &entry)?;

    Ok(TxOutcome::Applied {
        gas_used: STAKE_OP_GAS,
    })
}

fn apply_withdraw(
    ctx: &mut BlockCtx<'_>,
    sender: &Address,
    node_id: &NodeId,
    role: StakeRole,
    pool_id: Option<PoolId>,
    amount: Amount,
    height: Height,
) -> Result<TxOutcome> {
    let pool_bytes = pool_id.map(|p| p.0);
    let (delegator_key, mut entry) = match ctx.get_stake(node_id, role, pool_bytes, Some(sender))? {
        Some(e) => (Some(*sender), e),
        None => match ctx.get_stake(node_id, role, pool_bytes, None)? {
            Some(e) => (None, e),
            None => return Ok(TxOutcome::Rejected(RejectReason::StakeNotFound)),
        },
    };

    if amount > entry.amount {
        return Ok(TxOutcome::Rejected(RejectReason::StakeExceeded {
            requested: amount,
            available: entry.amount,
        }));
    }

    entry.amount -= amount;
    ctx.set_stake(node_id, role, pool_bytes, delegator_key.as_ref(), &entry)?;

    let unbond_id = ctx.next_unbond_id()?;
    let unbonding = UnbondingEntry {
        unbond_id,
        node_id: *node_id,
        role,
        pool_id,
        delegator: delegator_key,
        amount,
        started_at: height,
        completes_at: height + UNBONDING_PERIOD_BLOCKS,
    };
    ctx.set_unbonding(node_id, &unbonding)?;
    ctx.set_next_unbond_id(unbond_id + 1)?;

    Ok(TxOutcome::Applied {
        gas_used: STAKE_OP_GAS,
    })
}

fn apply_complete(
    ctx: &mut BlockCtx<'_>,
    sender: &Address,
    node_id: &NodeId,
    unbond_id: u64,
    height: Height,
) -> Result<TxOutcome> {
    let entry = match ctx.get_unbonding(node_id, unbond_id)? {
        Some(e) => e,
        None => return Ok(TxOutcome::Rejected(RejectReason::UnbondingNotFound)),
    };

    if let Some(d) = entry.delegator {
        if d != *sender {
            return Ok(TxOutcome::Rejected(RejectReason::UnbondingNotFound));
        }
    }

    if !entry.is_complete(height) {
        return Ok(TxOutcome::Rejected(RejectReason::UnbondingNotComplete {
            current: height,
            completes_at: entry.completes_at,
        }));
    }

    let credit_to = entry.delegator.unwrap_or(*sender);
    let mut acct = ctx.get_account(&credit_to)?.unwrap_or_default();
    acct.balance = acct.balance.saturating_add(entry.amount);
    ctx.set_account(&credit_to, &acct)?;
    ctx.delete_unbonding(node_id, unbond_id)?;

    Ok(TxOutcome::Applied {
        gas_used: STAKE_OP_GAS,
    })
}

fn apply_redelegate(
    ctx: &mut BlockCtx<'_>,
    sender: &Address,
    from: &NodeId,
    to: &NodeId,
    role: StakeRole,
    amount: Amount,
    height: Height,
) -> Result<TxOutcome> {
    if from == to {
        return Ok(TxOutcome::Rejected(RejectReason::RedelegateSameNode));
    }
    let pool_bytes: Option<[u8; 16]> = None;

    let (src_delegator, mut src) = match ctx.get_stake(from, role, pool_bytes, Some(sender))? {
        Some(e) => (Some(*sender), e),
        None => match ctx.get_stake(from, role, pool_bytes, None)? {
            Some(e) => (None, e),
            None => return Ok(TxOutcome::Rejected(RejectReason::StakeNotFound)),
        },
    };

    if amount > src.amount {
        return Ok(TxOutcome::Rejected(RejectReason::StakeExceeded {
            requested: amount,
            available: src.amount,
        }));
    }

    let cooldown_ends = src.bonded_at.saturating_add(REDELEGATE_COOLDOWN_BLOCKS);
    if height < cooldown_ends {
        return Ok(TxOutcome::Rejected(RejectReason::RedelegateCooldown {
            blocks_remaining: cooldown_ends.saturating_sub(height),
        }));
    }

    src.amount -= amount;
    ctx.set_stake(from, role, pool_bytes, src_delegator.as_ref(), &src)?;

    let existing_dst = ctx.get_stake(to, role, pool_bytes, src_delegator.as_ref())?;
    let dst = match existing_dst {
        Some(mut e) => {
            e.amount = e.amount.saturating_add(amount);
            e
        }
        None => StakeEntry {
            node_id: *to,
            role,
            pool_id: None,
            delegator: src_delegator,
            amount,
            bonded_at: height,
        },
    };
    ctx.set_stake(to, role, pool_bytes, src_delegator.as_ref(), &dst)?;

    Ok(TxOutcome::Applied {
        gas_used: STAKE_OP_GAS,
    })
}

/// §9.1 base minimum stake per role, in `ark_atom`.
///
/// Inlined here to avoid a `chain → staking` dependency cycle.
/// The authoritative values live in `arknet_staking::min_stake`;
/// this function mirrors the role-base portion only. Compute-role
/// model-specific multipliers (size × quant) are enforced during
/// epoch recomputation in the staking crate.
fn inline_base_min_stake(role: StakeRole) -> Amount {
    use arknet_common::types::ATOMS_PER_ARK;
    let base_ark: u128 = match role {
        StakeRole::Validator => 50_000,
        StakeRole::Router => 8_000,
        StakeRole::Verifier => 10_000,
        StakeRole::Compute => 5_000,
    };
    base_ark * ATOMS_PER_ARK
}

// Silence unused-warning when ChainError is only surfaced via `?`.
#[allow(dead_code)]
fn _keep_chain_error_in_scope(_: ChainError) {}
