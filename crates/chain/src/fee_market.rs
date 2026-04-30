//! EIP-1559 base-fee calculation.
//!
//! Pure function over parent-block state. Called by the block builder
//! (Phase 1 Week 7-8 consensus) and by light clients verifying fee
//! continuity. No I/O, no state — all inputs passed explicitly.
//!
//! The update rule targets a configurable utilization fraction. At
//! perfect target utilization, `base_fee` stays constant. Above target,
//! it rises toward a +12.5% ceiling per block; below target, it falls
//! toward a -12.5% floor per block.
//!
//! Reference: [EIP-1559](https://eips.ethereum.org/EIPS/eip-1559).

use arknet_common::types::{Amount, Gas};

use crate::errors::{ChainError, Result};

/// Maximum per-block change in base fee, expressed as an inverse denominator.
///
/// `delta ≤ parent_base_fee / BASE_FEE_MAX_CHANGE_DENOM`
///
/// 8 → ±12.5% per block. Matches Ethereum mainnet.
pub const BASE_FEE_MAX_CHANGE_DENOM: u128 = 8;

/// Minimum base fee. Prevents the fee from collapsing to zero during a
/// sustained under-target stretch (which would defeat the anti-spam
/// function of fees entirely).
pub const MIN_BASE_FEE: Amount = 1;

/// Target gas per block (half of the block gas limit in EIP-1559).
/// Set to 15M gas — matches Ethereum mainnet's target.
pub const TARGET_GAS_PER_BLOCK: Gas = 15_000_000;

/// Compute the next block's base fee from the parent block's state.
///
/// * `parent_base_fee` — base fee of block N-1.
/// * `parent_gas_used` — gas used by block N-1.
/// * `parent_gas_target` — desired gas per block (half of the block gas limit
///   in canonical EIP-1559).
///
/// Returns the base fee for block N.
///
/// # Errors
///
/// Returns [`ChainError::FeeMarket`] if `parent_gas_target == 0` (division
/// by zero). All other inputs are valid.
pub fn next_base_fee(
    parent_base_fee: Amount,
    parent_gas_used: Gas,
    parent_gas_target: Gas,
) -> Result<Amount> {
    if parent_gas_target == 0 {
        return Err(ChainError::FeeMarket("gas target must be > 0"));
    }

    let target = parent_gas_target as u128;
    let used = parent_gas_used as u128;

    // At target: no change.
    if used == target {
        return Ok(parent_base_fee.max(MIN_BASE_FEE));
    }

    let fee = if used > target {
        // Over target — increase.
        let excess = used - target;
        // delta = parent_base * excess / target / denom, minimum 1 wei of change
        let delta_num = parent_base_fee.saturating_mul(excess);
        let delta = (delta_num / target / BASE_FEE_MAX_CHANGE_DENOM).max(1);
        parent_base_fee.saturating_add(delta)
    } else {
        // Under target — decrease.
        let deficit = target - used;
        let delta_num = parent_base_fee.saturating_mul(deficit);
        // Note: no `.max(1)` on decrease — small deficits should produce
        // small (possibly zero) changes, matching Ethereum semantics.
        let delta = delta_num / target / BASE_FEE_MAX_CHANGE_DENOM;
        parent_base_fee.saturating_sub(delta)
    };

    Ok(fee.max(MIN_BASE_FEE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_at_target() {
        let fee = next_base_fee(1_000_000_000, 15_000_000, 15_000_000).unwrap();
        assert_eq!(fee, 1_000_000_000);
    }

    #[test]
    fn increases_over_target() {
        // 2x target → max change → +12.5%
        let fee = next_base_fee(1_000_000_000, 30_000_000, 15_000_000).unwrap();
        assert_eq!(fee, 1_125_000_000);
    }

    #[test]
    fn decreases_under_target() {
        // Half target → -6.25% (half of max change).
        let fee = next_base_fee(1_000_000_000, 7_500_000, 15_000_000).unwrap();
        assert_eq!(fee, 937_500_000);
    }

    #[test]
    fn zero_usage_hits_floor_ratio() {
        // No gas used → max decrease → -12.5%.
        let fee = next_base_fee(1_000_000_000, 0, 15_000_000).unwrap();
        assert_eq!(fee, 875_000_000);
    }

    #[test]
    fn clamps_to_min_on_sustained_underuse() {
        // If somehow the fee drops near MIN, stays at MIN.
        let fee = next_base_fee(MIN_BASE_FEE, 0, 15_000_000).unwrap();
        assert_eq!(fee, MIN_BASE_FEE);
    }

    #[test]
    fn small_excess_still_moves_by_at_least_one() {
        // 1 gas over target should produce +1 delta (rounding floor),
        // never stay identical.
        let fee = next_base_fee(1_000_000_000, 15_000_001, 15_000_000).unwrap();
        assert!(fee > 1_000_000_000);
    }

    #[test]
    fn rejects_zero_target() {
        let err = next_base_fee(1_000_000_000, 0, 0).unwrap_err();
        assert!(matches!(err, ChainError::FeeMarket(_)));
    }

    #[test]
    fn deterministic_under_repeat() {
        let a = next_base_fee(1_000_000_000, 20_000_000, 15_000_000).unwrap();
        let b = next_base_fee(1_000_000_000, 20_000_000, 15_000_000).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn golden_vector_plus_25_percent_usage() {
        // +25% over target → (25/100) × 12.5% ≈ 3.125% increase
        // 1_000_000_000 × 1.03125 = 1_031_250_000
        let fee = next_base_fee(1_000_000_000, 18_750_000, 15_000_000).unwrap();
        assert_eq!(fee, 1_031_250_000);
    }

    #[test]
    fn golden_vector_minus_50_percent_usage() {
        // -50% under target → 50% × 12.5% = 6.25% decrease
        // 1_000_000_000 × 0.9375 = 937_500_000
        let fee = next_base_fee(1_000_000_000, 7_500_000, 15_000_000).unwrap();
        assert_eq!(fee, 937_500_000);
    }
}
