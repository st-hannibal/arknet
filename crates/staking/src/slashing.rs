//! Slashing — §10 offense catalogue.
//!
//! A [`SlashEvidence`] is submitted on-chain; [`apply_slash`]
//! drains the offender's stake (self + delegators, pro-rata) and
//! distributes the penalty across three sinks:
//!
//! - **Burn** — default 90%. Drops permanent supply, makes the offense
//!   net-costly for the system.
//! - **Reporter** — default 5%. Pays the validator / user who
//!   submitted the evidence, funding the detection game.
//! - **Treasury** — default 5%. Recovered to the community fund.
//!
//! Governance can tune the percentages through the standard proposal
//! path; the constants here are the genesis defaults. The split
//! always sums to 100.
//!
//! # Why pro-rata delegators
//!
//! §9.2: delegators are slashed proportionally to their share of a
//! node's total bonded stake. This prevents a validator from hiding
//! behind a large delegator base — the penalty hits every participant
//! who chose this validator.

use arknet_chain::stake_entry::StakeEntry;
use arknet_chain::state::BlockCtx;
use arknet_chain::transactions::StakeRole;
use arknet_common::types::{Address, Amount, NodeId};

use crate::errors::Result;

/// Default percent split — burned share.
pub const BURN_PERCENT: u128 = 90;
/// Default percent split — paid to the reporter.
pub const REPORTER_PERCENT: u128 = 5;
/// Default percent split — credited to treasury.
pub const TREASURY_PERCENT: u128 = 5;

/// §10 offense taxonomy. Each variant carries the penalty fraction
/// (numerator / 100) baked into the enum so the dispatcher doesn't
/// need a side lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offense {
    /// Compute node served output from the wrong model hash.
    /// §10: 100% of stake.
    WrongModelHash,
    /// Same `job_id` appeared in two distinct receipts.
    /// §10: 100% of stake.
    DoubleClaimReceipt,
    /// Verifier and compute node colluded on a bogus result.
    /// §10: 100% of both stakes (caller applies twice).
    VerifierComputeCollusion,
    /// Prompt or user data leaked with signed evidence.
    /// §10: 100% of stake.
    DataLeak,
    /// Deterministic re-execution disagreed with the returned output.
    /// §10: 5% of stake.
    FailedDeterministicVerification,
    /// Statistical analysis flagged a router favoring specific nodes.
    /// §10: 5% of stake.
    RouterPreferentialRouting,
    /// Validator omitted a valid mint from a proposed block.
    /// §10: 10% of stake.
    CensoringMints,
    /// More than 3 timeouts in one hour.
    /// §10: 2% of stake.
    RepeatedTimeouts,
    /// Unnoticed downtime longer than 4 hours.
    /// §10: 1% of stake.
    ExtendedDowntime,
    /// Verifier raised a dispute that resolved against them.
    /// §10: 10% of stake.
    FalseDispute,
    /// Node claimed TEE capability but served inference outside a real
    /// enclave, or submitted a forged attestation quote.
    /// §10: 100% of stake.
    FakeTeeAttestation,
}

impl Offense {
    /// Slash fraction as `(numerator, 100)`.
    pub fn penalty_percent(self) -> u128 {
        match self {
            Offense::WrongModelHash => 100,
            Offense::DoubleClaimReceipt => 100,
            Offense::VerifierComputeCollusion => 100,
            Offense::DataLeak => 100,
            Offense::FailedDeterministicVerification => 5,
            Offense::RouterPreferentialRouting => 5,
            Offense::CensoringMints => 10,
            Offense::RepeatedTimeouts => 2,
            Offense::ExtendedDowntime => 1,
            Offense::FalseDispute => 10,
            Offense::FakeTeeAttestation => 100,
        }
    }
}

/// Aggregated outcome of a slash, for on-chain event emission +
/// metrics. All amounts in `ark_atom`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashReport {
    /// Total stake drained across operator + delegators.
    pub total_slashed: Amount,
    /// Portion burned (dropped from supply).
    pub burned: Amount,
    /// Portion paid to the reporter account.
    pub to_reporter: Amount,
    /// Portion credited to treasury.
    pub to_treasury: Amount,
    /// Count of per-delegator entries touched.
    pub entries_affected: usize,
}

