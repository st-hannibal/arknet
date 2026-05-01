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
use arknet_network::{InferenceResponseEvent, NetworkHandle, PeerId};
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

/// [`InferenceDispatcher`] that forwards requests to a remote compute
/// node over the `/arknet/inference/1` p2p protocol.
pub struct RemoteComputeDispatcher {
    network: NetworkHandle,
    peer_id: PeerId,
    response_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<InferenceResponseEvent>>>,
}

impl RemoteComputeDispatcher {
    /// Build a dispatcher targeting `peer_id`. `response_rx` is the
    /// inference response channel from [`arknet_network::InferenceChannels`].
    pub fn new(
        network: NetworkHandle,
        peer_id: PeerId,
        response_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<InferenceResponseEvent>>>,
    ) -> Self {
        Self {
            network,
            peer_id,
            response_rx,
        }
    }
}

#[async_trait]
impl InferenceDispatcher for RemoteComputeDispatcher {
    async fn dispatch(
        &self,
        req: InferenceJobRequest,
    ) -> RouterResult<ReceiverStream<InferenceJobEvent>> {
        let wire_req =
            borsh::to_vec(&req).map_err(|e| RouterError::Dispatch(format!("encode: {e}")))?;

        let outbound_id = self
            .network
            .send_inference_request(self.peer_id, wire_req)
            .await
            .map_err(|e| RouterError::Dispatch(format!("send: {e}")))?;

        let mut rx = self.response_rx.lock().await;
        let resp = loop {
            match tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv()).await {
                Ok(Some(ev)) if ev.request_id == outbound_id => break ev,
                Ok(Some(_)) => continue,
                Ok(None) => {
                    return Err(RouterError::Dispatch("response channel closed".into()));
                }
                Err(_) => {
                    return Err(RouterError::Dispatch("inference request timed out".into()));
                }
            }
        };
        drop(rx);

        let wire_resp = resp
            .result
            .map_err(|e| RouterError::Dispatch(format!("remote error: {e}")))?;

        let inference_resp: arknet_network::InferenceResponse = borsh::from_slice(&wire_resp)
            .map_err(|e| RouterError::Dispatch(format!("decode response: {e}")))?;

        let (tx, out_rx) = mpsc::channel::<InferenceJobEvent>(64);
        for raw_event in inference_resp.events {
            let event: InferenceJobEvent = borsh::from_slice(&raw_event)
                .map_err(|e| RouterError::Dispatch(format!("decode event: {e}")))?;
            let _ = tx.send(event).await;
        }
        drop(tx);

        Ok(ReceiverStream::new(out_rx))
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
            prefer_tee: false,
            encrypted_prompt: None,
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
            prefer_tee: false,
            encrypted_prompt: None,
        };
        assert_eq!(mint_job_id(&req, 42), mint_job_id(&req, 42));
    }
}
