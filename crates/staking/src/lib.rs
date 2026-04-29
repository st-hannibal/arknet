//! Staking: deposits, delegation, unbonding, slashing.
//!
//! # Phase 1 Week 9 scope
//!
//! - [`min_stake`] — §9.1 formulas with bootstrap-epoch override.
//! - [`slashing`] — evidence handlers for all §10 offenses with
//!   pro-rata delegator impact.
//! - [`validator_set`] — epoch-boundary recomputation (DPoS rank
//!   post-bootstrap; static genesis set during bootstrap).
//!
//! The stake lifecycle dispatcher lives in `arknet_chain::stake_apply`
//! — keeping it in the chain crate avoids a chain ↔ staking
//! dependency cycle (chain defines `BlockCtx`, `RejectReason`,
//! `TxOutcome`; staking would have to import those to build ops, and
//! chain would have to import staking to dispatch them). The handlers
//! here are read-mostly helpers that augment what `stake_apply` does
//! rather than replace it.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod errors;
pub mod min_stake;
pub mod slashing;
pub mod validator_set;

pub use errors::{Result, StakingError};
pub use min_stake::{
    base_ark, min_stake, quant_factor_x10, size_mult_x10, ModelSize, Quantization,
    BASE_COMPUTE_ARK, BASE_ROUTER_ARK, BASE_VALIDATOR_ARK, BASE_VERIFIER_ARK,
};
pub use slashing::{
    apply_slash, Offense, SlashReport, BURN_PERCENT, REPORTER_PERCENT, TREASURY_PERCENT,
};
pub use validator_set::{rank_candidates, recompute_validator_set, MAX_ACTIVE_VALIDATORS};
