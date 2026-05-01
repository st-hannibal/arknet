//! Proposal lifecycle: submit → discuss → vote → tally → execute.
//!
//! §13 of PROTOCOL_SPEC: 14-day cycle (7d discussion + 7d voting).
//! Emergency proposals use a 1h vote with no discussion phase.
//!
//! # State representation
//!
//! Proposals live in `CF_PROPOSALS` (key: `proposal_id` as u64 BE,
//! value: borsh `ProposalRecord`). Votes in `CF_VOTES` (key:
//! `proposal_id(8) || voter_addr(20)`, value: borsh `VoteChoice`).

use arknet_chain::transactions::{Proposal, VoteChoice};
use arknet_common::types::{Address, Amount, Height, Timestamp};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// Minimum deposit to submit a proposal (10,000 ARK in ark_atom).
pub const PROPOSAL_DEPOSIT: Amount = 10_000 * 1_000_000_000;

/// Gas for submitting a proposal.
pub const GOV_PROPOSAL_GAS: u64 = 500_000;

/// Gas for casting a vote.
pub const GOV_VOTE_GAS: u64 = 30_000;

/// Quorum: percentage of bonded stake that must vote (>50%).
pub const QUORUM_BPS: u64 = 5_000;

/// Approval threshold: percentage of non-abstain votes that must be
/// Yes (>66%).
pub const APPROVAL_THRESHOLD_BPS: u64 = 6_600;

/// Veto threshold: if NoWithVeto exceeds this percentage of total
/// votes, the deposit is burned (>33%).
pub const VETO_THRESHOLD_BPS: u64 = 3_300;

/// Lifecycle phase of a proposal.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub enum ProposalPhase {
    /// Discussion period — no voting allowed yet.
    Discussion,
    /// Voting period — votes accepted.
    Voting,
    /// Tally complete — proposal passed.
    Passed,
    /// Tally complete — proposal rejected.
    Rejected,
    /// Tally complete — rejected with veto (deposit burned).
    RejectedWithVeto,
    /// Passed + activation height reached — changes applied.
    Executed,
}

/// On-chain proposal record (extends the tx-level `Proposal` with
/// lifecycle state).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct ProposalRecord {
    /// The original proposal body from the transaction.
    pub proposal: Proposal,
    /// Current lifecycle phase.
    pub phase: ProposalPhase,
    /// Block height at which the proposal was submitted.
    pub submitted_at: Height,
    /// Tally snapshot (populated after voting ends).
    pub tally: Option<Tally>,
}

/// Vote tally.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub struct Tally {
    /// Total stake-weighted Yes votes.
    pub yes: Amount,
    /// Total stake-weighted No votes.
    pub no: Amount,
    /// Total stake-weighted Abstain votes.
    pub abstain: Amount,
    /// Total stake-weighted NoWithVeto votes.
    pub no_with_veto: Amount,
}

impl Tally {
    /// Total votes cast (all four buckets).
    pub fn total(&self) -> Amount {
        self.yes
            .saturating_add(self.no)
            .saturating_add(self.abstain)
            .saturating_add(self.no_with_veto)
    }

    /// Non-abstain total (yes + no + veto).
    pub fn non_abstain(&self) -> Amount {
        self.yes
            .saturating_add(self.no)
            .saturating_add(self.no_with_veto)
    }

    /// Record one vote weighted by `stake`.
    pub fn add_vote(&mut self, choice: VoteChoice, stake: Amount) {
        match choice {
            VoteChoice::Yes => self.yes = self.yes.saturating_add(stake),
            VoteChoice::No => self.no = self.no.saturating_add(stake),
            VoteChoice::Abstain => self.abstain = self.abstain.saturating_add(stake),
            VoteChoice::NoWithVeto => self.no_with_veto = self.no_with_veto.saturating_add(stake),
        }
    }

