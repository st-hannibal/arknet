//! Content-addressed LRU disk cache.
//!
//! Layout under `<root>/objects/<aa>/<bb...>.gguf` where `aabb...` is the
//! hex SHA-256 of the file. The two-level fan-out keeps directory listings
//! reasonable even with thousands of entries.
//!
//! # Concurrency
//!
//! The in-memory index is guarded by a [`parking_lot::Mutex`]. Cache
//! operations are fast (a map lookup, a metadata read) so contention is
//! not a concern. Disk I/O happens outside the lock.
//!
//! Pi-class operators can override the byte cap to a few GB via
//! `models.cache_max_bytes` in `node.toml`; the default of 200 GB fits
//! a 70B Q4 plus headroom on a typical GPU workstation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use arknet_crypto::hash::Sha256Digest;
use parking_lot::Mutex;
use tokio::fs;
use tracing::{debug, info, warn};

use crate::errors::{ModelError, Result};
use crate::puller::partial_path;

/// Default total cache size. Overridable per-node in config.
pub const DEFAULT_CACHE_MAX_BYTES: u64 = 200 * 1024 * 1024 * 1024;

/// Cache configuration. Constructed from `node.toml`'s `[models]` section.
#[derive(Clone, Debug)]
pub struct CacheConfig {
    /// Root directory. Everything else lives under `<root>/objects/`.
    pub root: PathBuf,
    /// Hard cap on total cached bytes. Eviction is triggered when a new
    /// insert would exceed this value.
    pub max_bytes: u64,
}

impl CacheConfig {
    /// Builder with the default size cap.
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root,
            max_bytes: DEFAULT_CACHE_MAX_BYTES,
        }
    }

    /// Set a custom byte cap.
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Object directory: `<root>/objects/`.
    pub fn objects_dir(&self) -> PathBuf {
        self.root.join("objects")
    }

    /// Final on-disk path for a given digest.
    pub fn path_for(&self, digest: &Sha256Digest) -> PathBuf {
        let hex = hex::encode(digest.as_bytes());
        let (prefix, rest) = hex.split_at(2);
        self.objects_dir().join(prefix).join(format!("{rest}.gguf"))
    }
}

#[derive(Clone, Debug)]
struct CacheEntry {
    size_bytes: u64,
    last_used: SystemTime,
}

/// In-memory index over the on-disk cache. The index is rebuilt by
/// walking the objects directory on open; subsequent calls keep it
/// in sync.
#[derive(Debug)]
pub struct Cache {
    cfg: CacheConfig,
    index: Arc<Mutex<HashMap<Sha256Digest, CacheEntry>>>,
}

impl Cache {
    /// Open (or create) the cache at `cfg.root`.
    ///
    /// Walks the `objects/` subtree to rebuild the index. Any file that
    /// does not match the `<aa>/<bb...>.gguf` naming convention is left
    /// alone.
    pub async fn open(cfg: CacheConfig) -> Result<Self> {
        fs::create_dir_all(cfg.objects_dir()).await?;
        let index = Arc::new(Mutex::new(HashMap::new()));
        let cache = Self { cfg, index };
        cache.rebuild_index().await?;
        Ok(cache)
    }

