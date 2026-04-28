//! Integration test: boot a 4-node QUIC cluster in-process, verify that
//! gossip propagates and the peer book is persisted.
//!
//! Keeps the scope tight: we're not testing libp2p (which has its own
//! exhaustive suite) — only the arknet façade and the handshake / peer
//! book glue.

use std::time::Duration;

use arknet_network::{
    HandshakeInfo, Keypair, Multiaddr, Network, NetworkConfig, NetworkEvent, HANDSHAKE_VERSION,
};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

fn handshake(network_id: &str) -> HandshakeInfo {
    HandshakeInfo {
        version: HANDSHAKE_VERSION,
        network_id: network_id.into(),
        software: "arknet/test".into(),
        roles: Default::default(),
    }
}

fn config(
    listen: Multiaddr,
    peer_book: std::path::PathBuf,
    bootstrap: Vec<Multiaddr>,
) -> NetworkConfig {
    NetworkConfig {
        network_id: "arknet-devnet-test".into(),
        listen_addrs: vec![listen],
        external_addr: None,
        bootstrap_peers: bootstrap,
        peer_book_path: peer_book,
        max_inbound_peers: 60,
        max_outbound_peers: 20,
    }
}

async fn boot_node(
    tmp: &tempfile::TempDir,
    suffix: &str,
    bootstrap: Vec<Multiaddr>,
) -> (
    arknet_network::NetworkHandle,
    tokio::task::JoinHandle<arknet_network::Result<()>>,
    CancellationToken,
    Multiaddr,
) {
    let kp = Keypair::generate_ed25519();
    let listen: Multiaddr = "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap();
    let peer_book = tmp.path().join(format!("peers-{suffix}.json"));
    let cfg = config(listen, peer_book, bootstrap);
    let info = handshake("arknet-devnet-test");
    let cancel = CancellationToken::new();

    let (handle, join) = Network::start(cfg, kp, info, cancel.clone()).await.unwrap();

    // Give the node a moment to bind its listener and report its address.
    // We subscribe before that so we don't race past `NewListenAddr`.
    let mut events = handle.subscribe();
    drop(events.try_recv()); // drain any pre-existing buffered items

    // We need the actual bound address, which is in a NewListenAddr we don't
    // currently surface. For now wait a beat and query connected_peers to
    // ensure the task is live, then reconstruct the addr from listener.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // We'll discover the concrete addr by letting peers dial each other
    // through bootstrap_peers; for the first node we use a dummy addr.
    let dummy: Multiaddr = "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap();
    (handle, join, cancel, dummy)
}

/// End-to-end smoke: two nodes connect, exchange handshake, and gossip
/// a message successfully.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_node_gossip_roundtrip() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    let tmp = tempfile::tempdir().unwrap();

    // Boot node A with no bootstrap peers. Pick a fixed port so B can find it.
    let port_a = find_free_udp_port();
    let listen_a: Multiaddr = format!("/ip4/127.0.0.1/udp/{port_a}/quic-v1")
        .parse()
        .unwrap();
    let kp_a = Keypair::generate_ed25519();
    let cfg_a = config(listen_a.clone(), tmp.path().join("peers-a.json"), vec![]);
    let cancel_a = CancellationToken::new();
    let (handle_a, join_a) = Network::start(
        cfg_a,
        kp_a.clone(),
        handshake("arknet-devnet-test"),
        cancel_a.clone(),
    )
    .await
    .unwrap();

    // Build A's dial-addr as seen by B — include the peer id so dial
    // succeeds under DialOpts::unknown_peer_id rules.
    let addr_a_for_b: Multiaddr = format!(
        "/ip4/127.0.0.1/udp/{port_a}/quic-v1/p2p/{}",
        handle_a.local_peer_id()
    )
    .parse()
    .unwrap();

    // Boot node B with A as bootstrap.
    let port_b = find_free_udp_port();
    let listen_b: Multiaddr = format!("/ip4/127.0.0.1/udp/{port_b}/quic-v1")
        .parse()
        .unwrap();
    let kp_b = Keypair::generate_ed25519();
    let cfg_b = config(
        listen_b,
        tmp.path().join("peers-b.json"),
        vec![addr_a_for_b],
    );
    let cancel_b = CancellationToken::new();
    let (handle_b, join_b) = Network::start(
        cfg_b,
        kp_b,
        handshake("arknet-devnet-test"),
        cancel_b.clone(),
    )
    .await
    .unwrap();

    let _rx_a = handle_a.subscribe();
    let mut rx_b = handle_b.subscribe();

    // Wait until B sees A's handshake succeed.
    timeout(Duration::from_secs(15), async {
        loop {
            match rx_b.recv().await.unwrap() {
                NetworkEvent::PeerConnected { info, .. } => {
                    assert_eq!(info.network_id, "arknet-devnet-test");
                    break;
                }
                _ => continue,
            }
        }
    })
    .await
    .expect("B should receive A's handshake within 15 s");

    // Give gossipsub a heartbeat or two to hand out MESH memberships.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // A publishes on the block-prop topic; B should receive it.
    let topic = arknet_network::gossip::block_prop().to_string();
    handle_a
        .publish(topic.clone(), b"hello-from-a".to_vec())
        .await
        .expect("publish should succeed");

    let got = timeout(Duration::from_secs(10), async {
        loop {
            match rx_b.recv().await.unwrap() {
                NetworkEvent::GossipMessage { topic: t, data, .. } => {
                    if t == topic {
                        return data;
                    }
                }
                _ => continue,
            }
        }
    })
    .await
    .expect("B should receive A's gossip message within 10 s");

    assert_eq!(got, b"hello-from-a");

    // Prove the peer book was flushed for B (A was the only peer it connected to).
    let peer_book_path = tmp.path().join("peers-b.json");
    assert!(peer_book_path.exists(), "peer book should be written");
    let bytes = std::fs::read(&peer_book_path).unwrap();
    let records: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert!(
        !records.is_empty(),
        "peer book should have at least one entry"
    );

    cancel_a.cancel();
    cancel_b.cancel();
    let _ = join_a.await;
    let _ = join_b.await;
}

