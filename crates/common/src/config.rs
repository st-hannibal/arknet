//! Shared configuration loading.
//!
//! Loads config from (in order, later overrides earlier):
//! 1. Embedded defaults.
//! 2. `node.toml` at the provided path (or `$HOME/.arknet/node.toml` if `None`).
//! 3. Environment variables prefixed `ARKNET_` (double-underscore as section separator).
//!
//! Phase 0 stub: parses the shape without validating semantics. Validation
//! (stake minimums, role combinations, hardware budgets) lands in [`arknet-node`]
//! during Weeks 11-12.

use std::path::{Path, PathBuf};

use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};

use crate::errors::{CommonError, Result};

/// Top-level node configuration.
///
/// Deserialized from `node.toml`. See [`docs/NODE_OPERATOR_GUIDE.md`][guide]
/// for the operator-facing reference.
///
/// [guide]: ../../../docs/NODE_OPERATOR_GUIDE.md
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Identity + data-dir settings.
    #[serde(default)]
    pub node: NodeSection,

    /// Which roles are active on this node.
    #[serde(default)]
    pub roles: RolesSection,

    /// Per-role resource budgets.
    #[serde(default)]
    pub resources: ResourcesSection,

    /// Operator preferences (payout, region, reward thresholds).
    #[serde(default)]
    pub operator: OperatorSection,

    /// P2P networking.
    #[serde(default)]
    pub network: NetworkSection,

    /// Telemetry + observability.
    #[serde(default)]
    pub telemetry: TelemetrySection,

    /// Trusted Execution Environment (TEE) settings for confidential inference.
    #[serde(default)]
    pub tee: TeeSection,

    /// Data availability layer (Celestia / EigenDA).
    #[serde(default)]
    pub da: DaSection,
}

/// `[node]` section.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSection {
    /// Human-readable node name (shown in logs + explorer).
    pub name: String,
    /// `mainnet` | `testnet` | `devnet`.
    pub network: String,
    /// Data directory (state DB, keys, model cache).
    pub data_dir: PathBuf,
    /// Log verbosity (`trace`, `debug`, `info`, `warn`, `error`).
    pub log_level: String,
}

impl Default for NodeSection {
    fn default() -> Self {
        Self {
            name: "arknet-node".into(),
            network: "devnet".into(),
            data_dir: PathBuf::from("/var/lib/arknet"),
            log_level: "info".into(),
        }
    }
}

/// `[roles]` section.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RolesSection {
    /// Run L1 validator.
    #[serde(default)]
    pub validator: bool,
    /// Run L2 router.
    #[serde(default)]
    pub router: bool,
    /// Run L2 compute (inference).
    #[serde(default)]
    pub compute: bool,
    /// Run L2 verifier.
    #[serde(default)]
    pub verifier: bool,
}

/// `[resources]` section.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourcesSection {
    /// Compute-role hardware budget.
    #[serde(default)]
    pub compute: ComputeResources,
    /// Router-role hardware budget.
    #[serde(default)]
    pub router: RouterResources,
    /// Verifier-role hardware budget.
    #[serde(default)]
    pub verifier: VerifierResources,
    /// Validator-role settings.
    #[serde(default)]
    pub validator: ValidatorResources,
}

/// Per-compute-role budget.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputeResources {
    /// GPU device indices to use (CUDA / ROCm / Metal).
    #[serde(default)]
    pub gpu_devices: Vec<u32>,
    /// VRAM cap across all loaded models (GiB).
    #[serde(default)]
    pub max_vram_gb: u32,
    /// Cap on concurrent inference jobs.
    #[serde(default)]
    pub max_concurrent_jobs: u32,
    /// Model IDs to pre-load at startup, formatted `"<id>:<quant>"`.
    #[serde(default)]
    pub loaded_models: Vec<String>,
    /// Allow the scheduler to evict loaded models for higher-demand ones.
    #[serde(default)]
    pub model_swap_enabled: bool,
}

/// Per-router-role budget.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterResources {
    /// Cap on % of total CPU usable for routing.
    #[serde(default)]
    pub cpu_percent: u32,
    /// Maximum in-flight routes.
    #[serde(default)]
    pub max_concurrent_routes: u32,
    /// Outbound bandwidth cap (Mbps).
    #[serde(default)]
    pub bandwidth_mbps: u32,
}

/// Per-verifier-role budget.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifierResources {
    /// `true` if verifier shares GPUs with the compute role.
    #[serde(default)]
    pub gpu_share_with_compute: bool,
    /// Cap on verifications per hour.
    #[serde(default)]
    pub max_verifications_per_hour: u32,
}

/// Per-validator-role settings.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatorResources {
    /// State DB path (defaults to `$data_dir/l1`).
    #[serde(default)]
    pub state_db_path: Option<PathBuf>,
    /// Max peers to maintain for gossip.
    #[serde(default)]
    pub gossip_peers_max: u32,
    /// Optional remote signer (tmkms-compatible).
    #[serde(default)]
    pub remote_signer: Option<String>,
}

/// `[operator]` section.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorSection {
    /// Payout address for rewards.
    #[serde(default)]
    pub payout_address: Option<String>,
    /// Preferred region (for latency-aware routing).
    #[serde(default)]
    pub preferred_region: Option<String>,
    /// Minimum reward-per-job threshold below which this node won't bid.
    #[serde(default)]
    pub min_reward_per_job: Option<String>,
    /// Auto-pull models when they become profitable.
    #[serde(default)]
    pub auto_model_pull: bool,
    /// Auto-rebalance across pools based on demand.
    #[serde(default)]
    pub auto_pool_rebalance: bool,
}

