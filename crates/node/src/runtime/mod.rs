//! Node runtime glue.
//!
//! [`NodeRuntime`] wires together every long-lived service the node
//! binary needs:
//!
//! - [`MetricsRegistry`] (already installed at this point).
//! - [`ModelManager`] backed by the data-dir's `models/` cache and a
//!   [`MockRegistry`]. (The on-chain registry is Phase 1 work; for
//!   now operators point `loaded_models` at test fixtures or leave it
//!   empty and drive everything via CLI.)
//! - [`InferenceEngine`] over the same model manager — CLI commands,
//!   the role scheduler, and the RPC endpoint share one engine.
//!
//! Construction is cheap (no model loads); the first `ensure_local`
//! through the engine does the real work.

#![allow(dead_code)]

pub mod shutdown;

use std::collections::HashMap;
use std::sync::Arc;

use arknet_common::config::NodeConfig;
use arknet_inference::{InferenceConfig, InferenceEngine};
use arknet_model_manager::{CacheConfig, MockRegistry, ModelManager};

use crate::errors::Result;
use crate::metrics::MetricsRegistry;
use crate::paths;

/// Long-lived handles for the services the runtime owns.
///
/// Clones are cheap (all inner state is `Arc`-shared); pass this into
/// the role scheduler, the RPC server, and CLI commands freely.
#[derive(Clone)]
pub struct NodeRuntime {
    pub cfg: Arc<NodeConfig>,
    pub metrics: MetricsRegistry,
    pub model_manager: ModelManager,
    pub inference: InferenceEngine,
    pub data_dir: std::path::PathBuf,
}

impl NodeRuntime {
    /// Build a runtime for the given data-dir. Does not start any
    /// server or load any model — that's the scheduler's job.
    pub async fn open(data_dir: std::path::PathBuf, cfg: NodeConfig) -> Result<Self> {
        let metrics = MetricsRegistry::install()?;

        // Phase 0 uses an empty MockRegistry; operators drive model
        // refs via the CLI, which resolves through the mock and fails
        // loudly on unknown models. Phase 1 replaces this with the
        // on-chain registry.
        let registry = Arc::new(MockRegistry::from_manifests(HashMap::new()));
        let cache_cfg = CacheConfig::with_root(paths::models_dir(&data_dir));
        let model_manager = ModelManager::open(cache_cfg, registry).await?;

        let inference_cfg = InferenceConfig {
            max_context_tokens: 8192,
            serving_threads: cfg.resources.compute.max_concurrent_jobs.max(1).min(
                u32::try_from(
                    std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(8),
                )
                .unwrap_or(8),
            ),
        };
        let inference = InferenceEngine::new(inference_cfg, model_manager.clone());

        Ok(Self {
            cfg: Arc::new(cfg),
            metrics,
            model_manager,
            inference,
            data_dir,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runtime_opens_on_empty_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = NodeConfig::default();
        let rt = NodeRuntime::open(tmp.path().to_path_buf(), cfg)
            .await
            .unwrap();
        assert!(rt.data_dir.exists());
        // Nothing is loaded — just confirm the wiring succeeded.
        assert!(rt.metrics.render().contains("arknet_node_starts_total"));
    }
}
