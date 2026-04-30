//! Free-tier quota tracking.
//!
//! §16 of PROTOCOL_SPEC: every wallet gets a small allowance of
//! inference jobs per hour and per day without paying. Routers (and
//! compute nodes accepting direct requests) enforce the quota locally
//! and gossip consumption via `arknet/quota/tick/1` so the whole
//! network converges on the same picture within a heartbeat.
//!
//! # Phase 1 scope
//!
//! - Fixed UTC buckets: `(wallet, floor(now / 3600_000))` /
//!   `(wallet, floor(now / 86_400_000))`.
//! - Deterministic bucket keys — a gossiped tick merges trivially by
//!   adding counts.
//! - Deterministic clock source (caller supplies `now_ms`) so tests
//!   are reproducible.
//!
//! Out of Phase 1:
//!
//! - On-chain settlement of quota consumption (would bloat state).
//! - Per-user rate limits beyond the two-bucket shape.
//! - Signed per-tick attestations. Phase 1 trusts the local router's
//!   account of its own ticks; Phase 2 adds a compute-side signature.

use std::collections::HashMap;

use arknet_common::types::{Address, Timestamp};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// Default per-wallet hourly free-tier limit (§16).
pub const DEFAULT_HOURLY_LIMIT: u32 = 10;
/// Default per-wallet daily free-tier limit (§16).
pub const DEFAULT_DAILY_LIMIT: u32 = 100;

const MS_PER_HOUR: u64 = 3_600 * 1_000;
const MS_PER_DAY: u64 = 24 * MS_PER_HOUR;

/// Quota configuration. Cheap to clone; tune via config file once we
/// have a governance-backed per-wallet quota table.
#[derive(Clone, Debug)]
pub struct FreeTierConfig {
    /// Max jobs per UTC hour.
    pub hourly_limit: u32,
    /// Max jobs per UTC day.
    pub daily_limit: u32,
}

impl Default for FreeTierConfig {
    fn default() -> Self {
        Self {
            hourly_limit: DEFAULT_HOURLY_LIMIT,
            daily_limit: DEFAULT_DAILY_LIMIT,
        }
    }
}

/// Outcome of a quota check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuotaOutcome {
    /// Request is within both hourly and daily caps.
    Allowed {
        /// Requests remaining this hour.
        hourly_remaining: u32,
        /// Requests remaining this day.
        daily_remaining: u32,
    },
    /// Caller exceeded the hourly cap. Rejected before the daily bucket
    /// is touched so the ordering is observable.
    HourlyExceeded {
        /// Unix ms at which the hourly bucket rolls over.
        retry_after_ms: u64,
    },
    /// Caller exceeded the daily cap.
    DailyExceeded {
        /// Unix ms at which the daily bucket rolls over.
        retry_after_ms: u64,
    },
}

/// UTC hour/day bucket counters.
///
/// Keyed by `(wallet, bucket_index)`. The bucket index is
/// `floor(unix_ms / bucket_width_ms)`; a timestamp in the next window
/// gets a different key and doesn't touch prior counts.
#[derive(Default)]
struct BucketStore {
    counts: HashMap<(Address, u64), u32>,
}

impl BucketStore {
    fn get(&self, wallet: &Address, idx: u64) -> u32 {
        *self.counts.get(&(*wallet, idx)).unwrap_or(&0)
    }

    fn add(&mut self, wallet: Address, idx: u64, delta: u32) {
        let e = self.counts.entry((wallet, idx)).or_insert(0);
        *e = e.saturating_add(delta);
    }

    /// Drop every entry older than `cutoff_idx`.
    ///
    /// Caller passes the bucket index `floor((now - retention_ms) / width)`
    /// so buckets we'll never consult again don't accumulate.
    fn prune(&mut self, cutoff_idx: u64) {
        self.counts.retain(|(_, idx), _| *idx >= cutoff_idx);
    }
}

/// Local free-tier quota tracker. Safe to call from multiple tasks via
/// a [`parking_lot::Mutex`] (kept by the caller — this struct is
/// `!Sync` on its own).
pub struct FreeTierTracker {
    cfg: FreeTierConfig,
    hourly: BucketStore,
    daily: BucketStore,
}

impl FreeTierTracker {
    /// Build a tracker with the given config.
    pub fn new(cfg: FreeTierConfig) -> Self {
        Self {
            cfg,
            hourly: BucketStore::default(),
            daily: BucketStore::default(),
        }
    }

