//! Commit path: `malachitebft_core_consensus::Effect::Decide` →
//! atomic state mutation.
//!
//! When the state machine produces a `Decide`, the engine has exactly
//! one thing left to do for the height: apply the decided block's
//! transactions to canonical state and make the write durable. This
//! module isolates that path.
//!
//! # Contract
//!
//! - **Replay, don't trust proposer state.** The proposer's
//!   `preview_state_root` lives only in its own memory; every
//!   validator must re-run `apply_tx` so replicas converge on the
//!   same on-disk state.
//! - **Atomic commit.** One [`BlockCtx::commit`] per block. If the
//!   state root after replay does not match the block header,
//!   consensus disagrees with the proposer — the block is rejected
//!   and [`CommitError::StateRootMismatch`] is returned. In Phase 1
//!   this is fatal: the engine should halt rather than silently
//!   diverge.
//! - **Mempool cleanup.** After a successful commit, all included
//!   transaction hashes are removed from the mempool. Transactions
//!   that were *drained* during propose but rejected at replay
//!   remain out of the mempool — they would have been rejected again
//!   next round and only waste block space.
//! - **No base-fee rollover here.** The next block's base fee is
//!   computed by the engine from the committed block's `gas_used`
//!   via [`arknet_chain::fee_market::next_base_fee`]. See
//!   PROTOCOL_SPEC §7.2.

use arknet_chain::apply::{apply_tx, TxOutcome};
use arknet_chain::block::Block;
use arknet_chain::governance_entry::ProposalPhase;
use arknet_chain::state::BlockCtx;
use arknet_chain::transactions::Transaction;
use arknet_chain::State;
use arknet_common::types::{Address, Amount, Gas, Height, StateRoot, Timestamp};
use arknet_governance::proposals::{Tally, TallyOutcome};
use arknet_staking::slashing::{apply_slash, Offense};

use crate::mempool::Mempool;

/// Result of applying a decided block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReport {
    /// The committed state root. Matches `block.header.state_root`.
    pub state_root: StateRoot,
    /// Sum of `gas_used` across accepted transactions. Drives the
    /// next block's EIP-1559 base fee.
    pub gas_used: Gas,
    /// Count of txs applied (excludes replay-time rejections).
    pub applied_count: usize,
}

/// Errors surfaced from the commit path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommitError {
    /// State root after replay does not match the block header.
    /// Proposer and validator disagree on the post-apply state —
    /// fatal for the engine.
    #[error("state_root mismatch: header={header:?}, replayed={replayed:?}")]
    StateRootMismatch {
        /// Root carried in the decided block's header.
        header: StateRoot,
        /// Root produced by this node after replaying the block.
        replayed: StateRoot,
    },

    /// An underlying chain-state operation returned an error. Fatal.
    #[error("chain state: {0}")]
    ChainState(String),
}

impl From<arknet_chain::errors::ChainError> for CommitError {
    fn from(e: arknet_chain::errors::ChainError) -> Self {
        CommitError::ChainState(e.to_string())
    }
}

