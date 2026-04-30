//! Persistent chain state: accounts, stakes, validators, parameters.
//!
//! # Architecture
//!
//! - **RocksDB** holds the canonical serialized state, partitioned into
//!   column families (one per domain).
//! - **Sparse Merkle Tree** mirrors the `accounts` column and produces a
//!   deterministic 32-byte [`StateRoot`]. Non-account columns are folded
//!   into the root via keyed leaves (see `root.rs`). Phase 1 Week 3-4
//!   commits only accounts to the SMT; stakes / validators move into the
//!   tree in Week 9 when slashing needs proof-friendly state.
//! - **WriteBatch overlay** collects all mutations during block
//!   application. `commit_block` flushes atomically; a dropped `BlockCtx`
//!   discards the overlay — partial state cannot persist.
//!
//! # What is intentionally out of scope (Week 3-4)
//!
//! - Pending-unbonding queue (Week 9).
//! - Pool state (Week 10).
//! - Receipt pending-verification queue (Week 11).
//! - Proposal / vote tallies (Week 9+).
//! - Free-tier quota tracking (Week 10).

use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use rocksdb::{ColumnFamily, ColumnFamilyDescriptor, Options, WriteBatch, DB};
use sparse_merkle_tree::{
    blake2b::Blake2bHasher, default_store::DefaultStore, traits::Value, SparseMerkleTree, H256,
};

use arknet_common::types::{Address, Height, JobId, NodeId, StateRoot};

use crate::account::Account;
use crate::errors::{ChainError, Result};
use crate::stake_entry::StakeEntry;
use crate::transactions::StakeRole;
use crate::unbonding::UnbondingEntry;
use crate::validator::ValidatorInfo;

// ─── Column families ──────────────────────────────────────────────────────

const CF_ACCOUNTS: &str = "accounts";
const CF_STAKES: &str = "stakes";
const CF_VALIDATORS: &str = "validators";
const CF_PARAMS: &str = "params";
const CF_META: &str = "meta"; // last committed state root, height, etc.
const CF_UNBONDINGS: &str = "unbondings";
/// Dedup index for anchored inference receipts.
/// Keyed by `job_id` (32 bytes); value is the height the receipt
/// landed at. `Transaction::ReceiptBatch` rejects any receipt whose
/// `job_id` is already present — §6's "seen exactly once" invariant.
const CF_RECEIPTS_SEEN: &str = "receipts_seen";
/// Escrow entries keyed by `job_id` (32 bytes).
const CF_ESCROWS: &str = "escrows";
/// Governance proposals keyed by `proposal_id` (u64 BE).
const CF_PROPOSALS: &str = "proposals";
/// Governance votes keyed by `proposal_id(8) || voter(20)`.
const CF_VOTES: &str = "votes";

// ─── Value wrapper for SMT account leaves ─────────────────────────────────

/// Wraps [`Account`] so the SMT can turn it into an [`H256`] leaf.
///
/// Empty accounts hash to [`H256::zero()`] so the SMT treats missing and
/// explicitly-empty accounts identically — any state root change must reflect
/// a real state delta.
#[derive(Clone, Default, Debug)]
struct AccountLeaf(Account);

impl Value for AccountLeaf {
    fn to_h256(&self) -> H256 {
        if self.0.is_empty() {
            return H256::zero();
        }
        let encoded = borsh::to_vec(&self.0).expect("account encode infallible");
        let digest = arknet_crypto::hash::blake3(&encoded);
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(digest.as_bytes());
        bytes.into()
    }
    fn zero() -> Self {
        AccountLeaf(Account::ZERO)
    }
}

type AccountSmt = SparseMerkleTree<Blake2bHasher, AccountLeaf, DefaultStore<AccountLeaf>>;

// ─── Key helpers ──────────────────────────────────────────────────────────

fn smt_key_for_account(addr: &Address) -> H256 {
    // Pad the 20-byte address out to 32 bytes so it fits H256. The padding
    // is deterministic and collision-free because addresses are already
    // hashes.
    let mut bytes = [0u8; 32];
    bytes[..20].copy_from_slice(addr.as_bytes());
    bytes.into()
}

/// Stake composite key.
///
/// Layout: `node_id(32) | role(1) | pool_present(1) | pool(16?) | delegator_present(1) | delegator(20?)`.
/// Every `(node_id, role, pool_id?, delegator?)` tuple has exactly one
/// `StakeEntry` — self-stake is `delegator = None`, each delegator gets
/// its own entry for pro-rata slashing math (§9.2).
fn stake_key(
    node_id: &NodeId,
    role: StakeRole,
    pool_id_opt: Option<[u8; 16]>,
    delegator: Option<&Address>,
) -> Vec<u8> {
    let mut k = Vec::with_capacity(32 + 1 + 17 + 21);
    k.extend_from_slice(node_id.as_bytes());
    k.push(role as u8);
    match pool_id_opt {
        Some(p) => {
            k.push(1);
            k.extend_from_slice(&p);
        }
        None => {
            k.push(0);
        }
    }
    match delegator {
        Some(a) => {
            k.push(1);
            k.extend_from_slice(a.as_bytes());
        }
        None => {
            k.push(0);
        }
    }
    k
}

