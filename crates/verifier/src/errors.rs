//! Verifier-role error hierarchy.

use thiserror::Error;

/// Verifier-crate result alias.
pub type Result<T> = std::result::Result<T, VerifierError>;

/// Everything the verifier role can fail on.
#[derive(Debug, Error)]
pub enum VerifierError {
    /// Re-execution needed deterministic mode but the receipt said
    /// serving mode — we won't produce a check that's sensitive to
    /// sampling noise.
    #[error("not a deterministic job: cannot re-execute")]
    NonDeterministic,

    /// VRF check for the current verifier against `job_id` says we
    /// weren't selected for this one.
    #[error("not selected: VRF output above threshold")]
    NotSelected,

    /// Re-execution backend refused.
    #[error("re-exec failed: {0}")]
    ReexecFailed(String),

    /// Signing / key error.
    #[error("signing: {0}")]
    Signing(String),

    /// Receipt carried a `ComputeProof` variant the verifier doesn't
    /// understand (TEE / ZK arrive in later phases).
    #[error("unsupported compute proof variant at Phase 1")]
    UnsupportedProof,

    /// Internal invariant failure.
    #[error("internal: {0}")]
    Internal(String),
}
