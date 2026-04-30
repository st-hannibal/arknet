//! The consensus engine: owns the malachite state machine and drives
//! it in its own tokio task.
//!
//! # Topology
//!
//! ```text
//!                      ┌─────────────────────────────────┐
//!                      │  ConsensusEngine (tokio task)   │
//!                      │                                 │
//!    NetworkEvent ───► │  select! loop:                  │ ─► NetworkHandle.publish(topic, bytes)
//!    (vote/prop        │    • inbound gossip             │    (votes, proposals)
//!     gossip)          │    • timeout expiry             │
//!                      │    • external tx submit         │
//!                      │                                 │
//!    RpcHandle    ───► │  calls process!(input, state,   │ ─► chain::State.commit_block
//!    commands          │    metrics, effect handler)     │    (on Decide)
//!                      │                                 │
//!                      └─────────────────────────────────┘
//! ```
//!
//! # Coroutine mechanics
//!
//! Malachite's state machine is a **synchronous generator**
//! ([`genawaiter::sync`]). The `process!` macro hides this — we feed
//! it one `Input` at a time, and it yields [`Effect`]s. Each effect
//! handler returns a `Resume<Ctx>`; the generator resumes until
//! complete, at which point we have either zero or several new
//! `Input`s to feed back (a proposal we accepted, a decided block,
//! etc.). We keep a `pending_inputs: VecDeque<Input>` queue so the
//! task can drain it before pulling the next network event.
//!
//! # Phase 1 scope
//!
//! - Proposer-only value streaming (`ValuePayload::ProposalOnly`).
//! - Static validator set per height (epoch rotation lands Week 9).
//! - No WAL — replica restart starts fresh at its current height.
//!   Crash recovery lands in Week 9 alongside the malachite-wal
//!   crate.
//! - Round-robin proposer (`ArknetContext::select_proposer` is
//!   deterministic; VRF is Week 9+).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time::{sleep_until, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use arknet_chain::block::Block;
use arknet_chain::transactions::SignedTransaction;
use arknet_chain::State as ChainState;
use arknet_common::types::{BlockHash, Hash256, NodeId};
use arknet_network::{NetworkEvent, NetworkHandle};
use malachitebft_core_consensus::{
    Effect, Input, Params, ProposedValue, Resumable, Resume, State as MalachiteState, ValuePayload,
};
use malachitebft_core_types::{
    Height as MalachiteHeight, Proposal as MalachiteProposal, Timeout, TimeoutKind,
    ValidatorSet as MalachiteValidatorSet, Validity, ValueOrigin,
};
use malachitebft_metrics::Metrics;

use crate::block_builder::{BlockBuilder, BuildParams};
use crate::commit::commit_block;
use crate::context::ArknetContext;
use crate::errors::{ConsensusError, Result};
use crate::height::Height;
use crate::mempool::Mempool;
use crate::network_bridge::{classify_inbound, outbound_message, pubkey_for_address, InboundMsg};
use crate::signing::ArknetSigningProvider;
use crate::validators::{ChainAddress, ChainValidatorSet};
use crate::value::ChainValue;

/// Timeout defaults. Tendermint-style: each step times out after a
/// base duration plus a per-round delta. These are tight enough for
/// a local devnet and generous enough that a 4-node cluster over
/// LAN reliably reaches consensus.
#[derive(Clone, Debug)]
pub struct TimeoutConfig {
    /// Propose-step base timeout.
    pub propose: Duration,
    /// Prevote-step base timeout.
    pub prevote: Duration,
    /// Precommit-step base timeout.
    pub precommit: Duration,
    /// Round-synchronization rebroadcast timeout.
    pub rebroadcast: Duration,
    /// Per-round extension applied to every step.
    pub per_round_delta: Duration,
    /// Target block interval — pacing between `Decide` and the next
    /// `StartHeight` call. Real networks run at 1-2 s; devnet uses
    /// 500 ms to keep feedback tight.
    pub block_interval: Duration,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            propose: Duration::from_millis(3_000),
            prevote: Duration::from_millis(1_000),
            precommit: Duration::from_millis(1_000),
            rebroadcast: Duration::from_millis(2_000),
            per_round_delta: Duration::from_millis(500),
            block_interval: Duration::from_millis(500),
        }
    }
}

