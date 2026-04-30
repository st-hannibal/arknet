//! arknet L2 compute role.
//!
//! The compute node:
//!
//! 1. Accepts a signed [`InferenceJobRequest`] from a router.
//! 2. Runs it through the shared [`arknet_inference::InferenceEngine`]
//!    (inherited from Phase 0).
//! 3. Streams tokens back as [`InferenceJobEvent`] wire messages.
//! 4. Builds a [`HashChainBuilder`] attestation alongside the stream
//!    for Week 11's receipt pipeline.
//!
//! Plus supporting modules:
//!
//! - [`free_tier`] — per-wallet quota tracking + gossip tick merging.
//! - [`attestation`] — the rolling hash-chain compute-proof builder.
//! - [`wire`] — borsh-encoded req/resp types shared with the router.
//!
//! # Phase 1 scope
//!
//! - Single-node in-process composition (router + compute in the same
//!   process or on localhost).
//! - Multi-node libp2p `request_response` transport lands in Week 11
//!   alongside the verifier so both end-to-end L2 flows use the same
//!   wire protocol.
//! - Payment settlement + receipt batching happen in Week 11.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod attestation;
pub mod errors;
pub mod free_tier;
pub mod job;
pub mod wire;

pub use attestation::{HashChainBuilder, DOMAIN_HASHCHAIN};
pub use errors::{ComputeError, Result};
pub use free_tier::{
    bucket_indices, FreeTierConfig, FreeTierTick, FreeTierTracker, QuotaOutcome,
    DEFAULT_DAILY_LIMIT, DEFAULT_HOURLY_LIMIT,
};
pub use job::{ComputeJobRunner, JOB_EVENT_BUFFER, NONCE_CACHE_CAP};
pub use wire::{
    derive_user_address, InferenceJobEvent, InferenceJobRequest, InferenceRequestSigningBody,
    StopKind, INFERENCE_REQUEST_DOMAIN, REQUEST_MAX_SKEW_MS,
};
