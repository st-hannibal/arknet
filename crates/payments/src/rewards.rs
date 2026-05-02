//! Reward computation and distribution.
//!
//! §4 + §8 of TOKENOMICS: every settled receipt mints a block reward
//! computed from the output token count, model characteristics, and
//! performance bonuses. The total reward (user payment + block reward)
//! is split 75/7/5/5/3/5 across six recipients.
//!
//! All arithmetic is integer-only (`u128` ark_atom). Multipliers are
//! represented as `(numerator, 10_000)` fixed-point so governance can
//! tune them without floating-point non-determinism.

use arknet_common::types::{Address, Amount};

/// Reward split percentages (sum = 100).
pub const COMPUTE_PERCENT: u128 = 80;
/// Verifier cut.
pub const VERIFIER_PERCENT: u128 = 7;
/// Treasury cut.
pub const TREASURY_PERCENT: u128 = 5;
/// Burned (deflationary).
pub const BURN_PERCENT: u128 = 3;
/// Delegator cut (pro-rata by delegated stake on compute node).
pub const DELEGATOR_PERCENT: u128 = 5;

/// Model size multiplier (×10_000 fixed-point).
///
/// `size_mult(7B) = 10_000` (= 1.0×). The curve tracks hardware
/// cost within a ~2× band: a 70B model is ~12× more expensive to
/// serve than a 7B (VRAM, power, opportunity cost), and the
/// multiplier gives 4×. The gap is covered by user payments, which
/// scale with output quality — users pay more for larger models.
/// The multiplier ensures block rewards don't penalize operators
/// who invest in expensive hardware.
pub fn size_mult(param_billions: u64) -> u64 {
    match param_billions {
        0..=7 => 10_000,   // 1.0×
        8..=13 => 15_000,  // 1.5×
        14..=30 => 25_000, // 2.5×
        31..=70 => 40_000, // 4.0×
        _ => 100_000,      // 10.0× (400B+)
    }
}

/// Model category multiplier (×10_000 fixed-point).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ModelCategory {
    /// Standard text generation.
    Text = 0,
    /// Embedding models.
    Embedding = 1,
    /// Image generation.
    Image = 2,
    /// Audio models.
    Audio = 3,
    /// Structured output (JSON mode, function calling).
    Structured = 4,
    /// Multimodal (vision + text).
    Multimodal = 5,
}

impl ModelCategory {
    /// Multiplier in ×10_000 fixed-point.
    pub fn mult(self) -> u64 {
        match self {
            ModelCategory::Text => 10_000,       // 1.0×
            ModelCategory::Embedding => 3_000,   // 0.3×
            ModelCategory::Image => 20_000,      // 2.0×
            ModelCategory::Audio => 15_000,      // 1.5×
            ModelCategory::Structured => 12_000, // 1.2×
            ModelCategory::Multimodal => 30_000, // 3.0×
        }
    }
}

/// Latency bonus (×10_000 fixed-point). Up to 1.20× for beating the
/// expected TTFT.
pub fn latency_mult(actual_ms: u64, expected_ms: u64) -> u64 {
    if expected_ms == 0 || actual_ms >= expected_ms {
        return 10_000; // 1.0×
    }
    let improvement = 10_000 - (actual_ms * 10_000 / expected_ms);
    let bonus = improvement * 2_000 / 10_000; // max 20% bonus
    10_000 + bonus
}

/// Uptime bonus (×10_000 fixed-point). 1.10× if >95% over 30 days.
pub fn uptime_mult(uptime_bps: u64) -> u64 {
    if uptime_bps > 9_500 {
        11_000 // 1.10×
    } else {
        10_000 // 1.0×
    }
}

/// Compute the block reward for a single job.
///
/// `emission_per_token` is the per-token rate from the current epoch
/// (see [`crate::emission::per_token_rate`]).
pub fn compute_block_reward(
    output_tokens: u32,
    emission_per_token: Amount,
    category: ModelCategory,
    param_billions: u64,
    latency_ms: u64,
    expected_latency_ms: u64,
    uptime_bps: u64,
) -> Amount {
    let base = (output_tokens as u128).saturating_mul(emission_per_token);
    let sm = size_mult(param_billions) as u128;
    let cm = category.mult() as u128;
    let lm = latency_mult(latency_ms, expected_latency_ms) as u128;
    let um = uptime_mult(uptime_bps) as u128;
    // Each multiplier is ×10_000; chain four divisions to avoid
    // overflow on the intermediate product.
    base.saturating_mul(sm) / 10_000 * cm / 10_000 * lm / 10_000 * um / 10_000
}

/// The five-way reward distribution (router removed in v1.0.8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewardDistribution {
    /// Amount to the compute node operator.
    pub compute: Amount,
    /// Amount to the verifier that checked the receipt.
    pub verifier: Amount,
    /// Amount to the protocol treasury.
    pub treasury: Amount,
    /// Amount burned (dropped from supply).
    pub burned: Amount,
    /// Amount to delegators (pro-rata by stake on compute node).
    pub delegators: Amount,
}

