//! The model manager — top-level orchestrator.
//!
//! Usage:
//! ```no_run
//! # use arknet_model_manager::{ModelManager, CacheConfig, MockRegistry, ModelRef, Result};
//! # use std::path::PathBuf;
//! # use std::sync::Arc;
//! # use std::collections::HashMap;
//! # async fn example() -> Result<()> {
//! let cfg = CacheConfig::with_root(PathBuf::from("/var/lib/arknet/models"));
//! let registry = Arc::new(MockRegistry::from_manifests(HashMap::new()));
//! let mgr = ModelManager::open(cfg, registry).await?;
//! let model_ref = ModelRef::parse("meta/M-F16").unwrap();
//! let sandbox = mgr.ensure_local(&model_ref).await?;
//! // sandbox.path() -> handoff to inference engine
//! # Ok(())
//! # }
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use tracing::{info, warn};

use crate::cache::{Cache, CacheConfig};
use crate::errors::{ModelError, Result};
use crate::gguf;
use crate::puller::{partial_path, Puller};
use crate::registry::ModelRegistry;
use crate::sandbox::{self, SandboxedModel};
use crate::types::ModelRef;

/// Top-level entry point. Wraps the registry, puller, cache, and sandbox.
///
/// Cheap to clone — the inner state is reference-counted.
#[derive(Clone)]
pub struct ModelManager {
    inner: Arc<Inner>,
}

struct Inner {
    cache: Cache,
    registry: Arc<dyn ModelRegistry>,
    puller: Puller,
}

impl ModelManager {
    /// Open (or create) the manager with the given cache config and
    /// registry. Rebuilds the cache index from disk.
    pub async fn open(cache_cfg: CacheConfig, registry: Arc<dyn ModelRegistry>) -> Result<Self> {
        let cache = Cache::open(cache_cfg).await?;
        Ok(Self {
            inner: Arc::new(Inner {
                cache,
                registry,
                puller: Puller::new(),
            }),
        })
    }

    /// Build with a caller-provided puller. Used in tests.
    pub async fn open_with_puller(
        cache_cfg: CacheConfig,
        registry: Arc<dyn ModelRegistry>,
        puller: Puller,
    ) -> Result<Self> {
        let cache = Cache::open(cache_cfg).await?;
        Ok(Self {
            inner: Arc::new(Inner {
                cache,
                registry,
                puller,
            }),
        })
    }

    /// Resolve, pull if needed, verify, and return a sandboxed handle.
    ///
    /// Flow:
    /// 1. Ask registry for the manifest.
    /// 2. Cache hit? Re-hash is skipped — the digest is the cache key.
    /// 3. Cache miss → pull into a staging partial file, streaming the
    ///    SHA-256 as bytes arrive.
    /// 4. Validate GGUF header and check that `general.file_type`
    ///    matches the requested quant.
    /// 5. Rename into the content-addressed cache slot and return a
    ///    sandboxed view.
    pub async fn ensure_local(&self, r: &ModelRef) -> Result<SandboxedModel> {
        let manifest = self.inner.registry.resolve(r).await?;

        if let Some(path) = self.inner.cache.get(&manifest.sha256).await {
            info!(%r, path = %path.display(), "cache hit");
            self.validate_gguf(&path, &manifest).await?;
            return Ok(sandbox::prepare(&path));
        }

        let staging = self.staging_path(&manifest.sha256);
        if let Some(parent) = staging.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        info!(%r, target = %staging.display(), "cache miss; pulling");
        self.inner.puller.pull(&manifest, &staging).await?;

        self.validate_gguf(&staging, &manifest).await?;

        let stored = self.inner.cache.insert(manifest.sha256, &staging).await?;
        Ok(sandbox::prepare(&stored))
    }

    /// Drop cached entries until total bytes fit the cap. Intended for
    /// periodic operator-triggered cleanup.
    pub async fn gc(&self) -> Result<u64> {
        self.inner.cache.gc_for(0).await
    }

    /// Direct cache accessor — used by operator tooling and tests.
    /// Access the underlying model registry.
    pub fn registry(&self) -> &Arc<dyn ModelRegistry> {
        &self.inner.registry
    }

    /// Access the download cache.
    pub fn cache(&self) -> &Cache {
        &self.inner.cache
    }

    async fn validate_gguf(
        &self,
        path: &std::path::Path,
        manifest: &crate::types::ModelManifest,
    ) -> Result<()> {
        let header = match gguf::parse_header(path).await {
            Ok(h) => h,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "gguf parse failed; evicting");
                self.inner.cache.evict(&manifest.sha256).await?;
                return Err(e);
            }
        };

        let expected = manifest.quant.gguf_file_type();
        match header.file_type {
            Some(actual) if actual == expected => Ok(()),
            Some(actual) => {
                self.inner.cache.evict(&manifest.sha256).await?;
                Err(ModelError::Gguf(format!(
                    "quant mismatch: manifest {:?} (file_type={expected}), header file_type={actual}",
                    manifest.quant
                )))
            }
            None => {
                // Some older files omit general.file_type. Don't block on this in Phase 0,
                // but log so we can track how common it is once real models land.
                warn!(
                    path = %path.display(),
                    "gguf header missing general.file_type; skipping quant check"
                );
                Ok(())
            }
        }
    }

    /// Path where an in-flight download stages before renaming into the cache.
    fn staging_path(&self, digest: &arknet_crypto::hash::Sha256Digest) -> PathBuf {
        let final_path = self.inner.cache.config().path_for(digest);
        // Stage next to the final slot so rename is same-filesystem.
        partial_path(&final_path)
    }
}