impl TimeoutConfig {
    fn duration_for(&self, t: Timeout) -> Duration {
        let round = (t.round.as_i64().max(0) as u32).saturating_sub(0);
        let base = match t.kind {
            TimeoutKind::Propose => self.propose,
            TimeoutKind::Prevote => self.prevote,
            TimeoutKind::Precommit => self.precommit,
            TimeoutKind::Rebroadcast => self.rebroadcast,
        };
        base + self.per_round_delta.saturating_mul(round)
    }
}

/// Static configuration handed to [`ConsensusEngine::start`].
pub struct EngineConfig {
    /// Chain identifier (`"arknet-devnet-1"`).
    pub chain_id: String,
    /// Protocol version (carried into each block header).
    pub version: u32,
    /// First height this node is expected to produce. `Height(1)` on
    /// a fresh chain; current-height + 1 on restart once the block
    /// store is wired up.
    pub initial_height: Height,
    /// Active validator set at the initial height.
    pub validator_set: ChainValidatorSet,
    /// Current EIP-1559 base fee. The engine advances this per block
    /// via the fee-market rules.
    pub base_fee: u128,
    /// Block gas ceiling.
    pub gas_limit: u64,
    /// Block gas target (for fee-market updates).
    pub gas_target: u64,
    /// This node's operator address (must exist in `validator_set`).
    pub local_address: ChainAddress,
    /// This node's NodeId (goes into block headers we propose).
    pub local_node_id: NodeId,
    /// Timeout configuration.
    pub timeouts: TimeoutConfig,
}

/// Commands accepted over [`ConsensusHandle`].
///
/// `SubmitTx` boxes the transaction because a signed tx can be up to
/// 1 MiB (per `arknet_chain::MAX_SIGNED_TX_BYTES`); inlining it would
/// bloat every enum value uselessly.
#[derive(Debug)]
enum Command {
    SubmitTx {
        tx: Box<SignedTransaction>,
        reply:
            tokio::sync::oneshot::Sender<std::result::Result<arknet_common::types::TxHash, String>>,
    },
    /// Query the current committed height.
    CurrentHeight {
        reply: tokio::sync::oneshot::Sender<Height>,
    },
}

/// Cheap-to-clone handle for RPC + node-level code.
#[derive(Clone)]
pub struct ConsensusHandle {
    tx: mpsc::Sender<Command>,
    block_events: tokio::sync::broadcast::Sender<Arc<Block>>,
}

impl ConsensusHandle {
    /// Admit a signed transaction into the mempool. Returns the tx hash
    /// on success, or an error message if the pool rejected it.
    pub async fn submit_tx(
        &self,
        tx: SignedTransaction,
    ) -> std::result::Result<arknet_common::types::TxHash, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Command::SubmitTx {
                tx: Box::new(tx),
                reply,
            })
            .await
            .map_err(|e| format!("engine task exited: {e}"))?;
        rx.await.map_err(|e| format!("engine reply lost: {e}"))?
    }

    /// Read the latest committed height.
    pub async fn current_height(&self) -> std::result::Result<Height, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Command::CurrentHeight { reply })
            .await
            .map_err(|e| format!("engine task exited: {e}"))?;
        rx.await.map_err(|e| format!("engine reply lost: {e}"))
    }

    /// Wait for the next finalized block. Returns an `Arc<Block>` so
    /// multiple subscribers can reference the same allocation. Used by
    /// the verifier role body to scan receipts.
    pub async fn next_finalized_block(&self) -> std::result::Result<Arc<Block>, String> {
        let mut rx = self.block_events.subscribe();
        rx.recv()
            .await
            .map_err(|e| format!("block event recv: {e}"))
    }
}