impl RewardDistribution {
    /// Total across all six recipients.
    pub fn total(&self) -> Amount {
        self.compute + self.verifier + self.treasury + self.burned + self.delegators
    }
}

/// Split `total_value` (user payment + block reward) into five
/// recipient buckets. Remainder from rounding goes to treasury.
pub fn distribute_reward(total_value: Amount) -> RewardDistribution {
    let compute = total_value * COMPUTE_PERCENT / 100;
    let verifier = total_value * VERIFIER_PERCENT / 100;
    let burned = total_value * BURN_PERCENT / 100;
    let delegators = total_value * DELEGATOR_PERCENT / 100;
    let treasury = total_value - compute - verifier - burned - delegators;

    RewardDistribution {
        compute,
        verifier,
        treasury,
        burned,
        delegators,
    }
}

/// Credit the reward distribution to the given addresses via the
/// block context. `burned` is dropped (not credited to anyone).
///
/// Callers pass the concrete addresses for each role; delegation
/// pro-rata is the caller's responsibility (iterate delegators of
/// the compute node and split `dist.delegators` by share).
pub fn credit_rewards(
    ctx: &mut arknet_chain::BlockCtx<'_>,
    dist: &RewardDistribution,
    compute_addr: &Address,
    verifier_addr: &Address,
    treasury_addr: &Address,
) -> std::result::Result<(), arknet_chain::ChainError> {
    for (addr, amount) in [
        (compute_addr, dist.compute),
        (verifier_addr, dist.verifier),
        (treasury_addr, dist.treasury),
    ] {
        if amount > 0 {
            let mut acct = ctx.get_account(addr)?.unwrap_or_default();
            acct.balance = acct.balance.saturating_add(amount);
            ctx.set_account(addr, &acct)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_sums_to_100() {
        assert_eq!(
            COMPUTE_PERCENT
                + VERIFIER_PERCENT
                + TREASURY_PERCENT
                + BURN_PERCENT
                + DELEGATOR_PERCENT,
            100
        );
    }

    #[test]
    fn distribute_sums_to_total() {
        for total in [0, 1, 100, 999_999, 1_000_000_000_000u128] {
            let d = distribute_reward(total);
            assert_eq!(d.total(), total, "failed for total={total}");
        }
    }

    #[test]
    fn compute_gets_80_percent() {
        let d = distribute_reward(1_000_000);
        assert_eq!(d.compute, 800_000);
    }

    #[test]
    fn burned_is_3_percent() {
        let d = distribute_reward(1_000_000);
        assert_eq!(d.burned, 30_000);
    }

    #[test]
    fn size_mult_7b() {
        assert_eq!(size_mult(7), 10_000);
    }

    #[test]
    fn size_mult_70b() {
        assert_eq!(size_mult(70), 40_000);
    }

    #[test]
    fn size_mult_400b_plus() {
        assert_eq!(size_mult(400), 100_000);
    }

    #[test]
    fn latency_bonus_at_expected() {
        assert_eq!(latency_mult(100, 100), 10_000);
    }

    #[test]
    fn latency_bonus_faster_than_expected() {
        let m = latency_mult(50, 100);
        assert!(m > 10_000, "should get bonus for beating TTFT");
        assert!(m <= 12_000, "bonus capped at 20%");
    }

    #[test]
    fn latency_no_bonus_when_slower() {
        assert_eq!(latency_mult(200, 100), 10_000);
    }

    #[test]
    fn uptime_bonus_above_95() {
        assert_eq!(uptime_mult(9_600), 11_000);
    }

    #[test]
    fn uptime_no_bonus_below_95() {
        assert_eq!(uptime_mult(9_400), 10_000);
    }

    #[test]
    fn block_reward_nonzero_for_typical_job() {
        let reward = compute_block_reward(
            500,       // output tokens
            1_000_000, // 0.001 ARK per token
            ModelCategory::Text,
            70,    // 70B model
            80,    // 80ms actual
            100,   // 100ms expected
            9_600, // 96% uptime
        );
        assert!(reward > 0);
        // 500 * 1M = 500M base, ×4.0 size, ×1.0 category, ~1.04 latency, ×1.1 uptime
        // ≈ 500M * 4.0 * 1.04 * 1.1 ≈ 2.288B
    }

    #[test]
    fn block_reward_zero_for_zero_tokens() {
        let reward = compute_block_reward(0, 1_000_000, ModelCategory::Text, 7, 100, 100, 9_600);
        assert_eq!(reward, 0);
    }

    #[test]
    fn category_embedding_is_30_percent() {
        assert_eq!(ModelCategory::Embedding.mult(), 3_000);
    }
}
