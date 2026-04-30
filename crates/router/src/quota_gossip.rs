//! Free-tier quota tick emitter + receiver.
//!
//! When a router role is active, a small background task polls the
//! router's [`FreeTierTracker`] once per heartbeat and publishes any
//! observed consumption onto `arknet/quota/tick/1` so every other
//! router in the network converges on the same bucket counts.
//!
//! This module is transport-agnostic — it takes a
//! [`QuotaGossipTransport`] trait so tests can plug in a local
//! channel and production can plug in the libp2p
//! `NetworkHandle::publish` entry point.
//!
//! The emitter / receiver are kept in the router crate (rather than
//! the node binary) so a multi-router Phase-2 test harness can boot
//! the full gossip path in-process without spinning up libp2p.

use std::sync::Arc;
use std::time::Duration;

use arknet_common::types::{Address, Timestamp};
use arknet_compute::free_tier::{bucket_indices, FreeTierTick, FreeTierTracker};
use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Default heartbeat between ticks (1 second).
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Abstract transport. Production ties this to
/// [`arknet_network::NetworkHandle::publish`]; tests wire a local
/// broadcast channel.
#[async_trait]
pub trait QuotaGossipTransport: Send + Sync {
    /// Publish a borsh-encoded tick.
    async fn publish(&self, bytes: Vec<u8>) -> Result<(), String>;
}

/// Observed consumption that hasn't been gossiped yet. The emitter
/// drains this after every publish.
#[derive(Default, Debug, Clone)]
pub struct PendingConsumption {
    /// `wallet → (hour_bucket, day_bucket, hourly_count, daily_count)`.
    inner: std::collections::HashMap<Address, (u64, u64, u32, u32)>,
}

impl PendingConsumption {
    /// Record one more job against `wallet` at `now_ms`.
    pub fn record(&mut self, wallet: Address, now_ms: Timestamp) {
        let (h, d) = bucket_indices(now_ms);
        let entry = self.inner.entry(wallet).or_insert((h, d, 0, 0));
        entry.0 = h;
        entry.1 = d;
        entry.2 = entry.2.saturating_add(1);
        entry.3 = entry.3.saturating_add(1);
    }

    /// Drain into a list of signed [`FreeTierTick`]s with consecutive
    /// nonces starting at `next_nonce`.
    pub fn drain(&mut self, next_nonce: &mut u64, now_ms: Timestamp) -> Vec<FreeTierTick> {
        let mut out = Vec::with_capacity(self.inner.len());
        for (wallet, (h, d, hourly_count, daily_count)) in self.inner.drain() {
            out.push(FreeTierTick {
                wallet,
                hour_bucket: h,
                day_bucket: d,
                hourly_count,
                daily_count,
                nonce: *next_nonce,
                emitted_at_ms: now_ms,
            });
            *next_nonce = next_nonce.saturating_add(1);
        }
        out
    }

    /// Count of tracked wallets.
    pub fn wallet_count(&self) -> usize {
        self.inner.len()
    }

    /// `true` if nothing is pending.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// Gossip emitter loop.
///
/// Spawn this with [`spawn_emitter`]. The loop wakes every
/// `interval`, drains any pending consumption, and publishes one tick
/// per wallet. `shutdown` exits the loop cleanly.
pub async fn run_emitter<T: QuotaGossipTransport + 'static>(
    pending: Arc<Mutex<PendingConsumption>>,
    transport: Arc<T>,
    interval: Duration,
    shutdown: CancellationToken,
) {
    let mut next_nonce: u64 = 0;
    let mut tick = tokio::time::interval(interval);
    tick.tick().await; // skip the immediate fire

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                debug!("quota gossip emitter shutting down");
                return;
            }
            _ = tick.tick() => {
                let now_ms = now_ms();
                let drained = pending.lock().drain(&mut next_nonce, now_ms);
                for t in drained {
                    match borsh::to_vec(&t) {
                        Ok(bytes) => {
                            if let Err(e) = transport.publish(bytes).await {
                                warn!(error=%e, "quota tick publish failed");
                            }
                        }
                        Err(e) => warn!(error=%e, "tick borsh encode failed"),
                    }
                }
            }
        }
    }
}

/// Convenience: spawn the emitter loop on tokio. Returns a
/// [`mpsc::Receiver`] of published ticks for test observation; in
/// production, drop or ignore.
pub fn spawn_emitter<T: QuotaGossipTransport + 'static>(
    pending: Arc<Mutex<PendingConsumption>>,
    transport: Arc<T>,
    interval: Duration,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_emitter(pending, transport, interval, shutdown))
}