    async fn rebuild_index(&self) -> Result<()> {
        let mut added = 0u64;
        let objects = self.cfg.objects_dir();
        let mut dir = match fs::read_dir(&objects).await {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };

        while let Some(entry) = dir.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let prefix = entry.file_name().to_string_lossy().to_string();
            if prefix.len() != 2 {
                continue;
            }

            let mut sub = fs::read_dir(entry.path()).await?;
            while let Some(f) = sub.next_entry().await? {
                let name = f.file_name().to_string_lossy().to_string();
                let Some(rest) = name.strip_suffix(".gguf") else {
                    continue;
                };
                if rest.len() != 62 {
                    continue;
                }
                let hex_str = format!("{prefix}{rest}");
                let Ok(bytes) = hex::decode(&hex_str) else {
                    continue;
                };
                let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) else {
                    continue;
                };
                let digest = Sha256Digest(arr);

                let meta = f.metadata().await?;
                let last_used = meta.accessed().unwrap_or_else(|_| SystemTime::now());

                self.index.lock().insert(
                    digest,
                    CacheEntry {
                        size_bytes: meta.len(),
                        last_used,
                    },
                );
                added += 1;
            }
        }

        debug!(added, "rebuilt cache index");
        Ok(())
    }

    /// Total bytes currently tracked.
    pub fn total_bytes(&self) -> u64 {
        self.index.lock().values().map(|e| e.size_bytes).sum()
    }

    /// Number of entries currently tracked.
    pub fn len(&self) -> usize {
        self.index.lock().len()
    }

    /// Whether the cache has no entries.
    pub fn is_empty(&self) -> bool {
        self.index.lock().is_empty()
    }

    /// Configuration.
    pub fn config(&self) -> &CacheConfig {
        &self.cfg
    }

    /// Return the on-disk path for `digest` if it is in the cache. Records
    /// the access time so LRU eviction sees it as recently used.
    pub async fn get(&self, digest: &Sha256Digest) -> Option<PathBuf> {
        {
            let mut idx = self.index.lock();
            let entry = idx.get_mut(digest)?;
            entry.last_used = SystemTime::now();
        }
        let path = self.cfg.path_for(digest);
        if fs::try_exists(&path).await.unwrap_or(false) {
            Some(path)
        } else {
            // Index was out of date; prune and miss.
            self.index.lock().remove(digest);
            None
        }
    }

    /// Reserve room for an incoming file of `incoming_bytes` by evicting
    /// the least-recently-used entries until the total would fit under
    /// [`CacheConfig::max_bytes`]. Returns the number of bytes freed.
    pub async fn gc_for(&self, incoming_bytes: u64) -> Result<u64> {
        if incoming_bytes > self.cfg.max_bytes {
            return Err(ModelError::Cache(format!(
                "incoming {incoming_bytes} exceeds cache cap {}",
                self.cfg.max_bytes
            )));
        }

        let mut freed: u64 = 0;
        loop {
            let total = self.total_bytes();
            if total + incoming_bytes <= self.cfg.max_bytes {
                break;
            }

            let victim = {
                let idx = self.index.lock();
                idx.iter().min_by_key(|(_, e)| e.last_used).map(|(d, _)| *d)
            };

            let Some(victim) = victim else {
                break;
            };

            let path = self.cfg.path_for(&victim);
            match fs::remove_file(&path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }

            if let Some(entry) = self.index.lock().remove(&victim) {
                freed += entry.size_bytes;
                info!(
                    digest = %hex::encode(victim.as_bytes()),
                    bytes = entry.size_bytes,
                    "evicted cache entry"
                );
            }
        }

        Ok(freed)
    }

    /// Insert an already-verified file into the cache under its digest.
    /// Caller is responsible for ensuring the file at `src_path` really
    /// hashes to `digest`; the puller does this before calling.
    pub async fn insert(&self, digest: Sha256Digest, src_path: &Path) -> Result<PathBuf> {
        let meta = fs::metadata(src_path).await?;
        let size = meta.len();

        self.gc_for(size).await?;

        let dest = self.cfg.path_for(&digest);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await?;
        }

        match fs::rename(src_path, &dest).await {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(18) => {
                // EXDEV cross-device link; fall back to copy + remove.
                fs::copy(src_path, &dest).await?;
                fs::remove_file(src_path).await?;
            }
            Err(e) => return Err(e.into()),
        }

        self.index.lock().insert(
            digest,
            CacheEntry {
                size_bytes: size,
                last_used: SystemTime::now(),
            },
        );
        Ok(dest)
    }

    /// Remove an entry (e.g. on corruption detection). No error if absent.
    pub async fn evict(&self, digest: &Sha256Digest) -> Result<()> {
        let path = self.cfg.path_for(digest);
        let partial = partial_path(&path);
        for p in [&path, &partial] {
            match fs::remove_file(p).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(path = %p.display(), error = %e, "failed to evict file");
                }
            }
        }
        self.index.lock().remove(digest);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_crypto::hash::sha256;

    async fn write_file(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.unwrap();
        }
        fs::write(path, bytes).await.unwrap();
    }

    fn fresh_cfg(root: &Path, max_bytes: u64) -> CacheConfig {
        CacheConfig::with_root(root.to_path_buf()).with_max_bytes(max_bytes)
    }

    #[tokio::test]
    async fn insert_and_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(fresh_cfg(dir.path(), 1024)).await.unwrap();

        let data = b"hello world".to_vec();
        let digest = sha256(&data);

        let staging = dir.path().join("staging.bin");
        write_file(&staging, &data).await;

        let stored = cache.insert(digest, &staging).await.unwrap();
        assert!(stored.exists());

        let got = cache.get(&digest).await.unwrap();
        assert_eq!(got, stored);
    }

    #[tokio::test]
    async fn get_returns_none_for_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(fresh_cfg(dir.path(), 1024)).await.unwrap();
        let digest = sha256(b"nothing");
        assert!(cache.get(&digest).await.is_none());
    }

    #[tokio::test]
    async fn eviction_triggers_when_over_cap() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(fresh_cfg(dir.path(), 20)).await.unwrap();

        for i in 0..3u8 {
            let data = vec![i; 10];
            let digest = sha256(&data);
            let staging = dir.path().join(format!("s{i}.bin"));
            write_file(&staging, &data).await;
            cache.insert(digest, &staging).await.unwrap();
            // Nudge last_used so the oldest is unambiguous.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert!(cache.total_bytes() <= 20);
        assert_eq!(cache.len(), 2);
    }

    #[tokio::test]
    async fn index_rebuilds_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = fresh_cfg(dir.path(), 1024);
        {
            let cache = Cache::open(cfg.clone()).await.unwrap();
            let data = b"abc".to_vec();
            let d = sha256(&data);
            let staging = dir.path().join("s.bin");
            write_file(&staging, &data).await;
            cache.insert(d, &staging).await.unwrap();
        }

        let reopened = Cache::open(cfg).await.unwrap();
        assert_eq!(reopened.len(), 1);
    }

    #[tokio::test]
    async fn incoming_exceeds_cap_errors() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(fresh_cfg(dir.path(), 10)).await.unwrap();
        let err = cache.gc_for(100).await.unwrap_err();
        assert!(matches!(err, ModelError::Cache(_)));
    }

    #[tokio::test]
    async fn evict_removes_file_and_index() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(fresh_cfg(dir.path(), 1024)).await.unwrap();

        let data = b"evict me".to_vec();
        let digest = sha256(&data);
        let staging = dir.path().join("s.bin");
        write_file(&staging, &data).await;
        cache.insert(digest, &staging).await.unwrap();

        cache.evict(&digest).await.unwrap();
        assert!(cache.get(&digest).await.is_none());
    }
}
