//! KV-cache checkpoint API.
//!
//! Phase 0 ships the trait only — the body is a `NotImplemented` stub.
//! Phase 1 fills in real serialization so routers can fail a request
//! over to a backup compute node mid-generation.
//!
//! The API is locked now so downstream code (router / compute) can
//! compile against it without changes when the real implementation
//! lands.

use bytes::Bytes;

use crate::errors::{InferenceError, Result};

/// A session whose state can be snapshotted and restored.
///
/// Phase 0: both methods return `InferenceError::NotImplemented`. Phase
/// 1 adds real KV-cache serialization. Downstream code should depend
/// on this trait, not on the concrete `Session` type, so the Phase-1
/// swap is transparent.
pub trait CheckpointableSession {
    /// Produce a serialized snapshot of the session's KV cache + cursor.
    fn snapshot(&self) -> Result<Bytes>;

    /// Restore a session from a snapshot produced by [`snapshot`].
    fn restore(bytes: &[u8]) -> Result<Self>
    where
        Self: Sized;
}

/// Phase-0 stub holder so downstream code can reference the type even
/// though no real sessions implement it yet.
pub struct Phase0CheckpointStub;

impl CheckpointableSession for Phase0CheckpointStub {
    fn snapshot(&self) -> Result<Bytes> {
        Err(InferenceError::NotImplemented(
            "KV-cache checkpoint is a Phase 1 feature",
        ))
    }

    fn restore(_bytes: &[u8]) -> Result<Self> {
        Err(InferenceError::NotImplemented(
            "KV-cache checkpoint is a Phase 1 feature",
        ))
    }
}