/// Apply the decided `block` to `state`, commit atomically, and prune
/// the mempool of any transactions that landed.
///
/// Also runs, inside the same atomic context:
///
/// - `set_current_height(block.header.height)` — keeps
///   `apply_tx`'s bootstrap-epoch check (§9.4) reading the correct
///   height, and survives restart via RocksDB.
/// - `staking::recompute_validator_set` iff the new height lands on
///   an epoch boundary (§16 `EPOCH_LENGTH_BLOCKS`). The recompute
///   happens AFTER the block's transactions so stake deposits from
///   this block count toward next-epoch membership.
pub fn commit_block(
    state: &State,
    mempool: &mut Mempool,
    block: &Block,
) -> Result<CommitReport, CommitError> {
    let mut ctx = state.begin_block();
    let mut gas_used: Gas = 0;
    let mut applied_count = 0usize;

    for tx in &block.txs {
        match apply_tx(&mut ctx, tx)? {
            TxOutcome::Applied { gas_used: g } => {
                gas_used = gas_used.saturating_add(g);
                applied_count += 1;
            }
            TxOutcome::Rejected(_reason) => {
                // Block already committed via consensus — reject at
                // replay time is an indicator of non-determinism in
                // apply_tx (must not happen in Phase 1 Transfer
                // flow). Record in tracing for diagnosis but do not
                // halt: the block's header will catch any real
                // divergence via the state-root check below.
                tracing::warn!(
                    tx_hash = ?tx.hash(),
                    "tx rejected at commit replay; header state_root check will surface if this desyncs"
                );
            }
        }
    }

    // Slash dispatch: accepted Dispute txs trigger
    // `apply_slash(FailedDeterministicVerification)` against the
    // compute node. Reporter cut goes to `dispute.reporter`; burn
    // + treasury use the genesis-default treasury address.
    let treasury = genesis_treasury_address();
    for tx in &block.txs {
        if let Transaction::Dispute(d) = &tx.tx {
            if d.claimed_output_hash != d.reexec_output_hash {
                if let Err(e) = apply_slash(
                    &mut ctx,
                    &d.compute_node,
                    arknet_chain::StakeRole::Compute,
                    Offense::FailedDeterministicVerification,
                    &d.reporter,
                    &treasury,
                ) {
                    tracing::warn!(
                        job_id = ?d.job_id,
                        compute = ?d.compute_node,
                        error = %e,
                        "slash for dispute failed — compute node may have no stake"
                    );
                }
            }
        }
    }

    // Epoch rotation (§9.5 + §16 `EPOCH_LENGTH_BLOCKS`). Run before
    // the state-root preview so the rotation is part of the same
    // root the block header commits to.
    if arknet_chain::bootstrap::is_epoch_boundary(block.header.height) {
        let active = arknet_staking::recompute_validator_set(&mut ctx, block.header.height)
            .map_err(|e| CommitError::ChainState(format!("recompute_validator_set: {e}")))?;
        tracing::info!(
            height = block.header.height,
            active_validators = active,
            "validator-set recomputed at epoch boundary"
        );

        // Epoch-boundary minting: process pending rewards from the
        // ending epoch. Two-phase settlement: receipts queued in
        // CF_PENDING_REWARDS during epoch N, minted here at epoch N+1.
        let ending_epoch =
            arknet_payments::emission::epoch_for_height(block.header.height).saturating_sub(1);
        epoch_boundary_mint(&mut ctx, ending_epoch, block.header.height)?;

        // Governance lifecycle: advance proposal phases and tally votes
        // whose voting period has elapsed.
        process_governance_proposals(&mut ctx, block.header.timestamp_ms)?;
    }

    // Persist the committed height so bootstrap checks in the next
    // block's `apply_tx` see the correct value.
    ctx.set_current_height(block.header.height)?;

    // Persist the next block's base fee so the apply layer can price
    // transactions by the EIP-1559 curve.
    let next_fee = arknet_chain::fee_market::next_base_fee(
        block.header.base_fee,
        gas_used,
        arknet_chain::fee_market::TARGET_GAS_PER_BLOCK,
    )
    .unwrap_or(block.header.base_fee);
    ctx.set_base_fee(next_fee)?;

    // Compute the post-replay root while the ctx is still mutable.
    let replayed_root = ctx.preview_state_root()?;
    if replayed_root != block.header.state_root {
        return Err(CommitError::StateRootMismatch {
            header: block.header.state_root,
            replayed: replayed_root,
        });
    }

    let committed_root = ctx.commit()?;
    debug_assert_eq!(committed_root, replayed_root);

    // Emit Phase-1 metrics for the committed block.
    metrics::gauge!("arknet_consensus_height").set(block.header.height as f64);
    let mut receipts_count: u64 = 0;
    let mut disputes_count: u64 = 0;
    let mut escrow_settles: u64 = 0;
    let mut models_registered: u64 = 0;
    for tx in &block.txs {
        match &tx.tx {
            Transaction::ReceiptBatch(batch) => {
                receipts_count += batch.receipts.len() as u64;
            }
            Transaction::Dispute(_) => {
                disputes_count += 1;
            }
            Transaction::EscrowSettle { .. } => {
                escrow_settles += 1;
            }
            Transaction::RegisterModel { .. } => {
                models_registered += 1;
            }
            _ => {}
        }
    }
    if receipts_count > 0 {
        metrics::counter!("arknet_receipts_anchored_total").increment(receipts_count);
    }
    if disputes_count > 0 {
        metrics::counter!("arknet_disputes_filed_total").increment(disputes_count);
    }
    if escrow_settles > 0 {
        metrics::counter!("arknet_escrow_settles_total").increment(escrow_settles);
    }
    if models_registered > 0 {
        metrics::counter!("arknet_models_registered_total").increment(models_registered);
    }

    // Drop any committed tx from the mempool (idempotent if missing).
    let landed: Vec<_> = block.txs.iter().map(|t| t.hash()).collect();
    mempool.remove_many(&landed);

    Ok(CommitReport {
        state_root: committed_root,
        gas_used,
        applied_count,
    })
}

