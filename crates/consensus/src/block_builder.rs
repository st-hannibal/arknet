//! Proposer-side block construction.
//!
//! On [`malachite_core_consensus::AppMsg::GetValue`] (i.e. "you are
//! the proposer for `(height, round)` — produce a value"), the engine
//! calls [`BlockBuilder::build`]. The builder:
//!
//! 1. Drains transactions from the mempool under the active block
//!    gas / byte budget (Phase 1 uses fee == gas, priced 1:1).
//! 2. Opens a [`BlockCtx`] against the canonical state store.
//! 3. Runs [`arknet_chain::apply::apply_tx`] over each drained tx.
//!    Rejected txs are dropped; applied txs contribute to `gas_used`
//!    and are included in the body.
//! 4. Computes `tx_root` / `receipt_root` and calls
//!    [`BlockCtx::preview_state_root`] to get the post-apply
//!    [`StateRoot`] **without committing** — commit only happens on
//!    `Decided` inside [`commit`](crate::commit).
//! 5. Returns a [`BuiltBlock`] bundling the [`Block`] and the txs
//!    that were drained from the mempool (so the engine can re-admit
//!    them if the block never finalizes).
//!
//! # Why "preview, not commit"
//!
//! Committing at propose time would persist state for a block that
//! may never reach 2/3 precommits, forking local state from peers.
//! Malachite's state machine is explicit: value construction is
//! cheap and repeatable; state transition is gated on `Decided`.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arknet_chain::apply::{apply_tx, TxOutcome};
use arknet_chain::block::{receipt_root, tx_root, Block, BlockHeader};
use arknet_chain::transactions::SignedTransaction;
use arknet_chain::State;
use arknet_common::types::{Amount, BlockHash, Gas, Hash256, NodeId};
use malachitebft_core_types::Height as MalachiteHeight;

use crate::errors::{ConsensusError, Result};
use crate::height::Height;
use crate::mempool::Mempool;

/// Block gas ceiling. Matches the genesis `gas_limit` default; the
/// real value is fed in by the engine from the current
/// `GenesisParams`. Kept here as a safe upper bound for tests.
pub const DEFAULT_BLOCK_GAS_LIMIT: Gas = 30_000_000;

/// Block body byte ceiling (under the 10 MiB [`arknet_chain::MAX_BLOCK_BYTES`]
/// hard cap — leaves headroom for the header + receipt list).
pub const DEFAULT_BLOCK_BYTES_BUDGET: usize = 8 * 1024 * 1024;

/// A freshly constructed block, paired with the transactions drained
/// from the mempool so the caller can readmit them on a failed round.
pub struct BuiltBlock {
    /// The proposed block, ready for gossip.
    pub block: Block,
    /// Transactions that were *drained* (not necessarily applied).
    /// The engine calls [`Mempool::readmit`] if consensus fails to
    /// decide on this value.
    pub drained: Vec<Arc<SignedTransaction>>,
}

/// Parameters threaded in from the engine per-block.
#[derive(Clone, Debug)]
pub struct BuildParams {
    /// Chain identifier (goes into the header).
    pub chain_id: String,
    /// Protocol version (goes into the header).
    pub version: u32,
    /// Parent block header hash. `BlockHash::new([0;32])` at genesis.
    pub parent_hash: BlockHash,
    /// Hash of the validator set active for this height.
    pub validator_set_hash: Hash256,
    /// Proposer's node id (goes into the header).
    pub proposer: NodeId,
    /// Current EIP-1559 base fee (carried into the header).
    pub base_fee: Amount,
    /// Block gas ceiling.
    pub gas_limit: Gas,
    /// Block body byte budget.
    pub bytes_budget: usize,
}

/// Proposer-side block construction helper.
pub struct BlockBuilder;

