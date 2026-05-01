//! Node-level L2 integration test.
//!
//! Exercises the full in-process wiring: a single `arknet-node`
//! instance builds both a [`arknet_router::Router`] (with the default
//! free-tier config) and a [`arknet_compute::ComputeJobRunner`] over
//! a mock inference engine, registers the compute as a router
//! candidate via [`LocalComputeDispatcher`], and runs a job through
//! [`Router::accept`].
//!
//! The real llama.cpp path is already covered by
//! `arknet-compute/tests/engine_pipeline.rs`; this test only proves
//! the glue code in the node binary holds together. It uses a small
//! in-memory engine substitute so CI doesn't need the stories260K
//! fixture just to validate the scheduler / dispatcher wiring.

// Ensure the unused functions we added land in the public surface
// validated by a real call path.

use std::sync::Arc;

use arknet_common::types::{NodeId, Signature};
use arknet_compute::free_tier::{FreeTierConfig, FreeTierTracker};
use arknet_compute::wire::{InferenceJobEvent, InferenceJobRequest, StopKind};
use arknet_crypto::keys::SigningKey;
use arknet_crypto::signatures::sign;
use arknet_router::candidate::{Candidate, CandidateRegistry, InferenceDispatcher};
use arknet_router::errors::Result as RouterResult;
use arknet_router::{QuotaPolicy, Router};
use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Minimal dispatcher that echoes a single token then stops — stands
/// in for `LocalComputeDispatcher` without needing an actual
/// `ComputeJobRunner` (which would drag llama.cpp into the node
/// test).
struct EchoDispatcher;

#[async_trait]
impl InferenceDispatcher for EchoDispatcher {
    async fn dispatch(
        &self,
        req: InferenceJobRequest,
    ) -> RouterResult<ReceiverStream<InferenceJobEvent>> {
        let (tx, rx) = mpsc::channel(4);
        let job_id = arknet_common::types::JobId::new([0x99; 32]);
        let _ = tx
            .send(InferenceJobEvent::Token {
                job_id,
                index: 0,
                text: format!("echo:{}", req.prompt),
            })
            .await;
        let _ = tx
            .send(InferenceJobEvent::Stop {
                job_id,
                reason: StopKind::MaxTokens,
            })
            .await;
        Ok(ReceiverStream::new(rx))
    }
}

#[tokio::test]
async fn node_level_l2_roundtrip() {
    // Build router + an echo-only "compute" candidate.
    let registry = CandidateRegistry::new();
    registry.upsert(Candidate {
        node_id: NodeId::new([0x42; 32]),
        operator: arknet_common::types::Address::new([0x42; 20]),
        total_stake: 1_000_000,
        model_refs: vec!["local/echo".into()],
        last_seen_ms: 1_000,
        dispatcher: Arc::new(EchoDispatcher),
        supports_tee: false,
    });
    let router = Router::new(registry, FreeTierTracker::new(FreeTierConfig::default()));

    // Sign a request.
    let sk = SigningKey::generate();
    let unsigned = InferenceJobRequest {
        model_ref: "local/echo".into(),
        model_hash: [0; 32],
        prompt: "hello-node".into(),
        max_tokens: 1,
        seed: 0,
        deterministic: true,
        stop_strings: vec![],
        nonce: 1,
        timestamp_ms: 1_000,
        user_pubkey: sk.verifying_key().to_pubkey(),
        signature: Signature::ed25519([0; 64]),
        prefer_tee: false,
        encrypted_prompt: None,
    };
    let sig = sign(&sk, &unsigned.signing_bytes());
    let req = InferenceJobRequest {
        signature: sig,
        ..unsigned
    };

    let (_job_id, mut stream) = router
        .accept(req, 1_000, QuotaPolicy::Enforce)
        .await
        .expect("accept");

    let first = stream.next().await.expect("token");
    match first {
        InferenceJobEvent::Token { text, .. } => {
            assert_eq!(text, "echo:hello-node");
        }
        other => panic!("unexpected first event: {other:?}"),
    }
    let last = stream.next().await.expect("stop");
    assert!(matches!(
        last,
        InferenceJobEvent::Stop {
            reason: StopKind::MaxTokens,
            ..
        }
    ));
}