/// Process all pending rewards for the ending epoch. Computes the
/// per-token emission rate from the epoch's total output tokens, mints
/// block rewards from the emission budget, and distributes using the
/// 75/7/5/5/3/5 split with delegator pro-rata.
fn epoch_boundary_mint(
    ctx: &mut BlockCtx<'_>,
    ending_epoch: u64,
    current_height: Height,
) -> Result<(), CommitError> {
    let pending = ctx
        .state()
        .iter_pending_rewards_for_epoch(ending_epoch)
        .map_err(|e| CommitError::ChainState(format!("iter_pending_rewards: {e}")))?;

    if pending.is_empty() {
        return Ok(());
    }

    let total_tokens: u64 = pending.iter().map(|p| p.output_tokens as u64).sum();

    let year = arknet_payments::emission::year_for_height(current_height);
    let mut emission = arknet_payments::emission::EpochEmissionState {
        epoch: arknet_payments::emission::epoch_for_height(current_height),
        budget: arknet_payments::emission::epoch_budget(year),
        minted: 0,
        total_minted: 0,
    };

    let per_token = if total_tokens > 0 {
        arknet_payments::emission::per_token_rate(&emission, total_tokens)
    } else {
        0
    };

    let mut total_minted: Amount = 0;
    for pr in &pending {
        let base_reward = arknet_payments::rewards::compute_block_reward(
            pr.output_tokens,
            per_token,
            arknet_payments::rewards::ModelCategory::Text,
            7,
            100,
            100,
            9_500,
        );

        // TEE-verified jobs earn a multiplier on emission (1.5x at genesis).
        let block_reward = base_reward * pr.tee_multiplier_bps as u128 / 10_000;

        let minted = emission.try_mint(block_reward);
        if minted == 0 {
            continue;
        }
        total_minted = total_minted.saturating_add(minted);

        let dist = arknet_payments::rewards::distribute_reward(minted);

        for (addr, amount) in [
            (&pr.compute_addr, dist.compute),
            (&pr.verifier_addr, dist.verifier),
            (&pr.router_addr, dist.router),
            (&pr.treasury_addr, dist.treasury),
        ] {
            if amount > 0 {
                let mut acct = ctx
                    .get_account(addr)
                    .map_err(|e| CommitError::ChainState(e.to_string()))?
                    .unwrap_or_default();
                acct.balance = acct.balance.saturating_add(amount);
                ctx.set_account(addr, &acct)
                    .map_err(|e| CommitError::ChainState(e.to_string()))?;
            }
        }

        // Delegator pro-rata split: look up all stake entries for the
        // compute node and distribute the 5% delegator cut proportionally.
        if dist.delegators > 0 {
            distribute_to_delegators(ctx, &pr.compute_addr, dist.delegators)?;
        }

        // Clean up the pending reward entry.
        ctx.delete_pending_reward(&pr.job_id)
            .map_err(|e| CommitError::ChainState(e.to_string()))?;
    }

    if total_minted > 0 {
        tracing::info!(
            epoch = ending_epoch,
            pending_jobs = pending.len(),
            total_minted,
            "epoch-boundary minting complete"
        );
        metrics::counter!("arknet_rewards_minted_total").increment(total_minted as u64);
    }

    Ok(())
}

