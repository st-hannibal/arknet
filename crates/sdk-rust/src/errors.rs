//! SDK error types.

use thiserror::Error;

/// SDK errors.
#[derive(Debug, Error)]
pub enum SdkError {
    /// HTTP transport error.
    #[error("http: {0}")]
    Http(String),
    /// API returned a non-2xx status.
    #[error("api error (status {status}): {body}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// Response body.
        body: String,
    },
    /// Wallet I/O error (load/save).
    #[error("wallet: {0}")]
    Wallet(String),
    /// Cryptographic operation failed.
    #[error("crypto: {0}")]
    Crypto(String),
    /// P2P transport or protocol error.
    #[error("p2p: {0}")]
    P2p(String),
    /// Wire encoding/decoding error.
    #[error("wire: {0}")]
    Wire(String),
    /// No wallet configured where one is required.
    #[error("no wallet: operation requires a wallet but none was provided")]
    NoWallet,
}

/// SDK result type.
pub type Result<T> = std::result::Result<T, SdkError>;
