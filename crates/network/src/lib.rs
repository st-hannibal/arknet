//! arknet peer-to-peer networking.
//!
//! # Layout
//!
//! - [`config`] — [`NetworkConfig`] + multiaddr parsing helpers.
//! - [`errors`] — [`NetworkError`] shared by every module.
//! - [`peer`] — persistent [`peer::PeerBook`] (JSON, reloaded on boot).
//! - [`handshake`] — arknet-specific payload piggybacked on libp2p identify.
//! - [`gossip`] — GossipSub topic catalogue (versioned names).
//! - [`behaviour`] — composed `NetworkBehaviour` (gossipsub + kad + identify + ping).
//! - [`network`] — the public façade: [`Network`] boot + [`NetworkHandle`] surface.
//!
//! Downstream crates should depend only on [`NetworkHandle`] and
//! [`NetworkEvent`] — everything else is implementation detail and
//! subject to change when libp2p is upgraded.
//!
//! # Scope (Phase 1 Week 5-6)
//!
//! - QUIC primary + TCP fallback, both with Noise + yamux.
//! - GossipSub for 6 topics (tx, block, vote, pool, receipt, gov).
//! - Kademlia DHT for peer discovery.
//! - identify for agent-version exchange (our handshake blob is stuffed
//!   into `agent_version`).
//! - Persistent peer book at `<data-dir>/peers.json`.
//!
//! Out of scope until later weeks:
//!
//! - Encrypted direct channels for prompt forwarding (Phase 2).
//! - AutoNAT / relay / dcutr for NAT traversal (Phase 2).
//! - Per-topic selective subscription keyed on role bitmap (Week 10).
//! - Per-peer scoring and automatic ban list (Week 11+).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod behaviour;
pub mod config;
pub mod errors;
pub mod gossip;
pub mod handshake;
pub mod inference_proto;
pub mod network;
pub mod peer;

pub use config::{parse_multiaddrs, NetworkConfig, NetworkId};
pub use errors::{NetworkError, Result};
pub use handshake::{HandshakeInfo, PeerRoles, HANDSHAKE_VERSION};
pub use inference_proto::{
    build_inference_behaviour, InferenceBehaviour, InferenceCodec, InferenceResponse, WireRequest,
    WireResponse, INFERENCE_PROTOCOL, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
};
pub use network::{
    default_topics, InboundInferenceRequest, InferenceChannels, InferenceResponseEvent, Network,
    NetworkEvent, NetworkHandle,
};
pub use peer::{PeerBook, PeerRecord};

// Re-export the core libp2p types a caller needs without forcing them
// to add libp2p as a direct dependency.
pub use libp2p::{identity, identity::Keypair, request_response, Multiaddr, PeerId};
