//! L2 circuit breaker.
//!
//! §14 of SECURITY.md: auto-pause new inference jobs when anomalous
//! conditions are detected. The L1 chain keeps producing blocks;
//! only L2 job intake is paused.
//!
//! # Triggers
//!
//! 1. **Verification failure rate >50%** in the current epoch.
//! 2. **>3 validators miss 10 consecutive blocks.**
//! 3. **>20% of bonded supply slashed in one epoch.**
//!
//! # Effect
//!
//! Router intake rejects new inference jobs. Staking, governance,
//! and transfers continue unaffected. The breaker auto-resets after
//! one clean epoch (no trigger fires). A governance emergency
//! proposal (1h vote, no discussion) can force an override.

/// Verification failure threshold post-bootstrap (>50%, ×10_000 BPS).
pub const VERIFICATION_FAILURE_THRESHOLD_BPS: u64 = 5_000;

/// During bootstrap: trip at >20% to compensate for zero economic
/// security (nobody has stake to lose yet).
pub const BOOTSTRAP_VERIFICATION_FAILURE_THRESHOLD_BPS: u64 = 2_000;

/// Maximum consecutive missed blocks before counting a validator.
pub const CONSECUTIVE_MISS_THRESHOLD: u64 = 10;

/// Maximum validators missing before trigger fires.
pub const MISSING_VALIDATORS_THRESHOLD: u64 = 3;

/// Maximum percentage of bonded supply slashed in one epoch (×10_000).
pub const SLASH_THRESHOLD_BPS: u64 = 2_000;

/// Circuit breaker state. Stored in-memory on the node; not
/// persisted to chain state (L2-level concern, not consensus).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CircuitBreakerState {
    /// `true` if the breaker is currently tripped.
    pub tripped: bool,
    /// Epoch at which the breaker was last tripped.
    pub tripped_at_epoch: u64,
    /// Epoch at which the breaker last auto-reset.
    pub reset_at_epoch: u64,
    /// `true` if a governance emergency override is active.
    pub governance_override: bool,
    // Per-epoch accumulators (reset at epoch boundary).
    /// Receipts verified this epoch.
    pub epoch_verified: u64,
    /// Receipts that failed verification this epoch.
    pub epoch_failed: u64,
    /// Total bonded supply at epoch start.
    pub epoch_bonded_supply: u128,
    /// Amount slashed this epoch.
    pub epoch_slashed: u128,
    /// `true` during the bootstrap epoch — uses tighter thresholds.
    pub in_bootstrap: bool,
    /// Guardian halt — founder emergency stop during bootstrap.
    /// Self-destructs when `in_bootstrap` transitions to `false`.
    pub guardian_halted: bool,
}

impl CircuitBreakerState {
    /// Initialize at genesis (not tripped).
    pub fn genesis() -> Self {
        Self {
            tripped: false,
            tripped_at_epoch: 0,
            reset_at_epoch: 0,
            governance_override: false,
            epoch_verified: 0,
            epoch_failed: 0,
            epoch_bonded_supply: 0,
            epoch_slashed: 0,
            in_bootstrap: true,
            guardian_halted: false,
        }
    }

    /// `true` if new inference jobs should be rejected.
    pub fn is_paused(&self) -> bool {
        self.guardian_halted || (self.tripped && !self.governance_override)
    }

    /// Guardian emergency halt (bootstrap only).
    pub fn guardian_halt(&mut self) {
        if self.in_bootstrap {
            self.guardian_halted = true;
            tracing::warn!("guardian halt — L2 intake paused by founder key");
        }
    }

    /// Guardian resume.
    pub fn guardian_resume(&mut self) {
        self.guardian_halted = false;
    }

    /// Transition out of bootstrap. Clears guardian powers.
    pub fn end_bootstrap(&mut self) {
        self.in_bootstrap = false;
        self.guardian_halted = false;
    }

    /// Record a verified receipt.
    pub fn record_verified(&mut self) {
        self.epoch_verified = self.epoch_verified.saturating_add(1);
    }

    /// Record a failed verification.
    pub fn record_failed(&mut self) {
        self.epoch_failed = self.epoch_failed.saturating_add(1);
    }

    /// Record a slash amount.
    pub fn record_slash(&mut self, amount: u128) {
        self.epoch_slashed = self.epoch_slashed.saturating_add(amount);
    }

