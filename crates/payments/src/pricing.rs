//! Dynamic pricing oracle.
//!
//! §5.1 + TOKENOMICS §14: utilization-based EMA.
//!
//! ```text
//! raw_price = base_price × utilization^1.5
//! smoothed  = 0.7 × current + 0.3 × raw_price
//! ```
//!
//! Updated every epoch boundary (3600 blocks). The oracle reads
//! `jobs_this_epoch / capacity_this_epoch` as the utilization metric.
//!
//! # Integer approximation
//!
//! `utilization^1.5` is approximated as `util * sqrt(util)` where
//! `sqrt` is a 64-bit integer square root. All intermediate values
//! are scaled by `PRICE_SCALE = 10^9` to preserve precision without
//! floating point.

use arknet_common::types::Amount;

/// Scaling factor for price arithmetic (10^9).
pub const PRICE_SCALE: u128 = 1_000_000_000;

/// Genesis base price: 0.00001 ARK per output token (in ark_atom).
/// = 10_000 ark_atom.
pub const GENESIS_BASE_PRICE: Amount = 10_000;

/// Floor price — 0 (free tier exists).
pub const PRICE_FLOOR: Amount = 0;

/// Ceiling price — 0.01 ARK per token = 10_000_000 ark_atom.
/// Governance-adjustable.
pub const PRICE_CEILING: Amount = 10_000_000;

/// EMA weight for current price at stable demand (70%, scaled ×10_000).
pub const EMA_CURRENT_WEIGHT_STABLE: u64 = 7_000;
/// EMA weight for new observation at stable demand.
pub const EMA_NEW_WEIGHT_STABLE: u64 = 3_000;
/// EMA weight at moderate demand change (>20%).
pub const EMA_CURRENT_WEIGHT_MODERATE: u64 = 6_000;
/// EMA weight at moderate demand change.
pub const EMA_NEW_WEIGHT_MODERATE: u64 = 4_000;
/// EMA weight at large demand spike (>50%).
pub const EMA_CURRENT_WEIGHT_FAST: u64 = 5_000;
/// EMA weight at large demand spike.
pub const EMA_NEW_WEIGHT_FAST: u64 = 5_000;

/// Oracle state. Stored in `CF_META` under key `pricing_state`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PricingState {
    /// Current smoothed price (ark_atom per output token).
    pub price: Amount,
    /// Jobs observed in the current epoch (numerator of utilization).
    pub epoch_jobs: u64,
    /// Estimated capacity in the current epoch (denominator). Set
    /// from the active compute node count × expected throughput.
    pub epoch_capacity: u64,
    /// Epoch number this state was last updated at.
    pub epoch: u64,
}

impl PricingState {
    /// Genesis state.
    pub fn genesis() -> Self {
        Self {
            price: GENESIS_BASE_PRICE,
            epoch_jobs: 0,
            epoch_capacity: 1,
            epoch: 0,
        }
    }

    /// Record one more job in the current epoch.
    pub fn record_job(&mut self) {
        self.epoch_jobs = self.epoch_jobs.saturating_add(1);
    }

    /// Compute the utilization ratio as a fraction of `PRICE_SCALE`.
    /// Clamped to `[0, PRICE_SCALE]`.
    pub fn utilization_scaled(&self) -> u128 {
        if self.epoch_capacity == 0 {
            return 0;
        }
        let raw =
            (self.epoch_jobs as u128).saturating_mul(PRICE_SCALE) / (self.epoch_capacity as u128);
        raw.min(PRICE_SCALE)
    }

    /// Advance to a new epoch. Recomputes the smoothed price using
    /// the EMA formula, resets per-epoch counters.
    pub fn advance_epoch(&mut self, new_epoch: u64, new_capacity: u64) {
        if new_epoch <= self.epoch {
            return;
        }
        let util = self.utilization_scaled();
        let raw_price = compute_raw_price(GENESIS_BASE_PRICE, util);
        let (w_current, w_new) = adaptive_ema_weights(self.price, raw_price);
        self.price = ema_smooth_weighted(self.price, raw_price, w_current, w_new);
        self.price = self.price.clamp(PRICE_FLOOR, PRICE_CEILING);
        self.epoch = new_epoch;
        self.epoch_jobs = 0;
        self.epoch_capacity = new_capacity.max(1);
    }
}

/// `base_price × utilization^1.5` using integer sqrt.
///
/// `util_scaled` is in `[0, PRICE_SCALE]` where `PRICE_SCALE` = 1.0.
///
/// Strategy: compute `util^1.5` as `util * sqrt(util)` in a
/// fixed-point space where `S = PRICE_SCALE`:
///
/// ```text
/// u = util_scaled / S          (real utilization in [0, 1])
/// u^1.5 = u * sqrt(u)
/// price = base_price * u^1.5
///       = base_price * (util_scaled / S) * sqrt(util_scaled / S)
///       = base_price * util_scaled * sqrt(util_scaled) / (S * sqrt(S))
/// ```
fn compute_raw_price(base_price: Amount, util_scaled: u128) -> Amount {
    if util_scaled == 0 {
        return 0;
    }
    let sqrt_u = isqrt(util_scaled);
    let sqrt_s = isqrt(PRICE_SCALE);
    // base_price * util_scaled * sqrt_u / (PRICE_SCALE * sqrt_s)
    let numerator = base_price
        .saturating_mul(util_scaled)
        .saturating_mul(sqrt_u);
    let denominator = PRICE_SCALE.saturating_mul(sqrt_s);
    if denominator == 0 {
        return 0;
    }
    numerator / denominator
}