/// Apply a slash to every stake entry bound to `node_id` under
/// `role`. Returns an aggregated [`SlashReport`].
///
/// Treasury address is passed in (governance-configurable; defaults
/// to the genesis treasury address). Reporter may be the slasher's
/// operator address or the node that submitted the evidence tx.
pub fn apply_slash(
    ctx: &mut BlockCtx<'_>,
    node_id: &NodeId,
    role: StakeRole,
    offense: Offense,
    reporter: &Address,
    treasury: &Address,
) -> Result<SlashReport> {
    let percent = offense.penalty_percent();

    // Enumerate every stake entry for this node+role. We also trim
    // in-flight unbondings — otherwise an offender could race an
    // unbond start with the slash tx.
    let entries: Vec<StakeEntry> = ctx
        .state()
        .iter_stakes_for_node(node_id)?
        .into_iter()
        .filter(|e| e.role == role)
        .collect();

    let mut total: Amount = 0;
    let mut affected = 0;

    for mut entry in entries {
        let pool_bytes = entry.pool_id.map(|p| p.0);
        let slash = entry.amount * percent / 100;
        if slash == 0 {
            continue;
        }
        entry.amount -= slash;
        ctx.set_stake(
            &entry.node_id,
            entry.role,
            pool_bytes,
            entry.delegator.as_ref(),
            &entry,
        )?;
        total = total.saturating_add(slash);
        affected += 1;
    }

    // Trim unbondings whose completion is still ahead — see
    // SECURITY.md §2 "delegator exits just before slash".
    let current_height = ctx.state().current_height()?.unwrap_or(0);
    let pending = ctx.state().iter_unbondings_for_node(node_id)?;
    for mut un in pending {
        if un.role != role {
            continue;
        }
        if un.completes_at <= current_height {
            // Already complete — out of scope for trimming; the user
            // just hasn't pulled the funds back yet. §10 + §9.2 treat
            // completed unbondings as returned-to-holder.
            continue;
        }
        let slash = un.amount * percent / 100;
        if slash == 0 {
            continue;
        }
        un.amount -= slash;
        ctx.set_unbonding(node_id, &un)?;
        total = total.saturating_add(slash);
        affected += 1;
    }

    // Split total across burn / reporter / treasury.
    let burned = total * BURN_PERCENT / 100;
    let to_reporter = total * REPORTER_PERCENT / 100;
    // Treasury gets whatever is left after burn + reporter so the three
    // shares sum to exactly `total` even with rounding.
    let to_treasury = total.saturating_sub(burned + to_reporter);

    if to_reporter > 0 {
        let mut acct = ctx.get_account(reporter)?.unwrap_or_default();
        acct.balance = acct.balance.saturating_add(to_reporter);
        ctx.set_account(reporter, &acct)?;
    }
    if to_treasury > 0 {
        let mut acct = ctx.get_account(treasury)?.unwrap_or_default();
        acct.balance = acct.balance.saturating_add(to_treasury);
        ctx.set_account(treasury, &acct)?;
    }
    // `burned` is dropped — never credited anywhere. This is the
    // sink that shrinks the `total_supply` counter when we wire it.

    Ok(SlashReport {
        total_slashed: total,
        burned,
        to_reporter,
        to_treasury,
        entries_affected: affected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_sums_to_total() {
        assert_eq!(BURN_PERCENT + REPORTER_PERCENT + TREASURY_PERCENT, 100);
    }

    #[test]
    fn penalty_percents_match_spec() {
        assert_eq!(Offense::WrongModelHash.penalty_percent(), 100);
        assert_eq!(Offense::DoubleClaimReceipt.penalty_percent(), 100);
        assert_eq!(Offense::VerifierComputeCollusion.penalty_percent(), 100);
        assert_eq!(Offense::DataLeak.penalty_percent(), 100);
        assert_eq!(
            Offense::FailedDeterministicVerification.penalty_percent(),
            5
        );
        assert_eq!(Offense::RouterPreferentialRouting.penalty_percent(), 5);
        assert_eq!(Offense::CensoringMints.penalty_percent(), 10);
        assert_eq!(Offense::RepeatedTimeouts.penalty_percent(), 2);
        assert_eq!(Offense::ExtendedDowntime.penalty_percent(), 1);
        assert_eq!(Offense::FalseDispute.penalty_percent(), 10);
        assert_eq!(Offense::FakeTeeAttestation.penalty_percent(), 100);
    }
}
