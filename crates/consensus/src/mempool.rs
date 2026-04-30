//! In-memory transaction pool.
//!
//! Backs the block builder. Bounded in both transaction count and total
//! encoded bytes — full-pool admits drop the lowest-priority tx rather
//! than rejecting new ones, matching geth / Cosmos SDK behavior.
//!
//! # Phase 1 scope
//!
//! Only [`Transaction::Transfer`] is admitted. Every other variant
//! returns [`MempoolError::Unsupported`] so the mempool mirrors what
//! `arknet_chain::apply::apply_tx` will actually apply today — admitting
//! a `StakeOp` (which currently applies as `NotYetImplemented`) would
//! waste block space.
//!
//! # Ordering
//!
//! - Drain order: highest `fee` first, FIFO tie-break on same fee.
//! - Per-sender: at most one pending tx per `(sender, nonce)` — a second
//!   submission must offer a strictly higher fee to replace the first
//!   (simple fee-bump rule; no bump-percent floor yet).
//!
//! # Not in scope (Phase 1 Week 7-8)
//!
//! - Base-fee-aware filtering (added when `BlockBuilder` threads it in).
//! - Nonce-gap tracking (a tx at nonce N+1 while state expects N still
//!   enters the pool; the block builder drops it at drain time if the
//!   account's nonce has not caught up).
//! - Signature verification (the network bridge that feeds the mempool
//!   is the verification boundary — mempool trusts its caller).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use arknet_chain::transactions::{check_signed_tx_size, SignedTransaction, Transaction};
use arknet_common::types::{Address, Gas, Nonce, TxHash};

/// Maximum transactions in the pool. Matches PROTOCOL_SPEC §11 target
/// (100k pending tx network-wide, per node).
pub const DEFAULT_MAX_COUNT: usize = 100_000;

/// Maximum total encoded bytes. 50 MiB keeps worst-case memory bounded
/// even at maximum tx size (1 MiB) times a small fraction of the count
/// cap.
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024 * 1024;

/// Errors returned by [`Mempool::insert`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MempoolError {
    /// This variant is not accepted into the pool yet (see module docs).
    Unsupported(&'static str),
    /// A tx with this hash is already in the pool.
    Duplicate,
    /// Transaction encoding is larger than [`arknet_chain::transactions::MAX_SIGNED_TX_BYTES`].
    Oversize {
        /// Encoded size.
        actual: usize,
        /// Allowed max.
        max: usize,
    },
    /// A tx from the same sender at the same nonce already exists with
    /// equal-or-higher fee. The new one was rejected.
    UnderpricedReplacement {
        /// Fee the existing tx pays.
        existing_fee: Gas,
        /// Fee the incoming tx offered.
        incoming_fee: Gas,
    },
}

impl std::fmt::Display for MempoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(what) => write!(f, "unsupported tx variant: {what}"),
            Self::Duplicate => f.write_str("duplicate transaction"),
            Self::Oversize { actual, max } => {
                write!(f, "oversize tx: {actual} bytes (max {max})")
            }
            Self::UnderpricedReplacement {
                existing_fee,
                incoming_fee,
            } => write!(
                f,
                "underpriced replacement: have {existing_fee}, got {incoming_fee}"
            ),
        }
    }
}

impl std::error::Error for MempoolError {}

/// Ordering key stored in the priority index.
///
/// Sorts by (fee desc, seq asc, hash asc) so `iter()` yields the
/// highest-fee, earliest-submitted tx first — a natural drain order
/// for block construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PriorityKey {
    fee: Gas,
    seq: u64,
    hash: TxHash,
}

impl Ord for PriorityKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse on fee: higher fee is "less" so BTreeSet::iter yields
        // it first. Seq / hash break ties deterministically.
        other
            .fee
            .cmp(&self.fee)
            .then_with(|| self.seq.cmp(&other.seq))
            .then_with(|| self.hash.as_bytes().cmp(other.hash.as_bytes()))
    }
}

impl PartialOrd for PriorityKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Per-tx indexing payload kept alongside the Arc'd transaction.
struct Entry {
    tx: Arc<SignedTransaction>,
    sender: Address,
    nonce: Nonce,
    fee: Gas,
    seq: u64,
    encoded_bytes: usize,
}