/// Unbonding entry key: `node_id(32) | u64 BE unbond_id(8)`.
fn unbond_key(node_id: &NodeId, unbond_id: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(32 + 8);
    k.extend_from_slice(node_id.as_bytes());
    k.extend_from_slice(&unbond_id.to_be_bytes());
    k
}

// Meta keys (single-row values under `CF_META`).
const META_NEXT_UNBOND_ID: &[u8] = b"next_unbond_id";
const META_CURRENT_HEIGHT: &[u8] = b"current_height";

// ─── State handle ─────────────────────────────────────────────────────────

/// Top-level chain state — one instance per node process.
///
/// Holds the RocksDB handle and an in-memory SMT mirroring the `accounts`
/// column. Reads hit the overlay first, then the DB. Writes go through
/// [`BlockCtx`] which commits or rolls back atomically.
pub struct State {
    db: Arc<DB>,
    smt: Arc<Mutex<AccountSmt>>,
}

impl State {
    /// Open (or create) a chain state database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);

        let cfs = [
            CF_ACCOUNTS,
            CF_STAKES,
            CF_VALIDATORS,
            CF_PARAMS,
            CF_META,
            CF_UNBONDINGS,
            CF_RECEIPTS_SEEN,
            CF_ESCROWS,
            CF_PROPOSALS,
            CF_VOTES,
        ]
        .iter()
        .map(|name| ColumnFamilyDescriptor::new(*name, Options::default()))
        .collect::<Vec<_>>();

        let db = DB::open_cf_descriptors(&opts, path, cfs)
            .map_err(|e| ChainError::Codec(format!("rocksdb open: {e}")))?;

        // Rebuild the SMT from whatever accounts are on disk.
        let cf = db
            .cf_handle(CF_ACCOUNTS)
            .ok_or_else(|| ChainError::Codec("accounts CF missing".into()))?;
        let mut smt = AccountSmt::default();
        let iter = db.iterator_cf(cf, rocksdb::IteratorMode::Start);
        for kv in iter {
            let (k, v) = kv.map_err(|e| ChainError::Codec(format!("rocksdb iter: {e}")))?;
            if k.len() != 20 {
                continue;
            }
            let mut addr_bytes = [0u8; 20];
            addr_bytes.copy_from_slice(&k);
            let addr = Address::new(addr_bytes);
            let account: Account = borsh::from_slice(&v)
                .map_err(|e| ChainError::Codec(format!("account decode: {e}")))?;
            smt.update(smt_key_for_account(&addr), AccountLeaf(account))
                .map_err(|e| ChainError::Codec(format!("smt update: {e:?}")))?;
        }

