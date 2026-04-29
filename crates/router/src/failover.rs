//! Router failover — retry a failed dispatch on the next-ranked
//! candidate.
//!
//! Phase 1 scope: **pre-stream** failover only. If the primary's
//! dispatcher returns an error *before* any token is produced we try
//! the next-best candidate, and so on until the list is exhausted.
//!
//! Mid-stream resumption (router has already sent tokens to the
//! client, then the compute dies partway through) needs llama.cpp
//! checkpointing — that's explicit Week 12 scope and deferred here.
//! When the backup dispatcher returns tokens that would overlap with
//! what's already been sent, the router currently drops the new
//! stream and emits a `StopKind::Cancelled`. The Week 12 spec upgrades
//! this to a real resume.

use std::sync::Arc;
use std::time::Duration;

use arknet_common::types::{JobId, Timestamp};
use arknet_compute::wire::{InferenceJobEvent, InferenceJobRequest, StopKind};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::time;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, warn};

use crate::candidate::{Candidate, InferenceDispatcher};
use crate::errors::{Result, RouterError};

/// Max time to wait for the first token from a dispatched candidate
/// before falling over to the next one (pre-stream failover only).
pub const PRIMARY_FIRST_TOKEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Internal router stream — one terminal event always closes the
/// receiver side, matching the compute wire contract.
pub type RouterStream = ReceiverStream<InferenceJobEvent>;

/// Drive `request` through `ranked` candidates in order. Return the
/// first stream that yielded at least one token; on every earlier
/// failure, advance to the next candidate.
///
/// Returns [`RouterError::NoCandidate`] when every candidate failed
/// before producing a token. Returns any inflight compute error as
/// [`RouterError::Compute`] inside the stream's terminal `Error`
/// variant.
pub async fn dispatch_with_failover(
    ranked: Vec<Candidate>,
    request: InferenceJobRequest,
    job_id: JobId,
) -> Result<RouterStream> {
    if ranked.is_empty() {
        return Err(RouterError::NoCandidate);
    }

    let mut last_err: Option<RouterError> = None;
    for candidate in ranked {
        debug!(node=%candidate.node_id, %job_id, "trying candidate");
        match dispatch_one(candidate.dispatcher.clone(), request.clone(), job_id).await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                warn!(%job_id, error=%e, "candidate failed — trying next");
                last_err = Some(e);
            }
        }
    }

    Err(last_err.unwrap_or(RouterError::NoCandidate))
}

/// Send `request` to one candidate. Waits up to
/// [`PRIMARY_FIRST_TOKEN_TIMEOUT`] for either the first non-error
/// event or the terminal `Stop`. On any pre-token failure returns
/// [`RouterError::Dispatch`] so [`dispatch_with_failover`] can move on.
async fn dispatch_one(
    dispatcher: Arc<dyn InferenceDispatcher>,
    request: InferenceJobRequest,
    job_id: JobId,
) -> Result<RouterStream> {
    let mut inner = dispatcher.dispatch(request).await?;

    // Peek at the first event under a timeout. If it is an `Error`,
    // dispatch failed cleanly — surface to the caller as a
    // [`RouterError::Dispatch`] so failover advances. If no event
    // arrives within the timeout we also fail over.
    let first = time::timeout(PRIMARY_FIRST_TOKEN_TIMEOUT, inner.next()).await;
    let first = match first {
        Ok(Some(ev)) => ev,
        Ok(None) => return Err(RouterError::Dispatch("empty stream".into())),
        Err(_) => return Err(RouterError::Dispatch("first-token timeout".into())),
    };

    if let InferenceJobEvent::Error { message, .. } = &first {
        return Err(RouterError::Compute {
            message: message.clone(),
        });
    }

    // Good first event. Build a forwarded stream that carries `first`
    // then drains `inner` to completion.
    let (tx, rx) = mpsc::channel::<InferenceJobEvent>(64);
    tokio::spawn(async move {
        if tx.send(first).await.is_err() {
            return;
        }
        while let Some(ev) = inner.next().await {
            let is_terminal = matches!(
                ev,
                InferenceJobEvent::Stop { .. } | InferenceJobEvent::Error { .. }
            );
            if tx.send(ev).await.is_err() {
                return;
            }
            if is_terminal {
                return;
            }
        }
        // Inner stream ended without a terminal — synthesize a
        // `Cancelled` so the receiver doesn't hang.
        let _ = tx
            .send(InferenceJobEvent::Stop {
                job_id,
                reason: StopKind::Cancelled,
            })
            .await;
    });

    Ok(ReceiverStream::new(rx))
}