/// The consensus engine entry point.
pub struct ConsensusEngine;

impl ConsensusEngine {
    /// Start the engine. Spawns a tokio task that owns the state
    /// machine, mempool, and chain state. Returns a handle + the join
    /// handle for orderly shutdown.
    pub fn start(
        cfg: EngineConfig,
        chain_state: Arc<ChainState>,
        network: NetworkHandle,
        signer: Arc<ArknetSigningProvider>,
        shutdown: CancellationToken,
    ) -> (ConsensusHandle, tokio::task::JoinHandle<Result<()>>) {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(64);
        let (block_tx, _) = tokio::sync::broadcast::channel::<Arc<Block>>(64);
        let handle = ConsensusHandle {
            tx: cmd_tx,
            block_events: block_tx.clone(),
        };
        let join = tokio::spawn(run(
            cfg,
            chain_state,
            network,
            signer,
            cmd_rx,
            block_tx,
            shutdown,
        ));
        (handle, join)
    }
}

/// Pending timeout: when it expires, feed `Input::TimeoutElapsed` to the
/// state machine. We keep an explicit deadline so a `CancelTimeout`
/// effect can clear it.
struct PendingTimeout {
    timeout: Timeout,
    deadline: Instant,
}

async fn run(
    cfg: EngineConfig,
    chain_state: Arc<ChainState>,
    network: NetworkHandle,
    signer: Arc<ArknetSigningProvider>,
    mut cmd_rx: mpsc::Receiver<Command>,
    block_events: tokio::sync::broadcast::Sender<Arc<Block>>,
    shutdown: CancellationToken,
) -> Result<()> {
    info!(
        height = %cfg.initial_height.0,
        validator_count = cfg.validator_set.count(),
        local_address = %cfg.local_address,
        "consensus engine starting"
    );

    let params = Params::<ArknetContext> {
        initial_height: cfg.initial_height,
        initial_validator_set: cfg.validator_set.clone(),
        address: cfg.local_address,
        threshold_params: Default::default(),
        value_payload: ValuePayload::ProposalOnly,
    };
    let mut state = MalachiteState::<ArknetContext>::new(ArknetContext, params, 128);
    let metrics = Metrics::new();

    let mempool = Arc::new(Mutex::new(Mempool::default()));
    let mut current_height = cfg.initial_height;
    let mut base_fee = cfg.base_fee;
    let mut parent_hash = BlockHash::new([0u8; 32]);
    let mut validator_set_hash: Hash256 = *arknet_crypto::hash::blake3(b"genesis-vset").as_bytes();

    let mut pending_timeouts: Vec<PendingTimeout> = Vec::new();
    let mut pending_inputs: VecDeque<Input<ArknetContext>> = VecDeque::new();
    // Drained txs from the last `GetValue` — readmitted on a failed round.
    let mut last_proposed_drain: Option<(Height, Vec<Arc<SignedTransaction>>)> = None;
    // Most recently accepted block for the current height, from either
    // the local proposer path or inbound gossip. Consumed on `Decide`.
    let mut decided_block_cache: Option<Block> = None;

    let mut net_rx = network.subscribe();

    // Kick off consensus for the initial height.
    pending_inputs.push_back(Input::StartHeight(
        cfg.initial_height,
        cfg.validator_set.clone(),
    ));

    loop {
        // Drain the pending inputs queue before waiting for I/O.
        while let Some(input) = pending_inputs.pop_front() {
            debug!(?input, "feeding input to state machine");
            let follow_ups = drive_one_input(
                &mut state,
                &metrics,
                &signer,
                &network,
                &cfg,
                &mempool,
                &chain_state,
                &mut pending_timeouts,
                &mut current_height,
                &mut base_fee,
                &mut parent_hash,
                &mut validator_set_hash,
                &mut last_proposed_drain,
                &mut decided_block_cache,
                &block_events,
                input,
            )
            .await?;
            pending_inputs.extend(follow_ups);
        }

        // Compute the next timeout deadline.
        let next_deadline = pending_timeouts
            .iter()
            .map(|t| t.deadline)
            .min()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));

        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("consensus engine shutting down");
                return Ok(());
            }

            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    Command::SubmitTx { tx, reply } => {
                        let r = mempool.lock().insert(*tx).map_err(|e| e.to_string());
                        let _ = reply.send(r);
                    }
                    Command::CurrentHeight { reply } => {
                        let _ = reply.send(current_height);
                    }
                }
            }

            evt = net_rx.recv() => {
                match evt {
                    Ok(NetworkEvent::GossipMessage { topic, data, source }) => {
                        match classify_inbound(&topic, &data) {
                            Some(Ok(InboundMsg::Vote(sv))) => {
                                debug!(peer = %source, height = %sv.message.height.0, "inbound vote");
                                pending_inputs.push_back(Input::Vote(*sv));
                            }
                            Some(Ok(InboundMsg::Proposal(sp))) => {
                                let sp = *sp;
                                debug!(peer = %source, height = %sp.message.height.0, "inbound proposal");
                                // Cache the block so the `Decide` arm can commit it.
                                // Only overwrite if the incoming proposal matches
                                // the active height — otherwise we'd clobber a good
                                // cache with a stale proposal.
                                if sp.message.height == current_height
                                    || sp.message.height.0 == current_height.0 + 1
                                {
                                    decided_block_cache = Some(sp.message.value.block.clone());
                                }
                                // Feed the proposal AND the corresponding
                                // ProposedValue so the state machine has a full
                                // view in ProposalOnly mode.
                                let proposer_addr = *sp.message.validator_address();
                                let pv = ProposedValue::<ArknetContext> {
                                    height: sp.message.height,
                                    round: sp.message.round,
                                    valid_round: sp.message.pol_round,
                                    proposer: proposer_addr,
                                    value: sp.message.value.clone(),
                                    validity: Validity::Valid, // Phase 1: trust the proposer body; full block validity lands Week 9 with stake-aware checks
                                };
                                pending_inputs.push_back(Input::Proposal(sp));
                                pending_inputs.push_back(Input::ProposedValue(pv, ValueOrigin::Consensus));
                            }
                            Some(Err(e)) => {
                                warn!(peer = %source, topic = %topic, error = %e, "dropping malformed gossip");
                            }
                            None => {
                                // Not a consensus topic — ignore silently.
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "network broadcast lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        error!("network event channel closed; engine exiting");
                        return Ok(());
                    }
                }
            }

            _ = sleep_until(next_deadline) => {
                // Any timeout whose deadline is in the past fires.
                let now = Instant::now();
                let (expired, still_pending): (Vec<_>, Vec<_>) = pending_timeouts
                    .drain(..)
                    .partition(|t| t.deadline <= now);
                pending_timeouts = still_pending;
                for t in expired {
                    debug!(timeout = ?t.timeout, "timeout elapsed");
                    pending_inputs.push_back(Input::TimeoutElapsed(t.timeout));
                }
            }
        }
    }
}

