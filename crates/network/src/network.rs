//! Public network façade.
//!
//! Downstream crates (consensus, mempool, receipts) depend on
//! [`NetworkHandle`] — a small, libp2p-agnostic surface — rather than
//! the raw `Swarm`. This keeps libp2p churn contained in this crate: a
//! major version bump of rust-libp2p should only affect code below
//! [`Network::start`].
//!
//! # Ownership model
//!
//! [`Network::start`] spawns a dedicated tokio task that owns the
//! [`libp2p::Swarm`] and drives it to completion. The returned
//! [`NetworkHandle`] is the sole interaction surface and is cheap to
//! clone — internally it's a pair of mpsc senders.

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use libp2p::identity::Keypair;
use libp2p::request_response;
use libp2p::swarm::{DialError, SwarmEvent};
use libp2p::{gossipsub, identify, Multiaddr, PeerId, Swarm, SwarmBuilder};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::behaviour::{ArknetBehaviour, ArknetBehaviourEvent};
use crate::config::NetworkConfig;
use crate::errors::{NetworkError, Result};
use crate::gossip::{self, all_topics};
use crate::handshake::HandshakeInfo;
use crate::peer::PeerBook;

/// Events surfaced by the network layer to downstream consumers.
///
/// Everything libp2p-specific is stripped out: only the data a protocol
/// module actually needs crosses the channel.
#[derive(Debug, Clone)]
pub enum NetworkEvent {
    /// A peer connected and completed the arknet handshake.
    PeerConnected {
        /// libp2p PeerId.
        peer: PeerId,
        /// Remote handshake payload.
        info: HandshakeInfo,
    },
    /// A peer disconnected (graceful close or dropped socket).
    PeerDisconnected {
        /// libp2p PeerId.
        peer: PeerId,
    },
    /// A gossipsub message was received.
    GossipMessage {
        /// Topic string the message arrived on.
        topic: String,
        /// Raw message bytes. Protocol-specific decoding is the caller's
        /// responsibility.
        data: Vec<u8>,
        /// Peer that forwarded the message to us.
        source: PeerId,
    },
}

/// Inbound inference request delivered via a dedicated channel.
pub struct InboundInferenceRequest {
    /// Peer that sent the request.
    pub peer: PeerId,
    /// Borsh-encoded `InferenceJobRequest`.
    pub data: Vec<u8>,
    /// Opaque id to pass back when sending the response.
    pub request_id: request_response::InboundRequestId,
}

/// Response to an outbound inference request.
#[derive(Debug, Clone)]
pub struct InferenceResponseEvent {
    /// Original request id returned by `send_inference_request`.
    pub request_id: request_response::OutboundRequestId,
    /// Peer that responded.
    pub peer: PeerId,
    /// Borsh-encoded `InferenceResponse`, or error message on failure.
    pub result: std::result::Result<Vec<u8>, String>,
}

/// Commands sent into the network task from outside.
#[derive(Debug)]
enum Command {
    Publish {
        topic: String,
        data: Vec<u8>,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    ConnectedPeers {
        reply: tokio::sync::oneshot::Sender<Vec<PeerId>>,
    },
    Dial {
        addr: Multiaddr,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    SendInferenceRequest {
        peer: PeerId,
        data: Vec<u8>,
        reply: tokio::sync::oneshot::Sender<request_response::OutboundRequestId>,
    },
    SendInferenceResponse {
        request_id: request_response::InboundRequestId,
        data: Vec<u8>,
    },
}

/// Cheap-to-clone handle exposing the network surface.
///
/// Uses bounded mpsc — the intent is to surface backpressure to the
/// caller rather than let an unbounded queue hide it.
#[derive(Clone)]
pub struct NetworkHandle {
    commands: mpsc::Sender<Command>,
    events: tokio::sync::broadcast::Sender<NetworkEvent>,
    local_peer_id: PeerId,
}

/// Channels for inference request/response routing, returned alongside
/// the `NetworkHandle` from [`Network::start`]. Not cloneable — the
/// consumer (compute/router role) takes ownership.
pub struct InferenceChannels {
    /// Inbound inference requests from remote peers (compute consumes).
    pub requests: mpsc::Receiver<InboundInferenceRequest>,
    /// Responses to outbound inference requests (router consumes).
    pub responses: mpsc::Receiver<InferenceResponseEvent>,
}

impl NetworkHandle {
    /// This node's libp2p peer id.
    pub fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    /// Publish `data` on `topic`. Returns an error if the topic isn't
    /// subscribed or the gossipsub publish queue is full.
    pub async fn publish(&self, topic: impl Into<String>, data: Vec<u8>) -> Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::Publish {
                topic: topic.into(),
                data,
                reply: tx,
            })
            .await
            .map_err(|e| NetworkError::TaskExited(format!("send Publish: {e}")))?;
        rx.await
            .map_err(|e| NetworkError::TaskExited(format!("recv Publish reply: {e}")))?
    }

