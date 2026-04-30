//! Validator-set epoch rotation + DPoS ranking.
//!
//! # Phase 1 model
//!
//! - **During bootstrap** (§9.4): the validator set is the hardcoded
//!   genesis set, locked across every epoch. No stake gating.
//! - **Post-bootstrap**: rank every registered validator by
//!   `self_stake + delegated_stake`, pick top
//!   [`MAX_ACTIVE_VALIDATORS`], emit the set for the incoming
//!   epoch.
//!
//! The consensus engine calls [`recompute_validator_set`] exactly once
//! at each `is_epoch_boundary(height)` block. Validators who fall off
//! get their records marked `jailed = false` (they're just not active);
//! validators newly above the threshold get inserted.
//!
//! # VRF proposer selection
//!
//! Phase 1 uses round-robin inside the engine (`ArknetContext::select_proposer`).
//! VRF-based selection with weighted picking ships Phase 2 when
//! `arknet_crypto::vrf` is audited.

use arknet_chain::bootstrap::in_bootstrap_epoch;
use arknet_chain::state::BlockCtx;
use arknet_chain::transactions::StakeRole;
use arknet_chain::validator::ValidatorInfo;
use arknet_common::types::{Amount, Height, NodeId};

use crate::errors::Result;
use crate::min_stake::{ModelSize, Quantization};

/// §16 `MAX_VALIDATORS_AT_GENESIS`. Phase 1 keeps this as the
/// post-bootstrap cap too; larger sets are a governance hard fork.
pub const MAX_ACTIVE_VALIDATORS: usize = 21;

/// One candidate in the ranker's working set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Validator record (may be updated with a new `voting_power`
    /// before re-insert).
    pub info: ValidatorInfo,
    /// Sum of `self_stake + delegated_stake` under
    /// `StakeRole::Validator`, no pool dimension.
    pub total_stake: Amount,
}

/// Rank every registered validator by total stake (self + delegated)
/// and return the top [`MAX_ACTIVE_VALIDATORS`]. The returned
/// candidates are ordered stake-descending, address-ascending for
/// deterministic tie-break (matches the CometBFT convention used
/// elsewhere in the engine).
pub fn rank_candidates(ctx: &BlockCtx<'_>) -> Result<Vec<Candidate>> {
    let mut candidates: Vec<Candidate> = Vec::new();
    for info in ctx.state().iter_validators()? {
        let total = sum_validator_stake(ctx, &info.node_id)?;
        candidates.push(Candidate {
            info,
            total_stake: total,
        });
    }
    candidates.sort_by(|a, b| {
        b.total_stake
            .cmp(&a.total_stake)
            .then_with(|| a.info.operator.cmp(&b.info.operator))
    });
    candidates.truncate(MAX_ACTIVE_VALIDATORS);
    Ok(candidates)
}

/// Epoch-boundary hook. Writes the new active set back to state, and
/// returns the count of active validators so the engine can surface
/// it on `/metrics` + bootstrap-epoch gate checks.
///
/// Passed to the engine's `Decide` path; run only when
/// `bootstrap::is_epoch_boundary(height)` is true.
pub fn recompute_validator_set(ctx: &mut BlockCtx<'_>, height: Height) -> Result<u32> {
    // During bootstrap, the genesis set stays fixed. We still iterate
    // it so records whose stake has grown see their `voting_power`
    // updated (useful for post-bootstrap ranking — we rank by
    // `total_stake` directly, not by `voting_power`, but some
    // auxiliary tooling reads `voting_power`).
    let candidates = rank_candidates(ctx)?;
    let bootstrap = in_bootstrap_epoch(height, candidates.len() as u32);

    let mut active = 0u32;
    for c in &candidates {
        // Post-bootstrap: a validator with zero total stake is evicted.
        // During bootstrap: all genesis validators keep their slot even
        // with zero stake (§9.4 override).
        let mut info = c.info.clone();
        if bootstrap || c.total_stake > 0 {
            info.voting_power = total_stake_to_voting_power(c.total_stake);
            info.jailed = false;
            ctx.set_validator(&info.node_id, &info)?;
            active += 1;
        } else {
            // Remove the validator record — they're no longer active.
            ctx.delete_validator(&info.node_id)?;
        }
    }
    Ok(active)
}

/// Voting-power encoding: 1 unit per 1 ARK staked, floored at 1 so a
/// validator with any stake always has weight. Post-bootstrap
/// eviction happens at zero stake; pre-eviction the floor keeps
/// malachite's quorum math stable.
///
/// Governance may revisit this once exchange rates matter.
fn total_stake_to_voting_power(total_atoms: Amount) -> u64 {
    const ATOMS_PER_ARK: u128 = 1_000_000_000;
    let ark = total_atoms / ATOMS_PER_ARK;
    ark.max(1).min(u64::MAX as u128) as u64
}

/// Sum every stake entry under `node_id` with role = Validator.
///
/// Validator stakes don't have a pool dimension (pools are a
/// compute-role concept), so we only inspect `role == Validator`.
fn sum_validator_stake(ctx: &BlockCtx<'_>, node_id: &NodeId) -> Result<Amount> {
    let entries = ctx.state().iter_stakes_for_node(node_id)?;
    let mut total: Amount = 0;
    for e in entries {
        if e.role == StakeRole::Validator {
            total = total.saturating_add(e.amount);
        }
    }
    Ok(total)
}

// The `ModelSize` / `Quantization` imports are retained for the
// forward-looking min_stake check described in §9.5. They're unused
// here today but keep the shape stable for Week 10's pool-aware
// ranker — the item lives in the staking crate's public surface.
#[allow(dead_code)]
fn _keep_imports_in_scope(_: ModelSize, _: Quantization) {}