/// Run a single `Input` through `process!` and collect any follow-up
/// `Input`s the effect handler queued.
#[allow(clippy::too_many_arguments)]
async fn drive_one_input(
    state: &mut MalachiteState<ArknetContext>,
    metrics: &Metrics,
    signer: &Arc<ArknetSigningProvider>,
    network: &NetworkHandle,
    cfg: &EngineConfig,
    mempool: &Arc<Mutex<Mempool>>,
    chain_state: &Arc<ChainState>,
    pending_timeouts: &mut Vec<PendingTimeout>,
    current_height: &mut Height,
    base_fee: &mut u128,
    parent_hash: &mut BlockHash,
    validator_set_hash: &mut Hash256,
    last_proposed_drain: &mut Option<(Height, Vec<Arc<SignedTransaction>>)>,
    decided_block_cache: &mut Option<Block>,
    block_events: &tokio::sync::broadcast::Sender<Arc<Block>>,
    input: Input<ArknetContext>,
) -> Result<Vec<Input<ArknetContext>>> {
    let mut follow_ups: Vec<Input<ArknetContext>> = Vec::new();

    let result: std::result::Result<(), malachitebft_core_consensus::Error<ArknetContext>> = malachitebft_core_consensus::process!(
        input: input,
        state: state,
        metrics: metrics,
        with: effect => handle_effect(
            effect,
            signer,
            network,
            cfg,
            mempool,
            chain_state,
            pending_timeouts,
            current_height,
            base_fee,
            parent_hash,
            validator_set_hash,
            last_proposed_drain,
            decided_block_cache,
            block_events,
            &mut follow_ups,
        ).await
    );

    if let Err(e) = result {
        error!(error = ?e, "malachite state machine error");
        return Err(ConsensusError::Malachite(format!("{e:?}")));
    }
    Ok(follow_ups)
}