/// Absorb a borsh-encoded tick into `tracker`. Returns an error if
/// the bytes don't decode or the tick's nonce was already seen.
pub fn absorb_tick_bytes(
    tracker: &mut FreeTierTracker,
    recent_nonces: &mut Arc<Mutex<RecentNonces>>,
    bytes: &[u8],
) -> Result<(), String> {
    let tick: FreeTierTick =
        borsh::from_slice(bytes).map_err(|e| format!("decode quota tick: {e}"))?;
    if !recent_nonces.lock().insert(tick.wallet, tick.nonce) {
        return Err("duplicate quota tick".into());
    }
    tracker.absorb_tick(&tick);
    Ok(())
}

/// Bounded FIFO dedup for observed `(wallet, nonce)` pairs on the
/// receiver side. Same pattern as the compute crate's `NonceCache`.
pub struct RecentNonces {
    cap: usize,
    seen: std::collections::VecDeque<(Address, u64)>,
}

impl RecentNonces {
    /// Build an empty cache with capacity `cap`.
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            seen: std::collections::VecDeque::with_capacity(cap),
        }
    }

    /// `true` if the pair is fresh (and is now stored).
    pub fn insert(&mut self, wallet: Address, nonce: u64) -> bool {
        if self.seen.iter().any(|(w, n)| *w == wallet && *n == nonce) {
            return false;
        }
        if self.seen.len() >= self.cap {
            self.seen.pop_front();
        }
        self.seen.push_back((wallet, nonce));
        true
    }
}

/// Convenience: build a shared [`RecentNonces`] wrapper.
pub fn recent_nonces_shared(cap: usize) -> Arc<Mutex<RecentNonces>> {
    Arc::new(Mutex::new(RecentNonces::new(cap)))
}

/// Dummy transport that accumulates published bytes into a channel.
/// Used by tests; exported to make test plumbing easy.
pub struct ChannelTransport {
    /// Channel sender for captured payloads.
    pub sender: mpsc::UnboundedSender<Vec<u8>>,
}

impl ChannelTransport {
    /// Build a `(transport, receiver)` pair.
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(ChannelTransport { sender: tx }), rx)
    }
}

#[async_trait]
impl QuotaGossipTransport for ChannelTransport {
    async fn publish(&self, bytes: Vec<u8>) -> Result<(), String> {
        self.sender
            .send(bytes)
            .map_err(|e| format!("channel closed: {e}"))
    }
}

fn now_ms() -> Timestamp {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_compute::free_tier::FreeTierConfig;

    #[test]
    fn pending_consumption_accumulates() {
        let mut p = PendingConsumption::default();
        let w = Address::new([1; 20]);
        p.record(w, 0);
        p.record(w, 0);
        p.record(w, 0);
        assert_eq!(p.wallet_count(), 1);
        let mut nonce = 0;
        let drained = p.drain(&mut nonce, 0);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].hourly_count, 3);
        assert_eq!(drained[0].daily_count, 3);
        assert!(p.is_empty());
        assert_eq!(nonce, 1);
    }

    #[test]
    fn drain_assigns_sequential_nonces() {
        let mut p = PendingConsumption::default();
        for i in 0..3u8 {
            p.record(Address::new([i; 20]), 0);
        }
        let mut nonce = 10;
        let drained = p.drain(&mut nonce, 0);
        assert_eq!(drained.len(), 3);
        let mut nonces: Vec<u64> = drained.iter().map(|t| t.nonce).collect();
        nonces.sort();
        assert_eq!(nonces, vec![10, 11, 12]);
        assert_eq!(nonce, 13);
    }

    #[test]
    fn absorb_tick_bytes_rejects_replay() {
        let mut tracker = FreeTierTracker::new(FreeTierConfig::default());
        let mut nonces = recent_nonces_shared(16);
        let tick = FreeTierTick {
            wallet: Address::new([7; 20]),
            hour_bucket: 0,
            day_bucket: 0,
            hourly_count: 1,
            daily_count: 1,
            nonce: 42,
            emitted_at_ms: 0,
        };
        let bytes = borsh::to_vec(&tick).unwrap();
        absorb_tick_bytes(&mut tracker, &mut nonces, &bytes).expect("first ok");
        let err = absorb_tick_bytes(&mut tracker, &mut nonces, &bytes).unwrap_err();
        assert!(err.contains("duplicate"));
    }

    #[tokio::test]
    async fn emitter_publishes_pending_on_tick() {
        let (transport, mut rx) = ChannelTransport::new();
        let pending = Arc::new(Mutex::new(PendingConsumption::default()));
        pending.lock().record(Address::new([1; 20]), 0);

        let shutdown = CancellationToken::new();
        let handle = spawn_emitter(
            pending.clone(),
            transport.clone(),
            Duration::from_millis(20),
            shutdown.clone(),
        );
        // Wait for at least one tick.
        let bytes = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("published before timeout")
            .expect("some bytes");
        let tick: FreeTierTick = borsh::from_slice(&bytes).unwrap();
        assert_eq!(tick.hourly_count, 1);
        shutdown.cancel();
        let _ = handle.await;
    }
}
