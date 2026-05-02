//! Model registry: resolves a [`ModelRef`] to a [`ModelManifest`].
//!
//! Phase 0 ships [`MockRegistry`] — a JSON file loaded from disk. Phase 1
//! will add `OnChainRegistry` backed by `arknet-chain`. Both implement
//! [`ModelRegistry`] so callers never know the difference.

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::errors::{ModelError, Result};
use crate::types::{ModelManifest, ModelRef};

/// The registry abstraction the rest of the crate depends on.
///
/// Implementations must be cheap to clone via `Arc` — the manager holds
/// one instance and calls `resolve` per `ensure_local` invocation.
#[async_trait]
pub trait ModelRegistry: Send + Sync {
    /// Resolve a ref to its concrete manifest, or report unknown.
    async fn resolve(&self, r: &ModelRef) -> Result<ModelManifest>;

    /// Downcast support for runtime model registration.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Shape of the `models.json` file on disk.
///
/// Kept as its own serde type so the on-disk format can evolve without
/// touching [`ModelManifest`] (e.g. adding per-mirror health scores).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MockRegistryFile {
    /// Format version. Bump when fields change incompatibly.
    pub version: u32,
    /// All known manifests, keyed by canonical `ModelRef::to_string()`.
    pub manifests: HashMap<String, ModelManifest>,
}

/// Offline registry backed by a JSON file.
///
/// Use for Phase 0 development and integration tests. The file is read
/// once at construction. Supports runtime insertion via [`Self::insert`]
/// for `/v1/models/load`.
#[derive(Debug)]
pub struct MockRegistry {
    file: parking_lot::RwLock<MockRegistryFile>,
}

impl MockRegistry {
    /// Load from a JSON file on disk.
    pub async fn from_path(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).await?;
        let file: MockRegistryFile = serde_json::from_slice(&bytes)?;
        Ok(Self {
            file: parking_lot::RwLock::new(file),
        })
    }

    /// Construct directly from an in-memory table — mainly for tests.
    pub fn from_manifests(manifests: HashMap<String, ModelManifest>) -> Self {
        Self {
            file: parking_lot::RwLock::new(MockRegistryFile {
                version: 1,
                manifests,
            }),
        }
    }

    /// Construct from an already-materialized [`MockRegistryFile`].
    pub fn from_file(file: MockRegistryFile) -> Self {
        Self {
            file: parking_lot::RwLock::new(file),
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.file.read().manifests.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.file.read().manifests.is_empty()
    }

    /// Register a model manifest at runtime (used by `/v1/models/load`).
    pub fn insert(&self, model_ref: &str, manifest: ModelManifest) {
        self.file
            .write()
            .manifests
            .insert(model_ref.to_string(), manifest);
    }
}

#[async_trait]
impl ModelRegistry for MockRegistry {
    async fn resolve(&self, r: &ModelRef) -> Result<ModelManifest> {
        self.file
            .read()
            .manifests
            .get(&r.to_string())
            .cloned()
            .ok_or_else(|| ModelError::UnknownModel(r.to_string()))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_crypto::hash::{sha256, Sha256Digest};
    use url::Url;

    use crate::types::{GgufQuant, ModelId};

    fn sample_manifest() -> ModelManifest {
        let r = ModelRef::parse("meta-llama/Llama-3-7B-Instruct-Q4_K_M").unwrap();
        ModelManifest {
            id: ModelId([1u8; 32]),
            model_ref: r.clone(),
            mirrors: vec![Url::parse("https://example.com/x.gguf").unwrap()],
            sha256: sha256(b"placeholder"),
            size_bytes: 11,
            quant: GgufQuant::Q4KM,
            license: "apache-2.0".into(),
        }
    }

    #[tokio::test]
    async fn mock_registry_resolves_known_ref() {
        let m = sample_manifest();
        let mut tbl = HashMap::new();
        tbl.insert(m.model_ref.to_string(), m.clone());
        let reg = MockRegistry::from_manifests(tbl);

        let got = reg.resolve(&m.model_ref).await.unwrap();
        assert_eq!(got.size_bytes, m.size_bytes);
        assert_eq!(got.quant, m.quant);
    }

    #[tokio::test]
    async fn mock_registry_errors_on_unknown_ref() {
        let reg = MockRegistry::from_manifests(HashMap::new());
        let r = ModelRef::parse("ghost/model-F16").unwrap();
        let err = reg.resolve(&r).await.unwrap_err();
        assert!(matches!(err, ModelError::UnknownModel(_)));
    }

    #[tokio::test]
    async fn mock_registry_roundtrips_through_json() {
        let m = sample_manifest();
        let mut tbl = HashMap::new();
        tbl.insert(m.model_ref.to_string(), m.clone());
        let file = MockRegistryFile {
            version: 1,
            manifests: tbl,
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        tokio::fs::write(&path, serde_json::to_vec_pretty(&file).unwrap())
            .await
            .unwrap();

        let reg = MockRegistry::from_path(&path).await.unwrap();
        let got = reg.resolve(&m.model_ref).await.unwrap();
        assert_eq!(got.id, m.id);
    }

    #[test]
    fn sha256_digest_serde_works() {
        // Sanity: Sha256Digest must serialize (manifest embeds it).
        // The digest type implements Serialize/Deserialize via its inner Hash256 array.
        let d: Sha256Digest = sha256(b"x");
        let s = serde_json::to_string(&d).unwrap();
        let r: Sha256Digest = serde_json::from_str(&s).unwrap();
        assert_eq!(d, r);
    }
}