/// Map a single [`Effect`] to a [`Resume`], performing the side effect
/// along the way. The return type matches the `process!` macro's
/// handler contract.
#[allow(clippy::too_many_arguments)]
async fn handle_effect(
    effect: Effect<ArknetContext>,
    signer: &Arc<ArknetSigningProvider>,
    network: &NetworkHandle,
    cfg: &EngineConfig,
    mempool: &Arc<Mutex<Mempool>>,
    chain_state: &Arc<ChainState>,
    pending_timeouts: &mut Vec<PendingTimeout>,
    current_height: &mut Height,
    base_fee: &mut u128,
    parent_hash: &mut BlockHash,
    validator_set_hash: &mut Hash256,
    last_proposed_drain: &mut Option<(Height, Vec<Arc<SignedTransaction>>)>,
    decided_block_cache: &mut Option<Block>,
    block_events: &tokio::sync::broadcast::Sender<Arc<Block>>,
    follow_ups: &mut Vec<Input<ArknetContext>>,
) -> std::result::Result<Resume<ArknetContext>, malachitebft_core_consensus::Error<ArknetContext>> {
    use malachitebft_core_types::SigningProvider as _;

    match effect {
        Effect::ResetTimeouts(r) => {
            pending_timeouts.clear();
            Ok(r.resume_with(()))
        }
        Effect::CancelAllTimeouts(r) => {
            pending_timeouts.clear();
            Ok(r.resume_with(()))
        }
        Effect::CancelTimeout(t, r) => {
            pending_timeouts.retain(|p| p.timeout != t);
            Ok(r.resume_with(()))
        }
        Effect::ScheduleTimeout(t, r) => {
            let dur = cfg.timeouts.duration_for(t);
            pending_timeouts.retain(|p| p.timeout != t);
            pending_timeouts.push(PendingTimeout {
                timeout: t,
                deadline: Instant::now() + dur,
            });
            Ok(r.resume_with(()))
        }
        Effect::StartRound(_height, round, proposer, role, r) => {
            debug!(%round, %proposer, ?role, "start round");
            Ok(r.resume_with(()))
        }
        Effect::GetValidatorSet(_height, r) => {
            // Phase 1: static validator set across heights. Week 9 swaps
            // this for a per-height lookup into the stake module.
            Ok(r.resume_with(Some(cfg.validator_set.clone())))
        }
        Effect::PublishConsensusMsg(msg, r) => {
            let (topic, bytes) = outbound_message(&msg);
            if let Err(e) = network.publish(topic, bytes).await {
                warn!(topic, error = %e, "consensus publish failed");
            }
            Ok(r.resume_with(()))
        }
        Effect::PublishLivenessMsg(_msg, r) => {
            // Liveness republish — Phase 1 treats as best-effort no-op;
            // regular PublishConsensusMsg already covers the base case.
            Ok(r.resume_with(()))
        }
        Effect::RepublishVote(sv, r) => {
            let bytes = crate::network_bridge::encode_signed_vote(&sv);
            let _ = network
                .publish(crate::network_bridge::TOPIC_CONSENSUS_VOTE, bytes)
                .await;
            Ok(r.resume_with(()))
        }
        Effect::RepublishRoundCertificate(_cert, r) => {
            // Phase 1: round certificates are not gossiped on their own
            // topic yet — Week 9 adds a dedicated `consensus/cert/1` lane.
            Ok(r.resume_with(()))
        }
        Effect::GetValue(height, round, _timeout, r) => {
            // We are proposer. Build a block synchronously within the
            // handler budget; feed it back as `Input::Propose` on the
            // next tick.
            let params = BuildParams {
                chain_id: cfg.chain_id.clone(),
                version: cfg.version,
                parent_hash: *parent_hash,
                validator_set_hash: *validator_set_hash,
                proposer: cfg.local_node_id,
                base_fee: *base_fee,
                gas_limit: cfg.gas_limit,
                bytes_budget: crate::block_builder::DEFAULT_BLOCK_BYTES_BUDGET,
            };
            let mut mp = mempool.lock();
            match BlockBuilder::build(chain_state, &mut mp, height, params) {
                Ok(built) => {
                    *last_proposed_drain = Some((height, built.drained));
                    *decided_block_cache = Some(built.block.clone());
                    let value = ChainValue::new(built.block);
                    follow_ups.push(Input::Propose(
                        malachitebft_core_consensus::LocallyProposedValue::new(
                            height, round, value,
                        ),
                    ));
                }
                Err(e) => {
                    error!(error = %e, %height, "block builder failed; cannot propose");
                }
            }
            Ok(r.resume_with(()))
        }
        Effect::RestreamProposal(_h, _r0, _vr, _addr, _vid, r) => {
            // ProposalOnly mode — nothing to restream at the parts level.
            Ok(r.resume_with(()))
        }
        Effect::SyncValue(_resp, r) => {
            // Sync protocol not wired in Phase 1 (Week 9+).
            Ok(r.resume_with(()))
        }
        Effect::Decide(cert, _extensions, r) => {
            let height = cert.height;
            info!(
                %height,
                round = %cert.round,
                signatures = cert.commit_signatures.len(),
                "consensus decided"
            );
            // The decided block is pulled out of the engine-local
            // `decided_block_cache` — the network-receive and
            // local-propose paths both write to that cache before the
            // state machine can reach `Decide`. Pulling it here keeps
            // the commit path deterministic (same block bytes both
            // the proposer and replicas committed to via votes).
            if let Some(block) = decided_block_cache.take() {
                if block.header.height == height.as_u64()
                    && block.hash().as_bytes() == cert.value_id.0.as_bytes()
                {
                    if let Err(e) = apply_commit(
                        chain_state,
                        mempool,
                        &block,
                        current_height,
                        base_fee,
                        parent_hash,
                        cfg,
                        block_events,
                    ) {
                        error!(error = %e, "apply_commit failed");
                    }
                } else {
                    error!(
                        expected = ?cert.value_id,
                        got = ?block.hash(),
                        "cached decided block does not match commit certificate"
                    );
                }
            } else {
                error!(
                    %height,
                    "no cached block for decide — proposer cache and gossip-receive cache both missed"
                );
            }
            // Drop drained-tx stash for this height (whether we committed
            // or not — the mempool pruning happens inside apply_commit).
            if let Some((h, _)) = last_proposed_drain.as_ref() {
                if *h == height {
                    *last_proposed_drain = None;
                }
            }
            // Queue the next height.
            let next = Height(height.as_u64() + 1);
            follow_ups.push(Input::StartHeight(next, cfg.validator_set.clone()));
            Ok(r.resume_with(()))
        }
        Effect::SignVote(vote, r) => {
            let signed = signer.sign_vote(vote);
            Ok(r.resume_with(signed))
        }
        Effect::SignProposal(prop, r) => {
            let signed = signer.sign_proposal(prop);
            Ok(r.resume_with(signed))
        }
        Effect::VerifySignature(msg, pk, r) => {
            use malachitebft_core_consensus::ConsensusMsg;
            let ok = match &msg.message {
                ConsensusMsg::Vote(v) => signer.verify_signed_vote(v, &msg.signature, &pk),
                ConsensusMsg::Proposal(p) => signer.verify_signed_proposal(p, &msg.signature, &pk),
            };
            Ok(r.resume_with(ok))
        }
        Effect::VerifyCommitCertificate(_cert, _vs, _tp, r) => {
            // Phase 1: trust certs produced by the local state machine's
            // own vote aggregation. Real-peer cert verification lands
            // alongside sync protocol in Week 9.
            Ok(r.resume_with(Ok(())))
        }
        Effect::VerifyPolkaCertificate(_cert, _vs, _tp, r) => Ok(r.resume_with(Ok(()))),
        Effect::VerifyRoundCertificate(_cert, _vs, _tp, r) => Ok(r.resume_with(Ok(()))),
        Effect::WalAppend(_entry, r) => {
            // Phase 1: no WAL. Restart behaviour is "start fresh at
            // current height" — acceptable for devnet, not for
            // testnet.
            Ok(r.resume_with(()))
        }
        Effect::ExtendVote(_h, _r0, _vid, r) => {
            // `Extension = ()` — no app-level extension.
            Ok(r.resume_with(None))
        }
        Effect::VerifyVoteExtension(_, _, _, _, _, r) => Ok(r.resume_with(Ok(()))),
    }
}