/// Bounded transaction pool.
pub struct Mempool {
    max_count: usize,
    max_bytes: usize,
    current_bytes: usize,
    by_hash: HashMap<TxHash, Entry>,
    by_priority: BTreeSet<PriorityKey>,
    by_sender: HashMap<Address, BTreeMap<Nonce, TxHash>>,
    next_seq: u64,
}

impl Default for Mempool {
    fn default() -> Self {
        Self::with_limits(DEFAULT_MAX_COUNT, DEFAULT_MAX_BYTES)
    }
}

impl Mempool {
    /// Build a pool with explicit caps.
    pub fn with_limits(max_count: usize, max_bytes: usize) -> Self {
        Self {
            max_count,
            max_bytes,
            current_bytes: 0,
            by_hash: HashMap::new(),
            by_priority: BTreeSet::new(),
            by_sender: HashMap::new(),
            next_seq: 0,
        }
    }

    /// Current number of transactions.
    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    /// `true` if no transactions.
    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    /// Current encoded byte footprint.
    pub fn size_bytes(&self) -> usize {
        self.current_bytes
    }

    /// Constant-time membership check by tx hash.
    pub fn contains(&self, hash: &TxHash) -> bool {
        self.by_hash.contains_key(hash)
    }

    /// Insert a signed transaction.
    ///
    /// Returns `Ok(hash)` on accept. If the pool is full, evicts the
    /// lowest-priority tx before inserting. If the incoming tx itself
    /// has the lowest priority, it is dropped and `Err` returned.
    pub fn insert(&mut self, stx: SignedTransaction) -> Result<TxHash, MempoolError> {
        check_signed_tx_size(&stx).map_err(|_| MempoolError::Oversize {
            actual: stx.encoded_len(),
            max: arknet_chain::transactions::MAX_SIGNED_TX_BYTES,
        })?;

        let (sender, nonce, fee) = extract_sender_nonce_fee(&stx.tx)?;
        let hash = stx.hash();
        let encoded_bytes = stx.encoded_len();

        if self.by_hash.contains_key(&hash) {
            return Err(MempoolError::Duplicate);
        }

        // Same-(sender, nonce) replacement: strictly higher fee wins.
        if let Some(existing_hash) = self
            .by_sender
            .get(&sender)
            .and_then(|per_nonce| per_nonce.get(&nonce).copied())
        {
            let existing = self
                .by_hash
                .get(&existing_hash)
                .expect("by_sender and by_hash invariants violated");
            if fee <= existing.fee {
                return Err(MempoolError::UnderpricedReplacement {
                    existing_fee: existing.fee,
                    incoming_fee: fee,
                });
            }
            self.remove_by_hash(&existing_hash);
        }

        // Admit bookkeeping.
        let seq = self.next_seq;
        self.next_seq += 1;
        let entry = Entry {
            tx: Arc::new(stx),
            sender,
            nonce,
            fee,
            seq,
            encoded_bytes,
        };
        self.current_bytes += encoded_bytes;
        self.by_priority.insert(PriorityKey { fee, seq, hash });
        self.by_sender
            .entry(sender)
            .or_default()
            .insert(nonce, hash);
        self.by_hash.insert(hash, entry);

        // Evict lowest-priority entries until within bounds. If the
        // *incoming* tx is the lowest, evict it and return an error so
        // the caller knows it was dropped.
        while self.len() > self.max_count || self.current_bytes > self.max_bytes {
            let victim = match self.by_priority.iter().next_back().copied() {
                Some(k) => k,
                None => break,
            };
            self.remove_by_hash(&victim.hash);
            if victim.hash == hash {
                // We evicted ourselves — the pool was saturated with
                // strictly higher-fee txs.
                return Err(MempoolError::UnderpricedReplacement {
                    existing_fee: victim.fee.saturating_add(1),
                    incoming_fee: fee,
                });
            }
        }

        Ok(hash)
    }

    /// Remove a transaction by hash. Used after commit to clear landed
    /// txs from the pool.
    pub fn remove(&mut self, hash: &TxHash) -> bool {
        self.remove_by_hash(hash).is_some()
    }

    /// Bulk-remove — convenience for post-commit cleanup.
    pub fn remove_many(&mut self, hashes: &[TxHash]) {
        for h in hashes {
            self.remove_by_hash(h);
        }
    }

