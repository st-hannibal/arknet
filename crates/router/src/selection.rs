//! Compute-candidate selection.
//!
//! Phase 1 picks by `(total_stake desc, node_id asc)`. That is the
//! simplest ordering that still expresses "more skin in the game wins"
//! and resolves ties deterministically. Once the metrics pipeline
//! lands (Week 12) the ranker gains latency / success-rate inputs and
//! graduates to the full SELECTION_SCORE.
//!
//! The selector returns a *ranked vector* rather than a single pick so
//! [`crate::failover`] has an ordered candidate list to walk without
//! re-running the ranker.

use arknet_common::types::Timestamp;

use crate::candidate::{Candidate, CandidateRegistry};
use crate::errors::{Result, RouterError};

/// Rank every [`Candidate`] in `registry` that serves `model_ref` and
/// is fresh at `now_ms`. The best pick is at index 0.
pub fn rank_for(
    registry: &CandidateRegistry,
    model_ref: &str,
    now_ms: Timestamp,
) -> Vec<Candidate> {
    let mut pool = registry.eligible_for(model_ref, now_ms);
    pool.sort_by(|a, b| {
        b.total_stake
            .cmp(&a.total_stake)
            .then_with(|| a.node_id.0.cmp(&b.node_id.0))
    });
    pool
}

/// Pick the top candidate or return [`RouterError::NoCandidate`].
pub fn pick(registry: &CandidateRegistry, model_ref: &str, now_ms: Timestamp) -> Result<Candidate> {
    let mut ranked = rank_for(registry, model_ref, now_ms);
    if ranked.is_empty() {
        return Err(RouterError::NoCandidate);
    }
    Ok(ranked.remove(0))
}

/// Rank candidates that support TEE. When `prefer_tee = true`, only
/// TEE-capable nodes are considered. Returns [`RouterError::NoTeeCandidate`]
/// via [`pick_tee`] if none qualify — no silent downgrade.
pub fn rank_for_tee(
    registry: &CandidateRegistry,
    model_ref: &str,
    now_ms: Timestamp,
) -> Vec<Candidate> {
    let mut pool = rank_for(registry, model_ref, now_ms);
    pool.retain(|c| c.supports_tee);
    pool
}

/// Pick the top TEE-capable candidate, or [`RouterError::NoTeeCandidate`].
pub fn pick_tee(
    registry: &CandidateRegistry,
    model_ref: &str,
    now_ms: Timestamp,
) -> Result<Candidate> {
    let mut ranked = rank_for_tee(registry, model_ref, now_ms);
    if ranked.is_empty() {
        return Err(RouterError::NoTeeCandidate);
    }
    Ok(ranked.remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::UnreachableDispatcher;
    use arknet_common::types::{Address, NodeId};
    use std::sync::Arc;

    fn candidate(byte: u8, stake: u128) -> Candidate {
        Candidate {
            node_id: NodeId::new([byte; 32]),
            operator: Address::new([byte; 20]),
            total_stake: stake,
            model_refs: vec!["local/stories260K".into()],
            last_seen_ms: 1_000,
            dispatcher: Arc::new(UnreachableDispatcher),
            supports_tee: false,
        }
    }

    #[test]
    fn higher_stake_wins() {
        let r = CandidateRegistry::new();
        r.upsert(candidate(1, 100));
        r.upsert(candidate(2, 500));
        r.upsert(candidate(3, 300));
        let ranked = rank_for(&r, "local/stories260K", 1_000);
        assert_eq!(ranked.len(), 3);
        assert_eq!(ranked[0].node_id.0[0], 2, "highest stake first");
        assert_eq!(ranked[1].node_id.0[0], 3);
        assert_eq!(ranked[2].node_id.0[0], 1);
    }

    #[test]
    fn stake_tie_breaks_on_node_id() {
        let r = CandidateRegistry::new();
        r.upsert(candidate(5, 100));
        r.upsert(candidate(2, 100));
        r.upsert(candidate(9, 100));
        let ranked = rank_for(&r, "local/stories260K", 1_000);
        assert_eq!(ranked[0].node_id.0[0], 2, "lowest id first on tie");
        assert_eq!(ranked[1].node_id.0[0], 5);
        assert_eq!(ranked[2].node_id.0[0], 9);
    }

    #[test]
    fn pick_returns_error_when_empty() {
        let r = CandidateRegistry::new();
        assert!(matches!(
            pick(&r, "local/stories260K", 1_000),
            Err(RouterError::NoCandidate)
        ));
    }

    fn tee_candidate(byte: u8, stake: u128) -> Candidate {
        Candidate {
            node_id: NodeId::new([byte; 32]),
            operator: Address::new([byte; 20]),
            total_stake: stake,
            model_refs: vec!["local/stories260K".into()],
            last_seen_ms: 1_000,
            dispatcher: Arc::new(UnreachableDispatcher),
            supports_tee: true,
        }
    }

    #[test]
    fn tee_pick_returns_only_tee_candidates() {
        let r = CandidateRegistry::new();
        r.upsert(candidate(1, 500)); // no TEE
        r.upsert(tee_candidate(2, 300)); // TEE
        r.upsert(candidate(3, 400)); // no TEE
        let ranked = rank_for_tee(&r, "local/stories260K", 1_000);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].node_id.0[0], 2);
    }

    #[test]
    fn tee_pick_returns_error_when_no_tee_nodes() {
        let r = CandidateRegistry::new();
        r.upsert(candidate(1, 500));
        assert!(matches!(
            pick_tee(&r, "local/stories260K", 1_000),
            Err(RouterError::NoTeeCandidate)
        ));
    }

    #[test]
    fn tee_pick_respects_stake_ordering() {
        let r = CandidateRegistry::new();
        r.upsert(tee_candidate(1, 100));
        r.upsert(tee_candidate(2, 500));
        r.upsert(tee_candidate(3, 300));
        let ranked = rank_for_tee(&r, "local/stories260K", 1_000);
        assert_eq!(ranked[0].node_id.0[0], 2, "highest stake TEE node first");
    }

    #[test]
    fn wrong_model_filtered_out() {
        let r = CandidateRegistry::new();
        r.upsert(candidate(1, 1_000));
        assert!(matches!(
            pick(&r, "some-other", 1_000),
            Err(RouterError::NoCandidate)
        ));
    }
}
