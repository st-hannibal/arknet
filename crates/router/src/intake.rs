//! Router request intake.
//!
//! Flow:
//!
//! 1. Verify the user signature on the incoming [`InferenceJobRequest`].
//! 2. Derive the user address and consume one free-tier slot (unless
//!    the caller chose to skip the quota gate — pre-paid path is
//!    Phase 2).
//! 3. Allocate a [`JobId`] and pick a compute candidate.
//! 4. Call [`crate::failover::dispatch_with_failover`] and stream the
//!    result back to the caller.
//!
//! Every step is small + testable in isolation; the whole dance is
//! orchestrated by [`Router::accept`].

use std::sync::Arc;

use arknet_common::types::{Address, JobId, SignatureScheme, Timestamp};
use arknet_compute::free_tier::{FreeTierTracker, QuotaOutcome};
use arknet_compute::wire::{
    InferenceJobEvent, InferenceJobRequest, INFERENCE_REQUEST_DOMAIN, REQUEST_MAX_SKEW_MS,
};
use arknet_crypto::signatures::verify;
use parking_lot::Mutex;
use tracing::{debug, warn};

use crate::candidate::CandidateRegistry;
use crate::errors::{Result, RouterError};
use crate::failover::{dispatch_with_failover, RouterStream};
use crate::selection::{rank_for, rank_for_tee};

/// Whether to gate the request on the free-tier quota.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaPolicy {
    /// Default: enforce free-tier limits.
    Enforce,
    /// Skip the quota check (Phase 2 payment-channel path).
    Skip,
}

/// Router service. Holds a candidate registry + free-tier tracker.
#[derive(Clone)]
pub struct Router {
    registry: CandidateRegistry,
    quotas: Arc<Mutex<FreeTierTracker>>,
    next_job_salt: Arc<Mutex<u64>>,
}

impl Router {
    /// Build a router bound to a fresh free-tier tracker.
    pub fn new(registry: CandidateRegistry, quotas: FreeTierTracker) -> Self {
        Self {
            registry,
            quotas: Arc::new(Mutex::new(quotas)),
            next_job_salt: Arc::new(Mutex::new(0)),
        }
    }

    /// Candidate registry (shared handle).
    pub fn registry(&self) -> &CandidateRegistry {
        &self.registry
    }

    /// Quota tracker (shared handle).
    pub fn quotas(&self) -> &Arc<Mutex<FreeTierTracker>> {
        &self.quotas
    }

    /// Accept a signed request and return a stream of
    /// [`InferenceJobEvent`]s.
    pub async fn accept(
        &self,
        req: InferenceJobRequest,
        now_ms: Timestamp,
        policy: QuotaPolicy,
    ) -> Result<(JobId, RouterStream)> {
        verify_request(&req, now_ms)?;
        let user_addr = req.derived_user_address();

        if policy == QuotaPolicy::Enforce {
            match self.quotas.lock().consume(&user_addr, now_ms) {
                QuotaOutcome::Allowed { .. } => {}
                QuotaOutcome::HourlyExceeded { .. } => {
                    return Err(RouterError::FreeTierExhausted {
                        reason: "hourly limit exhausted".into(),
                    });
                }
                QuotaOutcome::DailyExceeded { .. } => {
                    return Err(RouterError::FreeTierExhausted {
                        reason: "daily limit exhausted".into(),
                    });
                }
            }
        }

        let job_id = self.mint_job_id(&user_addr, now_ms);
        let ranked = if req.prefer_tee {
            let tee_ranked = rank_for_tee(&self.registry, &req.model_ref, now_ms);
            if tee_ranked.is_empty() {
                return Err(RouterError::NoTeeCandidate);
            }
            tee_ranked
        } else {
            let all = rank_for(&self.registry, &req.model_ref, now_ms);
            if all.is_empty() {
                return Err(RouterError::NoCandidate);
            }
            all
        };
        debug!(
            candidates = ranked.len(),
            %job_id,
            "router: dispatching with failover"
        );
        let stream = dispatch_with_failover(ranked, req, job_id).await?;
        Ok((job_id, stream))
    }

    fn mint_job_id(&self, user: &Address, now_ms: Timestamp) -> JobId {
        let salt = {
            let mut s = self.next_job_salt.lock();
            *s = s.saturating_add(1);
            *s
        };
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"arknet-job-id-v1");
        hasher.update(user.as_bytes());
        hasher.update(&now_ms.to_le_bytes());
        hasher.update(&salt.to_le_bytes());
        let digest = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(digest.as_bytes());
        JobId::new(out)
    }
}