/// Advance governance proposal lifecycles at epoch boundaries.
///
/// For each proposal still in `Discussion` or `Voting` phase:
/// - If the block timestamp has passed `discussion_ends`, transition
///   from `Discussion` → `Voting`.
/// - If the block timestamp has passed `voting_ends`, tally the votes
///   and resolve to `Passed`, `Rejected`, or `RejectedWithVeto`.
///
/// On `Passed` or `Rejected`, the proposer's deposit is returned. On
/// `RejectedWithVeto`, the deposit is burned (not credited to anyone).
///
/// Vote weight uses the voter's account balance as a proxy for
/// governance power. This is standard token-weighted governance —
/// holders with more ARK carry proportionally more voting weight.
fn process_governance_proposals(
    ctx: &mut BlockCtx<'_>,
    now_ms: Timestamp,
) -> Result<(), CommitError> {
    let proposals = ctx
        .state()
        .iter_proposals()
        .map_err(|e| CommitError::ChainState(format!("iter_proposals: {e}")))?;

    if proposals.is_empty() {
        return Ok(());
    }

    // Compute total bonded stake once for quorum checks.
    let total_bonded = ctx
        .state()
        .total_bonded_stake()
        .map_err(|e| CommitError::ChainState(format!("total_bonded_stake: {e}")))?;

    let mut advanced = 0u64;
    let mut tallied = 0u64;

    for (id, mut record) in proposals {
        match record.phase {
            ProposalPhase::Discussion => {
                // Check if discussion period has ended. Use direct
                // timestamp comparison — avoids cross-crate enum types.
                if now_ms >= record.proposal.discussion_ends {
                    record.phase = ProposalPhase::Voting;
                    let bytes = borsh::to_vec(&record)
                        .map_err(|e| CommitError::ChainState(format!("proposal encode: {e}")))?;
                    ctx.set_proposal(id, &bytes)
                        .map_err(|e| CommitError::ChainState(e.to_string()))?;
                    advanced += 1;
                }
            }
            ProposalPhase::Voting => {
                // Check if voting period has ended.
                if now_ms < record.proposal.voting_ends {
                    continue;
                }

                // Tally votes.
                let votes = ctx
                    .state()
                    .iter_votes_for_proposal(id)
                    .map_err(|e| CommitError::ChainState(format!("iter_votes: {e}")))?;

                let mut tally = Tally::default();
                for (voter, choice) in &votes {
                    let weight = ctx
                        .get_account(voter)
                        .map_err(|e| CommitError::ChainState(e.to_string()))?
                        .map(|a| a.balance)
                        .unwrap_or(0);
                    tally.add_vote(*choice, weight);
                }

                let outcome = tally.evaluate(total_bonded);

                match outcome {
                    TallyOutcome::Passed => {
                        record.phase = ProposalPhase::Passed;
                        // Return deposit to proposer.
                        return_deposit(ctx, &record.proposal.proposer, record.proposal.deposit)?;
                    }
                    TallyOutcome::Rejected => {
                        record.phase = ProposalPhase::Rejected;
                        // Return deposit to proposer.
                        return_deposit(ctx, &record.proposal.proposer, record.proposal.deposit)?;
                    }
                    TallyOutcome::RejectedWithVeto => {
                        record.phase = ProposalPhase::RejectedWithVeto;
                        // Deposit is burned — not credited to anyone.
                    }
                }

                let bytes = borsh::to_vec(&record)
                    .map_err(|e| CommitError::ChainState(format!("proposal encode: {e}")))?;
                ctx.set_proposal(id, &bytes)
                    .map_err(|e| CommitError::ChainState(e.to_string()))?;
                tallied += 1;
            }
            // Already resolved — skip.
            ProposalPhase::Passed
            | ProposalPhase::Rejected
            | ProposalPhase::RejectedWithVeto
            | ProposalPhase::Executed => {}
        }
    }

    if advanced > 0 || tallied > 0 {
        tracing::info!(
            advanced,
            tallied,
            "governance proposals processed at epoch boundary"
        );
    }

    Ok(())
}

/// Credit the proposal deposit back to the proposer's account.
fn return_deposit(
    ctx: &mut BlockCtx<'_>,
    proposer: &Address,
    deposit: Amount,
) -> Result<(), CommitError> {
    if deposit == 0 {
        return Ok(());
    }
    let mut acct = ctx
        .get_account(proposer)
        .map_err(|e| CommitError::ChainState(e.to_string()))?
        .unwrap_or_default();
    acct.balance = acct.balance.saturating_add(deposit);
    ctx.set_account(proposer, &acct)
        .map_err(|e| CommitError::ChainState(e.to_string()))?;
    Ok(())
}

