//! Wires [`arknet_network::Network`] into the node binary.
//!
//! Translates the operator-facing [`NetworkSection`] + [`NodeSection`] into
//! an [`arknet_network::NetworkConfig`] and boots the network subsystem.
//! Also generates (or reloads) the node's libp2p Ed25519 keypair — each
//! arknet node has a single consensus key; the p2p layer uses it for
//! PeerId derivation so the p2p identity and the consensus identity are
//! the same.

use std::path::Path;

use arknet_common::config::{NetworkSection, NodeSection};
use arknet_network::{
    parse_multiaddrs, HandshakeInfo, Keypair, Network, NetworkConfig, NetworkHandle, PeerRoles,
    HANDSHAKE_VERSION,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::errors::{NodeError, Result};

/// Boot the network subsystem and return its handle + the task
/// JoinHandle the caller awaits on shutdown.
pub async fn start_network(
    data_dir: &Path,
    node: &NodeSection,
    net: &NetworkSection,
    roles: &arknet_common::config::RolesSection,
    shutdown: CancellationToken,
) -> Result<(NetworkHandle, JoinHandle<arknet_network::Result<()>>)> {
    // Phase 1 Week 5-6: generate a fresh keypair every start. Key
    // persistence + loading from `<data-dir>/keys/p2p.key` ships with
    // the operator key management work in Week 9.
    let keypair = Keypair::generate_ed25519();

    let listen_addrs = net_listen_addrs(net)?;
    let bootstrap = parse_multiaddrs(&net.bootstrap_peers)
        .map_err(|e| NodeError::Config(format!("bootstrap_peers: {e}")))?;

    let external_addr = match &net.external_address {
        Some(s) => Some(
            s.parse()
                .map_err(|e| NodeError::Config(format!("external_address {s:?}: {e}")))?,
        ),
        None => None,
    };

    let config = NetworkConfig {
        network_id: node.network.clone(),
        listen_addrs,
        external_addr,
        bootstrap_peers: bootstrap,
        peer_book_path: data_dir.join("peers.json"),
        max_inbound_peers: net.max_inbound_peers,
        max_outbound_peers: net.max_outbound_peers,
    };

    let handshake = HandshakeInfo {
        version: HANDSHAKE_VERSION,
        network_id: node.network.clone(),
        software: format!("arknet/{}", env!("CARGO_PKG_VERSION")),
        roles: PeerRoles {
            validator: roles.validator,
            router: roles.router,
            compute: roles.compute,
            verifier: roles.verifier,
        },
    };

    info!(
        peer_id = %keypair.public().to_peer_id(),
        network = %node.network,
        "booting p2p network"
    );

    let (handle, join) = Network::start(config, keypair, handshake, shutdown)
        .await
        .map_err(NodeError::from)?;

    Ok((handle, join))
}

/// Translate the operator-facing `p2p_listen` (host:port string) into a
/// pair of libp2p multiaddrs — one QUIC, one TCP — so the node reaches
/// both types of peers.
///
/// Format accepted: `host:port` (matching the existing node.toml shape).
/// If an operator wants finer control they can set `external_address` to
/// a raw multiaddr and we trust it verbatim.
fn net_listen_addrs(net: &NetworkSection) -> Result<Vec<arknet_network::Multiaddr>> {
    let (host, port) = net.p2p_listen.rsplit_once(':').ok_or_else(|| {
        NodeError::Config(format!(
            "p2p_listen must be host:port, got {:?}",
            net.p2p_listen
        ))
    })?;
    let port: u16 = port
        .parse()
        .map_err(|e| NodeError::Config(format!("p2p_listen port {port:?}: {e}")))?;

    let quic = format!("/ip4/{host}/udp/{port}/quic-v1")
        .parse()
        .map_err(|e| NodeError::Config(format!("build quic multiaddr: {e}")))?;
    let tcp = format!("/ip4/{host}/tcp/{port}")
        .parse()
        .map_err(|e| NodeError::Config(format!("build tcp multiaddr: {e}")))?;
    Ok(vec![quic, tcp])
}
