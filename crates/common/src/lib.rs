//! Shared types, errors, and utilities used across the arknet workspace.
//!
//! This crate defines the vocabulary of the protocol: addresses, hashes, amounts,
//! identifiers, error hierarchies, config schema, and serialization helpers.
//! Every other crate in the workspace depends on it.
//!
//! See [`docs/PROTOCOL_SPEC.md`](../../../docs/PROTOCOL_SPEC.md) for canonical
//! definitions and [`docs/SECURITY.md`](../../../docs/SECURITY.md) §12 for the
//! post-quantum migration plan.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod errors;
pub mod serialization;
pub mod types;

// Common re-exports for ergonomic downstream usage.
pub use errors::{CommonError, Result};
pub use types::{
    Address, Amount, ChannelId, Hash256, Height, JobId, KemScheme, NodeId, PoolId, PubKey,
    RoleBitmap, Signature, SignatureScheme, Timestamp, VrfScheme, ARK_SUPPLY_CAP, ATOMS_PER_ARK,
};