/// Run every pre-dispatch check on a request. Returns `Ok` iff the
/// request is safe to forward.
pub fn verify_request(req: &InferenceJobRequest, now_ms: Timestamp) -> Result<()> {
    if req.prompt.is_empty() && req.encrypted_prompt.is_none() {
        return Err(RouterError::BadRequest("empty prompt".into()));
    }
    if req.prefer_tee && req.encrypted_prompt.is_none() {
        return Err(RouterError::BadRequest(
            "prefer_tee requires encrypted_prompt".into(),
        ));
    }
    if req.max_tokens == 0 {
        return Err(RouterError::BadRequest("max_tokens must be > 0".into()));
    }
    if now_ms.saturating_sub(req.timestamp_ms) > REQUEST_MAX_SKEW_MS {
        return Err(RouterError::BadRequest("stale request".into()));
    }
    if req.user_pubkey.scheme != SignatureScheme::Ed25519 {
        return Err(RouterError::BadRequest(format!(
            "unsupported signature scheme at Phase 1: {:?}",
            req.user_pubkey.scheme
        )));
    }

    let signing_bytes = req.signing_bytes();
    verify(&req.user_pubkey, &signing_bytes, &req.signature).map_err(|e| {
        warn!(error=%e, "signature verification failed");
        RouterError::BadRequest("signature verification failed".into())
    })?;

    // The domain tag must be present in the signed bytes — this is
    // defense-in-depth against someone routing a different kind of
    // signed payload through this endpoint.
    if !signing_bytes
        .windows(INFERENCE_REQUEST_DOMAIN.len())
        .any(|w| w == INFERENCE_REQUEST_DOMAIN)
    {
        return Err(RouterError::BadRequest(
            "missing inference-request domain tag".into(),
        ));
    }

    let _ = req.derived_user_address();

    Ok(())
}

/// Helper: turn a stream into a convenience `(first_token, rest)` tuple.
/// Used by the /v1/inference proxy to report the right HTTP status
/// before streaming the body.
pub async fn first_and_rest(mut stream: RouterStream) -> (Option<InferenceJobEvent>, RouterStream) {
    use futures_util::StreamExt;
    let first = stream.next().await;
    (first, stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_common::types::Signature;
    use arknet_crypto::keys::SigningKey;
    use arknet_crypto::signatures::sign;

    fn sign_req(prompt: &str, timestamp_ms: Timestamp) -> InferenceJobRequest {
        let sk = SigningKey::generate();
        let pubkey = sk.verifying_key().to_pubkey();
        let unsigned = InferenceJobRequest {
            model_ref: "local/stories260K".into(),
            model_hash: [0; 32],
            prompt: prompt.into(),
            max_tokens: 1,
            seed: 0,
            deterministic: true,
            stop_strings: vec![],
            nonce: 1,
            timestamp_ms,
            user_pubkey: pubkey,
            // Placeholder — we'll overwrite after signing.
            signature: Signature::ed25519([0; 64]),
            prefer_tee: false,
            encrypted_prompt: None,
        };
        let bytes = unsigned.signing_bytes();
        let sig = sign(&sk, &bytes);
        InferenceJobRequest {
            signature: sig,
            ..unsigned
        }
    }

    #[test]
    fn signed_request_verifies() {
        let req = sign_req("hi", 1_000);
        verify_request(&req, 1_000).expect("verify");
    }

    #[test]
    fn empty_prompt_rejected() {
        let req = sign_req("", 1_000);
        assert!(matches!(
            verify_request(&req, 1_000),
            Err(RouterError::BadRequest(_))
        ));
    }

    #[test]
    fn stale_rejected() {
        let req = sign_req("hi", 0);
        // now = 2 minutes later.
        assert!(matches!(
            verify_request(&req, 2 * 60_000),
            Err(RouterError::BadRequest(_))
        ));
    }

    #[test]
    fn tampered_signature_rejected() {
        let mut req = sign_req("hi", 1_000);
        // Flip one byte in the signature.
        req.signature.bytes[0] ^= 0x01;
        assert!(matches!(
            verify_request(&req, 1_000),
            Err(RouterError::BadRequest(_))
        ));
    }

    #[test]
    fn tampered_prompt_rejected() {
        let mut req = sign_req("hi", 1_000);
        // Swap prompt after signing.
        req.prompt = "something else".into();
        assert!(matches!(
            verify_request(&req, 1_000),
            Err(RouterError::BadRequest(_))
        ));
    }
}
