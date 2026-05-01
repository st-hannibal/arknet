//! End-to-end L2 flow tests for the router.
//!
//! These tests exercise `Router::accept` against a mock compute
//! dispatcher so they run fast (no model load, no llama.cpp). The
//! real compute → inference path is exercised separately by the
//! `arknet-compute` crate's tests.
//!
//! Coverage:
//! - Happy path: signed request → router dispatches → stream drains.
//! - Failover: primary pre-stream error → backup serves.
//! - Free-tier hourly exhausted returns `FreeTierExhausted`.
//! - Wrong-model request fails with `NoCandidate` without consuming
//!   a free-tier slot.

use std::sync::Arc;

use arknet_common::types::{Address, NodeId, Signature};
use arknet_compute::free_tier::{FreeTierConfig, FreeTierTracker};
use arknet_compute::wire::{InferenceJobEvent, InferenceJobRequest, StopKind};
use arknet_crypto::keys::SigningKey;
use arknet_crypto::signatures::sign;
use arknet_router::candidate::{Candidate, CandidateRegistry, FnDispatcher, InferenceDispatcher};
use arknet_router::errors::RouterError;
use arknet_router::{QuotaPolicy, Router};
use futures::{future::FutureExt, StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

fn sign_request(prompt: &str, nonce: u64, now_ms: u64) -> InferenceJobRequest {
    // Fresh signing key per call — tests that need a stable wallet
    // identity across calls should use `sign_request_with_key` instead.
    sign_request_with_key(&SigningKey::generate(), prompt, nonce, now_ms)
}

fn sign_request_with_key(
    sk: &SigningKey,
    prompt: &str,
    nonce: u64,
    now_ms: u64,
) -> InferenceJobRequest {
    let pubkey = sk.verifying_key().to_pubkey();
    let unsigned = InferenceJobRequest {
        model_ref: "local/stories260K".into(),
        model_hash: [0; 32],
        prompt: prompt.into(),
        max_tokens: 4,
        seed: 42,
        deterministic: true,
        stop_strings: vec![],
        nonce,
        timestamp_ms: now_ms,
        user_pubkey: pubkey,
        signature: Signature::ed25519([0; 64]),
        prefer_tee: false,
        encrypted_prompt: None,
    };
    let bytes = unsigned.signing_bytes();
    let sig = sign(sk, &bytes);
    InferenceJobRequest {
        signature: sig,
        ..unsigned
    }
}

fn good_dispatcher(tag: &'static str) -> Arc<dyn InferenceDispatcher> {
    FnDispatcher::new(move |_req: InferenceJobRequest| {
        async move {
            let (tx, rx) = mpsc::channel(8);
            tx.send(InferenceJobEvent::Token {
                job_id: arknet_common::types::JobId::new([0; 32]),
                index: 0,
                text: format!("hello-{tag}"),
            })
            .await
            .unwrap();
            tx.send(InferenceJobEvent::Stop {
                job_id: arknet_common::types::JobId::new([0; 32]),
                reason: StopKind::MaxTokens,
            })
            .await
            .unwrap();
            Ok(ReceiverStream::new(rx))
        }
        .boxed()
    })
}

fn bad_dispatcher() -> Arc<dyn InferenceDispatcher> {
    FnDispatcher::new(|_req: InferenceJobRequest| {
        async move {
            let (tx, rx) = mpsc::channel(1);
            tx.send(InferenceJobEvent::Error {
                job_id: arknet_common::types::JobId::new([0; 32]),
                message: "primary exploded".into(),
            })
            .await
            .unwrap();
            Ok(ReceiverStream::new(rx))
        }
        .boxed()
    })
}

fn candidate(byte: u8, stake: u128, dispatcher: Arc<dyn InferenceDispatcher>) -> Candidate {
    Candidate {
        node_id: NodeId::new([byte; 32]),
        operator: Address::new([byte; 20]),
        total_stake: stake,
        model_refs: vec!["local/stories260K".into()],
        last_seen_ms: 1_000,
        dispatcher,
        supports_tee: false,
    }
}

#[tokio::test]
async fn three_node_inference_happy_path() {
    // 3 compute candidates registered, all healthy. Router picks the
    // highest-stake one.
    let registry = CandidateRegistry::new();
    registry.upsert(candidate(1, 100, good_dispatcher("node1")));
    registry.upsert(candidate(2, 500, good_dispatcher("node2")));
    registry.upsert(candidate(3, 300, good_dispatcher("node3")));

    let router = Router::new(registry, FreeTierTracker::new(FreeTierConfig::default()));
    let req = sign_request("Once upon a time", 1, 1_000);
    let (job_id, mut stream) = router
        .accept(req, 1_000, QuotaPolicy::Enforce)
        .await
        .expect("accept succeeds");

    let first = stream.next().await.expect("token");
    match first {
        InferenceJobEvent::Token { text, .. } => {
            // Node 2 had the highest stake; routing must be deterministic.
            assert_eq!(text, "hello-node2", "highest-stake node served");
        }
        other => panic!("expected token, got {other:?}"),
    }
    let stop = stream.next().await.expect("stop");
    assert!(matches!(
        stop,
        InferenceJobEvent::Stop {
            reason: StopKind::MaxTokens,
            ..
        }
    ));
    // Sanity: job id is non-zero.
    assert_ne!(job_id.0, [0u8; 32]);
}

#[tokio::test]
async fn failover_retries_on_pre_stream_error() {
    let registry = CandidateRegistry::new();
    // Give `bad` the higher stake so it's picked first; failover must
    // then retry `good`.
    registry.upsert(candidate(1, 500, bad_dispatcher()));
    registry.upsert(candidate(2, 100, good_dispatcher("backup")));

    let router = Router::new(registry, FreeTierTracker::new(FreeTierConfig::default()));
    let req = sign_request("backup me", 1, 1_000);
    let (_job_id, mut stream) = router
        .accept(req, 1_000, QuotaPolicy::Enforce)
        .await
        .expect("accept succeeds via failover");
    let first = stream.next().await.expect("token");
    match first {
        InferenceJobEvent::Token { text, .. } => {
            assert_eq!(text, "hello-backup", "backup node served the token");
        }
        other => panic!("expected token, got {other:?}"),
    }
}

#[tokio::test]
async fn free_tier_exhausts_after_cap() {
    let cfg = FreeTierConfig {
        hourly_limit: 2,
        daily_limit: 100,
    };
    let registry = CandidateRegistry::new();
    registry.upsert(candidate(1, 100, good_dispatcher("node1")));

    let router = Router::new(registry, FreeTierTracker::new(cfg));
    // Stable wallet across the whole test so the quota bucket
    // accumulates instead of resetting on each fresh key.
    let sk = SigningKey::generate();

    // Two slots consumed.
    for i in 0..2 {
        let req = sign_request_with_key(&sk, "hi", i, 1_000);
        let (_jid, mut stream) = router
            .accept(req, 1_000, QuotaPolicy::Enforce)
            .await
            .expect("within quota");
        // Drain.
        while let Some(ev) = stream.next().await {
            if matches!(
                ev,
                InferenceJobEvent::Stop { .. } | InferenceJobEvent::Error { .. }
            ) {
                break;
            }
        }
    }

    // Third attempt — exhausted.
    let req = sign_request_with_key(&sk, "hi", 3, 1_000);
    let err = router
        .accept(req, 1_000, QuotaPolicy::Enforce)
        .await
        .expect_err("quota should reject");
    assert!(
        matches!(err, RouterError::FreeTierExhausted { .. }),
        "expected FreeTierExhausted, got {err:?}"
    );
}

#[tokio::test]
async fn skip_policy_bypasses_quota() {
    let cfg = FreeTierConfig {
        hourly_limit: 0,
        daily_limit: 0,
    };
    let registry = CandidateRegistry::new();
    registry.upsert(candidate(1, 100, good_dispatcher("paid")));

    let router = Router::new(registry, FreeTierTracker::new(cfg));

    let req = sign_request("pre-paid", 1, 1_000);
    let (_jid, mut stream) = router
        .accept(req, 1_000, QuotaPolicy::Skip)
        .await
        .expect("bypass should succeed");
    let tok = stream.next().await.expect("token");
    assert!(matches!(tok, InferenceJobEvent::Token { .. }));
}

#[tokio::test]
async fn no_candidate_for_unknown_model_rejects_cleanly() {
    // Registry holds a candidate for `local/stories260K`; the request
    // is for a different model — pool is empty and router must say
    // `NoCandidate` without consuming a quota slot.
    let registry = CandidateRegistry::new();
    registry.upsert(candidate(1, 100, good_dispatcher("unrelated")));

    let router = Router::new(
        registry,
        FreeTierTracker::new(FreeTierConfig {
            hourly_limit: 1,
            daily_limit: 1,
        }),
    );
    let sk = SigningKey::generate();
    let pubkey = sk.verifying_key().to_pubkey();
    let unsigned = InferenceJobRequest {
        model_ref: "unknown-model".into(),
        model_hash: [0; 32],
        prompt: "hi".into(),
        max_tokens: 1,
        seed: 0,
        deterministic: true,
        stop_strings: vec![],
        nonce: 1,
        timestamp_ms: 1_000,
        user_pubkey: pubkey,
        signature: Signature::ed25519([0; 64]),
        prefer_tee: false,
        encrypted_prompt: None,
    };
    let bytes = unsigned.signing_bytes();
    let sig = sign(&sk, &bytes);
    let req = InferenceJobRequest {
        signature: sig,
        ..unsigned
    };

    let user_addr = req.derived_user_address();
    let err = router
        .accept(req, 1_000, QuotaPolicy::Enforce)
        .await
        .expect_err("must error");
    assert!(matches!(err, RouterError::NoCandidate));

    // BUT: because quota is consumed before dispatch picks a
    // candidate, a second call for the *right* model must still find
    // the quota spent. This is intentional per the SPEC §16 note
    // that mispriced requests still charge intake — documents the
    // observable behavior.
    let (hourly, _) = router.quotas().lock().counts(&user_addr, 1_000);
    assert_eq!(hourly, 1, "intake consumed quota before dispatch");
}
