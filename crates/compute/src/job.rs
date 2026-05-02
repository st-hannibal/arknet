//! Job lifecycle on the compute node.
//!
//! ```text
//!   Accept ──► Compute ──► [ Stream tokens ] ──► Complete
//!                  │
//!                  └─► Error (reported as terminal event)
//! ```
//!
//! One [`ComputeJobRunner`] per compute node (cheap to clone). Each
//! call to [`ComputeJobRunner::run`] is fully independent — bounded
//! mpsc backpressures producer on slow consumer, and dropping the
//! receiver stream aborts the underlying inference session at the next
//! event boundary.

use std::collections::VecDeque;
use std::sync::Arc;

use arknet_common::types::{JobId, PoolId, Timestamp};
use arknet_inference::{
    InferenceEngine, InferenceEvent, InferenceMode, InferenceRequest, SamplingParams,
};
use arknet_model_manager::ModelRef;
use futures::Stream;
use futures_util::StreamExt;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, warn};

use crate::attestation::HashChainBuilder;
use crate::errors::{ComputeError, Result};
use crate::wire::{InferenceJobEvent, InferenceJobRequest, StopKind};

/// Optional enclave key handle for TEE-capable compute nodes.
/// When set, the runner can decrypt `encrypted_prompt` payloads.
pub type EnclaveKey = Option<Arc<arknet_crypto::keys::KeyExchangeSecret>>;

/// Capped buffer for streamed [`InferenceJobEvent`]s between the
/// compute task and the caller.
pub const JOB_EVENT_BUFFER: usize = 64;

/// Replay-protection bucket: we remember this many (addr, nonce) pairs
/// and reject repeats. Phase 1 uses a bounded FIFO; Phase 2's payment
/// channels replace this with channel-level nonce checks.
pub const NONCE_CACHE_CAP: usize = 8_192;

/// One running compute node.
///
/// Holds the [`InferenceEngine`] + a bounded replay cache. Cheap to clone;
/// all inner state is behind `Arc`.
#[derive(Clone)]
pub struct ComputeJobRunner {
    engine: InferenceEngine,
    nonces: Arc<Mutex<NonceCache>>,
    enclave_key: EnclaveKey,
}

impl ComputeJobRunner {
    /// Build a runner backed by `engine`.
    pub fn new(engine: InferenceEngine) -> Self {
        Self {
            engine,
            nonces: Arc::new(Mutex::new(NonceCache::new(NONCE_CACHE_CAP))),
            enclave_key: None,
        }
    }

    /// Build a TEE-capable runner with an enclave decryption key.
    pub fn with_enclave_key(
        engine: InferenceEngine,
        key: arknet_crypto::keys::KeyExchangeSecret,
    ) -> Self {
        Self {
            engine,
            nonces: Arc::new(Mutex::new(NonceCache::new(NONCE_CACHE_CAP))),
            enclave_key: Some(Arc::new(key)),
        }
    }

    /// Drive `req` to completion, streaming back [`InferenceJobEvent`]s.
    ///
    /// The returned stream terminates after exactly one `Stop` or
    /// `Error` event.
    ///
    /// # Errors
    ///
    /// Failures that happen *before* the stream starts (bad nonce,
    /// model load failure) bubble up synchronously; in-flight errors
    /// become terminal [`InferenceJobEvent::Error`] events on the
    /// stream.
    pub async fn run(
        &self,
        req: InferenceJobRequest,
        model_ref: &ModelRef,
        pool_id: PoolId,
        job_id: JobId,
        now_ms: Timestamp,
    ) -> Result<impl Stream<Item = InferenceJobEvent> + Send + 'static> {
        // Decrypt encrypted prompt if present (TEE confidential inference).
        let prompt = if let Some(ref envelope) = req.encrypted_prompt {
            let key = self.enclave_key.as_ref().ok_or_else(|| {
                ComputeError::BadRequest(
                    "encrypted prompt received but node has no enclave key".into(),
                )
            })?;
            let sealed =
                arknet_crypto::aead::SealedPrompt {
                    ephemeral_pubkey: envelope.ephemeral_pubkey.as_slice().try_into().map_err(
                        |_| ComputeError::BadRequest("bad ephemeral pubkey length".into()),
                    )?,
                    nonce: envelope
                        .nonce
                        .as_slice()
                        .try_into()
                        .map_err(|_| ComputeError::BadRequest("bad nonce length".into()))?,
                    ciphertext: envelope.ciphertext.clone(),
                };
            let plaintext = arknet_crypto::aead::open_prompt(&sealed, key)
                .map_err(|e| ComputeError::BadRequest(format!("prompt decryption failed: {e}")))?;
            String::from_utf8(plaintext)
                .map_err(|e| ComputeError::BadRequest(format!("decrypted prompt not UTF-8: {e}")))?
        } else {
            req.prompt.clone()
        };

        // Sanity checks on the request shape before we spend any model
        // time.
        if prompt.is_empty() {
            return Err(ComputeError::BadRequest("empty prompt".into()));
        }
        if req.max_tokens == 0 {
            return Err(ComputeError::BadRequest("max_tokens must be > 0".into()));
        }
        if now_ms.saturating_sub(req.timestamp_ms) > crate::wire::REQUEST_MAX_SKEW_MS {
            return Err(ComputeError::BadRequest(
                "stale request (clock skew)".into(),
            ));
        }
        let user_addr = req.billing_address();
        if !self.nonces.lock().insert(user_addr.0, req.nonce) {
            return Err(ComputeError::BadRequest("replayed nonce".into()));
        }

