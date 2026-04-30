//! Error types for the networking layer.

use std::net::AddrParseError;

use thiserror::Error;

/// Fallible result type for network operations.
pub type Result<T> = std::result::Result<T, NetworkError>;

/// Errors surfaced by the arknet networking layer.
#[derive(Debug, Error)]
pub enum NetworkError {
    /// Config validation failed (bad multiaddr, incompatible caps, etc.).
    #[error("network config: {0}")]
    Config(String),

    /// Peer book (JSON) read/write failure.
    #[error("peer book: {0}")]
    PeerBook(String),

    /// libp2p transport failure (dial, listen).
    #[error("transport: {0}")]
    Transport(String),

    /// Behaviour construction failure (gossipsub, kademlia, identify).
    #[error("behaviour: {0}")]
    Behaviour(String),

    /// Handshake rejected — network-id / chain-id mismatch.
    #[error("handshake rejected from {peer}: {reason}")]
    Handshake {
        /// Remote peer id hex.
        peer: String,
        /// Human-readable reason for rejection.
        reason: String,
    },

    /// Background network task exited.
    #[error("network task exited: {0}")]
    TaskExited(String),

    /// I/O error (peer-book file, socket bind, etc.).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// JSON encode / decode failure on peer-book or handshake payloads.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// Address parsing failure.
    #[error("address parse: {0}")]
    AddrParse(#[from] AddrParseError),
}
