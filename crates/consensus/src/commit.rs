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
use arknet_chain::State;
use arknet_common::types::{Gas, StateRoot};

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
    }

    // Persist the committed height so bootstrap checks in the next
    // block's `apply_tx` see the correct value.
    ctx.set_current_height(block.header.height)?;

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

    // Drop any committed tx from the mempool (idempotent if missing).
    let landed: Vec<_> = block.txs.iter().map(|t| t.hash()).collect();
    mempool.remove_many(&landed);

    Ok(CommitReport {
        state_root: committed_root,
        gas_used,
        applied_count,
    })
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
