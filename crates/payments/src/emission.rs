//! Emission schedule — demand-scaled per-epoch minting budget.
//!
//! §3 of TOKENOMICS: 1B ARK hard cap, halving every 4 years after
//! Year 4. Emission is demand-scaled: if no verified jobs occur in an
//! epoch, the budget rolls forward (never burned).
//!
//! # Per-epoch budget
//!
//! ```text
//! annual_emission(year) = schedule lookup (see ANNUAL_EMISSION)
//! epochs_per_year       = 365 * 24 * 3600 / EPOCH_LENGTH_BLOCKS
//!                       ≈ 8766 epochs at 3600 blocks/epoch
//! epoch_budget          = annual_emission / epochs_per_year
//! ```
//!
//! # Per-job allocation
//!
//! When a receipt is settled, the reward minter draws from the
//! current epoch's budget proportionally:
//!
//! ```text
//! job_reward = epoch_budget * (job_output_tokens / epoch_total_tokens)
//! ```
//!
//! If `epoch_total_tokens` is unknown at settlement time (it's
//! accumulated as receipts land), the minter pre-allocates per-job
//! using the flat per-token emission rate and clamps to the epoch
//! budget at the boundary.
//!
//! # Rollover
//!
//! Unspent budget at epoch end carries to the next epoch's pool.
//! This prevents early empty epochs from permanently destroying
//! supply — important for the solo-launch cold start.

use arknet_common::types::{Amount, Height};

/// Hard supply cap — 1 billion ARK in ark_atom (9 decimals).
pub const TOTAL_SUPPLY_CAP: Amount = 1_000_000_000 * ATOMS_PER_ARK;

/// 1 ARK = 10^9 ark_atom.
pub const ATOMS_PER_ARK: Amount = 1_000_000_000;

/// Epoch length in blocks (matches `arknet_chain::EPOCH_LENGTH_BLOCKS`).
pub const EPOCH_LENGTH: Height = 3_600;

/// Seconds per block (1s target).
pub const SECS_PER_BLOCK: u64 = 1;

/// Epochs per year (approximate).
pub const EPOCHS_PER_YEAR: u64 = 365 * 24 * 3600 / EPOCH_LENGTH;

/// Annual emission schedule in whole ARK. Index = year (0-based).
/// After the table ends, emission follows the geometric tail:
/// `prev * 0.5` (halving) every 4 years.
const ANNUAL_EMISSION_ARK: &[Amount] = &[
    120_000_000, // Year 0 (launch year)
    105_000_000, // Year 1
    90_000_000,  // Year 2
    80_000_000,  // Year 3
    50_000_000,  // Year 4 (first halving)
    50_000_000,  // Year 5
    50_000_000,  // Year 6
    50_000_000,  // Year 7
    25_000_000,  // Year 8 (second halving)
    25_000_000,  // Year 9
    25_000_000,  // Year 10
    25_000_000,  // Year 11
    12_500_000,  // Year 12 (third halving)
    12_500_000,  // Year 13
    12_500_000,  // Year 14
    12_500_000,  // Year 15
];

/// Look up annual emission for the given year (0-based from genesis).
/// Past the table, halves every 4 years from the last table entry.
pub fn annual_emission_ark(year: u64) -> Amount {
    let table_len = ANNUAL_EMISSION_ARK.len() as u64;
    if year < table_len {
        return ANNUAL_EMISSION_ARK[year as usize];
    }
    let last = *ANNUAL_EMISSION_ARK.last().expect("table is non-empty");
    let extra_years = year - table_len;
    // First halving happens at +4 years past end of table.
    let extra_halvings = (extra_years + 4) / 4;
    last >> extra_halvings
}

/// Annual emission in ark_atom.
pub fn annual_emission(year: u64) -> Amount {
    annual_emission_ark(year).saturating_mul(ATOMS_PER_ARK)
}

/// Per-epoch emission budget in ark_atom for the given year.
pub fn epoch_budget(year: u64) -> Amount {
    annual_emission(year) / EPOCHS_PER_YEAR as u128
}

/// Compute which year (0-based) a block height falls in.
pub fn year_for_height(height: Height) -> u64 {
    let blocks_per_year = 365 * 24 * 3600 / SECS_PER_BLOCK;
    height / blocks_per_year
}

/// Compute which epoch (0-based) a block height falls in.
pub fn epoch_for_height(height: Height) -> u64 {
    height / EPOCH_LENGTH
}

/// Epoch emission state. Tracks minted-so-far + rollover from prior
/// epochs. Stored in `CF_META` under key `emission_state`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpochEmissionState {
    /// Current epoch number.
    pub epoch: u64,
    /// Budget available this epoch (base + rollover).
    pub budget: Amount,
    /// Amount already minted this epoch.
    pub minted: Amount,
    /// Cumulative total minted across all epochs (monotonic).
    pub total_minted: Amount,
}

impl EpochEmissionState {
    /// Initialize at genesis.
    pub fn genesis() -> Self {
        Self {
            epoch: 0,
            budget: epoch_budget(0),
            minted: 0,
            total_minted: 0,
        }
    }

    /// Remaining mintable this epoch.
    pub fn remaining(&self) -> Amount {
        self.budget.saturating_sub(self.minted)
    }

