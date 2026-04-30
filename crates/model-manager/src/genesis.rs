//! Genesis model registry seed.
//!
//! Parses the curated fixture at
//! [`fixtures/genesis-registry.toml`](../../fixtures/genesis-registry.toml)
//! into [`ModelManifest`]s that can either populate a [`MockRegistry`]
//! (dev / tests) or be submitted as `RegisterModel` transactions at
//! genesis (Phase 1 chain bootstrap).
//!
//! # Fair-launch invariant
//!
//! The registry is seeded; the token supply is not. Nothing in this
//! module mints ARK. Genesis validators pay no deposit — the
//! `RegisterModel` applier (Week 11) is what enforces the deposit for
//! post-genesis registrations.
//!
//! # Hash integrity
//!
//! The SHA-256 and byte size in the fixture come from the
//! `ops/scripts/gen-model-registry.py` helper which pulls them directly
//! off the Hugging Face LFS API. Editing either field by hand will
//! cause every pull of that model to fail integrity verification — use
//! the regen script.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use arknet_crypto::hash::{blake3, Sha256Digest};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::errors::{ModelError, Result};
use crate::registry::{MockRegistry, MockRegistryFile};
use crate::types::{ModelId, ModelManifest, ModelRef};

/// Wire format of the TOML genesis fixture.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenesisRegistryFile {
    /// Chain identifier this seed is intended for. Cross-checked against
    /// the node's configured network to prevent cross-network pollution.
    pub chain_id: String,
    /// UTC timestamp when the fixture was regenerated (informational).
    #[serde(default)]
    pub generated_at: Option<String>,
    /// One entry per model.
    pub models: Vec<GenesisModel>,
}

/// One entry in the seed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenesisModel {
    /// Canonical `<org>/<name>-<QUANT>` reference.
    pub model_ref: String,
    /// Hugging Face repo slug (owner / name).
    pub gguf_repo: String,
    /// File inside `gguf_repo` that holds the quantized weights.
    pub file_path: String,
    /// SHA-256 of `file_path`, 64 hex chars (no `0x`).
    pub sha256: String,
    /// Expected file size in bytes — early mismatch detection before the
    /// full stream even starts hashing.
    pub size_bytes: u64,
    /// SPDX short form (informational).
    pub license: String,
    /// Hardware tier: `low` / `mid` / `high`. Not consensus-relevant.
    pub tier: String,
    /// Human-facing description used in UIs.
    #[serde(default)]
    pub description: String,
}

/// Embedded copy of the TOML fixture, so a node boots with a useful
/// registry even when no external file is around.
pub const EMBEDDED_GENESIS_TOML: &str = include_str!("../fixtures/genesis-registry.toml");

impl GenesisRegistryFile {
    /// Parse the embedded fixture. Only fails on a code-review mistake
    /// that ships a broken TOML or an unknown quant tag.
    pub fn embedded() -> Result<Self> {
        Self::from_toml(EMBEDDED_GENESIS_TOML)
    }

    /// Parse from a TOML string.
    pub fn from_toml(s: &str) -> Result<Self> {
        toml::from_str(s).map_err(|e| ModelError::Codec(format!("genesis-registry.toml: {e}")))
    }

    /// Read a TOML file from disk (used by regen tooling / tests).
    pub async fn from_path(path: &Path) -> Result<Self> {
        let bytes = tokio::fs::read(path).await?;
        Self::from_toml(
            std::str::from_utf8(&bytes)
                .map_err(|e| ModelError::Codec(format!("genesis-registry.toml utf-8: {e}")))?,
        )
    }

    /// Convert every entry into a canonical [`ModelManifest`], keyed by
    /// its `ModelRef::to_string()` so the result can drop straight into
    /// [`MockRegistry::from_manifests`].
    pub fn to_manifests(&self) -> Result<HashMap<String, ModelManifest>> {
        let mut out = HashMap::with_capacity(self.models.len());
        for (idx, m) in self.models.iter().enumerate() {
            let manifest = m.to_manifest().map_err(|e| {
                ModelError::Codec(format!("genesis-registry[{idx}] ({}): {e}", m.model_ref))
            })?;
            out.insert(manifest.model_ref.to_string(), manifest);
        }
        Ok(out)
    }

    /// Build a ready-to-use [`MockRegistry`] populated with every entry.
    pub fn to_mock_registry(&self) -> Result<MockRegistry> {
        let manifests = self.to_manifests()?;
        Ok(MockRegistry::from_file(MockRegistryFile {
            version: 1,
            manifests,
        }))
    }

    /// Cross-check this seed applies to the node's configured network.
    /// The chain id in the fixture must equal the node's.
    pub fn check_chain_id(&self, expected: &str) -> Result<()> {
        if self.chain_id != expected {
            return Err(ModelError::Codec(format!(
                "genesis-registry.toml: chain_id mismatch (file={}, node={})",
                self.chain_id, expected
            )));
        }
        Ok(())
    }
}

