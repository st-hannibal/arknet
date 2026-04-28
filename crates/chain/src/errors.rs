//! Chain-layer error types. Transaction / block validation surfaces here
//! before state application, which has its own error hierarchy (Phase 1
//! Week 3-4).

use thiserror::Error;

/// Errors produced while working with chain primitives (blocks, txs,
/// headers, fee market) — independent of state application.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ChainError {
    /// A borsh encode / decode failed.
    #[error("codec error: {0}")]
    Codec(String),

    /// A transaction or block exceeded its size bound.
    #[error("oversize: {what} has {actual} bytes, max {max}")]
    Oversize {
        /// Identifier for the oversized object.
        what: &'static str,
        /// Observed byte length.
        actual: usize,
        /// Maximum allowed byte length.
        max: usize,
    },

    /// A fee-market update was given out-of-range inputs.
    #[error("fee-market out of range: {0}")]
    FeeMarket(&'static str),
}

/// Chain result type.
pub type Result<T> = std::result::Result<T, ChainError>;