/// For callers that don't have a [`RouterStream`] handy: synthesize a
/// stream containing just a terminal `Error` event. Makes surface
/// composition uniform — every intake path returns "a stream" that
/// either succeeds or carries a single terminal failure.
pub fn error_stream(job_id: JobId, message: impl Into<String>) -> RouterStream {
    let (tx, rx) = mpsc::channel(1);
    let ev = InferenceJobEvent::Error {
        job_id,
        message: message.into(),
    };
    // Fire-and-forget send into the fresh channel; capacity is 1 so
    // this can't block.
    let _ = tx.try_send(ev);
    ReceiverStream::new(rx)
}

/// Helper for metrics: timestamp now in ms.
pub fn now_ms() -> Timestamp {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{Candidate, FnDispatcher};
    use arknet_common::types::{Address, NodeId, PubKey, Signature};
    use futures::future::FutureExt;
    use std::sync::Arc;

    fn req() -> InferenceJobRequest {
        InferenceJobRequest {
            model_ref: "local/stories260K".into(),
            model_hash: [0; 32],
            prompt: "hi".into(),
            max_tokens: 1,
            seed: 0,
            deterministic: true,
            stop_strings: vec![],
            nonce: 1,
            timestamp_ms: 0,
            user_pubkey: PubKey::ed25519([0; 32]),
            signature: Signature::ed25519([0; 64]),
        }
    }

    fn candidate(byte: u8, dispatcher: Arc<dyn InferenceDispatcher>) -> Candidate {
        Candidate {
            node_id: NodeId::new([byte; 32]),
            operator: Address::new([byte; 20]),
            total_stake: 100 - byte as u128,
            model_refs: vec!["local/stories260K".into()],
            last_seen_ms: 0,
            dispatcher,
        }
    }

    #[tokio::test]
    async fn succeeds_on_first_candidate() {
        let good = FnDispatcher::new(move |_req: InferenceJobRequest| {
            async move {
                let (tx, rx) = mpsc::channel(4);
                tx.send(InferenceJobEvent::Token {
                    job_id: JobId::new([0; 32]),
                    index: 0,
                    text: "hi".into(),
                })
                .await
                .unwrap();
                tx.send(InferenceJobEvent::Stop {
                    job_id: JobId::new([0; 32]),
                    reason: StopKind::MaxTokens,
                })
                .await
                .unwrap();
                Ok(ReceiverStream::new(rx))
            }
            .boxed()
        });
        let ranked = vec![candidate(1, good)];
        let mut stream = dispatch_with_failover(ranked, req(), JobId::new([0; 32]))
            .await
            .unwrap();
        let a = stream.next().await.unwrap();
        let b = stream.next().await.unwrap();
        assert!(matches!(a, InferenceJobEvent::Token { .. }));
        assert!(matches!(
            b,
            InferenceJobEvent::Stop {
                reason: StopKind::MaxTokens,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn fails_over_to_next_when_first_errors_pre_stream() {
        let bad = FnDispatcher::new(|_req: InferenceJobRequest| {
            async move {
                let (tx, rx) = mpsc::channel(1);
                tx.send(InferenceJobEvent::Error {
                    job_id: JobId::new([0; 32]),
                    message: "primary exploded".into(),
                })
                .await
                .unwrap();
                Ok(ReceiverStream::new(rx))
            }
            .boxed()
        });
        let good = FnDispatcher::new(|_req: InferenceJobRequest| {
            async move {
                let (tx, rx) = mpsc::channel(4);
                tx.send(InferenceJobEvent::Token {
                    job_id: JobId::new([0; 32]),
                    index: 0,
                    text: "backup".into(),
                })
                .await
                .unwrap();
                tx.send(InferenceJobEvent::Stop {
                    job_id: JobId::new([0; 32]),
                    reason: StopKind::MaxTokens,
                })
                .await
                .unwrap();
                Ok(ReceiverStream::new(rx))
            }
            .boxed()
        });
        let ranked = vec![candidate(1, bad), candidate(2, good)];
        let mut stream = dispatch_with_failover(ranked, req(), JobId::new([0; 32]))
            .await
            .unwrap();
        let first = stream.next().await.unwrap();
        match first {
            InferenceJobEvent::Token { text, .. } => assert_eq!(text, "backup"),
            other => panic!("expected token from backup, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn errors_when_every_candidate_fails() {
        let bad = FnDispatcher::new(|_req: InferenceJobRequest| {
            async move { Err(RouterError::Dispatch("dead".into())) }.boxed()
        });
        let ranked = vec![candidate(1, bad.clone()), candidate(2, bad)];
        let err = dispatch_with_failover(ranked, req(), JobId::new([0; 32]))
            .await
            .unwrap_err();
        assert!(matches!(err, RouterError::Dispatch(_)));
    }
}