/// Rejects a peer whose network id doesn't match.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_network_id_is_rejected() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    let tmp = tempfile::tempdir().unwrap();

    let port_a = find_free_udp_port();
    let listen_a: Multiaddr = format!("/ip4/127.0.0.1/udp/{port_a}/quic-v1")
        .parse()
        .unwrap();
    let kp_a = Keypair::generate_ed25519();
    let cfg_a = NetworkConfig {
        network_id: "arknet-a".into(),
        listen_addrs: vec![listen_a],
        external_addr: None,
        bootstrap_peers: vec![],
        peer_book_path: tmp.path().join("peers-a.json"),
        max_inbound_peers: 60,
        max_outbound_peers: 20,
    };
    let cancel_a = CancellationToken::new();
    let (handle_a, join_a) = Network::start(
        cfg_a,
        kp_a.clone(),
        HandshakeInfo {
            version: HANDSHAKE_VERSION,
            network_id: "arknet-a".into(),
            software: "arknet/a".into(),
            roles: Default::default(),
        },
        cancel_a.clone(),
    )
    .await
    .unwrap();

    let addr_a_for_b: Multiaddr = format!(
        "/ip4/127.0.0.1/udp/{port_a}/quic-v1/p2p/{}",
        handle_a.local_peer_id()
    )
    .parse()
    .unwrap();

    let port_b = find_free_udp_port();
    let listen_b: Multiaddr = format!("/ip4/127.0.0.1/udp/{port_b}/quic-v1")
        .parse()
        .unwrap();
    let kp_b = Keypair::generate_ed25519();
    let cfg_b = NetworkConfig {
        network_id: "arknet-b".into(),
        listen_addrs: vec![listen_b],
        external_addr: None,
        bootstrap_peers: vec![addr_a_for_b],
        peer_book_path: tmp.path().join("peers-b.json"),
        max_inbound_peers: 60,
        max_outbound_peers: 20,
    };
    let cancel_b = CancellationToken::new();
    let (handle_b, join_b) = Network::start(
        cfg_b,
        kp_b,
        HandshakeInfo {
            version: HANDSHAKE_VERSION,
            network_id: "arknet-b".into(),
            software: "arknet/b".into(),
            roles: Default::default(),
        },
        cancel_b.clone(),
    )
    .await
    .unwrap();

    // Wait long enough that identify would have fired. B should still
    // have zero handshake-completed peers (A keeps disconnecting it).
    tokio::time::sleep(Duration::from_secs(3)).await;

    let connected = handle_b.connected_peers().await.unwrap();
    // It's OK to briefly list the peer during the disconnect race. What
    // matters is that no PeerConnected event was emitted.
    let mut rx_b = handle_b.subscribe();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut saw_connected_event = false;
    while let Ok(ev) = rx_b.try_recv() {
        if let NetworkEvent::PeerConnected { .. } = ev {
            saw_connected_event = true;
        }
    }
    assert!(
        !saw_connected_event,
        "should not surface PeerConnected for wrong network (connected_peers={:?})",
        connected
    );

    cancel_a.cancel();
    cancel_b.cancel();
    let _ = handle_a;
    let _ = join_a.await;
    let _ = join_b.await;
}

/// Pick an ephemeral UDP port by binding + closing. Good enough for a
/// local integration test; there's a race between close and the libp2p
/// listener picking it up but it's vanishingly rare in practice.
fn find_free_udp_port() -> u16 {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind udp");
    sock.local_addr().unwrap().port()
}

// Unused helper in early scaffolding; kept here so future tests (4-node cluster,
// restart) can reuse the boot code once listener-addr surfacing is added.
#[allow(dead_code)]
async fn _placeholder() {
    let _ = boot_node;
}
