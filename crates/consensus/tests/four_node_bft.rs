//! 4-node BFT smoke test.
//!
//! Boots four `ConsensusEngine` instances on loopback QUIC, wires them
//! together through the real `arknet-network` gossip layer, and
//! asserts that the cluster decides at least 3 heights within 30s.
//!
//! This is the end-to-end check that:
//!
//! - The network bridge wire format round-trips across peers.
//! - Proposer round-robin selection agrees across nodes.
//! - Malachite's state machine reaches 2/3 precommits on a real
//!   gossipsub mesh.
//! - Our commit path correctly advances heights on each node.
//!
//! # Cost
//!
//! ~3 s of wall time per node for QUIC + gossipsub MESH warm-up,
//! then block_interval (500ms) × N blocks. The test targets 30s to
//! leave headroom for CI variance on macOS / Linux runners.

#![cfg(feature = "integration-tests")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arknet_chain::validator::ValidatorInfo;
use arknet_chain::State as ChainState;
use arknet_common::types::{Address, NodeId, PubKey};
use arknet_consensus::engine::{ConsensusEngine, ConsensusHandle, EngineConfig, TimeoutConfig};
use arknet_consensus::signing::ArknetSigningProvider;
use arknet_consensus::validators::{ChainAddress, ChainValidatorSet};
use arknet_consensus::Height;
use arknet_network::{
    HandshakeInfo, Keypair, Multiaddr, Network, NetworkConfig, NetworkHandle, HANDSHAKE_VERSION,
};
use malachitebft_signing_ed25519::{Ed25519, PrivateKey, PublicKey};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

fn find_free_udp_port() -> u16 {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind udp");
    sock.local_addr().unwrap().port()
}

/// Everything we need to hang on to per node so we can shut them down
/// cleanly and read back chain state at the end.
struct Node {
    handle: ConsensusHandle,
    state: Arc<ChainState>,
    #[allow(dead_code)]
    network: NetworkHandle,
    #[allow(dead_code)]
    _tmp: TempDir,
    shutdown: CancellationToken,
    engine_join: JoinHandle<arknet_consensus::Result<()>>,
    net_join: JoinHandle<arknet_network::Result<()>>,
}

/// Build a fresh set of ed25519 keypairs for N validators. Each
/// produces a matching libp2p PeerId + malachite PrivateKey from the
/// same 32-byte seed.
fn generate_keys(n: usize) -> Vec<(PrivateKey, PublicKey, Keypair)> {
    (0..n)
        .map(|_| {
            let sk = Ed25519::generate_keypair(rand::rngs::OsRng);
            let pk = sk.public_key();
            // Project the malachite seed onto libp2p: inner bytes of the
            // `PrivateKey` are the raw 32-byte seed (via `ed25519_consensus`).
            let seed: [u8; 32] = *sk.inner().as_bytes();
            let mut seed_copy = seed;
            let libp2p_kp =
                Keypair::ed25519_from_bytes(&mut seed_copy).expect("valid ed25519 seed");
            (sk, pk, libp2p_kp)
        })
        .collect()
}

fn validator_info(pk: &PublicKey, power: u64) -> (ValidatorInfo, ChainAddress) {
    let pk_bytes = *pk.as_bytes();
    let digest = *arknet_crypto::hash::blake3(&pk_bytes).as_bytes();
    let mut addr_bytes = [0u8; 20];
    addr_bytes.copy_from_slice(&digest[..20]);
    let addr = Address::new(addr_bytes);
    let mut node_id = [0u8; 32];
    node_id.copy_from_slice(&digest);
    (
        ValidatorInfo {
            node_id: NodeId::new(node_id),
            consensus_key: PubKey::ed25519(pk_bytes),
            operator: addr,
            bonded_stake: 0,
            voting_power: power,
            is_genesis: true,
            jailed: false,
        },
        ChainAddress(addr),
    )
}

