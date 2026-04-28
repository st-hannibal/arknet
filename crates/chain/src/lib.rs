//! arknet L1 chain primitives: blocks, transactions, receipts, fee market.
//!
//! This crate defines the on-chain vocabulary — the types that cross
//! consensus, mempool, and state boundaries. State application logic
//! (`apply_tx`, RocksDB trie, genesis loader) lands in Phase 1 Week 3-4
//! and lives in separate modules within this crate.
//!
//! # What's here (Phase 1 Week 1-2)
//!
//! - [`block`] — `BlockHeader`, `Block`, domain-separated hashing.
//! - [`transactions`] — `Transaction` enum, `SignedTransaction`, stake ops,
//!   governance bodies, model-registry transactions.
//! - [`receipt`] — `InferenceReceipt`, `ReceiptBatch` (authoritative shape).
//! - [`fee_market`] — EIP-1559 base fee update rule (pure function).
//! - [`errors`] — chain-layer error hierarchy.
//!
//! # What's NOT here
//!
//! State trie, RocksDB, genesis loader, tx application, consensus
//! integration — all scheduled for later Phase 1 week-blocks.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod block;
pub mod errors;
pub mod fee_market;
pub mod receipt;
pub mod transactions;

pub use block::{check_block_size, receipt_root, tx_root, Block, BlockHeader, MAX_BLOCK_BYTES};
pub use errors::{ChainError, Result};
pub use fee_market::{next_base_fee, BASE_FEE_MAX_CHANGE_DENOM, MIN_BASE_FEE};
pub use receipt::{
    ComputeProof, DaLayer, DaReference, InferenceReceipt, Quantization, ReceiptBatch,
    TeeAttestation, MAX_RECEIPT_BATCH_BYTES, MAX_RECEIPT_BYTES, RECEIPT_BATCH_MAX,
};
pub use transactions::{
    check_signed_tx_size, OnChainModelManifest, Proposal, SignedTransaction, StakeOp, StakeRole,
    Transaction, VoteChoice, MAX_SIGNED_TX_BYTES,
};
