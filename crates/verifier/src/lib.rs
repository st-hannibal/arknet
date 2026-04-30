//! arknet L2 verifier role.
//!
//! A verifier:
//!
//! 1. Observes anchored [`arknet_chain::ReceiptBatch`] transactions.
//! 2. For each receipt, runs a VRF gate ([`selection`]) to decide
//!    whether this node is sampled to re-execute the job.
//! 3. If sampled, re-runs the deterministic inference
//!    ([`reexec::Reexecutor`]), rebuilds the hash chain, and compares
//!    the derived `output_hash` to the receipt's `output_hash`.
//! 4. On mismatch, builds + signs an on-chain
//!    [`arknet_chain::Transaction::Dispute`] ([`dispute::build_dispute`])
//!    that triggers the Week-9 slashing pathway.
//!
//! # Crate graph note
//!
//! We keep the [`reexec::Reexecutor`] trait transport-agnostic so the
//! verifier crate itself doesn't depend on `arknet-inference`. Node
//! binaries that actually run verification plug in a concrete
//! backend (typically the same `InferenceEngine` the compute role
//! holds).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod dispute;
pub mod errors;
pub mod reexec;
pub mod selection;

pub use dispute::{build_and_sign_dispute, build_dispute};
pub use errors::{Result, VerifierError};
pub use reexec::{rebuild_hash_chain, verify_receipt, Reexecutor, Verdict};
pub use selection::{
    sampling_threshold, select_verifier, verify_selection, vrf_input, Selection,
    DEFAULT_SAMPLING_RATE, VRF_DOMAIN,
};