    /// Try to mint `amount` from the current epoch's budget. Returns
    /// the actual amount minted (may be less if budget is exhausted
    /// or supply cap is reached).
    pub fn try_mint(&mut self, amount: Amount) -> Amount {
        let cap_remaining = TOTAL_SUPPLY_CAP.saturating_sub(self.total_minted);
        let budget_remaining = self.remaining();
        let actual = amount.min(budget_remaining).min(cap_remaining);
        self.minted = self.minted.saturating_add(actual);
        self.total_minted = self.total_minted.saturating_add(actual);
        actual
    }

    /// Advance to the next epoch. Unspent budget rolls over.
    pub fn advance_epoch(&mut self, new_height: Height) {
        let new_epoch = epoch_for_height(new_height);
        if new_epoch <= self.epoch {
            return;
        }
        let rollover = self.remaining();
        let year = year_for_height(new_height);
        let fresh = epoch_budget(year);
        self.epoch = new_epoch;
        self.budget = fresh.saturating_add(rollover);
        self.minted = 0;
    }
}

/// Per-token emission rate for the current epoch. Used by the reward
/// calculator to price individual jobs.
///
/// Returns ark_atom per output token.
pub fn per_token_rate(state: &EpochEmissionState, epoch_total_tokens: u64) -> Amount {
    if epoch_total_tokens == 0 {
        return 0;
    }
    state.remaining() / epoch_total_tokens as u128
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supply_cap_is_one_billion() {
        assert_eq!(TOTAL_SUPPLY_CAP, 1_000_000_000_000_000_000);
    }

    #[test]
    fn year_0_emission() {
        assert_eq!(annual_emission_ark(0), 120_000_000);
        assert_eq!(annual_emission(0), 120_000_000 * ATOMS_PER_ARK);
    }

    #[test]
    fn halving_past_table() {
        // Table ends at year 15 (12_500_000). Year 16-19 = first
        // halving past table = 6_250_000.
        assert_eq!(annual_emission_ark(16), 6_250_000);
        assert_eq!(annual_emission_ark(19), 6_250_000);
        // Year 20-23 = second halving past table = 3_125_000.
        assert_eq!(annual_emission_ark(20), 3_125_000);
    }

    #[test]
    fn epoch_budget_year_0() {
        let b = epoch_budget(0);
        let expected = annual_emission(0) / EPOCHS_PER_YEAR as u128;
        assert_eq!(b, expected);
        assert!(b > 0);
    }

    #[test]
    fn genesis_state_has_budget() {
        let s = EpochEmissionState::genesis();
        assert_eq!(s.epoch, 0);
        assert!(s.budget > 0);
        assert_eq!(s.minted, 0);
        assert_eq!(s.total_minted, 0);
    }

    #[test]
    fn try_mint_clamps_to_budget() {
        let mut s = EpochEmissionState::genesis();
        let huge = s.budget + 1;
        let actual = s.try_mint(huge);
        assert_eq!(actual, s.budget - actual + actual); // minted == budget
        assert_eq!(s.remaining(), 0);
    }

    #[test]
    fn try_mint_clamps_to_supply_cap() {
        let mut s = EpochEmissionState::genesis();
        s.total_minted = TOTAL_SUPPLY_CAP - 100;
        s.budget = 1_000_000;
        let actual = s.try_mint(1_000_000);
        assert_eq!(actual, 100);
    }

    #[test]
    fn advance_epoch_rolls_over_unspent() {
        let mut s = EpochEmissionState::genesis();
        let original_budget = s.budget;
        s.try_mint(100);
        let unspent = original_budget - 100;

        s.advance_epoch(EPOCH_LENGTH);
        assert_eq!(s.epoch, 1);
        assert_eq!(s.minted, 0);
        let fresh = epoch_budget(0);
        assert_eq!(s.budget, fresh + unspent);
    }

    #[test]
    fn advance_epoch_noop_if_same_epoch() {
        let mut s = EpochEmissionState::genesis();
        let budget_before = s.budget;
        s.advance_epoch(EPOCH_LENGTH - 1);
        assert_eq!(s.epoch, 0);
        assert_eq!(s.budget, budget_before);
    }

    #[test]
    fn cumulative_emission_stays_below_cap() {
        // The geometric halving tail converges below the 1B cap.
        // The hard cap is enforced by `try_mint` + `TOTAL_SUPPLY_CAP`,
        // not by the emission schedule alone. Verify the schedule
        // never overshoots.
        let mut total: Amount = 0;
        for year in 0..500u64 {
            total = total.saturating_add(annual_emission(year));
        }
        assert!(
            total <= TOTAL_SUPPLY_CAP,
            "cumulative={total} exceeds cap={TOTAL_SUPPLY_CAP}"
        );
        // After 500 years the tail should have converged close to
        // its asymptote (~795M ARK with current table + halvings).
        assert!(total > 700_000_000 * ATOMS_PER_ARK);
    }

    #[test]
    fn per_token_rate_nonzero_when_tokens_exist() {
        let s = EpochEmissionState::genesis();
        let rate = per_token_rate(&s, 1_000_000);
        assert!(rate > 0);
    }

    #[test]
    fn per_token_rate_zero_when_no_tokens() {
        let s = EpochEmissionState::genesis();
        assert_eq!(per_token_rate(&s, 0), 0);
    }
}
