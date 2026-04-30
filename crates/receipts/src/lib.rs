//! Inference receipts: aggregation, Merkle batching, L1 anchoring.
//!
//! # Scope
//!
//! - [`batch`] — [`ReceiptBatchBuilder`] accumulates
//!   [`arknet_chain::InferenceReceipt`]s, seals them into a
//!   [`arknet_chain::ReceiptBatch`] with a SHA-256 Merkle root, and
//!   enforces the per-spec size + count caps.
//! - [`anchor`] — [`build_anchor_tx`] wraps a sealed batch in a
//!   [`arknet_chain::SignedTransaction`] ready for `/v1/tx`.
//!
//! The chain-side handler (`apply_receipt_batch`) is in `arknet-chain`
//! to keep the crate topology acyclic.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod anchor;
pub mod batch;
pub mod errors;

pub use anchor::build_anchor_tx;
pub use batch::{
    aggregator_signing_digest, compute_batch_id, compute_merkle_root, hash_receipt,
    ReceiptBatchBuilder, DOMAIN_RECEIPT_BATCH_SIG, DOMAIN_RECEIPT_LEAF,
};
pub use errors::{ReceiptError, Result};