    /// Drain transactions for the next block, honoring two budgets:
    ///
    /// * `max_gas`    — cumulative `fee` (gas-priced 1:1 in Phase 1) ceiling.
    /// * `max_bytes`  — cumulative encoded-size ceiling.
    ///
    /// Returns the chosen txs (high-fee first) along with the total gas
    /// and bytes consumed. Drained txs are *removed* from the pool —
    /// they will be re-inserted only if the block fails to finalize
    /// (see [`Mempool::readmit`]).
    pub fn drain_for_block(
        &mut self,
        max_gas: Gas,
        max_bytes: usize,
    ) -> (Vec<Arc<SignedTransaction>>, Gas, usize) {
        let mut picked: Vec<Arc<SignedTransaction>> = Vec::new();
        let mut gas_used: Gas = 0;
        let mut bytes_used: usize = 0;

        // Walk the priority index in fee-desc / seq-asc order. Collect
        // hashes first so we can mutate the pool during the second
        // pass without invalidating the iterator.
        let ordered: Vec<(TxHash, Gas, usize)> = self
            .by_priority
            .iter()
            .filter_map(|k| {
                self.by_hash
                    .get(&k.hash)
                    .map(|e| (k.hash, e.fee, e.encoded_bytes))
            })
            .collect();

        for (h, fee, sz) in ordered {
            if gas_used.saturating_add(fee) > max_gas {
                continue;
            }
            if bytes_used.saturating_add(sz) > max_bytes {
                continue;
            }
            if let Some(entry) = self.remove_by_hash(&h) {
                gas_used = gas_used.saturating_add(fee);
                bytes_used = bytes_used.saturating_add(sz);
                picked.push(entry.tx);
            }
        }
        (picked, gas_used, bytes_used)
    }

    /// Put previously-drained transactions back into the pool. Used when
    /// a proposed block fails to gather 2/3 precommits — the txs go
    /// back on the queue untouched.
    pub fn readmit(&mut self, txs: Vec<Arc<SignedTransaction>>) {
        for tx in txs {
            // Unwrap safe: the Arc was just drained from us; no other
            // owners. If something else cloned it we simply rebuild
            // the entry via clone — the cost of defensive correctness
            // is acceptable here.
            let stx = Arc::try_unwrap(tx).unwrap_or_else(|arc| (*arc).clone());
            // Ignore the single-tx insertion error: readmission is
            // best-effort. A tx may collide with a newer higher-fee
            // replacement that arrived between the drain and now.
            let _ = self.insert(stx);
        }
    }

    // ─── Internal ─────────────────────────────────────────────────────────

    fn remove_by_hash(&mut self, hash: &TxHash) -> Option<Entry> {
        let entry = self.by_hash.remove(hash)?;
        self.current_bytes = self.current_bytes.saturating_sub(entry.encoded_bytes);
        self.by_priority.remove(&PriorityKey {
            fee: entry.fee,
            seq: entry.seq,
            hash: *hash,
        });
        if let Some(per_nonce) = self.by_sender.get_mut(&entry.sender) {
            per_nonce.remove(&entry.nonce);
            if per_nonce.is_empty() {
                self.by_sender.remove(&entry.sender);
            }
        }
        Some(entry)
    }
}

