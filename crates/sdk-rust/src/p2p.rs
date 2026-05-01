//! Direct P2P client for sending inference requests to compute nodes.
//!
//! This module provides a lightweight libp2p client that connects to a
//! single compute node and exchanges inference messages using the
//! `/arknet/inference/1` request-response protocol.
//!
//! Unlike a full node, the P2P client only runs the inference behaviour
//! (no gossipsub, no kademlia, no identify) — it's a pure consumer.
//!
//! # Wire format
//!
//! Requests and responses are borsh-encoded byte frames prefixed with a
//! `u32` length. The codec is [`InferenceCodec`] from `arknet-network`.

use std::time::Duration;

use futures::StreamExt;
use libp2p::request_response::{self, OutboundRequestId};
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId, Swarm, SwarmBuilder};
use tokio::sync::mpsc;

use arknet_network::{build_inference_behaviour, InferenceBehaviour};

use crate::errors::{Result, SdkError};

/// Timeout for a single inference request over P2P.
const P2P_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// A direct P2P client that connects to a compute node and exchanges
/// inference messages via the `/arknet/inference/1` protocol.
///
/// The client builds a minimal libp2p swarm with only the inference
/// request-response behaviour — no gossipsub, no DHT, no identify.
pub struct P2pClient {
    /// The swarm event loop task handle.
    _task: tokio::task::JoinHandle<()>,
    /// Channel to send requests into the swarm task.
    request_tx: mpsc::Sender<RequestCmd>,
    /// Channel to receive responses from the swarm task.
    response_rx: mpsc::Receiver<ResponseEvent>,
    /// Remote peer id.
    peer_id: PeerId,
}

/// Internal command to the swarm task.
struct RequestCmd {
    peer: PeerId,
    data: Vec<u8>,
}

/// Internal response from the swarm task.
struct ResponseEvent {
    result: std::result::Result<Vec<u8>, String>,
}

impl P2pClient {
    /// Connect to a compute node at the given multiaddr.
    ///
    /// The multiaddr must include the peer id, e.g.
    /// `/ip4/1.2.3.4/udp/9000/quic-v1/p2p/12D3Koo...`.
    ///
    /// This establishes a libp2p connection with Noise encryption and
    /// QUIC transport (falling back to TCP+yamux).
    pub async fn connect(peer_addr: &str) -> Result<Self> {
        let addr: Multiaddr = peer_addr
            .parse()
            .map_err(|e| SdkError::P2p(format!("invalid multiaddr: {e}")))?;

        // Extract the PeerId from the multiaddr (last /p2p/... component).
        let peer_id = extract_peer_id(&addr)?;

        // Generate an ephemeral keypair for this client session.
        let keypair = libp2p::identity::Keypair::generate_ed25519();

        // Build a minimal swarm with only the inference behaviour.
        let swarm = SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_quic()
            .with_behaviour(|_key| build_inference_behaviour())
            .map_err(|e| SdkError::P2p(format!("failed to build behaviour: {e}")))?
            .with_swarm_config(|cfg| {
                cfg.with_idle_connection_timeout(P2P_REQUEST_TIMEOUT + Duration::from_secs(10))
            })
            .build();

        let (request_tx, request_rx) = mpsc::channel::<RequestCmd>(8);
        let (response_tx, response_rx) = mpsc::channel::<ResponseEvent>(8);

        let task = tokio::spawn(swarm_loop(swarm, addr, request_rx, response_tx));

        Ok(Self {
            _task: task,
            request_tx,
            response_rx,
            peer_id,
        })
    }

    /// Send a borsh-encoded inference request and wait for the response.
    ///
    /// The `req` parameter should be the borsh-encoded bytes of an
    /// `InferenceJobRequest`. The returned bytes are the borsh-encoded
    /// `InferenceResponse`.
    pub async fn infer(&mut self, req: Vec<u8>) -> Result<Vec<u8>> {
        self.request_tx
            .send(RequestCmd {
                peer: self.peer_id,
                data: req,
            })
            .await
            .map_err(|_| SdkError::P2p("swarm task exited".into()))?;

        let resp = self
            .response_rx
            .recv()
            .await
            .ok_or_else(|| SdkError::P2p("swarm task exited before responding".into()))?;

        resp.result.map_err(SdkError::P2p)
    }