    /// Ask the network task for its currently-connected peer ids.
    pub async fn connected_peers(&self) -> Result<Vec<PeerId>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::ConnectedPeers { reply: tx })
            .await
            .map_err(|e| NetworkError::TaskExited(format!("send ConnectedPeers: {e}")))?;
        rx.await
            .map_err(|e| NetworkError::TaskExited(format!("recv peers: {e}")))
    }

    /// Dial a peer at the given multiaddr. Useful for the node CLI's
    /// `peers add` command.
    pub async fn dial(&self, addr: Multiaddr) -> Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::Dial { addr, reply: tx })
            .await
            .map_err(|e| NetworkError::TaskExited(format!("send Dial: {e}")))?;
        rx.await
            .map_err(|e| NetworkError::TaskExited(format!("recv Dial reply: {e}")))?
    }

    /// Subscribe to the network event stream. Each subscriber sees every
    /// event emitted after it subscribes; slow consumers see
    /// [`tokio::sync::broadcast::error::RecvError::Lagged`] rather than
    /// blocking the network task.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<NetworkEvent> {
        self.events.subscribe()
    }

    /// Send an inference request to a remote peer. Returns the outbound
    /// request id; the response arrives as a `NetworkEvent::InferenceResponse`.
    pub async fn send_inference_request(
        &self,
        peer: PeerId,
        data: Vec<u8>,
    ) -> Result<request_response::OutboundRequestId> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::SendInferenceRequest {
                peer,
                data,
                reply: tx,
            })
            .await
            .map_err(|e| NetworkError::TaskExited(format!("send InferenceRequest: {e}")))?;
        rx.await
            .map_err(|e| NetworkError::TaskExited(format!("recv InferenceRequest reply: {e}")))
    }

    /// Send a response to an inbound inference request.
    pub async fn send_inference_response(
        &self,
        request_id: request_response::InboundRequestId,
        data: Vec<u8>,
    ) -> Result<()> {
        self.commands
            .send(Command::SendInferenceResponse { request_id, data })
            .await
            .map_err(|e| NetworkError::TaskExited(format!("send InferenceResponse: {e}")))?;
        Ok(())
    }
}

/// Owned network layer. Call [`Network::start`] once at node boot; pass
/// the resulting [`NetworkHandle`] into every module that needs to
/// gossip.
pub struct Network;

