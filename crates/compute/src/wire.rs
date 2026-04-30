//! Wire types for the router ↔ compute inference protocol.
//!
//! Borsh-encoded. The same types are used over libp2p `request_response`
//! (Phase 1+) and in unit tests that drive the compute role in-process.
//!
//! # Versioning
//!
//! Top-level protocol id is `/arknet/inference/1` — a breaking change to
//! these types is a new `/2` path. Field order here is part of the wire
//! contract; do not reorder.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use arknet_common::types::{Address, Hash256, JobId, Nonce, PubKey, Signature, Timestamp};

/// A request for a single inference job, signed by the user.
///
/// `user_pubkey` is the signing key; `signature` covers the borsh encoding
/// of every other field in this struct. `user_address` is derived as
/// `blake3(user_pubkey.bytes)[0..20]` — the same derivation used by
/// `arknet-chain`'s `derive_address_from_signer`.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct InferenceJobRequest {
    /// Canonical model identifier (e.g. `"local/stories260K"`).
    pub model_ref: String,
    /// Expected model digest — compute refuses to serve if mismatch.
    pub model_hash: Hash256,
    /// Prompt text. Phase 1 is plaintext; Phase 2 adds X25519 envelope.
    pub prompt: String,
    /// Max new tokens.
    pub max_tokens: u32,
    /// Deterministic-mode seed (ignored in serving mode).
    pub seed: u64,
    /// `true` → force `InferenceMode::Deterministic` on the compute node.
    pub deterministic: bool,
    /// Stop strings.
    pub stop_strings: Vec<String>,
    /// Per-user replay nonce. Stored in a bounded LRU on the compute node.
    pub nonce: Nonce,
    /// Unix ms — rejected if older than [`REQUEST_MAX_SKEW_MS`].
    pub timestamp_ms: Timestamp,
    /// User signing pubkey.
    pub user_pubkey: PubKey,
    /// Signature over borsh encoding of every prior field.
    pub signature: Signature,
}

/// Derived user address = `blake3(pubkey.bytes)[0..20]`.
///
/// Kept out of the signed body so the wire format doesn't duplicate
/// information already carried by `user_pubkey`.
pub fn derive_user_address(pubkey: &PubKey) -> Address {
    let digest = blake3::hash(&pubkey.bytes);
    let bytes = digest.as_bytes();
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes[..20]);
    Address::new(out)
}

/// How old a request can be before it's rejected as stale (30s).
///
/// Small enough that replay is useless; large enough that mild clock
/// skew doesn't break honest clients.
pub const REQUEST_MAX_SKEW_MS: u64 = 30_000;

/// Streaming event returned by the compute node.
///
/// Mirrors `arknet_inference::InferenceEvent` but carries the `JobId`
/// so a router multiplexing multiple jobs can demux. One `Stop` variant
/// terminates the stream.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum InferenceJobEvent {
    /// A decoded token text + index.
    Token {
        /// Job this event belongs to.
        job_id: JobId,
        /// Zero-based index in the output sequence.
        index: u32,
        /// Decoded UTF-8 fragment.
        text: String,
    },
    /// Generation ended for the reason carried.
    Stop {
        /// Job this event belongs to.
        job_id: JobId,
        /// Terminal reason.
        reason: StopKind,
    },
    /// Compute-side error — final event on the stream.
    Error {
        /// Job this event belongs to.
        job_id: JobId,
        /// Error message (opaque; for logging, not consensus).
        message: String,
    },
}

/// Why generation ended. Mirror of `arknet_inference::StopReason`
/// intentionally reencoded into borsh-friendly shape.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum StopKind {
    /// Hit `max_tokens`.
    MaxTokens,
    /// Model's EOS token produced.
    EndOfStream,
    /// Caller-provided stop string matched.
    StopString(String),
    /// Caller (router / client) cancelled.
    Cancelled,
}

impl From<arknet_inference::StopReason> for StopKind {
    fn from(s: arknet_inference::StopReason) -> Self {
        match s {
            arknet_inference::StopReason::MaxTokens => StopKind::MaxTokens,
            arknet_inference::StopReason::EndOfStream => StopKind::EndOfStream,
            arknet_inference::StopReason::StopString(s) => StopKind::StopString(s),
            arknet_inference::StopReason::Cancelled => StopKind::Cancelled,
        }
    }
}

