//! In-process bridge from the router's [`InferenceDispatcher`] to a
//! local [`arknet_compute::ComputeJobRunner`].
//!
//! Used when a single `arknet` binary runs both `router` and `compute`
//! roles (common on a developer laptop and in the Week 10 integration
//! tests). When Week 11 introduces the libp2p `request_response` wire,
//! this module grows a second impl (`RemoteComputeDispatcher`) that
//! wraps the networked transport.
//!
//! The dispatcher owns: the [`ComputeJobRunner`], the [`ModelRef`] the
//! local compute node advertises, the pool id, and a clock source
//! (swappable so deterministic tests can drive time).

// Consumed by [`crate::compute_role::register_self_as_candidate`] when
// router + compute co-locate. Week 11 adds a concrete call site in
// the multi-role boot path; until then clippy sees the helpers as
// dead, so silence that at the module level.
#![allow(dead_code)]

use std::sync::Arc;

use arknet_common::types::{JobId, PoolId, Timestamp};
use arknet_compute::{
    wire::{InferenceJobEvent, InferenceJobRequest, StopKind},
    ComputeJobRunner,
};
use arknet_model_manager::ModelRef;
use arknet_router::candidate::InferenceDispatcher;
use arknet_router::errors::{Result as RouterResult, RouterError};
use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::warn;

/// Concrete [`InferenceDispatcher`] that forwards each request to a
/// local [`ComputeJobRunner`].
pub struct LocalComputeDispatcher {
    runner: ComputeJobRunner,
    pool_id: PoolId,
    model_ref: ModelRef,
    clock: Arc<dyn Fn() -> Timestamp + Send + Sync>,
}

impl LocalComputeDispatcher {
    /// Build a dispatcher bound to `runner`.
    pub fn new(runner: ComputeJobRunner, pool_id: PoolId, model_ref: ModelRef) -> Self {
        Self {
            runner,
            pool_id,
            model_ref,
            clock: Arc::new(default_clock),
        }
    }

    /// Override the clock. Tests use this to feed deterministic
    /// timestamps into both the request skew check and the job id.
    pub fn with_clock<F>(mut self, f: F) -> Self
    where
        F: Fn() -> Timestamp + Send + Sync + 'static,
    {
        self.clock = Arc::new(f);
        self
    }
}

#[async_trait]
impl InferenceDispatcher for LocalComputeDispatcher {
    async fn dispatch(
        &self,
        req: InferenceJobRequest,
    ) -> RouterResult<ReceiverStream<InferenceJobEvent>> {
        let now = (self.clock)();
        let job_id = mint_job_id(&req, now);
        let inner_stream = self
            .runner
            .run(req, &self.model_ref, self.pool_id, job_id, now)
            .await
            .map_err(|e| RouterError::Dispatch(e.to_string()))?;

        let (tx, rx) = mpsc::channel::<InferenceJobEvent>(64);
        tokio::spawn(async move {
            let mut inner = std::pin::pin!(inner_stream);
            while let Some(ev) = inner.next().await {
                let terminal = matches!(
                    ev,
                    InferenceJobEvent::Stop { .. } | InferenceJobEvent::Error { .. }
                );
                if tx.send(ev).await.is_err() {
                    warn!(%job_id, "receiver dropped; aborting dispatcher stream");
                    return;
                }
                if terminal {
                    return;
                }
            }
            let _ = tx
                .send(InferenceJobEvent::Stop {
                    job_id,
                    reason: StopKind::Cancelled,
                })
                .await;
        });
        Ok(ReceiverStream::new(rx))
    }
}

/// Deterministic job id derivation. Must match what the integration
/// tests assert on.
fn mint_job_id(req: &InferenceJobRequest, now: Timestamp) -> JobId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arknet-job-id-v1");
    hasher.update(&req.derived_user_address().0);
    hasher.update(&req.nonce.to_le_bytes());
    hasher.update(&now.to_le_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_bytes());
    JobId::new(out)
}

fn default_clock() -> Timestamp {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_common::types::{PubKey, Signature};

    #[test]
    fn job_id_changes_on_different_nonce() {
        let mut req = InferenceJobRequest {
            model_ref: "x".into(),
            model_hash: [0; 32],
            prompt: "hi".into(),
            max_tokens: 1,
            seed: 0,
            deterministic: true,
            stop_strings: vec![],
            nonce: 1,
            timestamp_ms: 0,
            user_pubkey: PubKey::ed25519([0xab; 32]),
            signature: Signature::ed25519([0; 64]),
        };
        let a = mint_job_id(&req, 0);
        req.nonce = 2;
        let b = mint_job_id(&req, 0);
        assert_ne!(a, b);
    }

    #[test]
    fn job_id_is_stable_for_same_inputs() {
        let req = InferenceJobRequest {
            model_ref: "x".into(),
            model_hash: [0; 32],
            prompt: "hi".into(),
            max_tokens: 1,
            seed: 0,
            deterministic: true,
            stop_strings: vec![],
            nonce: 1,
            timestamp_ms: 0,
            user_pubkey: PubKey::ed25519([0xab; 32]),
            signature: Signature::ed25519([0; 64]),
        };
        assert_eq!(mint_job_id(&req, 42), mint_job_id(&req, 42));
    }
}