impl Network {
    /// Boot the network. Spawns a dedicated task that owns the swarm;
    /// returns the handle + a JoinHandle the caller can await (typically
    /// awaited after the cancellation token is tripped).
    pub async fn start(
        config: NetworkConfig,
        keypair: Keypair,
        handshake: HandshakeInfo,
        shutdown: CancellationToken,
    ) -> Result<(NetworkHandle, InferenceChannels, JoinHandle<Result<()>>)> {
        config.validate()?;

        let local_peer_id = keypair.public().to_peer_id();
        info!(peer_id = %local_peer_id, network_id = %config.network_id, "network starting");

        let mut swarm = SwarmBuilder::with_existing_identity(keypair.clone())
            .with_tokio()
            .with_tcp(
                libp2p::tcp::Config::default().nodelay(true),
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
            )
            .map_err(|e| NetworkError::Transport(format!("with_tcp: {e}")))?
            .with_quic()
            .with_relay_client(libp2p::noise::Config::new, libp2p::yamux::Config::default)
            .map_err(|e| NetworkError::Transport(format!("with_relay_client: {e}")))?
            .with_behaviour(|key, relay_client| {
                ArknetBehaviour::new(key, &handshake, relay_client)
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
            })
            .map_err(|e| NetworkError::Behaviour(format!("with_behaviour: {e}")))?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();

        // Listen on every configured address. Errors here are fatal — an
        // operator typo should fail loudly rather than silently downgrade
        // to a dial-only client.
        for addr in &config.listen_addrs {
            swarm
                .listen_on(addr.clone())
                .map_err(|e| NetworkError::Transport(format!("listen_on {addr}: {e}")))?;
        }
        if let Some(ext) = &config.external_addr {
            swarm.add_external_address(ext.clone());
        }

        // Subscribe to every arknet topic. Phase 1 does role-blind
        // subscription because the topic volume is low and selective
        // subscription needs real role wiring (Week 10).
        for topic in all_topics() {
            swarm
                .behaviour_mut()
                .gossipsub
                .subscribe(&topic)
                .map_err(|e| NetworkError::Behaviour(format!("gossipsub subscribe: {e}")))?;
        }

        // Dial bootstrap peers and listen on relay circuits through them
        // so other peers can reach us via the relay.
        for addr in &config.bootstrap_peers {
            match swarm.dial(addr.clone()) {
                Ok(()) => debug!(addr = %addr, "dialed bootstrap peer"),
                Err(e) => {
                    warn!(addr = %addr, error = %e, "bootstrap dial failed");
                    continue;
                }
            }
            // Extract the /p2p/<peer_id> from the bootstrap multiaddr and
            // listen via relay circuit through that peer.
            if addr
                .iter()
                .any(|p| matches!(p, libp2p::multiaddr::Protocol::P2p(_)))
            {
                let relay_listen = addr.clone().with(libp2p::multiaddr::Protocol::P2pCircuit);
                match swarm.listen_on(relay_listen.clone()) {
                    Ok(_) => info!(addr = %relay_listen, "listening via relay circuit"),
                    Err(e) => {
                        warn!(addr = %relay_listen, error = %e, "relay circuit listen failed")
                    }
                }
            }
        }

        let peer_book = PeerBook::load(&config.peer_book_path);

        // Re-dial peers we knew about before the restart. Again, failures
        // are logged not fatal.
        for record in peer_book.snapshot() {
            for addr_str in &record.addrs {
                let Ok(addr) = addr_str.parse::<Multiaddr>() else {
                    continue;
                };
                if let Err(e) = swarm.dial(addr.clone()) {
                    if !matches!(e, DialError::DialPeerConditionFalse(_)) {
                        debug!(addr = %addr, error = %e, "redial of known peer failed");
                    }
                }
            }
        }

        let (command_tx, command_rx) = mpsc::channel(64);
        let (event_tx, _) = tokio::sync::broadcast::channel(256);
        let (infer_req_tx, infer_req_rx) = mpsc::channel(64);
        let (infer_resp_tx, infer_resp_rx) = mpsc::channel(64);

        let handle = NetworkHandle {
            commands: command_tx,
            events: event_tx.clone(),
            local_peer_id,
        };

        let inference_channels = InferenceChannels {
            requests: infer_req_rx,
            responses: infer_resp_rx,
        };

        let expected = handshake.clone();
        let join = tokio::spawn(run_swarm(
            swarm,
            command_rx,
            event_tx,
            infer_req_tx,
            infer_resp_tx,
            peer_book,
            expected,
            shutdown,
        ));

        Ok((handle, inference_channels, join))
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_swarm(
    mut swarm: Swarm<ArknetBehaviour>,
    mut commands: mpsc::Receiver<Command>,
    events: tokio::sync::broadcast::Sender<NetworkEvent>,
    infer_req_tx: mpsc::Sender<InboundInferenceRequest>,
    infer_resp_tx: mpsc::Sender<InferenceResponseEvent>,
    peer_book: PeerBook,
    expected_handshake: HandshakeInfo,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut infer = InferenceState {
        req_tx: infer_req_tx,
        resp_tx: infer_resp_tx,
        pending_channels: HashMap::new(),
    };
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("network shutdown requested");
                let _ = peer_book.flush();
                return Ok(());
            }

            Some(cmd) = commands.recv() => {
                handle_command(&mut swarm, &mut infer, cmd);
            }

            Some(event) = swarm.next() => {
                handle_swarm_event(event, &mut swarm, &events, &peer_book, &expected_handshake, &mut infer);
            }
        }
    }
}

fn handle_command(swarm: &mut Swarm<ArknetBehaviour>, infer: &mut InferenceState, cmd: Command) {
    match cmd {
        Command::Publish { topic, data, reply } => {
            let topic_hash = gossipsub::IdentTopic::new(topic.clone());
            let res = swarm
                .behaviour_mut()
                .gossipsub
                .publish(topic_hash, data)
                .map(|_| ())
                .map_err(|e| NetworkError::Behaviour(format!("publish {topic}: {e}")));
            let _ = reply.send(res);
        }
        Command::ConnectedPeers { reply } => {
            let peers: Vec<PeerId> = swarm.connected_peers().copied().collect();
            let _ = reply.send(peers);
        }
        Command::Dial { addr, reply } => {
            let res = swarm
                .dial(addr.clone())
                .map_err(|e| NetworkError::Transport(format!("dial {addr}: {e}")));
            let _ = reply.send(res);
        }
        Command::SendInferenceRequest { peer, data, reply } => {
            let id = swarm.behaviour_mut().inference.send_request(&peer, data);
            let _ = reply.send(id);
        }
        Command::SendInferenceResponse { request_id, data } => {
            if let Some(channel) = infer.pending_channels.remove(&request_id) {
                let _ = swarm.behaviour_mut().inference.send_response(channel, data);
            } else {
                warn!(?request_id, "no pending channel for inference response");
            }
        }
    }
}

