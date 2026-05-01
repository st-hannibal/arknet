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
}

/// SDK result type.
pub type Result<T> = std::result::Result<T, SdkError>;