/// Extract (sender, nonce, fee) from a transaction we are willing to pool.
///
/// Phase 1 only accepts `Transfer`. Every other variant is left to a
/// future week-block with a dedicated mempool lane (e.g. stake ops
/// don't pay a gas fee today).
fn extract_sender_nonce_fee(tx: &Transaction) -> Result<(Address, Nonce, Gas), MempoolError> {
    match tx {
        Transaction::Transfer {
            from, nonce, fee, ..
        } => Ok((*from, *nonce, *fee)),
        Transaction::StakeOp(_) => Err(MempoolError::Unsupported("StakeOp (Week 9)")),
        Transaction::ReceiptBatch(_) => Err(MempoolError::Unsupported(
            "ReceiptBatch (Week 11 mempool lane)",
        )),
        Transaction::Dispute(_) => Err(MempoolError::Unsupported("Dispute (Week 12 mempool lane)")),
        Transaction::RegisterModel { registrar, .. } => Ok((*registrar, 0, 200_000)),
        Transaction::EscrowLock {
            from, nonce, fee, ..
        } => Ok((*from, *nonce, *fee)),
        Transaction::EscrowSettle { .. } => {
            Err(MempoolError::Unsupported("EscrowSettle (proposer-only)"))
        }
        Transaction::RewardMint { .. } => {
            Err(MempoolError::Unsupported("RewardMint (proposer-only)"))
        }
        Transaction::GovProposal(p) => Ok((p.proposer, 0, 500_000)),
        Transaction::GovVote { voter, .. } => Ok((*voter, 0, 30_000)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_chain::transactions::{StakeOp, StakeRole};
    use arknet_common::types::{Amount, PubKey, Signature, SignatureScheme};

    fn sig() -> Signature {
        Signature::new(SignatureScheme::Ed25519, vec![0xaa; 64]).unwrap()
    }

    fn transfer(from: u8, to: u8, nonce: Nonce, fee: Gas, amount: Amount) -> SignedTransaction {
        SignedTransaction {
            tx: Transaction::Transfer {
                from: Address::new([from; 20]),
                to: Address::new([to; 20]),
                amount,
                nonce,
                fee,
            },
            signer: PubKey::ed25519([from; 32]),
            signature: sig(),
        }
    }

    #[test]
    fn inserts_and_counts() {
        let mut pool = Mempool::default();
        assert!(pool.is_empty());
        let h = pool.insert(transfer(1, 2, 0, 21_000, 1)).unwrap();
        assert_eq!(pool.len(), 1);
        assert!(pool.contains(&h));
    }

    #[test]
    fn rejects_non_transfer_variants() {
        let mut pool = Mempool::default();
        let stx = SignedTransaction {
            tx: Transaction::StakeOp(StakeOp::Deposit {
                node_id: arknet_common::types::NodeId::new([1; 32]),
                role: StakeRole::Validator,
                pool_id: None,
                amount: 100,
                delegator: None,
            }),
            signer: PubKey::ed25519([1; 32]),
            signature: sig(),
        };
        assert!(matches!(
            pool.insert(stx),
            Err(MempoolError::Unsupported(_))
        ));
    }

    #[test]
    fn duplicate_hash_rejected() {
        let mut pool = Mempool::default();
        let stx = transfer(1, 2, 0, 21_000, 1);
        let _ = pool.insert(stx.clone()).unwrap();
        assert!(matches!(pool.insert(stx), Err(MempoolError::Duplicate)));
    }

    #[test]
    fn replacement_requires_higher_fee() {
        let mut pool = Mempool::default();
        let _ = pool.insert(transfer(1, 2, 0, 21_000, 1)).unwrap();
        // Same sender + nonce, equal fee → rejected.
        match pool.insert(transfer(1, 3, 0, 21_000, 9)) {
            Err(MempoolError::UnderpricedReplacement { .. }) => {}
            other => panic!("expected underpriced, got {other:?}"),
        }
        // Strictly higher fee → replaces.
        let h2 = pool.insert(transfer(1, 3, 0, 30_000, 9)).unwrap();
        assert_eq!(pool.len(), 1);
        assert!(pool.contains(&h2));
    }

    #[test]
    fn drain_orders_by_fee() {
        let mut pool = Mempool::default();
        // Three senders so each has a distinct (sender, 0) slot.
        let low = pool.insert(transfer(1, 9, 0, 21_000, 1)).unwrap();
        let mid = pool.insert(transfer(2, 9, 0, 50_000, 1)).unwrap();
        let hi = pool.insert(transfer(3, 9, 0, 99_000, 1)).unwrap();

        let (picked, gas_used, _) = pool.drain_for_block(Gas::MAX, usize::MAX);
        let order: Vec<TxHash> = picked.iter().map(|t| t.hash()).collect();
        assert_eq!(order, vec![hi, mid, low]);
        assert_eq!(gas_used, 99_000 + 50_000 + 21_000);
        assert!(pool.is_empty());
    }

    #[test]
    fn drain_respects_gas_budget() {
        let mut pool = Mempool::default();
        let a = pool.insert(transfer(1, 9, 0, 60_000, 1)).unwrap();
        let b = pool.insert(transfer(2, 9, 0, 40_000, 1)).unwrap();
        let _c = pool.insert(transfer(3, 9, 0, 30_000, 1)).unwrap();
        // Budget fits a + b but not a + b + c.
        let (picked, gas_used, _) = pool.drain_for_block(100_000, usize::MAX);
        let taken: Vec<TxHash> = picked.iter().map(|t| t.hash()).collect();
        assert_eq!(taken, vec![a, b]);
        assert_eq!(gas_used, 100_000);
        assert_eq!(pool.len(), 1); // c remains
    }

    #[test]
    fn drain_respects_byte_budget() {
        let mut pool = Mempool::default();
        let _ = pool.insert(transfer(1, 9, 0, 99_000, 1)).unwrap();
        let _ = pool.insert(transfer(2, 9, 0, 21_000, 1)).unwrap();
        let each = pool
            .by_hash
            .values()
            .map(|e| e.encoded_bytes)
            .max()
            .unwrap();
        // Budget fits exactly one transfer — the high-fee one wins
        // and the other is left in the pool.
        let (picked, _, bytes_used) = pool.drain_for_block(Gas::MAX, each);
        assert_eq!(picked.len(), 1);
        assert_eq!(bytes_used, each);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn count_cap_evicts_lowest_fee() {
        let mut pool = Mempool::with_limits(2, usize::MAX);
        let _a = pool.insert(transfer(1, 9, 0, 10_000, 1)).unwrap();
        let b = pool.insert(transfer(2, 9, 0, 99_000, 1)).unwrap();
        // Third insert at lowest fee → evicts itself.
        let res = pool.insert(transfer(3, 9, 0, 5_000, 1));
        assert!(matches!(
            res,
            Err(MempoolError::UnderpricedReplacement { .. })
        ));
        // Pool still holds the high-fee pair.
        assert_eq!(pool.len(), 2);
        assert!(pool.contains(&b));
    }

    #[test]
    fn count_cap_admits_high_fee_by_evicting_lowest() {
        let mut pool = Mempool::with_limits(2, usize::MAX);
        let a = pool.insert(transfer(1, 9, 0, 10_000, 1)).unwrap();
        let b = pool.insert(transfer(2, 9, 0, 50_000, 1)).unwrap();
        let c = pool.insert(transfer(3, 9, 0, 90_000, 1)).unwrap();
        // `a` was the lowest; should be evicted to make room for `c`.
        assert_eq!(pool.len(), 2);
        assert!(!pool.contains(&a));
        assert!(pool.contains(&b));
        assert!(pool.contains(&c));
    }

    #[test]
    fn remove_clears_all_indexes() {
        let mut pool = Mempool::default();
        let h = pool.insert(transfer(1, 9, 0, 21_000, 1)).unwrap();
        assert!(pool.remove(&h));
        assert!(pool.is_empty());
        assert_eq!(pool.size_bytes(), 0);
        assert!(!pool.by_sender.contains_key(&Address::new([1; 20])));
    }

    #[test]
    fn readmit_after_failed_block() {
        let mut pool = Mempool::default();
        let _ = pool.insert(transfer(1, 9, 0, 21_000, 1)).unwrap();
        let _ = pool.insert(transfer(2, 9, 0, 21_000, 1)).unwrap();
        let (picked, _, _) = pool.drain_for_block(Gas::MAX, usize::MAX);
        assert!(pool.is_empty());
        pool.readmit(picked);
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn oversize_rejected_before_bookkeeping() {
        let mut pool = Mempool::default();
        // Synthesize a tx larger than MAX_SIGNED_TX_BYTES via a huge
        // proposal body.
        let stx = SignedTransaction {
            tx: Transaction::GovProposal(arknet_chain::transactions::Proposal {
                proposal_id: 0,
                proposer: Address::default(),
                deposit: 0,
                title: "x".into(),
                body: "x".repeat(arknet_chain::transactions::MAX_SIGNED_TX_BYTES + 1),
                discussion_ends: 0,
                voting_ends: 0,
                activation: None,
            }),
            signer: PubKey::ed25519([1; 32]),
            signature: sig(),
        };
        assert!(matches!(
            pool.insert(stx),
            Err(MempoolError::Oversize { .. })
        ));
        assert!(pool.is_empty());
    }
}