    /// Current policy.
    pub fn config(&self) -> &FreeTierConfig {
        &self.cfg
    }

    /// Check whether `wallet` may consume one more free-tier job at
    /// `now_ms`. Returns the outcome **without** mutating state.
    pub fn check(&self, wallet: &Address, now_ms: Timestamp) -> QuotaOutcome {
        let (h_idx, d_idx) = bucket_indices(now_ms);
        let h_used = self.hourly.get(wallet, h_idx);
        let d_used = self.daily.get(wallet, d_idx);
        if h_used >= self.cfg.hourly_limit {
            return QuotaOutcome::HourlyExceeded {
                retry_after_ms: (h_idx + 1) * MS_PER_HOUR,
            };
        }
        if d_used >= self.cfg.daily_limit {
            return QuotaOutcome::DailyExceeded {
                retry_after_ms: (d_idx + 1) * MS_PER_DAY,
            };
        }
        QuotaOutcome::Allowed {
            hourly_remaining: self.cfg.hourly_limit - h_used,
            daily_remaining: self.cfg.daily_limit - d_used,
        }
    }

    /// Atomically check + reserve one job. If the return is `Allowed`,
    /// the counters are already incremented. If the quota is exceeded
    /// the tracker is unchanged.
    pub fn consume(&mut self, wallet: &Address, now_ms: Timestamp) -> QuotaOutcome {
        let outcome = self.check(wallet, now_ms);
        if matches!(outcome, QuotaOutcome::Allowed { .. }) {
            let (h_idx, d_idx) = bucket_indices(now_ms);
            self.hourly.add(*wallet, h_idx, 1);
            self.daily.add(*wallet, d_idx, 1);
        }
        outcome
    }

    /// Merge a gossiped tick into our local view. Idempotent-ish:
    /// because buckets are additive and UTC-aligned, applying the same
    /// tick twice *will* double-count, so callers must de-dupe ticks
    /// by [`FreeTierTick::nonce`] at the transport layer.
    pub fn absorb_tick(&mut self, tick: &FreeTierTick) {
        if tick.hourly_count > 0 {
            self.hourly
                .add(tick.wallet, tick.hour_bucket, tick.hourly_count);
        }
        if tick.daily_count > 0 {
            self.daily
                .add(tick.wallet, tick.day_bucket, tick.daily_count);
        }
    }

    /// Drop buckets outside the rolling retention window (2h / 2d to
    /// tolerate clock skew + straggling gossip).
    pub fn prune(&mut self, now_ms: Timestamp) {
        let (h_idx, d_idx) = bucket_indices(now_ms);
        self.hourly.prune(h_idx.saturating_sub(1));
        self.daily.prune(d_idx.saturating_sub(1));
    }

    /// Raw counter inspection — for tests + metrics only.
    #[doc(hidden)]
    pub fn counts(&self, wallet: &Address, now_ms: Timestamp) -> (u32, u32) {
        let (h_idx, d_idx) = bucket_indices(now_ms);
        (
            self.hourly.get(wallet, h_idx),
            self.daily.get(wallet, d_idx),
        )
    }
}

/// Gossip payload broadcast on `arknet/quota/tick/1`.
///
/// One tick = "router X charged Y free-tier jobs to wallet W in bucket B".
/// Peers add the counts into their own buckets. `nonce` is a per-router
/// monotonic id used to de-dupe replays at the gossip layer.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct FreeTierTick {
    /// Wallet whose quota was consumed.
    pub wallet: Address,
    /// UTC hour bucket index.
    pub hour_bucket: u64,
    /// UTC day bucket index.
    pub day_bucket: u64,
    /// Jobs counted in the hour bucket.
    pub hourly_count: u32,
    /// Jobs counted in the day bucket.
    pub daily_count: u32,
    /// Monotonic router-local id so duplicate deliveries can be
    /// ignored.
    pub nonce: u64,
    /// Unix ms the tick was produced.
    pub emitted_at_ms: Timestamp,
}

