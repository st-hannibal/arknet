//! Compute-candidate registry.
//!
//! A [`Candidate`] is the router's picture of a compute node that has
//! advertised itself on `arknet/pool/offer/1` gossip. In Phase 1 the
//! registry is in-memory; Phase 2 swaps this for a DHT-backed view.
//!
//! Candidates carry:
//!
//! - `node_id` — the compute node's 32-byte identity.
//! - `operator` — payout address.
//! - `total_stake` — their bonded stake (ranker input).
//! - `model_refs` — models they serve.
//! - `last_seen_ms` — wall-clock freshness; stale entries are ignored
//!   by the selector.
//!
//! Phase 1 also stores a direct `dispatcher` handle: in-process tests
//! and the role body attach the callable end of the transport here.
//! The libp2p `request_response` path in Week 11 adds a
//! `RemoteDispatcher` variant that maps `NodeId → PeerId`.

use std::collections::HashMap;
use std::sync::Arc;

use arknet_common::types::{Address, NodeId, Timestamp};
use arknet_compute::wire::{InferenceJobEvent, InferenceJobRequest};
use async_trait::async_trait;
use futures::Stream;
use parking_lot::RwLock;
use tokio_stream::wrappers::ReceiverStream;

use crate::errors::{Result, RouterError};

/// How long a candidate remains usable after its last heartbeat
/// (Phase 1: 5 minutes). Past this, the selector skips the entry.
pub const CANDIDATE_TTL_MS: u64 = 5 * 60 * 1_000;

/// Dispatches a signed inference request to a compute node and streams
/// back [`InferenceJobEvent`]s.
///
/// Abstracting over transport lets Week-10 tests compose compute +
/// router in-process while Week 11 swaps in a libp2p-backed impl
/// without touching the selection logic.
#[async_trait]
pub trait InferenceDispatcher: Send + Sync {
    /// Send `req` to the candidate and return an event stream. The
    /// stream must terminate after exactly one `Stop` or `Error`.
    async fn dispatch(&self, req: InferenceJobRequest)
        -> Result<ReceiverStream<InferenceJobEvent>>;
}

/// Typed dispatcher trait object.
pub type BoxedDispatcher = Arc<dyn InferenceDispatcher>;

/// One candidate compute node.
#[derive(Clone)]
pub struct Candidate {
    /// Compute node id.
    pub node_id: NodeId,
    /// Payout address.
    pub operator: Address,
    /// Bonded stake in `ark_atom`.
    pub total_stake: u128,
    /// Models this node advertises (canonical refs).
    pub model_refs: Vec<String>,
    /// Last-seen wall-clock ms.
    pub last_seen_ms: Timestamp,
    /// Dispatcher handle for sending the job.
    pub dispatcher: BoxedDispatcher,
    /// `true` if this node has a verified TEE capability registered
    /// on-chain. The router uses this to filter for confidential
    /// inference requests (`prefer_tee = true`).
    pub supports_tee: bool,
}

impl Candidate {
    /// `true` if the candidate's last heartbeat was within [`CANDIDATE_TTL_MS`].
    pub fn is_fresh(&self, now_ms: Timestamp) -> bool {
        now_ms.saturating_sub(self.last_seen_ms) <= CANDIDATE_TTL_MS
    }

    /// `true` if the candidate advertises `model_ref` (exact-match).
    pub fn serves(&self, model_ref: &str) -> bool {
        self.model_refs.iter().any(|r| r == model_ref)
    }
}

/// In-memory registry of compute candidates. Cheap to clone; internal
/// state is behind a read-write lock.
#[derive(Clone, Default)]
pub struct CandidateRegistry {
    inner: Arc<RwLock<HashMap<NodeId, Candidate>>>,
}

impl CandidateRegistry {
    /// Build an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or overwrite a candidate record.
    pub fn upsert(&self, c: Candidate) {
        self.inner.write().insert(c.node_id, c);
    }

    /// Remove a candidate by id.
    pub fn remove(&self, node: &NodeId) -> Option<Candidate> {
        self.inner.write().remove(node)
    }

    /// Snapshot of every candidate. The returned `Vec` is ordered by
    /// node id for determinism in tests.
    pub fn snapshot(&self) -> Vec<Candidate> {
        let guard = self.inner.read();
        let mut out: Vec<Candidate> = guard.values().cloned().collect();
        out.sort_by_key(|c| c.node_id.0);
        out
    }

