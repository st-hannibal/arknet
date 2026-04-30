//! KV-cache checkpoint API.
//!
//! Phase 1 fills in real serialization via `llama_state_get_data` /
//! `llama_state_set_data` so routers can fail a request over to a
//! backup compute node mid-generation.
//!
//! # Security
//!
//! The snapshot contains the full KV cache — it carries prompt tokens
//! in the clear. Callers must handle checkpoint blobs with the same
//! confidentiality as the prompt itself. Phase-2 encrypts blobs with
//! the user's X25519 session key (see `SECURITY.md §8`).
//!
//! # Disk layout (when persisted)
//!
//! The router writes checkpoints to
//! `<data-dir>/checkpoints/<job_id_hex>.state`. They are ephemeral:
//! a restart clears the directory because stale KV state from a
//! previous process is unusable (different context params).

use std::path::{Path, PathBuf};

use bytes::Bytes;
use tokio::fs;
use tracing::debug;

use crate::errors::Result;

/// A session whose state can be snapshotted and restored.
///
/// Implemented on [`crate::context::Context`] in Phase 1.
/// Downstream code (router / compute) depends on this trait, not on the
/// concrete `Context` type, so a Phase-3 TEE backend can swap in
/// without changing the checkpoint surface.
pub trait CheckpointableSession {
    /// Produce a serialized snapshot of the session's KV cache + cursor.
    fn snapshot(&self) -> Result<Bytes>;

    /// Restore a session from a snapshot produced by [`snapshot`].
    fn restore(&mut self, bytes: &[u8]) -> Result<()>;
}

impl CheckpointableSession for crate::context::Context<'_> {
    fn snapshot(&self) -> Result<Bytes> {
        let data = self.snapshot_state()?;
        Ok(Bytes::from(data))
    }

    fn restore(&mut self, bytes: &[u8]) -> Result<()> {
        self.restore_state(bytes)
    }
}

/// On-disk checkpoint store under `<root>/checkpoints/`.
///
/// Each checkpoint is named by its hex job-id so a backup compute
/// node can pull it by id when the router assigns a failover.
pub struct CheckpointStore {
    root: PathBuf,
}

impl CheckpointStore {
    /// Open (or create) the checkpoint directory.
    pub async fn open(data_dir: &Path) -> Result<Self> {
        let root = data_dir.join("checkpoints");
        fs::create_dir_all(&root).await?;
        Ok(Self { root })
    }

    /// Write a checkpoint blob to disk.
    pub async fn save(&self, job_id_hex: &str, data: &[u8]) -> Result<PathBuf> {
        let path = self.root.join(format!("{job_id_hex}.state"));
        fs::write(&path, data).await?;
        debug!(path=%path.display(), bytes=data.len(), "checkpoint saved");
        Ok(path)
    }

    /// Load a checkpoint blob from disk. Returns `None` if no
    /// checkpoint exists for the given job.
    pub async fn load(&self, job_id_hex: &str) -> Result<Option<Vec<u8>>> {
        let path = self.root.join(format!("{job_id_hex}.state"));
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read(&path).await?;
        debug!(path=%path.display(), bytes=data.len(), "checkpoint loaded");
        Ok(Some(data))
    }

    /// Remove a checkpoint after the job completes or the failover
    /// window expires.
    pub async fn remove(&self, job_id_hex: &str) -> Result<()> {
        let path = self.root.join(format!("{job_id_hex}.state"));
        if path.exists() {
            fs::remove_file(&path).await?;
        }
        Ok(())
    }

    /// Purge all checkpoints (called at node startup since stale KV
    /// state is unusable with fresh context params).
    pub async fn purge_all(&self) -> Result<usize> {
        let mut count = 0;
        let mut entries = fs::read_dir(&self.root).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "state") {
                fs::remove_file(&path).await?;
                count += 1;
            }
        }
        if count > 0 {
            debug!(count, "purged stale checkpoints");
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn store_save_load_remove_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::open(tmp.path()).await.unwrap();

        let data = b"fake-kv-cache-data-for-test";
        let path = store.save("abcd1234", data).await.unwrap();
        assert!(path.exists());

        let loaded = store.load("abcd1234").await.unwrap();
        assert_eq!(loaded.as_deref(), Some(data.as_slice()));

        store.remove("abcd1234").await.unwrap();
        assert!(!path.exists());
        assert!(store.load("abcd1234").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn load_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::open(tmp.path()).await.unwrap();
        assert!(store.load("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn purge_all_clears_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::open(tmp.path()).await.unwrap();
        store.save("a", b"1").await.unwrap();
        store.save("b", b"2").await.unwrap();
        store.save("c", b"3").await.unwrap();
        let removed = store.purge_all().await.unwrap();
        assert_eq!(removed, 3);
        assert!(store.load("a").await.unwrap().is_none());
    }
}