        Ok(Self {
            db: Arc::new(db),
            smt: Arc::new(Mutex::new(smt)),
        })
    }

    // ── Reads ────────────────────────────────────────────────────────────

    /// Look up an account. Returns `None` for empty / never-seen addresses.
    pub fn get_account(&self, addr: &Address) -> Result<Option<Account>> {
        let cf = self.cf(CF_ACCOUNTS)?;
        match self
            .db
            .get_cf(cf, addr.as_bytes())
            .map_err(|e| ChainError::Codec(format!("rocksdb get: {e}")))?
        {
            None => Ok(None),
            Some(bytes) => {
                let acct: Account = borsh::from_slice(&bytes)
                    .map_err(|e| ChainError::Codec(format!("account decode: {e}")))?;
                Ok(Some(acct))
            }
        }
    }

    /// Look up a stake entry by its composite key.
    ///
    /// `delegator = None` targets the node-operator's self-stake;
    /// `Some(addr)` targets a specific delegator's position (§9.2
    /// requires per-delegator entries so pro-rata slashing lines up
    /// with on-chain evidence).
    pub fn get_stake(
        &self,
        node_id: &NodeId,
        role: StakeRole,
        pool_id: Option<[u8; 16]>,
        delegator: Option<&Address>,
    ) -> Result<Option<StakeEntry>> {
        let cf = self.cf(CF_STAKES)?;
        match self
            .db
            .get_cf(cf, stake_key(node_id, role, pool_id, delegator))
            .map_err(|e| ChainError::Codec(format!("rocksdb get: {e}")))?
        {
            None => Ok(None),
            Some(bytes) => {
                let entry: StakeEntry = borsh::from_slice(&bytes)
                    .map_err(|e| ChainError::Codec(format!("stake decode: {e}")))?;
                Ok(Some(entry))
            }
        }
    }

    /// Iterate all stake entries bound to `node_id`. The stakes CF is
    /// prefix-keyed by `node_id`, so RocksDB's iterator stops naturally
    /// at the next node's prefix.
    ///
    /// Used by the validator-set ranker (§9.5) + slashing (§10) to sum
    /// self + delegated stake and to identify every delegator under a
    /// node for pro-rata penalties.
    pub fn iter_stakes_for_node(&self, node_id: &NodeId) -> Result<Vec<StakeEntry>> {
        let cf = self.cf(CF_STAKES)?;
        let prefix = node_id.as_bytes();
        let mode = rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward);
        let mut out = Vec::new();
        for kv in self.db.iterator_cf(cf, mode) {
            let (k, v) = kv.map_err(|e| ChainError::Codec(format!("rocksdb iter: {e}")))?;
            if !k.starts_with(prefix) {
                break;
            }
            let entry: StakeEntry = borsh::from_slice(&v)
                .map_err(|e| ChainError::Codec(format!("stake decode: {e}")))?;
            out.push(entry);
        }
        Ok(out)
    }

    /// Iterate every validator record. Used once per epoch boundary to
    /// rebuild the active set.
    pub fn iter_validators(&self) -> Result<Vec<ValidatorInfo>> {
        let cf = self.cf(CF_VALIDATORS)?;
        let mut out = Vec::new();
        for kv in self.db.iterator_cf(cf, rocksdb::IteratorMode::Start) {
            let (_, v) = kv.map_err(|e| ChainError::Codec(format!("rocksdb iter: {e}")))?;
            let info: ValidatorInfo = borsh::from_slice(&v)
                .map_err(|e| ChainError::Codec(format!("validator decode: {e}")))?;
            out.push(info);
        }
        Ok(out)
    }

    /// Look up a validator by node id.
    pub fn get_validator(&self, node_id: &NodeId) -> Result<Option<ValidatorInfo>> {
        let cf = self.cf(CF_VALIDATORS)?;
        match self
            .db
            .get_cf(cf, node_id.as_bytes())
            .map_err(|e| ChainError::Codec(format!("rocksdb get: {e}")))?
        {
            None => Ok(None),
            Some(bytes) => {
                let v: ValidatorInfo = borsh::from_slice(&bytes)
                    .map_err(|e| ChainError::Codec(format!("validator decode: {e}")))?;
                Ok(Some(v))
            }
        }
    }

    /// Retrieve an unbonding entry by its composite `(node_id, unbond_id)`
    /// key.
    pub fn get_unbonding(
        &self,
        node_id: &NodeId,
        unbond_id: u64,
    ) -> Result<Option<UnbondingEntry>> {
        let cf = self.cf(CF_UNBONDINGS)?;
        match self
            .db
            .get_cf(cf, unbond_key(node_id, unbond_id))
            .map_err(|e| ChainError::Codec(format!("rocksdb get: {e}")))?
        {
            None => Ok(None),
            Some(bytes) => {
                let e: UnbondingEntry = borsh::from_slice(&bytes)
                    .map_err(|e| ChainError::Codec(format!("unbonding decode: {e}")))?;
                Ok(Some(e))
            }
        }
    }

    /// Iterate every pending unbonding for a node. Used by
    /// `StakeOp::Complete` to validate and by slashing to trim
    /// in-flight amounts.
    pub fn iter_unbondings_for_node(&self, node_id: &NodeId) -> Result<Vec<UnbondingEntry>> {
        let cf = self.cf(CF_UNBONDINGS)?;
        let prefix = node_id.as_bytes();
        let mode = rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward);
        let mut out = Vec::new();
        for kv in self.db.iterator_cf(cf, mode) {
            let (k, v) = kv.map_err(|e| ChainError::Codec(format!("rocksdb iter: {e}")))?;
            if !k.starts_with(prefix) {
                break;
            }
            let entry: UnbondingEntry = borsh::from_slice(&v)
                .map_err(|e| ChainError::Codec(format!("unbonding decode: {e}")))?;
            out.push(entry);
        }
        Ok(out)
    }

    /// Read the last-committed block height. Used to bootstrap the
    /// consensus engine after a restart. Returns `None` on a fresh chain.
    pub fn current_height(&self) -> Result<Option<Height>> {
        let cf = self.cf(CF_META)?;
        match self
            .db
            .get_cf(cf, META_CURRENT_HEIGHT)
            .map_err(|e| ChainError::Codec(format!("rocksdb get: {e}")))?
        {
            None => Ok(None),
            Some(bytes) => {
                if bytes.len() != 8 {
                    return Err(ChainError::Codec(format!(
                        "current_height expected 8 bytes, got {}",
                        bytes.len()
                    )));
                }
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&bytes);
                Ok(Some(u64::from_be_bytes(arr)))
            }
        }
    }

    /// Read the monotonic unbonding-id counter.
    pub fn next_unbond_id(&self) -> Result<u64> {
        let cf = self.cf(CF_META)?;
        match self
            .db
            .get_cf(cf, META_NEXT_UNBOND_ID)
            .map_err(|e| ChainError::Codec(format!("rocksdb get: {e}")))?
        {
            None => Ok(0),
            Some(bytes) => {
                if bytes.len() != 8 {
                    return Err(ChainError::Codec("next_unbond_id invalid length".into()));
                }
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&bytes);
                Ok(u64::from_be_bytes(arr))
            }
        }
    }

    /// Read the next proposal id counter.
    pub fn next_proposal_id(&self) -> Result<u64> {
        let cf = self.cf(CF_META)?;
        match self
            .db
            .get_cf(cf, b"next_proposal_id")
            .map_err(|e| ChainError::Codec(format!("rocksdb get: {e}")))?
        {
            None => Ok(0),
            Some(bytes) => {
                if bytes.len() != 8 {
                    return Err(ChainError::Codec("next_proposal_id invalid length".into()));
                }
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&bytes);
                Ok(u64::from_be_bytes(arr))
            }
        }
    }

    /// `true` if a receipt for `job_id` was already anchored in a
    /// prior block. §6 invariant: `ReceiptBatch` must not double-anchor.
    pub fn is_receipt_seen(&self, job_id: &JobId) -> Result<bool> {
        let cf = self.cf(CF_RECEIPTS_SEEN)?;
        Ok(self
            .db
            .get_cf(cf, job_id.0)
            .map_err(|e| ChainError::Codec(format!("rocksdb get receipts_seen: {e}")))?
            .is_some())
    }

    /// Current state root (reflects committed state only).
    pub fn state_root(&self) -> StateRoot {
        let root = self.smt.lock().root().to_owned();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(root.as_slice());
        StateRoot::new(bytes)
    }

    // ── Block application ────────────────────────────────────────────────

    /// Begin a block — obtain a mutable context that buffers writes.
    pub fn begin_block(&self) -> BlockCtx<'_> {
        BlockCtx {
            state: self,
            batch: WriteBatch::default(),
            pending_smt_updates: Vec::new(),
            account_overlay: std::collections::HashMap::new(),
            stake_overlay: std::collections::HashMap::new(),
            unbonding_overlay: std::collections::HashMap::new(),
            receipt_seen_overlay: std::collections::HashMap::new(),
            pending_next_unbond_id: None,
            pending_current_height: None,
        }
    }

    // ── Internal ─────────────────────────────────────────────────────────

    fn cf(&self, name: &str) -> Result<&ColumnFamily> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| ChainError::Codec(format!("missing column family {name}")))
    }
}

