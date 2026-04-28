//! Persistent peer book.
//!
//! Stores `(peer_id, multiaddrs, first_seen, last_seen)` tuples as JSON at
//! the configured path. On startup we reload the file so the node can
//! re-dial the peers it knew about before the restart — this avoids
//! depending on bootstrap peers being reachable every time.
//!
//! The format is a plain JSON array; re-ordering or omitting fields is
//! fine since serde ignores unknown fields and fills in defaults. The
//! schema is intentionally tolerant — a corrupt peer book should warn
//! and be ignored, never crash the node.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use libp2p::{Multiaddr, PeerId};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::errors::{NetworkError, Result};

/// One row in the peer book.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRecord {
    /// Hex-encoded libp2p peer id (multihash).
    pub peer_id: String,
    /// Known multiaddrs for this peer. Multiple entries are possible (same
    /// peer reachable at `/ip4/…` and `/ip6/…`).
    pub addrs: Vec<String>,
    /// First Unix-epoch-ms we saw the peer. 0 if unknown.
    #[serde(default)]
    pub first_seen_ms: u64,
    /// Last Unix-epoch-ms we connected to the peer. 0 if unknown.
    #[serde(default)]
    pub last_seen_ms: u64,
}

/// In-memory peer book with a file-backed copy.
///
/// Writes go through [`insert_connected`] and are flushed to disk on drop
/// as well as on every insert — Phase 1 devnet only handles a few hundred
/// peers per node, so per-insert rewrites are cheap. We'll add
/// write-coalescing when the peer count grows.
pub struct PeerBook {
    path: PathBuf,
    entries: RwLock<HashMap<PeerId, PeerRecord>>,
}

impl PeerBook {
    /// Load the peer book at `path`, or start empty if the file is missing
    /// or corrupt. Corruption logs a warning but never fails construction:
    /// a bad peer book must not prevent the node from starting.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let entries = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Vec<PeerRecord>>(&bytes) {
                Ok(records) => records
                    .into_iter()
                    .filter_map(|r| Some((parse_peer_id(&r.peer_id)?, r)))
                    .collect(),
                Err(e) => {
                    warn!(error = %e, path = %path.display(), "corrupt peer book — starting empty");
                    HashMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                warn!(error = %e, path = %path.display(), "peer book read failed — starting empty");
                HashMap::new()
            }
        };
        Self {
            path,
            entries: RwLock::new(entries),
        }
    }

    /// Insert or update a peer after a successful outbound connection.
    pub fn insert_connected(&self, peer: &PeerId, addr: &Multiaddr) -> Result<()> {
        let now = now_ms();
        {
            let mut entries = self.entries.write();
            let record = entries.entry(*peer).or_insert_with(|| PeerRecord {
                peer_id: peer.to_string(),
                addrs: Vec::new(),
                first_seen_ms: now,
                last_seen_ms: now,
            });
            let addr_s = addr.to_string();
            if !record.addrs.contains(&addr_s) {
                record.addrs.push(addr_s);
            }
            record.last_seen_ms = now;
        }
        self.flush()
    }

    /// Remove a peer (e.g. after a handshake rejection or chronic failures).
    pub fn remove(&self, peer: &PeerId) -> Result<()> {
        let removed = self.entries.write().remove(peer).is_some();
        if removed {
            self.flush()?;
        }
        Ok(())
    }

    /// Snapshot of all peer records — safe to hold while connections
    /// change because the returned `Vec` is owned.
    pub fn snapshot(&self) -> Vec<PeerRecord> {
        let entries = self.entries.read();
        let mut records: Vec<PeerRecord> = entries.values().cloned().collect();
        records.sort_by(|a, b| a.peer_id.cmp(&b.peer_id));
        records
    }

    /// Count of known peers.
    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    /// `true` when the peer book has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }

    /// Write the current state back to disk atomically (write-to-tmp +
    /// rename).
    pub fn flush(&self) -> Result<()> {
        let records = self.snapshot();
        let encoded = serde_json::to_vec_pretty(&records)?;
        write_atomically(&self.path, &encoded)?;
        debug!(path = %self.path.display(), peers = records.len(), "peer book flushed");
        Ok(())
    }
}

fn parse_peer_id(s: &str) -> Option<PeerId> {
    s.parse::<PeerId>().ok()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| NetworkError::PeerBook(format!("create {:?}: {e}", parent)))?;
        }
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)
        .map_err(|e| NetworkError::PeerBook(format!("write tmp {:?}: {e}", tmp)))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| NetworkError::PeerBook(format!("rename tmp → {:?}: {e}", path)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::identity::Keypair;

    fn make_peer() -> PeerId {
        Keypair::generate_ed25519().public().to_peer_id()
    }

    #[test]
    fn load_returns_empty_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let book = PeerBook::load(tmp.path().join("peers.json"));
        assert!(book.is_empty());
    }

    #[test]
    fn insert_flushes_to_disk_and_reloads() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("peers.json");
        let book = PeerBook::load(&path);

        let p = make_peer();
        let a: Multiaddr = "/ip4/127.0.0.1/tcp/1234".parse().unwrap();
        book.insert_connected(&p, &a).unwrap();
        assert_eq!(book.len(), 1);

        drop(book);
        let reloaded = PeerBook::load(&path);
        assert_eq!(reloaded.len(), 1);
        let rec = &reloaded.snapshot()[0];
        assert_eq!(rec.peer_id, p.to_string());
        assert_eq!(rec.addrs, vec![a.to_string()]);
        assert!(rec.first_seen_ms > 0);
    }

    #[test]
    fn insert_merges_additional_addr() {
        let tmp = tempfile::tempdir().unwrap();
        let book = PeerBook::load(tmp.path().join("peers.json"));
        let p = make_peer();
        let a1: Multiaddr = "/ip4/127.0.0.1/tcp/1234".parse().unwrap();
        let a2: Multiaddr = "/ip6/::1/tcp/1234".parse().unwrap();
        book.insert_connected(&p, &a1).unwrap();
        book.insert_connected(&p, &a2).unwrap();
        book.insert_connected(&p, &a1).unwrap(); // duplicate is dedup'd
        assert_eq!(book.len(), 1);
        assert_eq!(book.snapshot()[0].addrs.len(), 2);
    }

    #[test]
    fn remove_clears_record() {
        let tmp = tempfile::tempdir().unwrap();
        let book = PeerBook::load(tmp.path().join("peers.json"));
        let p = make_peer();
        book.insert_connected(&p, &"/ip4/127.0.0.1/tcp/1".parse().unwrap())
            .unwrap();
        book.remove(&p).unwrap();
        assert!(book.is_empty());
    }

    #[test]
    fn corrupt_file_is_tolerated() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("peers.json");
        std::fs::write(&path, "{not json").unwrap();
        let book = PeerBook::load(&path); // must not panic
        assert!(book.is_empty());
    }
}
