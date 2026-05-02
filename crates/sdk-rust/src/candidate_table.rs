//! SDK-local candidate registry populated from gossip `PoolOffer` messages.
//!
//! The table is a lightweight mirror of the node's `CandidateRegistry` —
//! it stores which compute peers serve which models and how much capacity
//! they have. Stale entries are filtered at query time.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use arknet_common::types::{Address, Timestamp};

const CANDIDATE_TTL_MS: u64 = 5 * 60 * 1_000;

/// A compute node discovered via gossip.
#[derive(Clone, Debug)]
pub struct CandidateEntry {
    /// Raw libp2p PeerId bytes (from PoolOffer).
    pub peer_id_bytes: Vec<u8>,
    /// Models this node serves.
    pub model_refs: Vec<String>,
    /// Operator address.
    pub operator: Address,
    /// Staked amount (used for ranking).
    pub total_stake: u128,
    /// TEE capability.
    pub supports_tee: bool,
    /// Last gossip timestamp.
    pub timestamp_ms: Timestamp,
    /// Available inference slots.
    pub available_slots: u32,
    /// Known multiaddrs resolved via Kademlia / identify.
    pub multiaddrs: Vec<String>,
}

/// Thread-safe candidate table updated by the gossip listener and
/// queried by the inference path.
#[derive(Clone)]
pub struct CandidateTable {
    inner: Arc<RwLock<HashMap<Vec<u8>, CandidateEntry>>>,
}

impl CandidateTable {
    /// Create an empty table.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Insert or update a candidate from a gossip PoolOffer.
    pub fn upsert(&self, entry: CandidateEntry) {
        let mut map = self.inner.write().expect("candidate table poisoned");
        map.insert(entry.peer_id_bytes.clone(), entry);
    }

    /// Update the known multiaddrs for a peer.
    pub fn set_addrs(&self, peer_id_bytes: &[u8], addrs: Vec<String>) {
        let mut map = self.inner.write().expect("candidate table poisoned");
        if let Some(entry) = map.get_mut(peer_id_bytes) {
            entry.multiaddrs = addrs;
        }
    }

    /// Return candidates that serve `model` and are not stale, sorted
    /// by available capacity (descending) then stake (descending).
    pub fn eligible_for(&self, model: &str, now_ms: Timestamp) -> Vec<CandidateEntry> {
        let map = self.inner.read().expect("candidate table poisoned");
        let mut results: Vec<CandidateEntry> = map
            .values()
            .filter(|c| now_ms.saturating_sub(c.timestamp_ms) <= CANDIDATE_TTL_MS)
            .filter(|c| c.model_refs.iter().any(|m| m == model))
            .cloned()
            .collect();
        results.sort_by(|a, b| {
            b.available_slots
                .cmp(&a.available_slots)
                .then_with(|| b.total_stake.cmp(&a.total_stake))
        });
        results
    }

    /// Total number of entries (including stale).
    pub fn len(&self) -> usize {
        self.inner.read().expect("candidate table poisoned").len()
    }

    /// Whether the table has any entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for CandidateTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(peer: u8, models: &[&str], stake: u128, slots: u32, ts: u64) -> CandidateEntry {
        CandidateEntry {
            peer_id_bytes: vec![peer],
            model_refs: models.iter().map(|s| s.to_string()).collect(),
            operator: Address::new([peer; 20]),
            total_stake: stake,
            supports_tee: false,
            timestamp_ms: ts,
            available_slots: slots,
            multiaddrs: vec![],
        }
    }

    #[test]
    fn upsert_and_query() {
        let table = CandidateTable::new();
        table.upsert(entry(1, &["model-a"], 100, 2, 1000));
        table.upsert(entry(2, &["model-a", "model-b"], 200, 1, 1000));

        let results = table.eligible_for("model-a", 1000);
        assert_eq!(results.len(), 2);

        let results_b = table.eligible_for("model-b", 1000);
        assert_eq!(results_b.len(), 1);
        assert_eq!(results_b[0].peer_id_bytes, vec![2]);
    }

    #[test]
    fn stale_entries_filtered() {
        let table = CandidateTable::new();
        table.upsert(entry(1, &["model-a"], 100, 1, 1000));
        let now = 1000 + CANDIDATE_TTL_MS + 1;
        let results = table.eligible_for("model-a", now);
        assert!(results.is_empty());
    }

    #[test]
    fn sorted_by_capacity_then_stake() {
        let table = CandidateTable::new();
        table.upsert(entry(1, &["m"], 300, 1, 1000));
        table.upsert(entry(2, &["m"], 100, 3, 1000));
        table.upsert(entry(3, &["m"], 200, 3, 1000));

        let results = table.eligible_for("m", 1000);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].peer_id_bytes, vec![3]); // slots=3, stake=200
        assert_eq!(results[1].peer_id_bytes, vec![2]); // slots=3, stake=100
        assert_eq!(results[2].peer_id_bytes, vec![1]); // slots=1, stake=300
    }

    #[test]
    fn upsert_replaces_old_entry() {
        let table = CandidateTable::new();
        table.upsert(entry(1, &["old-model"], 100, 1, 1000));
        table.upsert(entry(1, &["new-model"], 200, 2, 2000));
        assert_eq!(table.len(), 1);
        let results = table.eligible_for("new-model", 2000);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].total_stake, 200);
    }

    #[test]
    fn set_addrs_updates_existing() {
        let table = CandidateTable::new();
        table.upsert(entry(1, &["m"], 100, 1, 1000));
        table.set_addrs(&[1], vec!["/ip4/1.2.3.4/tcp/26656".into()]);
        let results = table.eligible_for("m", 1000);
        assert_eq!(results[0].multiaddrs.len(), 1);
    }

    #[test]
    fn empty_table() {
        let table = CandidateTable::new();
        assert!(table.is_empty());
        assert_eq!(table.eligible_for("any", 0).len(), 0);
    }

    #[test]
    fn unknown_model_returns_empty() {
        let table = CandidateTable::new();
        table.upsert(entry(1, &["model-a"], 100, 1, 1000));
        assert!(table.eligible_for("model-z", 1000).is_empty());
    }
}