/// Mutating context for a single block's application.
///
/// Writes are buffered in a RocksDB `WriteBatch`, a list of pending SMT
/// updates, and an in-memory overlay of account writes so later reads
/// within the same block see prior mutations. Call
/// [`commit`](BlockCtx::commit) to apply atomically; drop without
/// committing to discard.
pub struct BlockCtx<'s> {
    state: &'s State,
    batch: WriteBatch,
    pending_smt_updates: Vec<(H256, AccountLeaf)>,
    /// Most recent uncommitted account writes this block. `None` means
    /// the account was explicitly emptied and should read as missing.
    account_overlay: std::collections::HashMap<Address, Option<Account>>,
    /// Stake writes buffered this block. Key mirrors the stake_key()
    /// byte layout; `None` marks a deletion so later reads in the same
    /// block see "absent" rather than the on-disk value.
    stake_overlay: std::collections::HashMap<Vec<u8>, Option<StakeEntry>>,
    /// Unbonding writes buffered this block (same semantics).
    unbonding_overlay: std::collections::HashMap<Vec<u8>, Option<UnbondingEntry>>,
    /// Receipt-seen marks buffered this block. Key: `job_id` bytes;
    /// value: height at which the receipt anchored.
    receipt_seen_overlay: std::collections::HashMap<[u8; 32], Height>,
    /// Pending next-unbond-id counter (overlay over META).
    pending_next_unbond_id: Option<u64>,
    /// Pending current-height (overlay over META).
    pending_current_height: Option<Height>,
}

