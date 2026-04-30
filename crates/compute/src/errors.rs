//! Compute-role error hierarchy.

use thiserror::Error;

/// Compute-crate result alias.
pub type Result<T> = std::result::Result<T, ComputeError>;

/// Errors the compute role can surface to its caller.
#[derive(Debug, Error)]
pub enum ComputeError {
    /// Inference engine rejected the request (bad model ref, decode failure, …).
    #[error("inference: {0}")]
    Inference(#[from] arknet_inference::InferenceError),
    /// Job was cancelled before it produced any tokens.
    #[error("job cancelled")]
    Cancelled,
    /// Internal channel dropped unexpectedly.
    #[error("internal: {0}")]
    Internal(String),
    /// Signing failure.
    #[error("signing: {0}")]
    Signing(String),
    /// Request payload invalid (e.g. empty prompt, over-long input).
    #[error("invalid request: {0}")]
    BadRequest(String),
}