/// `[network]` section.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSection {
    /// P2P listen address.
    pub p2p_listen: String,
    /// RPC listen address (local gRPC/REST).
    pub rpc_listen: String,
    /// Metrics listen address (Prometheus `/metrics`).
    pub metrics_listen: String,
    /// Bootstrap peers (libp2p multiaddrs).
    #[serde(default)]
    pub bootstrap_peers: Vec<String>,
    /// Externally reachable address (auto-detected if empty).
    #[serde(default)]
    pub external_address: Option<String>,
    /// Inbound peer cap.
    #[serde(default)]
    pub max_inbound_peers: u32,
    /// Outbound peer cap.
    #[serde(default)]
    pub max_outbound_peers: u32,
}

impl Default for NetworkSection {
    fn default() -> Self {
        Self {
            p2p_listen: "0.0.0.0:26656".into(),
            rpc_listen: "127.0.0.1:26657".into(),
            metrics_listen: "127.0.0.1:9090".into(),
            bootstrap_peers: Vec::new(),
            external_address: None,
            max_inbound_peers: 60,
            max_outbound_peers: 20,
        }
    }
}

/// `[telemetry]` section.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetrySection {
    /// Enable the Prometheus scrape endpoint.
    #[serde(default)]
    pub prometheus_enabled: bool,
    /// OTLP endpoint for distributed tracing (optional).
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    /// Sentry DSN for panic reports (optional, operator opt-in).
    #[serde(default)]
    pub sentry_dsn: Option<String>,
}

/// `[tee]` section — confidential inference via hardware TEE.
///
/// When `enabled = true`, the node generates an enclave keypair at boot,
/// registers its TEE capability on-chain, and accepts `prefer_tee`
/// requests with encrypted prompts.
///
/// ```toml
/// [tee]
/// enabled = true
/// platform = "intel-tdx"   # or "amd-sev-snp"
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeeSection {
    /// Enable TEE mode. Requires actual TEE hardware.
    #[serde(default)]
    pub enabled: bool,
    /// TEE platform: `"intel-tdx"`, `"amd-sev-snp"`, or `"arm-cca"`.
    #[serde(default)]
    pub platform: Option<String>,
    /// Path to the enclave keypair file. Defaults to `<data_dir>/keys/enclave.key`.
    #[serde(default)]
    pub enclave_key_path: Option<PathBuf>,
}

/// Data availability layer settings.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DaSection {
    /// DA layer to use: `"inline"` (default, no offload), `"celestia"`, `"eigenda"`.
    #[serde(default)]
    pub layer: String,
    /// RPC endpoint of the DA node (e.g. `http://localhost:26658` for Celestia light node).
    #[serde(default)]
    pub endpoint: String,
    /// Namespace identifier (hex). Defaults to the arknet namespace.
    #[serde(default)]
    pub namespace: String,
    /// Bearer auth token for the DA node RPC.
    #[serde(default)]
    pub auth_token: String,
}

impl NodeConfig {
    /// Load configuration by layering defaults, the TOML at `path`, and `ARKNET_*` env vars.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Figment::new()
            .merge(Serialized::defaults(NodeConfig::default()))
            .merge(Toml::file(path.as_ref()))
            .merge(Env::prefixed("ARKNET_").split("__"))
            .extract::<Self>()
            .map_err(|e| CommonError::Config(e.to_string()))
    }

    /// Load defaults + env only (no file).
    pub fn load_env_only() -> Result<Self> {
        Figment::new()
            .merge(Serialized::defaults(NodeConfig::default()))
            .merge(Env::prefixed("ARKNET_").split("__"))
            .extract::<Self>()
            .map_err(|e| CommonError::Config(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let c = NodeConfig::default();
        assert_eq!(c.node.network, "devnet");
        assert_eq!(c.node.log_level, "info");
        assert!(!c.roles.validator);
        assert!(!c.roles.router);
        assert!(!c.roles.compute);
        assert!(!c.roles.verifier);
        assert_eq!(c.network.p2p_listen, "0.0.0.0:26656");
    }

    #[test]
    fn loads_minimal_toml() {
        let tmp = tempdir();
        let path = tmp.path().join("node.toml");
        std::fs::write(
            &path,
            r#"
[node]
name     = "test-node"
network  = "testnet"
data_dir = "/tmp/arknet-test"
log_level = "debug"

[roles]
router  = true
compute = true
"#,
        )
        .unwrap();

        let c = NodeConfig::load(&path).expect("config loads");
        assert_eq!(c.node.name, "test-node");
        assert_eq!(c.node.network, "testnet");
        assert!(c.roles.router);
        assert!(c.roles.compute);
        assert!(!c.roles.validator);
    }

    #[test]
    fn rejects_unknown_fields() {
        let tmp = tempdir();
        let path = tmp.path().join("node.toml");
        std::fs::write(
            &path,
            r#"
[node]
name = "x"
network = "devnet"
data_dir = "/tmp"
log_level = "info"
mystery_field = 42
"#,
        )
        .unwrap();

        let res = NodeConfig::load(&path);
        assert!(res.is_err(), "expected deny_unknown_fields to reject");
    }

    /// Minimal inline `tempdir` to avoid pulling the `tempfile` crate into `arknet-common`.
    ///
    /// Uniqueness combines pid + a monotonic counter — nanos alone collide
    /// when two parallel tests reach this helper in the same tick on fast
    /// runners (macOS hit this on CI).
    fn tempdir() -> TempDir {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pid = std::process::id();
        let mut base = std::env::temp_dir();
        base.push(format!("arknet-test-{pid}-{seq}"));
        std::fs::create_dir_all(&base).unwrap();
        TempDir { path: base }
    }

    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
