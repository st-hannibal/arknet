//! arknet L2 router role.
//!
//! A router is the entry point for inference traffic on L2. It:
//!
//! 1. Accepts signed [`InferenceJobRequest`]s from clients.
//! 2. Verifies the signature + freshness + free-tier quota.
//! 3. Picks a compute node from the [`CandidateRegistry`].
//! 4. Dispatches via [`InferenceDispatcher`] with pre-stream failover.
//!
//! The router is transport-agnostic: an [`InferenceDispatcher`]
//! abstracts "send req → receive event stream". Phase 1's
//! in-process composition wires the dispatcher to a shared
//! [`arknet_compute::ComputeJobRunner`]; Week 11 swaps in a libp2p
//! `request_response` impl when the multi-node L2 mesh lands.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod candidate;
pub mod errors;
pub mod failover;
pub mod intake;
pub mod quota_gossip;
pub mod selection;

pub use arknet_compute::wire::{
    InferenceJobEvent, InferenceJobRequest, StopKind, INFERENCE_REQUEST_DOMAIN,
};
pub use candidate::{
    Candidate, CandidateRegistry, DispatchStream, FnDispatcher, InferenceDispatcher,
    UnreachableDispatcher, CANDIDATE_TTL_MS,
};
pub use errors::{Result, RouterError};
pub use failover::{
    dispatch_with_failover, error_stream, now_ms, RouterStream, PRIMARY_FIRST_TOKEN_TIMEOUT,
};
pub use intake::{first_and_rest, verify_request, QuotaPolicy, Router};
pub use quota_gossip::{
    absorb_tick_bytes, recent_nonces_shared, run_emitter, spawn_emitter, ChannelTransport,
    PendingConsumption, QuotaGossipTransport, RecentNonces, DEFAULT_TICK_INTERVAL,
};
pub use selection::{pick, rank_for};