    /// Evaluate triggers. Call at each epoch boundary.
    pub fn evaluate(&mut self, current_epoch: u64, _missing_validators: u64) {
        let total_receipts = self.epoch_verified.saturating_add(self.epoch_failed);

        let vf_threshold = if self.in_bootstrap {
            BOOTSTRAP_VERIFICATION_FAILURE_THRESHOLD_BPS
        } else {
            VERIFICATION_FAILURE_THRESHOLD_BPS
        };
        let trigger1 =
            total_receipts > 0 && self.epoch_failed * 10_000 / total_receipts > vf_threshold;

        let trigger2 = _missing_validators > MISSING_VALIDATORS_THRESHOLD;

        let trigger3 = self.epoch_bonded_supply > 0
            && self.epoch_slashed * 10_000 / self.epoch_bonded_supply as u64 as u128
                > SLASH_THRESHOLD_BPS as u128;

        if trigger1 || trigger2 || trigger3 {
            self.tripped = true;
            self.tripped_at_epoch = current_epoch;
            tracing::warn!(
                epoch = current_epoch,
                trigger1,
                trigger2,
                trigger3,
                "circuit breaker TRIPPED — L2 intake paused"
            );
        } else if self.tripped && !self.governance_override {
            self.tripped = false;
            self.reset_at_epoch = current_epoch;
            tracing::info!(
                epoch = current_epoch,
                "circuit breaker auto-reset — L2 intake resumed"
            );
        }
    }

    /// Reset per-epoch accumulators. Call at each epoch boundary
    /// before `evaluate`.
    pub fn advance_epoch(&mut self, bonded_supply: u128) {
        self.epoch_verified = 0;
        self.epoch_failed = 0;
        self.epoch_slashed = 0;
        self.epoch_bonded_supply = bonded_supply;
        self.governance_override = false;
    }

    /// Governance emergency override — force the breaker off.
    pub fn governance_force_resume(&mut self) {
        self.governance_override = true;
        tracing::info!("circuit breaker governance override — L2 intake forced open");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genesis_not_tripped() {
        let s = CircuitBreakerState::genesis();
        assert!(!s.is_paused());
        assert!(!s.tripped);
    }

    #[test]
    fn high_failure_rate_trips() {
        let mut s = CircuitBreakerState::genesis();
        s.epoch_bonded_supply = 1_000_000;
        for _ in 0..40 {
            s.record_verified();
        }
        for _ in 0..60 {
            s.record_failed();
        }
        s.evaluate(1, 0);
        assert!(s.is_paused());
        assert!(s.tripped);
    }

    #[test]
    fn low_failure_rate_does_not_trip() {
        let mut s = CircuitBreakerState::genesis();
        s.epoch_bonded_supply = 1_000_000;
        for _ in 0..90 {
            s.record_verified();
        }
        for _ in 0..10 {
            s.record_failed();
        }
        s.evaluate(1, 0);
        assert!(!s.is_paused());
    }

    #[test]
    fn slash_threshold_trips() {
        let mut s = CircuitBreakerState::genesis();
        s.epoch_bonded_supply = 1_000_000;
        s.record_slash(250_000); // 25% > 20%
        s.evaluate(1, 0);
        assert!(s.is_paused());
    }

    #[test]
    fn missing_validators_trips() {
        let mut s = CircuitBreakerState::genesis();
        s.epoch_bonded_supply = 1_000_000;
        s.evaluate(1, 4); // >3 missing
        assert!(s.is_paused());
    }

    #[test]
    fn auto_reset_after_clean_epoch() {
        let mut s = CircuitBreakerState::genesis();
        s.epoch_bonded_supply = 1_000_000;
        // Trip it.
        for _ in 0..60 {
            s.record_failed();
        }
        for _ in 0..40 {
            s.record_verified();
        }
        s.evaluate(1, 0);
        assert!(s.is_paused());

        // Advance to next epoch — clean.
        s.advance_epoch(2_000_000);
        s.record_verified();
        s.evaluate(2, 0);
        assert!(!s.is_paused());
        assert_eq!(s.reset_at_epoch, 2);
    }

    #[test]
    fn governance_override_forces_resume() {
        let mut s = CircuitBreakerState::genesis();
        s.tripped = true;
        s.tripped_at_epoch = 5;
        assert!(s.is_paused());

        s.governance_force_resume();
        assert!(!s.is_paused());
    }
}