/// Distribute the delegator cut pro-rata across all delegators of a
/// compute node. Falls back to crediting the compute address itself
/// if no delegators are found (preserves the Phase 1 behavior).
///
/// # Algorithm
///
/// 1. Scan `CF_VALIDATORS` to find the `NodeId` whose operator
///    address matches `compute_addr`. O(validators) per epoch —
///    acceptable at genesis scale.
/// 2. Look up all stake entries for that node with `Compute` role
///    and a non-`None` delegator.
/// 3. Split `delegator_total` pro-rata by each delegator's stake
///    amount. The last delegator receives the remainder so no
///    rounding dust is lost.
/// 4. If no validator or no delegators are found, fall back to
///    crediting the compute address (preserves Phase 1 behavior).
fn distribute_to_delegators(
    ctx: &mut BlockCtx<'_>,
    compute_addr: &Address,
    delegator_total: Amount,
) -> Result<(), CommitError> {
    if delegator_total == 0 {
        return Ok(());
    }

    // Find the node_id for this compute address by scanning validators.
    let validators = ctx
        .state()
        .iter_validators()
        .map_err(|e| CommitError::ChainState(e.to_string()))?;
    let node_id = validators
        .iter()
        .find(|v| v.operator == *compute_addr)
        .map(|v| v.node_id);

    let Some(node_id) = node_id else {
        // No validator found — fall back to crediting compute address.
        credit_address(ctx, compute_addr, delegator_total)?;
        return Ok(());
    };

    // Find all stake entries for this node with Compute role that
    // have a delegator address (self-stake entries have `None`).
    let stakes = ctx
        .state()
        .iter_stakes_for_node(&node_id)
        .map_err(|e| CommitError::ChainState(e.to_string()))?;
    let delegator_stakes: Vec<_> = stakes
        .iter()
        .filter(|s| s.role == arknet_chain::StakeRole::Compute && s.delegator.is_some())
        .collect();

    if delegator_stakes.is_empty() {
        // No delegators — credit to operator.
        credit_address(ctx, compute_addr, delegator_total)?;
        return Ok(());
    }

    // Pro-rata split by stake amount.
    let total_delegated: Amount = delegator_stakes.iter().map(|s| s.amount).sum();
    if total_delegated == 0 {
        // Edge case: all zero-stake delegators — credit to operator.
        credit_address(ctx, compute_addr, delegator_total)?;
        return Ok(());
    }

    let mut distributed: Amount = 0;
    let count = delegator_stakes.len();
    for (i, stake) in delegator_stakes.iter().enumerate() {
        let share = if i == count - 1 {
            // Last delegator gets the remainder to avoid rounding loss.
            delegator_total.saturating_sub(distributed)
        } else {
            delegator_total.saturating_mul(stake.amount) / total_delegated
        };
        if share > 0 {
            // Safety: filtered above to only entries with `delegator.is_some()`.
            let addr = stake.delegator.as_ref().expect("filtered for Some above");
            credit_address(ctx, addr, share)?;
        }
        distributed = distributed.saturating_add(share);
    }

    Ok(())
}

/// Credit `amount` to a single address. Small helper to reduce
/// repetition in [`distribute_to_delegators`].
fn credit_address(
    ctx: &mut BlockCtx<'_>,
    addr: &Address,
    amount: Amount,
) -> Result<(), CommitError> {
    let mut acct = ctx
        .get_account(addr)
        .map_err(|e| CommitError::ChainState(e.to_string()))?
        .unwrap_or_default();
    acct.balance = acct.balance.saturating_add(amount);
    ctx.set_account(addr, &acct)
        .map_err(|e| CommitError::ChainState(e.to_string()))?;
    Ok(())
}

