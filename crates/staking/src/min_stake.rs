//! `min_stake(role, pool, height)` — §9.1 + §9.4 math.
//!
//! Pure function over `(role, model size, quantization, height,
//! active_validator_count)`. The caller (apply layer + validator-set
//! ranker) looks up the model metadata from the registry and passes
//! it in.
//!
//! # Bootstrap override
//!
//! During the bootstrap epoch every role's minimum is 0 (§9.4). The
//! chain relaxes stake *requirements* only; slashing rules stay
//! active, so a validator that misbehaves in week 1 still loses
//! their stake (even when that stake is zero — the real penalty is
//! losing the permanent genesis-validator slot).

use arknet_chain::bootstrap::in_bootstrap_epoch;
use arknet_chain::transactions::StakeRole;
use arknet_common::types::{Amount, Height, ATOMS_PER_ARK};

/// §9.1 base stake per role, in **whole ARK** (not atoms).
pub const BASE_VALIDATOR_ARK: u128 = 50_000;
/// §9.1 base stake per role, in **whole ARK**.
pub const BASE_ROUTER_ARK: u128 = 8_000;
/// §9.1 base stake per role, in **whole ARK**.
pub const BASE_VERIFIER_ARK: u128 = 10_000;
/// §9.1 base stake per role, in **whole ARK**.
pub const BASE_COMPUTE_ARK: u128 = 5_000;

/// Model parameter-count bucket. Drives `size_mult`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelSize {
    /// ≤ 10B parameters.
    Small7B,
    /// 10B–30B.
    Mid13B,
    /// 30B–150B.
    Large70B,
    /// > 150B.
    Frontier400B,
}

/// Quantization bucket. Drives `quant_factor`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quantization {
    /// Full-precision float.
    Fp32,
    /// Half-precision float (fp16 or bf16).
    Fp16,
    /// 8-bit integer.
    Q8,
    /// 6-bit integer.
    Q6,
    /// 5-bit integer.
    Q5,
    /// 4-bit integer.
    Q4,
    /// 3-bit integer.
    Q3,
    /// 2-bit integer.
    Q2,
}

/// `size_mult` per §9.1. Scaled by 10 for integer math.
pub const fn size_mult_x10(size: ModelSize) -> u128 {
    match size {
        ModelSize::Small7B => 10,       // 1.0
        ModelSize::Mid13B => 20,        // 2.0
        ModelSize::Large70B => 50,      // 5.0
        ModelSize::Frontier400B => 200, // 20.0
    }
}

/// `quant_factor` per §9.1. Scaled by 10 for integer math.
pub const fn quant_factor_x10(q: Quantization) -> u128 {
    match q {
        Quantization::Fp32 => 20, // 2.0
        Quantization::Fp16 => 15, // 1.5 — covers bf16 too
        Quantization::Q8 => 10,   // 1.0
        Quantization::Q6 => 8,    // 0.8
        Quantization::Q5 => 7,    // 0.7
        Quantization::Q4 => 5,    // 0.5
        Quantization::Q3 => 4,    // 0.4
        Quantization::Q2 => 3,    // 0.3
    }
}

/// `BASE[role]` in ARK. Validator/Router/Verifier ignore the size+quant
/// multipliers; Compute applies both.
pub const fn base_ark(role: StakeRole) -> u128 {
    match role {
        StakeRole::Validator => BASE_VALIDATOR_ARK,
        StakeRole::Router => BASE_ROUTER_ARK,
        StakeRole::Verifier => BASE_VERIFIER_ARK,
        StakeRole::Compute => BASE_COMPUTE_ARK,
    }
}

/// §9.1 minimum stake expressed in `ark_atom`. `size` + `quant` only
/// apply to `StakeRole::Compute`; the other roles ignore them. During
/// the bootstrap epoch (§9.4) this returns 0.
pub fn min_stake(
    role: StakeRole,
    size: ModelSize,
    quant: Quantization,
    height: Height,
    active_validator_count: u32,
) -> Amount {
    if in_bootstrap_epoch(height, active_validator_count) {
        return 0;
    }
    let base = base_ark(role);
    let ark_required = match role {
        StakeRole::Compute => base * size_mult_x10(size) * quant_factor_x10(quant) / 100,
        _ => base,
    };
    ark_required * ATOMS_PER_ARK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_zeros_all_roles() {
        for role in [
            StakeRole::Validator,
            StakeRole::Router,
            StakeRole::Verifier,
            StakeRole::Compute,
        ] {
            assert_eq!(
                min_stake(role, ModelSize::Small7B, Quantization::Fp16, 10, 4),
                0,
                "{role:?} not zero in bootstrap"
            );
        }
    }

    #[test]
    fn validator_flat_post_bootstrap() {
        // After bootstrap (duration closed, validator count large),
        // validator stake is BASE × ATOMS_PER_ARK regardless of size/quant.
        let h = arknet_chain::bootstrap::BOOTSTRAP_MAX_BLOCKS + 1;
        let got = min_stake(
            StakeRole::Validator,
            ModelSize::Frontier400B,
            Quantization::Fp32,
            h,
            100,
        );
        assert_eq!(got, 50_000 * ATOMS_PER_ARK);
    }

    #[test]
    fn compute_70b_fp16_example() {
        // Spec example: 70B FP16 compute = 5,000 × 5.0 × 1.5 = 37,500 ARK
        let h = arknet_chain::bootstrap::BOOTSTRAP_MAX_BLOCKS + 1;
        let got = min_stake(
            StakeRole::Compute,
            ModelSize::Large70B,
            Quantization::Fp16,
            h,
            100,
        );
        assert_eq!(got, 37_500 * ATOMS_PER_ARK);
    }

    #[test]
    fn compute_7b_q4_example() {
        // Spec example: 7B Q4 compute = 5,000 × 1.0 × 0.5 = 2,500 ARK
        let h = arknet_chain::bootstrap::BOOTSTRAP_MAX_BLOCKS + 1;
        let got = min_stake(
            StakeRole::Compute,
            ModelSize::Small7B,
            Quantization::Q4,
            h,
            100,
        );
        assert_eq!(got, 2_500 * ATOMS_PER_ARK);
    }

    #[test]
    fn validator_zero_during_bootstrap_min_after() {
        let before = min_stake(
            StakeRole::Validator,
            ModelSize::Small7B,
            Quantization::Fp16,
            arknet_chain::bootstrap::BOOTSTRAP_MAX_BLOCKS - 1,
            5,
        );
        let after = min_stake(
            StakeRole::Validator,
            ModelSize::Small7B,
            Quantization::Fp16,
            arknet_chain::bootstrap::BOOTSTRAP_MAX_BLOCKS,
            5,
        );
        assert_eq!(before, 0);
        assert_eq!(after, 50_000 * ATOMS_PER_ARK);
    }
}