struct InferenceState {
    req_tx: mpsc::Sender<InboundInferenceRequest>,
    resp_tx: mpsc::Sender<InferenceResponseEvent>,
    pending_channels:
        HashMap<request_response::InboundRequestId, request_response::ResponseChannel<Vec<u8>>>,
}

fn handle_swarm_event(
    event: SwarmEvent<ArknetBehaviourEvent>,
    swarm: &mut Swarm<ArknetBehaviour>,
    events: &tokio::sync::broadcast::Sender<NetworkEvent>,
    peer_book: &PeerBook,
    expected: &HandshakeInfo,
    infer: &mut InferenceState,
) {
    match event {
        SwarmEvent::NewListenAddr { address, .. } => {
            info!(address = %address, "listening");
        }
        SwarmEvent::ConnectionEstablished {
            peer_id, endpoint, ..
        } => {
            let addr = endpoint.get_remote_address().clone();
            debug!(peer = %peer_id, %addr, "connection established");
            if let Err(e) = peer_book.insert_connected(&peer_id, &addr) {
                warn!(error = %e, "peer book write failed");
            }
        }
        SwarmEvent::ConnectionClosed { peer_id, .. } => {
            debug!(peer = %peer_id, "connection closed");
            let _ = events.send(NetworkEvent::PeerDisconnected { peer: peer_id });
        }
        SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
            warn!(peer = ?peer_id, error = %error, "outgoing connection error");
        }
        SwarmEvent::Behaviour(ArknetBehaviourEvent::Identify(identify::Event::Received {
            peer_id,
            info,
            ..
        })) => {
            match HandshakeInfo::from_agent_version(&info.agent_version) {
                Ok(Some(remote)) => match expected.check_compatible(&remote) {
                    Ok(()) => {
                        // Add their listen addrs to kademlia so routing tables
                        // converge faster.
                        for addr in &info.listen_addrs {
                            swarm
                                .behaviour_mut()
                                .kad
                                .add_address(&peer_id, addr.clone());
                        }
                        let _ = events.send(NetworkEvent::PeerConnected {
                            peer: peer_id,
                            info: remote,
                        });
                    }
                    Err(e) => {
                        warn!(peer = %peer_id, error = %e, "handshake rejected; disconnecting");
                        let _ = swarm.disconnect_peer_id(peer_id);
                        let _ = peer_book.remove(&peer_id);
                    }
                },
                Ok(None) => {
                    debug!(peer = %peer_id, agent = %info.agent_version, "non-arknet peer, ignoring");
                    let _ = swarm.disconnect_peer_id(peer_id);
                }
                Err(e) => {
                    warn!(peer = %peer_id, error = %e, "malformed handshake; disconnecting");
                    let _ = swarm.disconnect_peer_id(peer_id);
                }
            }
        }
        SwarmEvent::Behaviour(ArknetBehaviourEvent::Gossipsub(gossipsub::Event::Message {
            propagation_source,
            message,
            ..
        })) => {
            let topic = message.topic.to_string();
            let _ = events.send(NetworkEvent::GossipMessage {
                topic,
                data: message.data,
                source: propagation_source,
            });
        }
        SwarmEvent::Behaviour(ArknetBehaviourEvent::Inference(
            request_response::Event::Message { peer, message },
        )) => match message {
            request_response::Message::Request {
                request_id,
                request: data,
                channel,
            } => {
                infer.pending_channels.insert(request_id, channel);
                let _ = infer.req_tx.try_send(InboundInferenceRequest {
                    peer,
                    data,
                    request_id,
                });
            }
            request_response::Message::Response {
                request_id,
                response: data,
            } => {
                let _ = infer.resp_tx.try_send(InferenceResponseEvent {
                    request_id,
                    peer,
                    result: Ok(data),
                });
            }
        },
        SwarmEvent::Behaviour(ArknetBehaviourEvent::Inference(
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            },
        )) => {
            warn!(%peer, %error, "inference request failed");
            let _ = infer.resp_tx.try_send(InferenceResponseEvent {
                request_id,
                peer,
                result: Err(error.to_string()),
            });
        }
        SwarmEvent::Behaviour(_) => {
            // Ping, kademlia bookkeeping, identify-sent, inference inbound failure: not surfaced.
        }
        other => {
            debug!(event = ?other, "unhandled swarm event");
        }
    }
}

/// Pre-computed topic set used by the node binary for debug endpoints /
/// status UI. Re-exported so callers don't have to import [`crate::gossip`]
/// directly.
pub fn default_topics() -> Vec<gossipsub::IdentTopic> {
    gossip::all_topics()
}