        let handle = self
            .engine
            .load(model_ref)
            .await
            .map_err(ComputeError::Inference)?;
        debug!(
            model=%model_ref,
            model_digest=%hex::encode(handle.digest().as_bytes()),
            %job_id,
            "compute: loaded model"
        );

        let mode = if req.deterministic {
            InferenceMode::Deterministic
        } else {
            InferenceMode::Serving
        };
        let inference_req = InferenceRequest {
            prompt,
            max_tokens: req.max_tokens,
            mode,
            sampling: if mode == InferenceMode::Deterministic {
                SamplingParams::GREEDY
            } else {
                SamplingParams {
                    seed: req.seed,
                    ..SamplingParams::default()
                }
            },
            stop: req.stop_strings,
        };

        let inference_stream = self
            .engine
            .infer(&handle, inference_req)
            .await
            .map_err(ComputeError::Inference)?;

        let (tx, rx) = mpsc::channel::<InferenceJobEvent>(JOB_EVENT_BUFFER);

        // Fire-and-forget bridge: inference stream → job event stream.
        // `inference_stream` is `impl Stream + Send + 'static`, so we
        // can park it in an owned tokio task.
        tokio::spawn(bridge_events(inference_stream, tx, job_id));

        // `pool_id` is unused in the Week-10 path; it's on the API
        // surface because receipts + the L2 pool market need it by
        // Week 11. Drop the silence-warning when that lands.
        let _ = pool_id;

        Ok(ReceiverStream::new(rx))
    }
}

async fn bridge_events<S>(mut stream: S, tx: mpsc::Sender<InferenceJobEvent>, job_id: JobId)
where
    S: Stream<Item = arknet_inference::Result<InferenceEvent>> + Send + Unpin + 'static,
{
    let mut chain = HashChainBuilder::new();
    let mut index: u32 = 0;
    while let Some(item) = stream.next().await {
        let event = match item {
            Ok(InferenceEvent::Token(tok)) => {
                chain.absorb_token(&tok.text);
                let out = InferenceJobEvent::Token {
                    job_id,
                    index,
                    text: tok.text,
                };
                index = index.saturating_add(1);
                out
            }
            Ok(InferenceEvent::Stop(reason)) => InferenceJobEvent::Stop {
                job_id,
                reason: StopKind::from(reason),
            },
            Err(e) => InferenceJobEvent::Error {
                job_id,
                message: e.to_string(),
            },
        };
        if tx.send(event).await.is_err() {
            // Receiver dropped — propagate cancellation by letting the
            // inference stream's own backpressure close the channel on
            // the next poll.
            warn!(%job_id, "receiver dropped; aborting job stream");
            break;
        }
    }
    // `chain` is discarded here in Week-10; attestation wiring into
    // `InferenceReceipt::compute_proof = HashChain(…)` lands with the
    // receipt pipeline in Week 11.
    drop(chain);
}

/// Bounded (addr, nonce) replay cache.
///
/// Phase 1 stores a per-address FIFO of the last `cap` nonces seen.
/// Nonces are intentionally allowed out-of-order — routers may batch
/// requests — but duplicates are rejected. Once a user blows past
/// `cap`, their oldest nonces are forgotten; that is acceptable for
/// the free-tier path and payment channels handle real replay in Phase 2.
struct NonceCache {
    cap: usize,
    // `VecDeque` is ordered by arrival; linear scan is fine because
    // `cap` is small and hits are rare (the common case is a miss →
    // insert).
    seen: VecDeque<([u8; 20], u64)>,
}

impl NonceCache {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            seen: VecDeque::with_capacity(cap),
        }
    }

    /// `true` if the pair is new (and stored). `false` if it's a replay.
    fn insert(&mut self, addr: [u8; 20], nonce: u64) -> bool {
        if self.seen.iter().any(|(a, n)| *a == addr && *n == nonce) {
            return false;
        }
        if self.seen.len() >= self.cap {
            self.seen.pop_front();
        }
        self.seen.push_back((addr, nonce));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_cache_accepts_unique() {
        let mut c = NonceCache::new(8);
        assert!(c.insert([1; 20], 1));
        assert!(c.insert([1; 20], 2));
        assert!(c.insert([2; 20], 1));
    }

    #[test]
    fn nonce_cache_rejects_duplicate() {
        let mut c = NonceCache::new(8);
        assert!(c.insert([1; 20], 1));
        assert!(!c.insert([1; 20], 1));
    }

    #[test]
    fn nonce_cache_evicts_oldest_past_cap() {
        let mut c = NonceCache::new(2);
        assert!(c.insert([1; 20], 1));
        assert!(c.insert([1; 20], 2));
        // Third insert evicts the (1,1) entry.
        assert!(c.insert([1; 20], 3));
        // Now (1,1) can be re-used because it was evicted. This is the
        // known bound of the Phase-1 cache.
        assert!(c.insert([1; 20], 1));
    }
}