/// Genesis-default treasury address.
///
/// Phase 1 uses a well-known address derived deterministically from a
/// fixed domain tag. Phase 2 moves this to an explicit field in the
/// genesis config (governance-updatable). The address matches
/// `blake3(b"arknet-treasury-v1")[0..20]`.
fn genesis_treasury_address() -> Address {
    let digest = blake3::hash(b"arknet-treasury-v1");
    let mut out = [0u8; 20];
    out.copy_from_slice(&digest.as_bytes()[..20]);
    Address::new(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_builder::{
        BlockBuilder, BuildParams, DEFAULT_BLOCK_BYTES_BUDGET, DEFAULT_BLOCK_GAS_LIMIT,
    };
    use crate::height::Height;
    use arknet_chain::account::Account;
    use arknet_chain::transactions::{SignedTransaction, Transaction};
    use arknet_common::types::{Address, BlockHash, NodeId, PubKey, Signature, SignatureScheme};

    fn tmp_state() -> (tempfile::TempDir, State) {
        let tmp = tempfile::tempdir().unwrap();
        let state = State::open(tmp.path()).unwrap();
        (tmp, state)
    }

    fn seed_funded(state: &State, addr: Address, balance: u128) {
        let mut ctx = state.begin_block();
        ctx.set_account(&addr, &Account { balance, nonce: 0 })
            .unwrap();
        ctx.commit().unwrap();
    }

    fn transfer(from: u8, to: u8, nonce: u64, fee: u64, amount: u128) -> SignedTransaction {
        SignedTransaction {
            tx: Transaction::Transfer {
                from: Address::new([from; 20]),
                to: Address::new([to; 20]),
                amount,
                nonce,
                fee,
            },
            signer: PubKey::ed25519([from; 32]),
            signature: Signature::new(SignatureScheme::Ed25519, vec![0xaa; 64]).unwrap(),
        }
    }

    fn default_params() -> BuildParams {
        BuildParams {
            chain_id: "arknet-test".into(),
            version: 1,
            parent_hash: BlockHash::new([0; 32]),
            validator_set_hash: [0; 32],
            proposer: NodeId::new([1; 32]),
            base_fee: 1_000_000_000,
            gas_limit: DEFAULT_BLOCK_GAS_LIMIT,
            bytes_budget: DEFAULT_BLOCK_BYTES_BUDGET,
            genesis_message: String::new(),
        }
    }

    #[test]
    fn committed_root_matches_proposer_preview() {
        // Propose on one state; commit on a separate (fresh) state and
        // compare roots. The invariant consensus relies on.
        let (_tmp, proposer_state) = tmp_state();
        seed_funded(&proposer_state, Address::new([1; 20]), 10_000_000);

        let mut mempool = Mempool::default();
        let _ = mempool.insert(transfer(1, 9, 0, 21_000, 500)).unwrap();

        let built = BlockBuilder::build(&proposer_state, &mut mempool, Height(1), default_params())
            .unwrap();

        let (_tmp2, validator_state) = tmp_state();
        seed_funded(&validator_state, Address::new([1; 20]), 10_000_000);
        let mut vmempool = Mempool::default();

        let report = commit_block(&validator_state, &mut vmempool, &built.block).unwrap();
        assert_eq!(report.state_root, built.block.header.state_root);
        assert_eq!(report.applied_count, 1);
        assert_eq!(report.gas_used, 21_000);
    }

    #[test]
    fn state_root_mismatch_is_rejected() {
        let (_tmp, state) = tmp_state();
        seed_funded(&state, Address::new([1; 20]), 10_000_000);

        let mut mempool = Mempool::default();
        let _ = mempool.insert(transfer(1, 9, 0, 21_000, 500)).unwrap();
        let mut built =
            BlockBuilder::build(&state, &mut mempool, Height(1), default_params()).unwrap();

        // Tamper with the header.
        built.block.header.state_root = arknet_common::types::StateRoot::new([0xff; 32]);

        // Apply on a fresh validator state — replay root will differ.
        let (_tmp2, vstate) = tmp_state();
        seed_funded(&vstate, Address::new([1; 20]), 10_000_000);
        let mut vmempool = Mempool::default();
        let err = commit_block(&vstate, &mut vmempool, &built.block).unwrap_err();
        assert!(matches!(err, CommitError::StateRootMismatch { .. }));
        // Canonical state must be unchanged.
        // (A fresh ctx was opened inside `commit_block` and dropped.)
    }

    #[test]
    fn commit_prunes_mempool() {
        // Same-state flow: build a block from the mempool, then commit
        // it back to the same state. After commit, those txs must be
        // gone from the mempool.
        let (_tmp, state) = tmp_state();
        seed_funded(&state, Address::new([1; 20]), 10_000_000);

        let mut mempool = Mempool::default();
        mempool.insert(transfer(1, 9, 0, 21_000, 100)).unwrap();

        let built = BlockBuilder::build(&state, &mut mempool, Height(1), default_params()).unwrap();
        assert_eq!(mempool.len(), 0);

        // Re-admit so we have something to prune on commit.
        mempool.readmit(built.drained.clone());
        assert_eq!(mempool.len(), 1);

        let report = commit_block(&state, &mut mempool, &built.block).unwrap();
        assert_eq!(report.applied_count, 1);
        assert_eq!(mempool.len(), 0);
    }
}