/// Commit a decided block. Updates engine-local state (parent hash,
/// base fee, current height) and performs the chain-state mutation.
#[allow(clippy::too_many_arguments)]
fn apply_commit(
    chain_state: &Arc<ChainState>,
    mempool: &Arc<Mutex<Mempool>>,
    block: &Block,
    current_height: &mut Height,
    base_fee: &mut u128,
    parent_hash: &mut BlockHash,
    cfg: &EngineConfig,
    block_events: &tokio::sync::broadcast::Sender<Arc<Block>>,
) -> Result<()> {
    let mut mp = mempool.lock();
    let report = commit_block(chain_state, &mut mp, block)
        .map_err(|e| ConsensusError::BlockBuilder(e.to_string()))?;
    drop(mp);

    let committed_height = Height(block.header.height);
    *current_height = committed_height;
    *parent_hash = block.hash();
    let new_fee = arknet_chain::fee_market::next_base_fee(
        block.header.base_fee,
        report.gas_used,
        cfg.gas_target,
    )
    .unwrap_or(block.header.base_fee);
    *base_fee = new_fee;

    // Notify subscribers (verifier role, metrics) of the finalized block.
    let _ = block_events.send(Arc::new(block.clone()));

    info!(
        height = %committed_height.0,
        gas_used = report.gas_used,
        applied = report.applied_count,
        state_root = ?block.header.state_root,
        "block committed"
    );
    let _ = pubkey_for_address; // silence unused-import
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_config_scales_with_round() {
        let c = TimeoutConfig::default();
        let t0 = Timeout::prevote(malachitebft_core_types::Round::new(0));
        let t3 = Timeout::prevote(malachitebft_core_types::Round::new(3));
        assert!(c.duration_for(t3) > c.duration_for(t0));
    }

    #[test]
    fn timeout_config_nil_round_is_safe() {
        let c = TimeoutConfig::default();
        let t = Timeout::propose(malachitebft_core_types::Round::Nil);
        // Nil is -1 → clamped to 0; just assert no panic and > 0.
        assert!(c.duration_for(t) > Duration::ZERO);
    }
}