/// Adaptive EMA: weights depend on the magnitude of the price change.
/// Large demand spikes (>50% change) converge in ~2 epochs; stable
/// conditions maintain a smoother signal.
fn adaptive_ema_weights(current: Amount, new: Amount) -> (u64, u64) {
    if current == 0 {
        return (EMA_CURRENT_WEIGHT_FAST, EMA_NEW_WEIGHT_FAST);
    }
    let delta = if new > current {
        new - current
    } else {
        current - new
    };
    let change_pct = delta * 10_000 / current;
    if change_pct > 5_000 {
        (EMA_CURRENT_WEIGHT_FAST, EMA_NEW_WEIGHT_FAST)
    } else if change_pct > 2_000 {
        (EMA_CURRENT_WEIGHT_MODERATE, EMA_NEW_WEIGHT_MODERATE)
    } else {
        (EMA_CURRENT_WEIGHT_STABLE, EMA_NEW_WEIGHT_STABLE)
    }
}

/// Weighted EMA: `w_current × current + w_new × new`, where weights
/// sum to 10_000.
fn ema_smooth_weighted(current: Amount, new: Amount, w_current: u64, w_new: u64) -> Amount {
    let c = current * w_current as u128;
    let n = new * w_new as u128;
    (c + n) / 10_000
}

/// Integer square root (binary search). Returns `floor(sqrt(n))`.
fn isqrt(n: u128) -> u128 {
    if n <= 1 {
        return n;
    }
    let mut lo: u128 = 1;
    let mut hi: u128 = n.min(1 << 64);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if mid <= n / mid {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genesis_state_has_base_price() {
        let s = PricingState::genesis();
        assert_eq!(s.price, GENESIS_BASE_PRICE);
    }

    #[test]
    fn zero_utilization_zero_raw_price() {
        assert_eq!(compute_raw_price(10_000, 0), 0);
    }

    #[test]
    fn full_utilization_near_base_price() {
        // util=1.0 → util^1.5 = 1.0 → price ≈ base_price.
        let price = compute_raw_price(10_000, PRICE_SCALE);
        // Allow ±1% for integer rounding.
        assert!((9_900..=10_100).contains(&price), "got {price}");
    }

    #[test]
    fn half_utilization_lower_than_full() {
        let full = compute_raw_price(10_000, PRICE_SCALE);
        let half = compute_raw_price(10_000, PRICE_SCALE / 2);
        assert!(half < full);
    }

    #[test]
    fn ema_stable_preserves_value() {
        let result = ema_smooth_weighted(1_000, 1_000, 7_000, 3_000);
        assert_eq!(result, 1_000);
    }

    #[test]
    fn ema_stable_moves_toward_new() {
        let result = ema_smooth_weighted(1_000, 2_000, 7_000, 3_000);
        assert_eq!(result, 1_300);
    }

    #[test]
    fn adaptive_weights_fast_on_large_spike() {
        let (wc, wn) = adaptive_ema_weights(1_000, 2_000);
        // 100% change > 50% → fast weights
        assert_eq!(wc, EMA_CURRENT_WEIGHT_FAST);
        assert_eq!(wn, EMA_NEW_WEIGHT_FAST);
    }

    #[test]
    fn adaptive_weights_stable_on_small_change() {
        let (wc, wn) = adaptive_ema_weights(1_000, 1_100);
        // 10% change → stable weights
        assert_eq!(wc, EMA_CURRENT_WEIGHT_STABLE);
        assert_eq!(wn, EMA_NEW_WEIGHT_STABLE);
    }

    #[test]
    fn adaptive_weights_moderate_on_mid_change() {
        let (wc, wn) = adaptive_ema_weights(1_000, 1_400);
        // 40% change → moderate weights
        assert_eq!(wc, EMA_CURRENT_WEIGHT_MODERATE);
        assert_eq!(wn, EMA_NEW_WEIGHT_MODERATE);
    }

    #[test]
    fn advance_epoch_resets_counters() {
        let mut s = PricingState::genesis();
        s.epoch_jobs = 50;
        s.epoch_capacity = 100;
        s.advance_epoch(1, 200);
        assert_eq!(s.epoch, 1);
        assert_eq!(s.epoch_jobs, 0);
        assert_eq!(s.epoch_capacity, 200);
    }

    #[test]
    fn price_clamped_to_ceiling() {
        let mut s = PricingState::genesis();
        s.price = PRICE_CEILING + 1_000;
        s.epoch_jobs = 100;
        s.epoch_capacity = 1;
        s.advance_epoch(1, 1);
        assert!(s.price <= PRICE_CEILING);
    }

    #[test]
    fn record_job_increments() {
        let mut s = PricingState::genesis();
        s.record_job();
        s.record_job();
        assert_eq!(s.epoch_jobs, 2);
    }

    #[test]
    fn isqrt_of_perfect_squares() {
        assert_eq!(isqrt(0), 0);
        assert_eq!(isqrt(1), 1);
        assert_eq!(isqrt(4), 2);
        assert_eq!(isqrt(9), 3);
        assert_eq!(isqrt(100), 10);
        assert_eq!(isqrt(1_000_000), 1_000);
    }

    #[test]
    fn isqrt_of_non_perfect_floors() {
        assert_eq!(isqrt(2), 1);
        assert_eq!(isqrt(8), 2);
        assert_eq!(isqrt(10), 3);
    }
}