    /// Evaluate the tally against quorum + thresholds.
    ///
    /// `total_bonded` is the total bonded stake at the voting
    /// snapshot height.
    pub fn evaluate(&self, total_bonded: Amount) -> TallyOutcome {
        if total_bonded == 0 || self.total() == 0 {
            return TallyOutcome::Rejected;
        }

        let quorum_met = self.total() * 10_000 / total_bonded >= QUORUM_BPS as u128;
        if !quorum_met {
            return TallyOutcome::Rejected;
        }

        let veto_pct = if self.total() > 0 {
            self.no_with_veto * 10_000 / self.total()
        } else {
            0
        };
        if veto_pct >= VETO_THRESHOLD_BPS as u128 {
            return TallyOutcome::RejectedWithVeto;
        }

        let non_abstain = self.non_abstain();
        if non_abstain == 0 {
            return TallyOutcome::Rejected;
        }
        let approval_pct = self.yes * 10_000 / non_abstain;
        if approval_pct >= APPROVAL_THRESHOLD_BPS as u128 {
            TallyOutcome::Passed
        } else {
            TallyOutcome::Rejected
        }
    }
}

/// Result of a tally evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TallyOutcome {
    /// Quorum met + approval ≥ 66%.
    Passed,
    /// Quorum not met, or approval < 66%.
    Rejected,
    /// NoWithVeto exceeded 33% — deposit burned.
    RejectedWithVeto,
}

/// Determine the current phase based on timestamps.
///
/// `now_ms` is the block timestamp. The proposal's `discussion_ends`
/// and `voting_ends` drive the transition.
pub fn phase_for_time(proposal: &Proposal, now_ms: Timestamp) -> ProposalPhase {
    if now_ms < proposal.discussion_ends {
        ProposalPhase::Discussion
    } else if now_ms < proposal.voting_ends {
        ProposalPhase::Voting
    } else {
        // Past voting_ends — needs tally. Caller resolves final phase.
        ProposalPhase::Voting
    }
}

/// Vote key for `CF_VOTES`: `proposal_id(8 BE) || voter(20)`.
pub fn vote_key(proposal_id: u64, voter: &Address) -> Vec<u8> {
    let mut k = Vec::with_capacity(28);
    k.extend_from_slice(&proposal_id.to_be_bytes());
    k.extend_from_slice(voter.as_bytes());
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tally(yes: u128, no: u128, abstain: u128, veto: u128) -> Tally {
        Tally {
            yes,
            no,
            abstain,
            no_with_veto: veto,
        }
    }

    #[test]
    fn quorum_not_met_rejects() {
        // 1% of 1M bonded voted → below 50% quorum.
        let t = tally(10_000, 0, 0, 0);
        assert_eq!(t.evaluate(1_000_000), TallyOutcome::Rejected);
    }

    #[test]
    fn approval_above_66_passes() {
        // 70% yes of 600k non-abstain, quorum met.
        let t = tally(700_000, 300_000, 0, 0);
        assert_eq!(t.evaluate(1_000_000), TallyOutcome::Passed);
    }

    #[test]
    fn approval_below_66_rejects() {
        // 60% yes → below threshold.
        let t = tally(600_000, 400_000, 0, 0);
        assert_eq!(t.evaluate(1_000_000), TallyOutcome::Rejected);
    }

    #[test]
    fn veto_above_33_burns_deposit() {
        // 40% NoWithVeto → RejectedWithVeto.
        let t = tally(100_000, 100_000, 0, 400_000);
        assert_eq!(t.evaluate(1_000_000), TallyOutcome::RejectedWithVeto);
    }

    #[test]
    fn abstain_counted_toward_quorum_not_approval() {
        // 600k abstain + 300k yes + 100k no = 1M total (quorum met).
        // Non-abstain = 400k; yes = 300k = 75% → passes.
        let t = tally(300_000, 100_000, 600_000, 0);
        assert_eq!(t.evaluate(1_000_000), TallyOutcome::Passed);
    }

    #[test]
    fn zero_bonded_rejects() {
        let t = tally(100, 0, 0, 0);
        assert_eq!(t.evaluate(0), TallyOutcome::Rejected);
    }

    #[test]
    fn add_vote_accumulates() {
        let mut t = Tally::default();
        t.add_vote(VoteChoice::Yes, 100);
        t.add_vote(VoteChoice::Yes, 200);
        t.add_vote(VoteChoice::No, 50);
        assert_eq!(t.yes, 300);
        assert_eq!(t.no, 50);
    }

    #[test]
    fn vote_key_deterministic() {
        let a = vote_key(42, &Address::new([1; 20]));
        let b = vote_key(42, &Address::new([1; 20]));
        assert_eq!(a, b);
        let c = vote_key(42, &Address::new([2; 20]));
        assert_ne!(a, c);
    }

    #[test]
    fn deposit_constant() {
        assert_eq!(PROPOSAL_DEPOSIT, 10_000_000_000_000);
    }
}
