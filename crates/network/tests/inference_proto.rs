//! 2-node integration test for `/arknet/inference/1`.
//!
//! Boots two libp2p swarms with only the request-response behaviour
//! attached, the client dials the server, sends a small borsh-framed
//! request, and verifies the response round-trips bit-identically.
//!
//! The test is kept transport-minimal (TCP + noise + yamux, no full
//! arknet behaviour stack) so any regression in the codec / protocol
//! name / size caps is caught independently of the consensus gossip
//! path.

use std::time::Duration;

use arknet_network::{build_inference_behaviour, InferenceBehaviour, INFERENCE_PROTOCOL};
use futures::StreamExt;
use libp2p::request_response::{Event as RrEvent, Message as RrMessage};
use libp2p::swarm::SwarmEvent;
use libp2p::{identity::Keypair, Multiaddr, Swarm, SwarmBuilder};
use tokio::time::timeout;

fn build_swarm() -> Swarm<InferenceBehaviour> {
    let keypair = Keypair::generate_ed25519();
    SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            libp2p::tcp::Config::default().nodelay(true),
            libp2p::noise::Config::new,
            libp2p::yamux::Config::default,
        )
        .expect("tcp transport")
        .with_behaviour(|_| build_inference_behaviour())
        .expect("behaviour")
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inference_request_response_roundtrip() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    // Server swarm.
    let mut server = build_swarm();
    let listen_addr: Multiaddr = "/ip4/127.0.0.1/tcp/0".parse().unwrap();
    server.listen_on(listen_addr.clone()).expect("listen");

    // Discover the actual bound address.
    let server_addr: Multiaddr = timeout(Duration::from_secs(5), async {
        loop {
            match server.next().await {
                Some(SwarmEvent::NewListenAddr { address, .. }) => return address,
                _ => continue,
            }
        }
    })
    .await
    .expect("server listener");

    // Client swarm.
    let mut client = build_swarm();
    let server_peer_id = *server.local_peer_id();
    client.dial(server_addr.clone()).expect("dial");

    let payload = b"hello-verifier".to_vec();
    let expected_response = b"response-bytes".to_vec();

    // Drive both swarms concurrently; exit once the client receives a
    // response event and the test is done.
    let drive = async {
        let mut request_id = None;
        // Send the request as soon as we're connected to the server.
        let mut sent = false;
        loop {
            tokio::select! {
                ev = server.next() => {
                    match ev {
                        Some(SwarmEvent::Behaviour(RrEvent::Message { message, .. })) => {
                            if let RrMessage::Request { request, channel, .. } = message {
                                assert_eq!(request, payload);
                                server
                                    .behaviour_mut()
                                    .send_response(channel, expected_response.clone())
                                    .expect("send response");
                            }
                        }
                        Some(_) => continue,
                        None => return Err("server stream ended"),
                    }
                }
                ev = client.next() => {
                    match ev {
                        Some(SwarmEvent::ConnectionEstablished { peer_id, .. }) => {
                            if peer_id == server_peer_id && !sent {
                                let id = client
                                    .behaviour_mut()
                                    .send_request(&server_peer_id, payload.clone());
                                request_id = Some(id);
                                sent = true;
                            }
                        }
                        Some(SwarmEvent::Behaviour(RrEvent::Message { message, .. })) => {
                            if let RrMessage::Response { request_id: got_id, response } = message {
                                if Some(got_id) == request_id {
                                    assert_eq!(response, expected_response);
                                    return Ok(());
                                }
                            }
                        }
                        Some(_) => continue,
                        None => return Err("client stream ended"),
                    }
                }
            }
        }
    };

    timeout(Duration::from_secs(15), drive)
        .await
        .expect("roundtrip within 15 s")
        .expect("roundtrip ok");
}

#[test]
fn inference_protocol_constant_is_versioned() {
    // Guard against accidental rename of the wire path.
    assert_eq!(INFERENCE_PROTOCOL, "/arknet/inference/1");
}