    /// Number of candidates currently tracked.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// `true` if no candidates are tracked.
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    /// Filter candidates that serve `model_ref` and are fresh at `now_ms`.
    pub fn eligible_for(&self, model_ref: &str, now_ms: Timestamp) -> Vec<Candidate> {
        self.snapshot()
            .into_iter()
            .filter(|c| c.serves(model_ref) && c.is_fresh(now_ms))
            .collect()
    }
}

/// Trivial dispatcher wrapper that forwards to a closure. Convenience
/// for tests + the in-process Phase-1 wiring.
pub struct FnDispatcher<F>
where
    F: Fn(
            InferenceJobRequest,
        )
            -> futures::future::BoxFuture<'static, Result<ReceiverStream<InferenceJobEvent>>>
        + Send
        + Sync,
{
    f: F,
}

impl<F> FnDispatcher<F>
where
    F: Fn(
            InferenceJobRequest,
        )
            -> futures::future::BoxFuture<'static, Result<ReceiverStream<InferenceJobEvent>>>
        + Send
        + Sync,
{
    /// Wrap a closure into an [`InferenceDispatcher`].
    pub fn new(f: F) -> Arc<Self> {
        Arc::new(Self { f })
    }
}

#[async_trait]
impl<F> InferenceDispatcher for FnDispatcher<F>
where
    F: Fn(
            InferenceJobRequest,
        )
            -> futures::future::BoxFuture<'static, Result<ReceiverStream<InferenceJobEvent>>>
        + Send
        + Sync,
{
    async fn dispatch(
        &self,
        req: InferenceJobRequest,
    ) -> Result<ReceiverStream<InferenceJobEvent>> {
        (self.f)(req).await
    }
}

/// Transport-less stub dispatcher. Returns [`RouterError::Dispatch`]
/// on every call — useful for registering a compute node whose
/// transport hasn't been wired yet.
pub struct UnreachableDispatcher;

#[async_trait]
impl InferenceDispatcher for UnreachableDispatcher {
    async fn dispatch(
        &self,
        _req: InferenceJobRequest,
    ) -> Result<ReceiverStream<InferenceJobEvent>> {
        Err(RouterError::Dispatch("no transport wired".into()))
    }
}

/// A [`Stream`] over the dispatcher output, type-erased for
/// downstream polymorphism.
pub type DispatchStream = Box<dyn Stream<Item = InferenceJobEvent> + Send + Unpin>;

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(byte: u8, now: Timestamp) -> Candidate {
        Candidate {
            node_id: NodeId::new([byte; 32]),
            operator: Address::new([byte; 20]),
            total_stake: 1_000_000,
            model_refs: vec!["local/stories260K".into()],
            last_seen_ms: now,
            dispatcher: Arc::new(UnreachableDispatcher),
            supports_tee: false,
        }
    }

    #[test]
    fn upsert_and_snapshot() {
        let r = CandidateRegistry::new();
        assert!(r.is_empty());
        r.upsert(candidate(1, 1_000));
        r.upsert(candidate(2, 1_000));
        assert_eq!(r.len(), 2);
        let snap = r.snapshot();
        assert_eq!(snap[0].node_id.0[0], 1);
        assert_eq!(snap[1].node_id.0[0], 2);
    }

    #[test]
    fn stale_candidate_filtered() {
        let r = CandidateRegistry::new();
        r.upsert(candidate(1, 0));
        // 10 minutes later — past the 5-minute TTL.
        let now = 10 * 60 * 1_000;
        let eligible = r.eligible_for("local/stories260K", now);
        assert!(eligible.is_empty());
    }

    #[test]
    fn only_matching_model_returned() {
        let r = CandidateRegistry::new();
        let mut c1 = candidate(1, 1_000);
        c1.model_refs = vec!["local/stories260K".into()];
        let mut c2 = candidate(2, 1_000);
        c2.model_refs = vec!["some-other-model".into()];
        r.upsert(c1);
        r.upsert(c2);
        let eligible = r.eligible_for("local/stories260K", 1_000);
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].node_id.0[0], 1);
    }

    #[test]
    fn remove_drops_entry() {
        let r = CandidateRegistry::new();
        let c = candidate(9, 0);
        let id = c.node_id;
        r.upsert(c);
        assert_eq!(r.len(), 1);
        let removed = r.remove(&id);
        assert!(removed.is_some());
        assert!(r.is_empty());
    }
}
