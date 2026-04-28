//! arknet L1 chain primitives: blocks, transactions, receipts, fee market.
//!
//! Phase 1 Week 1-2 landed the on-chain vocabulary (types + encoding).
//! Week 3-4 adds the state layer — RocksDB-backed account store, a sparse
//! Merkle tree commitment, the transaction-application loop, and the
//! genesis loader.
//!
//! # Modules
//!
//! - [`block`] — `BlockHeader`, `Block`, domain-separated hashing.
//! - [`transactions`] — `Transaction` enum, `SignedTransaction`, stake ops,
//!   governance bodies, model-registry transactions.
//! - [`receipt`] — `InferenceReceipt`, `ReceiptBatch` (authoritative shape).
//! - [`fee_market`] — EIP-1559 base fee update rule (pure function).
//! - [`errors`] — chain-layer error hierarchy.
//! - [`account`] / [`stake_entry`] / [`validator`] — typed state records.
//! - [`state`] — RocksDB column families + SMT-backed state root.
//! - [`apply`] — transaction → state transition with lenient rejection.
//! - [`genesis`] — TOML genesis loader + fair-launch invariant check.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod account;
pub mod apply;
pub mod block;
pub mod errors;
pub mod fee_market;
pub mod genesis;
pub mod receipt;
pub mod stake_entry;
pub mod state;
pub mod transactions;
pub mod validator;

pub use account::Account;
pub use apply::{apply_tx, RejectReason, TxOutcome};
pub use block::{check_block_size, receipt_root, tx_root, Block, BlockHeader, MAX_BLOCK_BYTES};
pub use errors::{ChainError, Result};
pub use fee_market::{next_base_fee, BASE_FEE_MAX_CHANGE_DENOM, MIN_BASE_FEE};
pub use genesis::{load_genesis, GenesisConfig, GenesisParams, GenesisValidator};
pub use receipt::{
    ComputeProof, DaLayer, DaReference, InferenceReceipt, Quantization, ReceiptBatch,
    TeeAttestation, MAX_RECEIPT_BATCH_BYTES, MAX_RECEIPT_BYTES, RECEIPT_BATCH_MAX,
};
pub use stake_entry::StakeEntry;
pub use state::{BlockCtx, State};
pub use transactions::{
    check_signed_tx_size, OnChainModelManifest, Proposal, SignedTransaction, StakeOp, StakeRole,
    Transaction, VoteChoice, MAX_SIGNED_TX_BYTES,
};
pub use validator::ValidatorInfo;