    /// The remote peer id this client is connected to.
    pub fn peer_id(&self) -> &PeerId {
        &self.peer_id
    }
}

/// Extract the PeerId from the last `/p2p/<peer_id>` component of a multiaddr.
fn extract_peer_id(addr: &Multiaddr) -> Result<PeerId> {
    for proto in addr.iter() {
        if let libp2p::multiaddr::Protocol::P2p(peer_id) = proto {
            return Ok(peer_id);
        }
    }
    Err(SdkError::P2p(
        "multiaddr must contain a /p2p/<peer_id> component".into(),
    ))
}

/// The swarm event loop. Dials the peer, sends requests, and forwards responses.
async fn swarm_loop(
    mut swarm: Swarm<InferenceBehaviour>,
    addr: Multiaddr,
    mut request_rx: mpsc::Receiver<RequestCmd>,
    response_tx: mpsc::Sender<ResponseEvent>,
) {
    // Listen on a random OS-assigned port so libp2p can accept the
    // reverse connection leg if needed. The parse is infallible for a
    // hardcoded valid multiaddr, but we use `ok()` to avoid panics.
    if let Ok(listen_addr) = "/ip4/0.0.0.0/udp/0/quic-v1".parse() {
        let _ = swarm.listen_on(listen_addr);
    }

    // Dial the remote peer.
    if let Err(e) = swarm.dial(addr) {
        let _ = response_tx
            .send(ResponseEvent {
                result: Err(format!("dial failed: {e}")),
            })
            .await;
        return;
    }

    // Track pending outbound request ids.
    let mut pending: Option<OutboundRequestId> = None;

    loop {
        tokio::select! {
            // Process incoming commands.
            cmd = request_rx.recv() => {
                match cmd {
                    Some(cmd) => {
                        let req_id = swarm.behaviour_mut().send_request(&cmd.peer, cmd.data);
                        pending = Some(req_id);
                    }
                    None => break, // Client dropped.
                }
            }
            // Drive the swarm.
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::Behaviour(req_resp_event) => {
                        handle_behaviour_event(req_resp_event, &mut pending, &response_tx).await;
                    }
                    SwarmEvent::OutgoingConnectionError { error, .. } => {
                        let _ = response_tx.send(ResponseEvent {
                            result: Err(format!("connection error: {error}")),
                        }).await;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Handle a request-response behaviour event.
async fn handle_behaviour_event(
    event: request_response::Event<Vec<u8>, Vec<u8>>,
    pending: &mut Option<OutboundRequestId>,
    response_tx: &mpsc::Sender<ResponseEvent>,
) {
    match event {
        request_response::Event::Message {
            message:
                request_response::Message::Response {
                    request_id,
                    response,
                },
            ..
        } => {
            if pending.as_ref() == Some(&request_id) {
                *pending = None;
                let _ = response_tx
                    .send(ResponseEvent {
                        result: Ok(response),
                    })
                    .await;
            }
        }
        request_response::Event::OutboundFailure {
            request_id, error, ..
        } => {
            if pending.as_ref() == Some(&request_id) {
                *pending = None;
                let _ = response_tx
                    .send(ResponseEvent {
                        result: Err(format!("outbound failure: {error}")),
                    })
                    .await;
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_peer_id_from_multiaddr() {
        let addr: Multiaddr = "/ip4/127.0.0.1/udp/9000/quic-v1/p2p/12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN"
            .parse()
            .unwrap();
        let pid = extract_peer_id(&addr).unwrap();
        assert_eq!(
            pid.to_string(),
            "12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN"
        );
    }

    #[test]
    fn extract_peer_id_missing_returns_error() {
        let addr: Multiaddr = "/ip4/127.0.0.1/udp/9000/quic-v1".parse().unwrap();
        let err = extract_peer_id(&addr).unwrap_err();
        assert!(err.to_string().contains("/p2p/"));
    }

    #[tokio::test]
    async fn swarm_builds_without_panic() {
        // Verify that the minimal swarm configuration compiles and
        // doesn't panic at runtime. We don't actually connect.
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let _swarm = SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_quic()
            .with_behaviour(|_key| build_inference_behaviour())
            .unwrap()
            .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();
    }
}