impl GenesisModel {
    fn to_manifest(&self) -> std::result::Result<ModelManifest, String> {
        let model_ref = ModelRef::parse(&self.model_ref)?;
        let sha256 = parse_sha256(&self.sha256)?;
        let mirror = hf_mirror_url(&self.gguf_repo, &self.file_path)?;
        Ok(ModelManifest {
            // Stable id = blake3("<model_ref>|<sha256>") — cheap to
            // recompute, collision-resistant, and independent of the
            // mirror list (so rotating a mirror does not change the id).
            id: derive_model_id(&model_ref.to_string(), &sha256),
            model_ref,
            mirrors: vec![mirror],
            sha256,
            size_bytes: self.size_bytes,
            quant: /* reuse the parsed ref's quant */
                   ModelRef::parse(&self.model_ref)
                       .map_err(|e| format!("invalid model_ref quant: {e}"))?
                       .quant,
            license: self.license.clone(),
        })
    }
}

fn hf_mirror_url(repo: &str, file: &str) -> std::result::Result<Url, String> {
    let raw = format!("https://huggingface.co/{repo}/resolve/main/{file}");
    Url::from_str(&raw).map_err(|e| format!("bad mirror url {raw}: {e}"))
}

fn parse_sha256(hex: &str) -> std::result::Result<Sha256Digest, String> {
    let clean = hex.strip_prefix("0x").unwrap_or(hex);
    let bytes = hex::decode(clean).map_err(|e| format!("sha256 hex: {e}"))?;
    if bytes.len() != 32 {
        return Err(format!("sha256 must be 32 bytes, got {}", bytes.len()));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(Sha256Digest(arr))
}

fn derive_model_id(model_ref: &str, sha: &Sha256Digest) -> ModelId {
    let mut buf = Vec::with_capacity(model_ref.len() + 1 + 32);
    buf.extend_from_slice(model_ref.as_bytes());
    buf.push(b'|');
    buf.extend_from_slice(&sha.0);
    let digest = blake3(&buf);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_bytes());
    ModelId(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::GgufQuant;

    #[test]
    fn embedded_genesis_parses() {
        let genesis = GenesisRegistryFile::embedded().expect("embedded toml parses");
        assert!(!genesis.models.is_empty(), "genesis seed must not be empty");
        assert_eq!(genesis.chain_id, "arknet-devnet-1");
    }

    #[test]
    fn embedded_genesis_covers_every_tier() {
        let genesis = GenesisRegistryFile::embedded().unwrap();
        let tiers: std::collections::HashSet<_> =
            genesis.models.iter().map(|m| m.tier.as_str()).collect();
        assert!(
            tiers.contains("low"),
            "must have at least one low-tier model"
        );
        assert!(
            tiers.contains("mid"),
            "must have at least one mid-tier model"
        );
        assert!(
            tiers.contains("high"),
            "must have at least one high-tier model"
        );
    }

    #[test]
    fn every_entry_converts_to_manifest() {
        let genesis = GenesisRegistryFile::embedded().unwrap();
        let manifests = genesis.to_manifests().expect("all entries parse");
        assert_eq!(manifests.len(), genesis.models.len());
        for m in manifests.values() {
            assert_eq!(m.quant, GgufQuant::Q4KM);
            assert_eq!(m.sha256.0.len(), 32);
            assert!(m.size_bytes > 0);
            assert!(!m.mirrors.is_empty());
            assert_eq!(m.mirrors[0].scheme(), "https");
            assert_eq!(m.mirrors[0].host_str(), Some("huggingface.co"));
        }
    }

    #[test]
    fn model_ids_are_deterministic() {
        let a = GenesisRegistryFile::embedded()
            .unwrap()
            .to_manifests()
            .unwrap();
        let b = GenesisRegistryFile::embedded()
            .unwrap()
            .to_manifests()
            .unwrap();
        for (k, manifest) in &a {
            assert_eq!(
                b[k].id, manifest.id,
                "model id must be reproducible for {k}"
            );
        }
    }

    #[test]
    fn model_ids_are_unique() {
        let manifests = GenesisRegistryFile::embedded()
            .unwrap()
            .to_manifests()
            .unwrap();
        let ids: std::collections::HashSet<_> = manifests.values().map(|m| m.id).collect();
        assert_eq!(
            ids.len(),
            manifests.len(),
            "no two seed entries may derive the same ModelId"
        );
    }

    #[test]
    fn mock_registry_resolves_every_seed_entry() {
        use crate::registry::ModelRegistry;
        let genesis = GenesisRegistryFile::embedded().unwrap();
        let registry = genesis.to_mock_registry().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            for m in &genesis.models {
                let r = ModelRef::parse(&m.model_ref).unwrap();
                let manifest = registry.resolve(&r).await.unwrap();
                assert_eq!(manifest.size_bytes, m.size_bytes);
            }
        });
    }

    #[test]
    fn chain_id_check_matches() {
        let genesis = GenesisRegistryFile::embedded().unwrap();
        genesis.check_chain_id("arknet-devnet-1").unwrap();
        assert!(genesis.check_chain_id("arknet-mainnet").is_err());
    }

    #[test]
    fn rejects_bad_sha256() {
        let bad = GenesisModel {
            model_ref: "foo/bar-Q4_K_M".into(),
            gguf_repo: "foo/bar".into(),
            file_path: "x.gguf".into(),
            sha256: "not-hex".into(),
            size_bytes: 1,
            license: "mit".into(),
            tier: "low".into(),
            description: String::new(),
        };
        assert!(bad.to_manifest().is_err());
    }
}
