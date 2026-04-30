//! Errors produced by the inference engine.

use thiserror::Error;

/// Result alias for this crate.
pub type Result<T, E = InferenceError> = std::result::Result<T, E>;

/// Every failure mode a caller of the inference engine might see.
#[derive(Debug, Error)]
pub enum InferenceError {
    /// llama.cpp failed to load the model file.
    #[error("model load failed: {0}")]
    ModelLoad(String),

    /// Context init failed (usually OOM or bad params).
    #[error("context init failed: {0}")]
    ContextInit(String),

    /// Tokenization rejected the input (too long, bad encoding, ...).
    #[error("tokenize: {0}")]
    Tokenize(String),

    /// `llama_decode` returned a non-zero status.
    #[error("decode failed: code={code}")]
    Decode {
        /// llama.cpp status code.
        code: i32,
    },

    /// Client dropped the stream mid-generation.
    #[error("cancelled by caller")]
    Cancelled,

    /// A path is reserved for a later phase and not implemented yet.
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    /// KV-cache checkpoint save/restore failure.
    #[error("checkpoint: {0}")]
    Checkpoint(String),

    /// Underlying model-manager error while resolving / pulling a model.
    #[error("model manager: {0}")]
    ModelManager(#[from] arknet_model_manager::ModelError),

    /// IO error while reading a model or tokenizer vocab.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
