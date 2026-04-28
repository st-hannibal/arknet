//! Top-level error type for the node binary.
//!
//! Several variants here are consumed in Days 2-10 (config load errors
//! come from `arknet init` / `config check`, `RoleNotImplemented` fires
//! from `start` for non-compute roles, etc.). Suppress dead-code warns
//! during the Day 1 scaffold so clippy -D warnings stays green.

#![allow(dead_code)]

use thiserror::Error;

/// Result alias for this crate.
pub type Result<T, E = NodeError> = std::result::Result<T, E>;

/// Everything the node binary's CLI surface might fail on.
#[derive(Debug, Error)]
pub enum NodeError {
    /// A filesystem / data-dir-layout failure.
    #[error("paths: {0}")]
    Paths(String),

    /// Failure loading or validating `node.toml`.
    #[error("config: {0}")]
    Config(String),

    /// Underlying `arknet-common::CommonError` bubbled up from the shared
    /// config loader.
    #[error("config: {0}")]
    CommonConfig(#[from] arknet_common::errors::CommonError),

    /// Model-manager error (resolve / pull / cache).
    #[error("model-manager: {0}")]
    ModelManager(#[from] arknet_model_manager::ModelError),

    /// Inference error (load / decode).
    #[error("inference: {0}")]
    Inference(#[from] arknet_inference::InferenceError),

    /// `ModelRef::parse` failed — the `--model` argument was malformed.
    #[error("invalid model reference: {0}")]
    ModelRef(String),

    /// Tokio join error.
    #[error("join: {0}")]
    Join(#[from] tokio::task::JoinError),

    /// Underlying filesystem error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// TOML parse/emit error.
    #[error("toml: {0}")]
    Toml(#[from] toml::ser::Error),

    /// JSON (de)serialization error — for the HTTP endpoints and `status`.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// HTTP client error (reqwest) — `status` / `health` commands.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    /// The CLI was invoked with a role that's not implemented in Phase 0.
    #[error("role {0} is not implemented until Phase 1; only `compute` is live at Phase 0")]
    RoleNotImplemented(String),

    /// A request targeted a node feature that Phase 0 doesn't expose.
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    /// Networking subsystem failure (transport, handshake, peer book).
    #[error("network: {0}")]
    Network(#[from] arknet_network::NetworkError),
}