impl BlockBuilder {
    /// Build a proposal block for `height` using the current mempool
    /// and state.
    ///
    /// Returns a [`BuiltBlock`]. On state I/O errors, returns
    /// [`ConsensusError::ChainState`]; the engine should treat this
    /// as fatal and halt.
    pub fn build(
        state: &State,
        mempool: &mut Mempool,
        height: Height,
        params: BuildParams,
    ) -> Result<BuiltBlock> {
        let (drained, _gas_budget_used, _bytes_budget_used) =
            mempool.drain_for_block(params.gas_limit, params.bytes_budget);

        let mut ctx = state.begin_block();
        let mut included_txs: Vec<SignedTransaction> = Vec::with_capacity(drained.len());
        let mut tx_hashes: Vec<Hash256> = Vec::with_capacity(drained.len());
        let mut gas_used: Gas = 0;

        for tx in &drained {
            match apply_tx(&mut ctx, tx).map_err(ConsensusError::from)? {
                TxOutcome::Applied { gas_used: g } => {
                    gas_used = gas_used.saturating_add(g);
                    let h = tx.hash();
                    tx_hashes.push(*h.as_bytes());
                    included_txs.push((**tx).clone());
                }
                TxOutcome::Rejected(_reason) => {
                    // Tx no longer applies — drop silently. Rejections
                    // are not blocking; see CODING_STANDARDS "lenient
                    // mempool" rationale.
                }
            }
        }

        let state_root = ctx.preview_state_root()?;
        let tx_root_val = tx_root(&tx_hashes);
        let receipt_root_val = receipt_root(&[]); // Phase 1 Week 7-8: empty receipts
        let timestamp_ms = now_ms();

        // Note: the BlockCtx is dropped here, discarding the pending
        // writes. The `commit` module opens a fresh ctx and replays
        // the block's txs when malachite decides.
        drop(ctx);
        let _ = gas_used; // surfaced to the engine via the body (future: header field)

        let header = BlockHeader {
            version: params.version,
            chain_id: params.chain_id,
            height: height.as_u64(),
            timestamp_ms,
            parent_hash: params.parent_hash,
            state_root,
            tx_root: tx_root_val,
            receipt_root: receipt_root_val,
            proposer: params.proposer,
            validator_set_hash: params.validator_set_hash,
            base_fee: params.base_fee,
        };
        let block = Block {
            header,
            txs: included_txs,
            receipts: Vec::new(),
        };

        Ok(BuiltBlock { block, drained })
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Re-export used by tests and potential light-client validation: the
/// tx_root / receipt_root / state_root computed on a completed block
/// must match what [`BlockBuilder::build`] produced, or consensus will
/// reject it at commit-replay time.
#[allow(dead_code)]
pub(crate) fn recompute_roots(txs: &[SignedTransaction]) -> (Hash256, Hash256) {
    let tx_hashes: Vec<Hash256> = txs.iter().map(|t| *t.hash().as_bytes()).collect();
    (tx_root(&tx_hashes), receipt_root(&[]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_chain::account::Account;
    use arknet_chain::transactions::Transaction;
    use arknet_common::types::{Address, PubKey, Signature, SignatureScheme};

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
    fn empty_mempool_produces_empty_block() {
        let (_tmp, state) = tmp_state();
        let mut mempool = Mempool::default();
        let built = BlockBuilder::build(&state, &mut mempool, Height(1), default_params()).unwrap();
        assert!(built.block.txs.is_empty());
        assert_eq!(built.block.header.height, 1);
    }

    #[test]
    fn block_contains_only_applied_txs() {
        let (_tmp, state) = tmp_state();
        // Fund alice only; bob's tx will be rejected.
        seed_funded(&state, Address::new([1; 20]), 10_000_000);

        let mut mempool = Mempool::default();
        let _ = mempool.insert(transfer(1, 9, 0, 21_000, 1_000)).unwrap(); // alice → applies
        let _ = mempool.insert(transfer(2, 9, 0, 21_000, 1_000)).unwrap(); // bob  → rejected (no balance)

        let built = BlockBuilder::build(&state, &mut mempool, Height(1), default_params()).unwrap();
        assert_eq!(built.block.txs.len(), 1);
        let included = &built.block.txs[0];
        match &included.tx {
            Transaction::Transfer { from, .. } => assert_eq!(*from, Address::new([1; 20])),
            _ => panic!("wrong tx type"),
        }
        // Both were drained from the mempool.
        assert_eq!(built.drained.len(), 2);
    }

    #[test]
    fn block_state_root_matches_commit_replay() {
        // The whole reason `preview_state_root` exists: the proposer's
        // previewed root must equal what the full chain produces when
        // the block's txs are applied and committed.
        let (_tmp, state) = tmp_state();
        seed_funded(&state, Address::new([1; 20]), 10_000_000);
        let mut mempool = Mempool::default();
        let _ = mempool.insert(transfer(1, 9, 0, 21_000, 500)).unwrap();

        let built = BlockBuilder::build(&state, &mut mempool, Height(1), default_params()).unwrap();
        let previewed_root = built.block.header.state_root;

        // Replay through a fresh context, then commit.
        let mut ctx2 = state.begin_block();
        for tx in &built.block.txs {
            let _ = apply_tx(&mut ctx2, tx).unwrap();
        }
        let committed = ctx2.commit().unwrap();
        assert_eq!(previewed_root, committed);
        assert_eq!(state.state_root(), previewed_root);
    }

    #[test]
    fn header_has_expected_fields() {
        let (_tmp, state) = tmp_state();
        let mut mempool = Mempool::default();
        let params = default_params();
        let built = BlockBuilder::build(&state, &mut mempool, Height(7), params.clone()).unwrap();
        assert_eq!(built.block.header.chain_id, params.chain_id);
        assert_eq!(built.block.header.version, params.version);
        assert_eq!(built.block.header.height, 7);
        assert_eq!(built.block.header.proposer, params.proposer);
        assert_eq!(built.block.header.base_fee, params.base_fee);
        assert_eq!(built.block.header.parent_hash, params.parent_hash);
    }

    #[test]
    fn drained_count_reflects_mempool_drain() {
        let (_tmp, state) = tmp_state();
        seed_funded(&state, Address::new([1; 20]), 10_000_000);
        let mut mempool = Mempool::default();
        mempool.insert(transfer(1, 9, 0, 21_000, 1)).unwrap();
        mempool.insert(transfer(2, 9, 0, 21_000, 1)).unwrap();
        mempool.insert(transfer(3, 9, 0, 21_000, 1)).unwrap();

        let built = BlockBuilder::build(&state, &mut mempool, Height(1), default_params()).unwrap();
        assert_eq!(built.drained.len(), 3);
        assert_eq!(mempool.len(), 0);
    }

    #[test]
    fn tx_root_depends_on_included_order() {
        // Same mempool, re-shuffled by fee, should still produce a
        // deterministic tx_root ordered by the mempool's drain rule.
        let (_tmp, state) = tmp_state();
        seed_funded(&state, Address::new([1; 20]), 10_000_000);
        seed_funded(&state, Address::new([2; 20]), 10_000_000);

        let mut m1 = Mempool::default();
        m1.insert(transfer(1, 9, 0, 30_000, 1)).unwrap();
        m1.insert(transfer(2, 9, 0, 90_000, 1)).unwrap();
        let r1 = BlockBuilder::build(&state, &mut m1, Height(1), default_params())
            .unwrap()
            .block
            .header
            .tx_root;

        // Reset state and replay in the opposite insert order.
        let (_tmp2, state2) = tmp_state();
        seed_funded(&state2, Address::new([1; 20]), 10_000_000);
        seed_funded(&state2, Address::new([2; 20]), 10_000_000);
        let mut m2 = Mempool::default();
        m2.insert(transfer(2, 9, 0, 90_000, 1)).unwrap();
        m2.insert(transfer(1, 9, 0, 30_000, 1)).unwrap();
        let r2 = BlockBuilder::build(&state2, &mut m2, Height(1), default_params())
            .unwrap()
            .block
            .header
            .tx_root;

        // Both pools drain high-fee first → identical tx_roots.
        assert_eq!(r1, r2);
    }
}
