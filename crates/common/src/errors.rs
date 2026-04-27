//! Error hierarchy shared across the workspace.
//!
//! Pattern:
//! - Each crate defines its own domain error enum via [`thiserror`].
//! - Crate-level errors implement `From` into [`CommonError`] only where they
//!   need to surface at public boundaries.
//! - Never use `anyhow` inside libraries — reserve it for `main.rs`.

use thiserror::Error;

/// Top-level error type for the `arknet-common` crate.
///
/// This enum is intentionally small — most errors belong in a dedicated
/// domain enum in the crate that owns the operation.
#[derive(Debug, Error)]
pub enum CommonError {
    /// An invalid argument was passed to a protocol function.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// A value exceeded a protocol-defined limit.
    #[error("value out of range: {0}")]
    OutOfRange(String),

    /// Borsh (de)serialization failed.
    #[error("borsh (de)serialization error: {0}")]
    Borsh(String),

    /// JSON (de)serialization failed.
    #[error("json (de)serialization error: {0}")]
    Json(String),

    /// TOML parse error.
    #[error("toml parse error: {0}")]
    Toml(String),

    /// Config file could not be loaded or validated.
    #[error("config error: {0}")]
    Config(String),

    /// I/O failure (file, disk, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// Note: `borsh::io::Error` is a re-export of `std::io::Error`, so the `#[from]`
// on the `Io` variant already gives us `From<borsh::io::Error>`. See the
// `Borsh` variant for the human-readable path used by the serialization helpers.

impl From<serde_json::Error> for CommonError {
    fn from(e: serde_json::Error) -> Self {
        CommonError::Json(e.to_string())
    }
}

/// Protocol-wide result type.
pub type Result<T, E = CommonError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_include_context() {
        let e = CommonError::InvalidArgument("x must be positive".into());
        assert_eq!(e.to_string(), "invalid argument: x must be positive");
    }

    #[test]
    fn out_of_range_formats() {
        let e = CommonError::OutOfRange("height > u64::MAX".into());
        assert!(e.to_string().contains("out of range"));
    }

    #[test]
    fn io_error_conversion() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");
        let e: CommonError = io.into();
        assert!(matches!(e, CommonError::Io(_)));
    }
}
