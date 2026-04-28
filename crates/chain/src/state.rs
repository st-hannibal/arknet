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

use arknet_common::types::{Address, NodeId, StateRoot};

use crate::account::Account;
use crate::errors::{ChainError, Result};
use crate::stake_entry::StakeEntry;
use crate::transactions::StakeRole;
use crate::validator::ValidatorInfo;

// ─── Column families ──────────────────────────────────────────────────────

const CF_ACCOUNTS: &str = "accounts";
const CF_STAKES: &str = "stakes";
const CF_VALIDATORS: &str = "validators";
const CF_PARAMS: &str = "params";
const CF_META: &str = "meta"; // last committed state root, height, etc.

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

fn stake_key(node_id: &NodeId, role: StakeRole, pool_id_opt: Option<[u8; 16]>) -> Vec<u8> {
    let mut k = Vec::with_capacity(32 + 1 + 17);
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
    k
}

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

        let cfs = [CF_ACCOUNTS, CF_STAKES, CF_VALIDATORS, CF_PARAMS, CF_META]
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
    pub fn get_stake(
        &self,
        node_id: &NodeId,
        role: StakeRole,
        pool_id: Option<[u8; 16]>,
    ) -> Result<Option<StakeEntry>> {
        let cf = self.cf(CF_STAKES)?;
        match self
            .db
            .get_cf(cf, stake_key(node_id, role, pool_id))
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
}

impl BlockCtx<'_> {
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

    /// Buffer a stake entry write.
    pub fn set_stake(
        &mut self,
        node_id: &NodeId,
        role: StakeRole,
        pool_id: Option<[u8; 16]>,
        entry: &StakeEntry,
    ) -> Result<()> {
        let cf = self.state.cf(CF_STAKES)?;
        let key = stake_key(node_id, role, pool_id);
        if entry.is_empty() {
            self.batch.delete_cf(cf, &key);
        } else {
            let bytes = borsh::to_vec(entry)
                .map_err(|e| ChainError::Codec(format!("stake encode: {e}")))?;
            self.batch.put_cf(cf, &key, &bytes);
        }
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

    /// Look up an account, consulting this block's overlay first. Reads
    /// see writes from the same block even before [`commit`] — necessary
    /// for chained transfers (sender's nonce after tx N-1 → tx N).
    pub fn get_account(&self, addr: &Address) -> Result<Option<Account>> {
        if let Some(entry) = self.account_overlay.get(addr) {
            return Ok(entry.clone());
        }
        self.state.get_account(addr)
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
        ctx.set_stake(&node_id, StakeRole::Compute, None, &entry)
            .unwrap();
        ctx.commit().unwrap();

        let got = state
            .get_stake(&node_id, StakeRole::Compute, None)
            .unwrap()
            .unwrap();
        assert_eq!(got, entry);
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