/// Compute the `(hour_bucket, day_bucket)` indices for a given unix
/// ms timestamp.
pub fn bucket_indices(now_ms: Timestamp) -> (u64, u64) {
    (now_ms / MS_PER_HOUR, now_ms / MS_PER_DAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wallet(byte: u8) -> Address {
        Address::new([byte; 20])
    }

    #[test]
    fn default_limits_match_spec() {
        let d = FreeTierConfig::default();
        assert_eq!(d.hourly_limit, 10);
        assert_eq!(d.daily_limit, 100);
    }

    #[test]
    fn consume_happy_path() {
        let mut t = FreeTierTracker::new(FreeTierConfig::default());
        let w = wallet(1);
        for _ in 0..5 {
            match t.consume(&w, 1_700_000_000_000) {
                QuotaOutcome::Allowed { .. } => {}
                other => panic!("expected Allowed, got {other:?}"),
            }
        }
        let (h, d) = t.counts(&w, 1_700_000_000_000);
        assert_eq!(h, 5);
        assert_eq!(d, 5);
    }

    #[test]
    fn hourly_exhausted_rejects_further() {
        let cfg = FreeTierConfig {
            hourly_limit: 2,
            daily_limit: 100,
        };
        let mut t = FreeTierTracker::new(cfg);
        let w = wallet(2);
        assert!(matches!(
            t.consume(&w, 1_000_000),
            QuotaOutcome::Allowed { .. }
        ));
        assert!(matches!(
            t.consume(&w, 1_000_000),
            QuotaOutcome::Allowed { .. }
        ));
        // 3rd call — exhausted.
        assert!(matches!(
            t.consume(&w, 1_000_000),
            QuotaOutcome::HourlyExceeded { .. }
        ));
    }

    #[test]
    fn daily_exhausted_short_circuits() {
        let cfg = FreeTierConfig {
            hourly_limit: 100,
            daily_limit: 2,
        };
        let mut t = FreeTierTracker::new(cfg);
        let w = wallet(3);
        assert!(matches!(
            t.consume(&w, 1_000_000),
            QuotaOutcome::Allowed { .. }
        ));
        assert!(matches!(
            t.consume(&w, 1_000_000),
            QuotaOutcome::Allowed { .. }
        ));
        assert!(matches!(
            t.consume(&w, 1_000_000),
            QuotaOutcome::DailyExceeded { .. }
        ));
    }

    #[test]
    fn next_hour_bucket_resets_hourly() {
        let cfg = FreeTierConfig {
            hourly_limit: 1,
            daily_limit: 100,
        };
        let mut t = FreeTierTracker::new(cfg);
        let w = wallet(4);
        // First hour.
        let t0 = 0;
        assert!(matches!(t.consume(&w, t0), QuotaOutcome::Allowed { .. }));
        assert!(matches!(
            t.consume(&w, t0),
            QuotaOutcome::HourlyExceeded { .. }
        ));
        // Next hour (1 hour later + 1 ms).
        let t1 = MS_PER_HOUR;
        assert!(matches!(t.consume(&w, t1), QuotaOutcome::Allowed { .. }));
    }

    #[test]
    fn absorb_tick_merges_counts() {
        let mut t = FreeTierTracker::new(FreeTierConfig::default());
        let w = wallet(5);
        let now = MS_PER_HOUR * 3 + 500; // middle of the 3rd hour.
        let (h_idx, d_idx) = bucket_indices(now);
        let tick = FreeTierTick {
            wallet: w,
            hour_bucket: h_idx,
            day_bucket: d_idx,
            hourly_count: 4,
            daily_count: 4,
            nonce: 1,
            emitted_at_ms: now,
        };
        t.absorb_tick(&tick);
        let (h, d) = t.counts(&w, now);
        assert_eq!(h, 4);
        assert_eq!(d, 4);
    }

    #[test]
    fn prune_drops_old_buckets() {
        let mut t = FreeTierTracker::new(FreeTierConfig::default());
        let w = wallet(6);
        // Consume in hour 0.
        assert!(matches!(t.consume(&w, 0), QuotaOutcome::Allowed { .. }));
        // Jump 10 hours forward and prune.
        let now = 10 * MS_PER_HOUR;
        t.prune(now);
        // Hour-0 bucket is gone — so a check for hour 0 reads 0 again.
        let (h, _) = t.counts(&w, 0);
        assert_eq!(h, 0);
    }

    #[test]
    fn borsh_roundtrip_tick() {
        let tick = FreeTierTick {
            wallet: wallet(7),
            hour_bucket: 42,
            day_bucket: 1,
            hourly_count: 3,
            daily_count: 3,
            nonce: 99,
            emitted_at_ms: 1_700_000_000_000,
        };
        let bytes = borsh::to_vec(&tick).unwrap();
        let back: FreeTierTick = borsh::from_slice(&bytes).unwrap();
        assert_eq!(tick, back);
    }
}
