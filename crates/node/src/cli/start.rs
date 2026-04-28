//! `arknet start` — boot the node with the configured role.
//!
//! Loads config, opens the runtime, binds the `/metrics` endpoint,
//! launches the role body, and waits for shutdown (Ctrl-C / SIGTERM).

use std::path::Path;

use arknet_common::config::NodeConfig;
use clap::Args;
use tracing::{error, info};

use crate::errors::{NodeError, Result};
use crate::hardware::HardwareReport;
use crate::metrics;
use crate::paths;
use crate::rpc;
use crate::runtime::{shutdown, NodeRuntime};
use crate::scheduler::{self, Role};

#[derive(Args, Debug)]
pub struct StartArgs {
    /// Role to run. Phase 0 only supports `compute`.
    #[arg(long, default_value = "compute")]
    pub role: String,
}

pub async fn run(args: StartArgs, data_dir: Option<&Path>) -> Result<()> {
    let role: Role = args.role.parse()?;

    let root = paths::resolve(data_dir)?;
    paths::ensure_layout(&root)?;

    let toml_path = paths::node_toml(&root);
    let cfg = if toml_path.exists() {
        NodeConfig::load(&toml_path)?
    } else {
        info!(
            "no node.toml at {}; using built-in defaults",
            toml_path.display()
        );
        NodeConfig::load_env_only()?
    };

    print_banner(&root, &cfg);

    let rt = NodeRuntime::open(root.clone(), cfg.clone()).await?;
    let token = shutdown::install();

    // Launch /metrics in the background — it shuts itself down when
    // the token fires, same as the role body.
    let metrics_handle = {
        let bind = cfg
            .network
            .metrics_listen
            .parse()
            .map_err(|e| NodeError::Config(format!("metrics_listen: {e}")))?;
        let registry = rt.metrics.clone();
        let token_for_metrics = token.clone();
        tokio::spawn(async move { metrics::serve(bind, registry, token_for_metrics).await })
    };

    // Launch the Phase-0 HTTP RPC endpoint on the same shutdown token.
    // Minimal surface: /health, /v1/models, /v1/models/load,
    // /v1/inference (SSE stream).
    let rpc_handle = {
        let bind: std::net::SocketAddr = cfg
            .network
            .rpc_listen
            .parse()
            .map_err(|e| NodeError::Config(format!("rpc_listen: {e}")))?;
        let state = rpc::RpcState::new(rt.clone());
        let token_for_rpc = token.clone();
        tokio::spawn(async move { rpc::serve(bind, state, token_for_rpc).await })
    };

    // Drive the role. When it exits, request shutdown so the servers
    // come down with it.
    let role_result = scheduler::run(role, rt, token.clone()).await;
    token.cancel();

    if let Err(e) = metrics_handle.await? {
        error!(error = %e, "metrics server errored on shutdown");
    }
    if let Err(e) = rpc_handle.await? {
        error!(error = %e, "rpc server errored on shutdown");
    }

    role_result
}

fn print_banner(data_dir: &Path, cfg: &NodeConfig) {
    let hw = HardwareReport::probe();
    info!(
        node = %cfg.node.name,
        network = %cfg.node.network,
        data_dir = %data_dir.display(),
        "arknet starting"
    );
    // Write the hardware block to stderr plainly so it survives JSON
    // logging (which would otherwise fold it into a single record).
    eprintln!();
    eprintln!(
        "arknet {} / {}",
        env!("CARGO_PKG_VERSION"),
        cfg.node.network
    );
    eprint!("{hw}");
    eprintln!("  Data dir:     {}", data_dir.display());
    eprintln!("  RPC:          {}", cfg.network.rpc_listen);
    eprintln!("  Metrics:      {}", cfg.network.metrics_listen);
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_parse_from_args() {
        assert_eq!("compute".parse::<Role>().unwrap(), Role::Compute);
    }
}
