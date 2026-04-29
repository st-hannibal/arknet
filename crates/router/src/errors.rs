//! Router-role error hierarchy.

use arknet_compute::wire::InferenceJobEvent;
use thiserror::Error;

/// Router-crate result alias.
pub type Result<T> = std::result::Result<T, RouterError>;

/// Errors surfaced by the router role.
#[derive(Debug, Error)]
pub enum RouterError {
    /// No compute candidate satisfies the request (model, quant, stake).
    #[error("no candidate available")]
    NoCandidate,

    /// Free-tier quota exhausted for this wallet.
    #[error("free-tier exhausted: {reason}")]
    FreeTierExhausted {
        /// Operator-readable reason (hourly / daily).
        reason: String,
    },

    /// Signature / nonce / skew check failed on intake.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Downstream compute errored — wire event is attached.
    #[error("compute error: {message}")]
    Compute {
        /// Compute error message (non-structured).
        message: String,
    },

    /// Compute backend dispatch failed outright.
    #[error("dispatch: {0}")]
    Dispatch(String),

    /// Internal channel / signing / invariant failure.
    #[error("internal: {0}")]
    Internal(String),
}

impl RouterError {
    /// Convert a terminal compute-error event into a [`RouterError`].
    pub fn from_event(event: &InferenceJobEvent) -> Option<Self> {
        match event {
            InferenceJobEvent::Error { message, .. } => Some(RouterError::Compute {
                message: message.clone(),
            }),
            _ => None,
        }
    }
}
