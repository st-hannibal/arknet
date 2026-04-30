//! Errors produced by the model manager.

use std::path::PathBuf;

use thiserror::Error;

/// Result alias for this crate.
pub type Result<T, E = ModelError> = std::result::Result<T, E>;

/// Every failure mode a caller of the model manager might see.
///
/// Variants stay specific: the type encodes *where* the failure happened
/// (network / hash / gguf / cache) so upstream code can react without
/// parsing strings.
#[derive(Debug, Error)]
pub enum ModelError {
    /// The registry has no entry for the requested model reference.
    #[error("unknown model: {0}")]
    UnknownModel(String),

    /// HTTP layer returned a non-success status or failed mid-stream.
    #[error("download failed: {0}")]
    Download(String),

    /// All mirrors failed.
    #[error("no mirrors available for {0}")]
    NoMirrors(String),

    /// The downloaded bytes hash to a value different from the manifest.
    /// Fatal — never load a mis-hashed model.
    #[error("hash mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        /// Expected SHA-256 hex digest from the registry manifest.
        expected: String,
        /// Actual SHA-256 hex digest of the downloaded bytes.
        actual: String,
    },

    /// The downloaded file is shorter or longer than the manifest declared.
    #[error("size mismatch: expected {expected} bytes, got {actual} bytes")]
    SizeMismatch {
        /// Expected file size in bytes from the manifest.
        expected: u64,
        /// Actual file size in bytes observed.
        actual: u64,
    },

    /// GGUF header parsing / validation failed.
    #[error("gguf: {0}")]
    Gguf(String),

    /// An on-disk cache file was corrupted; it was discarded and should be re-pulled.
    #[error("corrupted cache entry at {0}: {1}")]
    CorruptedCache(PathBuf, String),

    /// Filesystem IO error (reading, writing, renaming).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Reqwest / HTTP error.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    /// Serde JSON error from the mock registry file.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// URL parse error.
    #[error("bad url: {0}")]
    BadUrl(#[from] url::ParseError),

    /// Cache configuration or state invariant was violated.
    #[error("cache: {0}")]
    Cache(String),

    /// Encoding / decoding failure (TOML, Borsh, etc.). Used for genesis
    /// registry seed parsing where an invalid fixture is a code-review
    /// mistake, not a runtime condition.
    #[error("codec: {0}")]
    Codec(String),
}