impl BlockCtx<'_> {
    /// Borrow the underlying [`State`]. Used by staking handlers that
    /// need to issue reads (`get_stake`, `get_unbonding`,
    /// `next_unbond_id`) while holding a mutable block context.
    pub fn state(&self) -> &State {
        self.state
    }

    /// Buffer an account write.
    pub fn set_account(&mut self, addr: &Address, acct: &Account) -> Result<()> {
        let cf = self.state.cf(CF_ACCOUNTS)?;
        let bytes =
            borsh::to_vec(acct).map_err(|e| ChainError::Codec(format!("account encode: {e}")))?;
        if acct.is_empty() {
            self.batch.delete_cf(cf, addr.as_bytes());
            self.account_overlay.insert(*addr, None);
        } else {
            self.batch.put_cf(cf, addr.as_bytes(), &bytes);
            self.account_overlay.insert(*addr, Some(acct.clone()));
        }
        self.pending_smt_updates
            .push((smt_key_for_account(addr), AccountLeaf(acct.clone())));
        Ok(())
    }

    /// Buffer a stake entry write. `delegator = None` targets the
    /// node operator's self-stake.
    pub fn set_stake(
        &mut self,
        node_id: &NodeId,
        role: StakeRole,
        pool_id: Option<[u8; 16]>,
        delegator: Option<&Address>,
        entry: &StakeEntry,
    ) -> Result<()> {
        let cf = self.state.cf(CF_STAKES)?;
        let key = stake_key(node_id, role, pool_id, delegator);
        if entry.is_empty() {
            self.batch.delete_cf(cf, &key);
            self.stake_overlay.insert(key, None);
        } else {
            let bytes = borsh::to_vec(entry)
                .map_err(|e| ChainError::Codec(format!("stake encode: {e}")))?;
            self.batch.put_cf(cf, &key, &bytes);
            self.stake_overlay.insert(key, Some(entry.clone()));
        }
        Ok(())
    }

    /// Buffer an unbonding-entry write.
    pub fn set_unbonding(&mut self, node_id: &NodeId, entry: &UnbondingEntry) -> Result<()> {
        let cf = self.state.cf(CF_UNBONDINGS)?;
        let key = unbond_key(node_id, entry.unbond_id);
        let bytes = borsh::to_vec(entry)
            .map_err(|e| ChainError::Codec(format!("unbonding encode: {e}")))?;
        self.batch.put_cf(cf, &key, &bytes);
        self.unbonding_overlay.insert(key, Some(entry.clone()));
        Ok(())
    }

    /// Remove an unbonding entry (called on `StakeOp::Complete` after
    /// the 14-day window has passed).
    pub fn delete_unbonding(&mut self, node_id: &NodeId, unbond_id: u64) -> Result<()> {
        let cf = self.state.cf(CF_UNBONDINGS)?;
        let key = unbond_key(node_id, unbond_id);
        self.batch.delete_cf(cf, &key);
        self.unbonding_overlay.insert(key, None);
        Ok(())
    }

    /// Record the next-unbond-id counter. Caller increments by 1 and
    /// passes in; the commit hook persists.
    pub fn set_next_unbond_id(&mut self, next: u64) -> Result<()> {
        let cf = self.state.cf(CF_META)?;
        self.batch
            .put_cf(cf, META_NEXT_UNBOND_ID, next.to_be_bytes());
        self.pending_next_unbond_id = Some(next);
        Ok(())
    }

    /// Record the current block height in META. Called by commit
    /// bookkeeping so `State::current_height()` survives restart.
    pub fn set_current_height(&mut self, height: Height) -> Result<()> {
        let cf = self.state.cf(CF_META)?;
        self.batch
            .put_cf(cf, META_CURRENT_HEIGHT, height.to_be_bytes());
        self.pending_current_height = Some(height);
        Ok(())
    }

    /// Look up a stake entry, consulting this block's overlay first.
    pub fn get_stake(
        &self,
        node_id: &NodeId,
        role: StakeRole,
        pool_id: Option<[u8; 16]>,
        delegator: Option<&Address>,
    ) -> Result<Option<StakeEntry>> {
        let key = stake_key(node_id, role, pool_id, delegator);
        if let Some(entry) = self.stake_overlay.get(&key) {
            return Ok(entry.clone());
        }
        self.state.get_stake(node_id, role, pool_id, delegator)
    }

    /// Look up an unbonding entry, consulting this block's overlay first.
    pub fn get_unbonding(
        &self,
        node_id: &NodeId,
        unbond_id: u64,
    ) -> Result<Option<UnbondingEntry>> {
        let key = unbond_key(node_id, unbond_id);
        if let Some(entry) = self.unbonding_overlay.get(&key) {
            return Ok(entry.clone());
        }
        self.state.get_unbonding(node_id, unbond_id)
    }

    /// Read the next-unbond-id counter, honoring any in-block override.
    pub fn next_unbond_id(&self) -> Result<u64> {
        if let Some(v) = self.pending_next_unbond_id {
            return Ok(v);
        }
        self.state.next_unbond_id()
    }

    /// Read the current chain height, honoring any in-block override.
    pub fn current_height(&self) -> Result<Option<Height>> {
        if let Some(h) = self.pending_current_height {
            return Ok(Some(h));
        }
        self.state.current_height()
    }

    /// `true` if `job_id` was already anchored — checks both the
    /// committed store and the current block's overlay so a batch that
    /// contains two receipts with the same `job_id` gets rejected at
    /// the second one.
    pub fn is_receipt_seen(&self, job_id: &JobId) -> Result<bool> {
        if self.receipt_seen_overlay.contains_key(&job_id.0) {
            return Ok(true);
        }
        self.state.is_receipt_seen(job_id)
    }

    /// Buffer a receipt-seen mark. Idempotent within a block.
    pub fn mark_receipt_seen(&mut self, job_id: &JobId, height: Height) -> Result<()> {
        let cf = self.state.cf(CF_RECEIPTS_SEEN)?;
        self.batch.put_cf(cf, job_id.0, height.to_be_bytes());
        self.receipt_seen_overlay.insert(job_id.0, height);
        Ok(())
    }

    /// Read an escrow entry by job id.
    pub fn get_escrow(&self, job_id: &JobId) -> Result<Option<Vec<u8>>> {
        let cf = self.state.cf(CF_ESCROWS)?;
        match self
            .state
            .db
            .get_cf(cf, job_id.0)
            .map_err(|e| ChainError::Codec(format!("rocksdb get escrow: {e}")))?
        {
            None => Ok(None),
            Some(bytes) => Ok(Some(bytes.to_vec())),
        }
    }

    /// Write an escrow entry.
    pub fn set_escrow(&mut self, job_id: &JobId, data: &[u8]) -> Result<()> {
        let cf = self.state.cf(CF_ESCROWS)?;
        self.batch.put_cf(cf, job_id.0, data);
        Ok(())
    }

    /// Delete an escrow entry (after settle or refund).
    pub fn delete_escrow(&mut self, job_id: &JobId) -> Result<()> {
        let cf = self.state.cf(CF_ESCROWS)?;
        self.batch.delete_cf(cf, job_id.0);
        Ok(())
    }

    /// Read a proposal record by id.
    pub fn get_proposal(&self, id: u64) -> Result<Option<Vec<u8>>> {
        let cf = self.state.cf(CF_PROPOSALS)?;
        match self
            .state
            .db
            .get_cf(cf, id.to_be_bytes())
            .map_err(|e| ChainError::Codec(format!("rocksdb get proposal: {e}")))?
        {
            None => Ok(None),
            Some(bytes) => Ok(Some(bytes.to_vec())),
        }
    }

    /// Write a proposal record.
    pub fn set_proposal(&mut self, id: u64, data: &[u8]) -> Result<()> {
        let cf = self.state.cf(CF_PROPOSALS)?;
        self.batch.put_cf(cf, id.to_be_bytes(), data);
        Ok(())
    }

    /// Read a vote record.
    pub fn get_vote(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let cf = self.state.cf(CF_VOTES)?;
        match self
            .state
            .db
            .get_cf(cf, key)
            .map_err(|e| ChainError::Codec(format!("rocksdb get vote: {e}")))?
        {
            None => Ok(None),
            Some(bytes) => Ok(Some(bytes.to_vec())),
        }
    }

    /// Write a vote record.
    pub fn set_vote(&mut self, key: &[u8], data: &[u8]) -> Result<()> {
        let cf = self.state.cf(CF_VOTES)?;
        self.batch.put_cf(cf, key, data);
        Ok(())
    }

    /// Write the next proposal id counter.
    pub fn set_next_proposal_id(&mut self, next: u64) -> Result<()> {
        let cf = self.state.cf(CF_META)?;
        self.batch
            .put_cf(cf, b"next_proposal_id", next.to_be_bytes());
        Ok(())
    }

    /// Buffer a validator record write.
    pub fn set_validator(&mut self, node_id: &NodeId, info: &ValidatorInfo) -> Result<()> {
        let cf = self.state.cf(CF_VALIDATORS)?;
        let bytes =
            borsh::to_vec(info).map_err(|e| ChainError::Codec(format!("validator encode: {e}")))?;
        self.batch.put_cf(cf, node_id.as_bytes(), &bytes);
        Ok(())
    }

    /// Remove a validator (e.g. evicted at epoch boundary for failing the
    /// post-bootstrap `min_stake(Validator)` check).
    pub fn delete_validator(&mut self, node_id: &NodeId) -> Result<()> {
        let cf = self.state.cf(CF_VALIDATORS)?;
        self.batch.delete_cf(cf, node_id.as_bytes());
        Ok(())
    }

    /// Look up an account, consulting this block's overlay first. Reads
    /// see writes from the same block even before [`commit`] — necessary
    /// for chained transfers (sender's nonce after tx N-1 → tx N).
    pub fn get_account(&self, addr: &Address) -> Result<Option<Account>> {
        if let Some(entry) = self.account_overlay.get(addr) {
            return Ok(entry.clone());
        }
        self.state.get_account(addr)
    }

    /// Peek the state root that **would** result if all pending SMT
    /// updates were committed right now, without actually persisting.
    ///
    /// Implementation: locks the SMT, stashes current leaf values for
    /// each pending key, applies the new leaves, records the root,
    /// then rolls each key back to its stashed value. Safe because
    /// [`DefaultStore`] is an in-memory HashMap and all operations
    /// happen under one lock.
    ///
    /// # Why consensus needs this
    ///
    /// The block proposer must write `state_root` into the header it
    /// signs — but committing at propose time would persist state for
    /// a block that may never reach 2/3 precommits. The engine keeps
    /// the [`BlockCtx`] alive across the voting phase and only calls
    /// [`commit`](Self::commit) on `Decided`.
    pub fn preview_state_root(&self) -> Result<StateRoot> {
        let mut smt = self.state.smt.lock();

        // Stash current leaves (default = zero if absent).
        let mut stashed: Vec<(H256, AccountLeaf)> =
            Vec::with_capacity(self.pending_smt_updates.len());
        for (key, _) in &self.pending_smt_updates {
            let prev = smt
                .get(key)
                .map_err(|e| ChainError::Codec(format!("smt preview get: {e:?}")))?;
            stashed.push((*key, AccountLeaf(prev.0)));
        }

        // Apply pending updates → read root.
        for (key, leaf) in &self.pending_smt_updates {
            smt.update(*key, leaf.clone())
                .map_err(|e| ChainError::Codec(format!("smt preview apply: {e:?}")))?;
        }
        let root = smt.root().to_owned();

        // Roll back.
        for (key, leaf) in stashed.into_iter().rev() {
            smt.update(key, leaf)
                .map_err(|e| ChainError::Codec(format!("smt preview rollback: {e:?}")))?;
        }

        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(root.as_slice());
        Ok(StateRoot::new(bytes))
    }

    /// Commit all buffered writes. Atomic: either the full block lands or
    /// nothing does.
    pub fn commit(self) -> Result<StateRoot> {
        self.state
            .db
            .write(self.batch)
            .map_err(|e| ChainError::Codec(format!("rocksdb write: {e}")))?;

        let mut smt = self.state.smt.lock();
        for (key, leaf) in self.pending_smt_updates {
            smt.update(key, leaf)
                .map_err(|e| ChainError::Codec(format!("smt update: {e:?}")))?;
        }
        let root = smt.root().to_owned();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(root.as_slice());
        Ok(StateRoot::new(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_state() -> (tempfile::TempDir, State) {
        let tmp = tempfile::tempdir().unwrap();
        let state = State::open(tmp.path()).unwrap();
        (tmp, state)
    }

    #[test]
    fn empty_state_has_zero_root() {
        let (_tmp, state) = tmp_state();
        assert_eq!(state.state_root(), StateRoot::new([0u8; 32]));
    }

    #[test]
    fn account_write_read_roundtrip() {
        let (_tmp, state) = tmp_state();
        let addr = Address::new([7; 20]);
        let acct = Account {
            balance: 500,
            nonce: 1,
        };

        let mut ctx = state.begin_block();
        ctx.set_account(&addr, &acct).unwrap();
        ctx.commit().unwrap();

        let got = state.get_account(&addr).unwrap();
        assert_eq!(got, Some(acct));
    }

    #[test]
    fn empty_account_is_deleted() {
        let (_tmp, state) = tmp_state();
        let addr = Address::new([8; 20]);

        // Write a balance…
        let mut ctx = state.begin_block();
        ctx.set_account(
            &addr,
            &Account {
                balance: 100,
                nonce: 0,
            },
        )
        .unwrap();
        ctx.commit().unwrap();
        assert!(state.get_account(&addr).unwrap().is_some());

        // …then zero it out.
        let mut ctx = state.begin_block();
        ctx.set_account(&addr, &Account::ZERO).unwrap();
        ctx.commit().unwrap();
        assert_eq!(state.get_account(&addr).unwrap(), None);
    }

    #[test]
    fn state_root_changes_with_writes() {
        let (_tmp, state) = tmp_state();
        let root0 = state.state_root();

        let mut ctx = state.begin_block();
        ctx.set_account(
            &Address::new([1; 20]),
            &Account {
                balance: 42,
                nonce: 0,
            },
        )
        .unwrap();
        ctx.commit().unwrap();
        let root1 = state.state_root();

        assert_ne!(root0, root1);
    }

    #[test]
    fn dropped_ctx_does_not_persist() {
        let (_tmp, state) = tmp_state();
        let root0 = state.state_root();

        let mut ctx = state.begin_block();
        ctx.set_account(
            &Address::new([1; 20]),
            &Account {
                balance: 999,
                nonce: 5,
            },
        )
        .unwrap();
        drop(ctx);

        assert_eq!(state.state_root(), root0);
        assert_eq!(state.get_account(&Address::new([1; 20])).unwrap(), None);
    }

    #[test]
    fn same_writes_produce_same_state_root() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        let s1 = State::open(tmp1.path()).unwrap();
        let s2 = State::open(tmp2.path()).unwrap();

        for state in [&s1, &s2] {
            let mut ctx = state.begin_block();
            for i in 1u8..=5 {
                ctx.set_account(
                    &Address::new([i; 20]),
                    &Account {
                        balance: i as u128 * 100,
                        nonce: i as u64,
                    },
                )
                .unwrap();
            }
            ctx.commit().unwrap();
        }

        assert_eq!(s1.state_root(), s2.state_root());
    }

    #[test]
    fn state_survives_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_owned();

        let root_before;
        {
            let state = State::open(&path).unwrap();
            let mut ctx = state.begin_block();
            for i in 1u8..=10 {
                ctx.set_account(
                    &Address::new([i; 20]),
                    &Account {
                        balance: i as u128 * 1_000,
                        nonce: i as u64,
                    },
                )
                .unwrap();
            }
            ctx.commit().unwrap();
            root_before = state.state_root();
        }

        let state = State::open(&path).unwrap();
        assert_eq!(state.state_root(), root_before);
        let acct = state.get_account(&Address::new([5; 20])).unwrap().unwrap();
        assert_eq!(acct.balance, 5 * 1_000);
        assert_eq!(acct.nonce, 5);
    }

    #[test]
    fn stake_roundtrip() {
        let (_tmp, state) = tmp_state();
        let node_id = NodeId::new([9; 32]);
        let entry = StakeEntry {
            node_id,
            role: StakeRole::Compute,
            pool_id: None,
            delegator: None,
            amount: 5_000,
            bonded_at: 42,
        };

        let mut ctx = state.begin_block();
        ctx.set_stake(&node_id, StakeRole::Compute, None, None, &entry)
            .unwrap();
        ctx.commit().unwrap();

        let got = state
            .get_stake(&node_id, StakeRole::Compute, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(got, entry);
    }

    #[test]
    fn preview_state_root_matches_commit_and_is_non_mutating() {
        let (_tmp, state) = tmp_state();
        let root_before = state.state_root();

        let mut ctx = state.begin_block();
        ctx.set_account(
            &Address::new([1; 20]),
            &Account {
                balance: 42,
                nonce: 0,
            },
        )
        .unwrap();
        ctx.set_account(
            &Address::new([2; 20]),
            &Account {
                balance: 7,
                nonce: 1,
            },
        )
        .unwrap();

        // Peek the post-apply root twice; must agree and must not
        // mutate the canonical tree.
        let preview_a = ctx.preview_state_root().unwrap();
        let preview_b = ctx.preview_state_root().unwrap();
        assert_eq!(preview_a, preview_b);
        assert_eq!(state.state_root(), root_before);

        // After commit, the canonical root must match the preview.
        let committed = ctx.commit().unwrap();
        assert_eq!(committed, preview_a);
        assert_eq!(state.state_root(), preview_a);
    }

    #[test]
    fn validator_roundtrip() {
        let (_tmp, state) = tmp_state();
        let v = ValidatorInfo {
            node_id: NodeId::new([4; 32]),
            consensus_key: arknet_common::types::PubKey::ed25519([5; 32]),
            operator: Address::new([6; 20]),
            bonded_stake: 0,
            voting_power: 1,
            is_genesis: true,
            jailed: false,
        };
        let mut ctx = state.begin_block();
        ctx.set_validator(&v.node_id, &v).unwrap();
        ctx.commit().unwrap();

        let got = state.get_validator(&v.node_id).unwrap().unwrap();
        assert_eq!(got, v);
    }
}
