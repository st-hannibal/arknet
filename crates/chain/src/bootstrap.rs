//! Bootstrap-epoch helpers.
//!
//! The fair-launch invariant (no premine, `INITIAL_SUPPLY_ARK = 0`)
//! means nobody can meet the §9.1 stake minimums at genesis. The
//! protocol resolves this with a **bootstrap epoch** during which
//! `min_stake(*, _, height_in_bootstrap) = 0` (§9.4). Slashing rules
//! remain active — this relaxation covers stake *requirements* only.
//!
//! The bootstrap window ends at whichever comes first:
//! - `height ≥ BOOTSTRAP_MAX_BLOCKS` (≈6 months at 1s blocks), or
//! - `active_validator_count ≥ BOOTSTRAP_VALIDATOR_TARGET`.
//!
//! This module owns the duration side (block-count test). The
//! validator-count test sits in the validator-set module, which can
//! read the live count from state.

use arknet_common::types::Height;

/// §16: `BOOTSTRAP_MAX_DURATION_MS / BLOCK_TIME_TARGET_MS`.
/// 6 months × 30 days × 86 400 s × 1 000 / 1 000 ms per block.
pub const BOOTSTRAP_MAX_BLOCKS: Height = 6 * 30 * 86_400;

/// §16: chain exits bootstrap once this many validators are active.
/// Set high to prevent premature exit from one operator running
/// many nodes. The 6-month time floor ensures broad distribution
/// even if 100 validators join quickly.
pub const BOOTSTRAP_VALIDATOR_TARGET: u32 = 100;

/// §16: epoch length (validator-set rebuild cadence).
pub const EPOCH_LENGTH_BLOCKS: Height = 3_600;

/// `true` while the bootstrap-duration gate is still open.
///
/// The *full* bootstrap check is "still bootstrapping UNLESS either
/// gate has tripped". The caller combines this with an active-validator
/// count lookup from state.
pub fn within_bootstrap_window(height: Height) -> bool {
    height < BOOTSTRAP_MAX_BLOCKS
}

/// Convenience: combine both gates. Returns `true` when the chain is
/// still in bootstrap.
pub fn in_bootstrap_epoch(height: Height, active_validator_count: u32) -> bool {
    within_bootstrap_window(height) && active_validator_count < BOOTSTRAP_VALIDATOR_TARGET
}

/// `true` if `height` is the first block of a new epoch (i.e. a
/// validator-set recomputation should fire at this height). Block 0
/// is genesis and does not fire the recomputation — the genesis set
/// lives through its entire first epoch.
pub fn is_epoch_boundary(height: Height) -> bool {
    height != 0 && height % EPOCH_LENGTH_BLOCKS == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_window_edge() {
        assert!(within_bootstrap_window(0));
        assert!(within_bootstrap_window(BOOTSTRAP_MAX_BLOCKS - 1));
        assert!(!within_bootstrap_window(BOOTSTRAP_MAX_BLOCKS));
    }

    #[test]
    fn either_gate_exits_bootstrap() {
        // Duration closed but validator count still short.
        assert!(!in_bootstrap_epoch(BOOTSTRAP_MAX_BLOCKS, 5));
        // Validator target met well before duration expires.
        assert!(!in_bootstrap_epoch(100, BOOTSTRAP_VALIDATOR_TARGET));
        // Still inside both gates.
        assert!(in_bootstrap_epoch(100, 5));
    }

    #[test]
    fn epoch_boundary_fires_at_multiples_but_not_zero() {
        assert!(!is_epoch_boundary(0));
        assert!(is_epoch_boundary(EPOCH_LENGTH_BLOCKS));
        assert!(is_epoch_boundary(EPOCH_LENGTH_BLOCKS * 2));
        assert!(!is_epoch_boundary(EPOCH_LENGTH_BLOCKS + 1));
    }
}