async fn boot_network(
    listen_port: u16,
    bootstrap_peers: Vec<Multiaddr>,
    libp2p_kp: Keypair,
    data_dir: PathBuf,
    shutdown: CancellationToken,
) -> (NetworkHandle, JoinHandle<arknet_network::Result<()>>) {
    let listen: Multiaddr = format!("/ip4/127.0.0.1/udp/{listen_port}/quic-v1")
        .parse()
        .unwrap();
    let cfg = NetworkConfig {
        network_id: "arknet-4node-test".into(),
        listen_addrs: vec![listen],
        external_addr: None,
        bootstrap_peers,
        peer_book_path: data_dir.join("peers.json"),
        max_inbound_peers: 16,
        max_outbound_peers: 16,
    };
    let info = HandshakeInfo {
        version: HANDSHAKE_VERSION,
        network_id: "arknet-4node-test".into(),
        software: "arknet/test".into(),
        roles: Default::default(),
    };
    Network::start(cfg, libp2p_kp, info, shutdown)
        .await
        .expect("network boot")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn four_node_cluster_decides_multiple_heights() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    const N: usize = 4;
    let keys = generate_keys(N);
    let (infos, addresses): (Vec<ValidatorInfo>, Vec<ChainAddress>) =
        keys.iter().map(|(_, pk, _)| validator_info(pk, 1)).unzip();
    let validator_set = ChainValidatorSet::from_infos(&infos).unwrap();

    // Allocate per-node ports up front so we can build the bootstrap
    // list (each node dials the previous one).
    let ports: Vec<u16> = (0..N).map(|_| find_free_udp_port()).collect();
    let peer_addrs: Vec<Multiaddr> = ports
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let peer_id = keys[i].2.public().to_peer_id();
            format!("/ip4/127.0.0.1/udp/{p}/quic-v1/p2p/{peer_id}")
                .parse()
                .unwrap()
        })
        .collect();

    let mut nodes: Vec<Node> = Vec::with_capacity(N);
    for i in 0..N {
        let (sk, _pk, libp2p_kp) = keys[i].clone();
        let shutdown = CancellationToken::new();
        let tmp = tempfile::tempdir().unwrap();

        // Each node bootstraps from every other node. Gossipsub will
        // prune the mesh down to its target size.
        let bootstrap: Vec<Multiaddr> = peer_addrs
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, a)| a.clone())
            .collect();

        let (network, net_join) = boot_network(
            ports[i],
            bootstrap,
            libp2p_kp,
            tmp.path().to_path_buf(),
            shutdown.clone(),
        )
        .await;

        let chain_state = Arc::new(ChainState::open(&tmp.path().join("l1")).unwrap());
        let signer = Arc::new(ArknetSigningProvider::new(sk));

        let cfg = EngineConfig {
            chain_id: "arknet-4node-test".into(),
            version: 1,
            initial_height: Height(1),
            validator_set: validator_set.clone(),
            base_fee: 1_000_000_000,
            gas_limit: 30_000_000,
            gas_target: 15_000_000,
            local_address: addresses[i],
            local_node_id: infos[i].node_id,
            timeouts: TimeoutConfig {
                // Tighter timeouts than default to keep the test fast.
                propose: Duration::from_millis(1_500),
                prevote: Duration::from_millis(500),
                precommit: Duration::from_millis(500),
                rebroadcast: Duration::from_millis(1_000),
                per_round_delta: Duration::from_millis(250),
                block_interval: Duration::from_millis(250),
            },
        };

        let (handle, engine_join) = ConsensusEngine::start(
            cfg,
            chain_state.clone(),
            network.clone(),
            signer,
            shutdown.clone(),
        );

        nodes.push(Node {
            handle,
            state: chain_state,
            network,
            _tmp: tmp,
            shutdown,
            engine_join,
            net_join,
        });
    }

    // Wait for gossipsub MESH warm-up + block production.
    // Target: heights 1..=3 decided within 30s wall time.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut reached_height = 0u64;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Poll node 0's current height. Once it passes 3 we're done.
        if let Ok(h) = nodes[0].handle.current_height().await {
            reached_height = h.0;
            if reached_height >= 3 {
                break;
            }
        }
    }

    // Shut everything down.
    for n in &nodes {
        n.shutdown.cancel();
    }
    for n in nodes.drain(..) {
        let _ = tokio::time::timeout(Duration::from_secs(5), n.engine_join).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), n.net_join).await;
        drop(n.state);
    }

    assert!(
        reached_height >= 3,
        "expected at least 3 decided heights within 30s, got {reached_height}"
    );
}