/// `InferenceJobRequest` payload for signing. The signature covers the
/// borsh encoding of this struct so a signer can't be tricked into
/// signing a semantically different request.
#[derive(Clone, Debug, BorshSerialize)]
pub struct InferenceRequestSigningBody<'a> {
    /// Domain separator — must not appear inside any other signed body.
    pub domain: &'static [u8],
    /// Model identifier.
    pub model_ref: &'a str,
    /// Model digest.
    pub model_hash: &'a Hash256,
    /// Prompt.
    pub prompt: &'a str,
    /// Max new tokens.
    pub max_tokens: u32,
    /// Deterministic seed.
    pub seed: u64,
    /// Deterministic flag.
    pub deterministic: bool,
    /// Stop strings.
    pub stop_strings: &'a [String],
    /// Nonce.
    pub nonce: Nonce,
    /// Timestamp.
    pub timestamp_ms: Timestamp,
    /// Pubkey.
    pub user_pubkey: &'a PubKey,
}

/// Domain tag for inference-request signatures. Never reused by any
/// other signed body in arknet.
pub const INFERENCE_REQUEST_DOMAIN: &[u8] = b"arknet-inference-req-v1";

impl InferenceJobRequest {
    /// Bytes a signer covers with their signature. Use this to both
    /// sign and verify.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let body = InferenceRequestSigningBody {
            domain: INFERENCE_REQUEST_DOMAIN,
            model_ref: &self.model_ref,
            model_hash: &self.model_hash,
            prompt: &self.prompt,
            max_tokens: self.max_tokens,
            seed: self.seed,
            deterministic: self.deterministic,
            stop_strings: &self.stop_strings,
            nonce: self.nonce,
            timestamp_ms: self.timestamp_ms,
            user_pubkey: &self.user_pubkey,
        };
        borsh::to_vec(&body).expect("borsh encoding of signing body is infallible")
    }

    /// Borsh-encoded address derived from `user_pubkey`.
    pub fn derived_user_address(&self) -> Address {
        derive_user_address(&self.user_pubkey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_common::types::SignatureScheme;

    fn sample() -> InferenceJobRequest {
        InferenceJobRequest {
            model_ref: "local/stories260K".into(),
            model_hash: [0xab; 32],
            prompt: "Once upon a time".into(),
            max_tokens: 16,
            seed: 42,
            deterministic: true,
            stop_strings: vec![".".into()],
            nonce: 7,
            timestamp_ms: 1_700_000_000_000,
            user_pubkey: PubKey::ed25519([0x11; 32]),
            signature: Signature::ed25519([0x22; 64]),
        }
    }

    #[test]
    fn request_borsh_roundtrip() {
        let r = sample();
        let bytes = borsh::to_vec(&r).unwrap();
        let back: InferenceJobRequest = borsh::from_slice(&bytes).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn event_borsh_roundtrip() {
        let job_id = JobId::new([0x55; 32]);
        let events = vec![
            InferenceJobEvent::Token {
                job_id,
                index: 0,
                text: "hello".into(),
            },
            InferenceJobEvent::Stop {
                job_id,
                reason: StopKind::MaxTokens,
            },
            InferenceJobEvent::Stop {
                job_id,
                reason: StopKind::StopString(".".into()),
            },
            InferenceJobEvent::Error {
                job_id,
                message: "boom".into(),
            },
        ];
        for ev in events {
            let bytes = borsh::to_vec(&ev).unwrap();
            let back: InferenceJobEvent = borsh::from_slice(&bytes).unwrap();
            assert_eq!(ev, back);
        }
    }

    #[test]
    fn signing_bytes_stable_and_domain_prefixed() {
        let r = sample();
        let b1 = r.signing_bytes();
        let b2 = r.signing_bytes();
        assert_eq!(b1, b2, "signing bytes must be deterministic");
        // The domain tag appears near the start (after its borsh length prefix).
        assert!(
            b1.windows(INFERENCE_REQUEST_DOMAIN.len())
                .any(|w| w == INFERENCE_REQUEST_DOMAIN),
            "domain tag must be present in signed bytes"
        );
    }

    #[test]
    fn derive_user_address_matches_chain_rule() {
        // Must match `arknet_chain::apply::derive_address_from_signer`:
        // blake3(pubkey.bytes)[0..20].
        let pk = PubKey::ed25519([0x33; 32]);
        let a = derive_user_address(&pk);
        let digest = blake3::hash(&pk.bytes);
        assert_eq!(a.as_bytes(), &digest.as_bytes()[..20]);
    }

    #[test]
    fn request_max_skew_is_30s() {
        assert_eq!(REQUEST_MAX_SKEW_MS, 30_000);
    }

    #[test]
    fn signature_scheme_sanity() {
        // Ed25519 is the only active scheme at genesis.
        assert!(SignatureScheme::Ed25519.is_active());
    }
}
