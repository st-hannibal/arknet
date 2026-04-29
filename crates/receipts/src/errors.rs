//! Receipts-crate error hierarchy.

use thiserror::Error;

/// Receipts-crate result alias.
pub type Result<T> = std::result::Result<T, ReceiptError>;

/// Everything the batching / anchoring pipeline can fail on.
#[derive(Debug, Error)]
pub enum ReceiptError {
    /// Batch builder was asked to build an empty batch.
    #[error("batch is empty")]
    Empty,

    /// Batch exceeded the protocol hard cap on receipts per batch.
    #[error("too many receipts in batch: {count} (max {max})")]
    TooManyReceipts {
        /// Receipts the builder held.
        count: usize,
        /// Protocol cap (§16).
        max: usize,
    },

    /// Borsh-encoded batch exceeded [`MAX_RECEIPT_BATCH_BYTES`].
    #[error("receipt batch exceeds size cap: {actual} > {max}")]
    Oversize {
        /// Encoded size.
        actual: usize,
        /// Protocol cap.
        max: usize,
    },

    /// Inner Merkle-tree construction error.
    #[error("merkle: {0}")]
    Merkle(String),

    /// Encoding failure (borsh bug — effectively impossible).
    #[error("encoding: {0}")]
    Encoding(String),
}
