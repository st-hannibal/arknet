//! Error types for the consensus engine.

use thiserror::Error;

/// Fallible result type used throughout [`crate`].
pub type Result<T> = std::result::Result<T, ConsensusError>;

/// Everything the consensus engine might fail on.
///
/// Malachite's raw driver is infallible for most inputs — when an error
/// shows up here it is almost always a wiring mistake (wrong signing
/// scheme, validator not in set, etc.) rather than a transient Byzantine
/// condition. Byzantine misbehaviour is handled by the state machine
/// itself and surfaces as evidence, not as errors.
#[derive(Debug, Error)]
pub enum ConsensusError {
    /// Signing-provider returned an error (key not loaded, signature
    /// encoding mismatch, ed25519 verification failed).
    #[error("signing: {0}")]
    Signing(String),

    /// A vote / proposal was received from an address that is not in
    /// the active validator set. Usually a bug in the network bridge
    /// deserialization, not a Byzantine event.
    #[error("unknown validator: {0}")]
    UnknownValidator(String),

    /// Block construction or replay failed (state-store error, mempool
    /// corruption, etc.).
    #[error("block builder: {0}")]
    BlockBuilder(String),

    /// Underlying chain state returned an error. Fatal — the engine
    /// cannot continue with a broken state DB.
    #[error("chain state: {0}")]
    ChainState(#[from] arknet_chain::errors::ChainError),

    /// Network layer failed to deliver a consensus message (disconnect,
    /// backpressure, serialization).
    #[error("network: {0}")]
    Network(#[from] arknet_network::NetworkError),

    /// Borsh encode / decode failure on a consensus message.
    #[error("codec: {0}")]
    Codec(String),

    /// Malachite core surfaced an error.
    #[error("malachite: {0}")]
    Malachite(String),

    /// A configuration invariant was violated (empty validator set,
    /// signing key missing, threshold params out of range).
    #[error("config: {0}")]
    Config(String),

    /// Internal task died (tokio oneshot recv failure, etc.).
    #[error("engine task: {0}")]
    EngineTask(String),
}
