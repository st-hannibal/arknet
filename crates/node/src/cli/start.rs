//! `arknet start` — boot the node with the configured role.
//!
//! Loads config, opens the runtime, binds the `/metrics` endpoint,
//! launches the role body, and waits for shutdown (Ctrl-C / SIGTERM).

use std::path::Path;

use arknet_common::config::NodeConfig;
use arknet_network::Keypair;
use clap::Args;
use tracing::{error, info};

use crate::errors::{NodeError, Result};
use crate::hardware::HardwareReport;
use crate::metrics;
use crate::network_boot;
use crate::paths;
use crate::rpc;
use crate::runtime::{shutdown, NodeRuntime};
use crate::scheduler::{self, Role};
use crate::validator;

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

    let token = shutdown::install();

    // Persistent keypair: load from `<data-dir>/keys/node.key` or
    // generate + save on first boot. The same 32-byte seed drives
    // both the libp2p PeerId and the consensus signing key.
    let keypair = load_or_generate_keypair(&root)?;

    // Boot the P2P network first so the runtime's RPC layer can
    // reference the handle when answering `/peers`.
    let (network_handle, inference_channels, network_join) = network_boot::start_network(
        &root,
        &cfg.node,
        &cfg.network,
        &cfg.roles,
        keypair.clone(),
        token.clone(),
    )
    .await?;

    let mut rt = NodeRuntime::open(root.clone(), cfg.clone())
        .await?
        .with_network(network_handle.clone());

    // Boot the validator role's consensus engine up front — the RPC
    // layer picks it up via `rt.consensus` when it starts below.
    let consensus_join = if role == Role::Validator {
        let (handle, join) =
            validator::start_validator(&root, &keypair, network_handle.clone(), token.clone())
                .await?;
        rt = rt.with_consensus(handle);
        Some(join)
    } else {
        None
    };

    let loaded_models: crate::compute_role::LoadedModels =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

    let mut inference_requests = None;

    if role == Role::Compute {
        let runner = arknet_compute::ComputeJobRunner::new(rt.inference.clone());
        rt = rt.with_compute(runner);
        inference_requests = Some(inference_channels.requests);
    }

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
        let state = rpc::RpcState::new(rt.clone()).with_loaded_models(loaded_models.clone());
        let token_for_rpc = token.clone();
        tokio::spawn(async move { rpc::serve(bind, state, token_for_rpc).await })
    };

    // Drive the role. When it exits, request shutdown so the servers
    // come down with it.
    let role_result = scheduler::run(role, rt, inference_requests, loaded_models, token.clone()).await;
    token.cancel();

    if let Err(e) = metrics_handle.await? {
        error!(error = %e, "metrics server errored on shutdown");
    }
    if let Err(e) = rpc_handle.await? {
        error!(error = %e, "rpc server errored on shutdown");
    }
    if let Some(join) = consensus_join {
        match join.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => error!(error = %e, "consensus engine exited with error"),
            Err(e) => error!(error = %e, "consensus engine panicked"),
        }
    }
    match network_join.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => error!(error = %e, "network task exited with error"),
        Err(e) => error!(error = %e, "network task panicked"),
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

/// Load the node's ed25519 keypair from disk, or generate and persist
/// one on first boot. The key file is `<data-dir>/keys/node.key` (raw
/// 64-byte ed25519 keypair: 32 secret || 32 public).
fn load_or_generate_keypair(data_dir: &Path) -> Result<Keypair> {
    let key_path = paths::keys_dir(data_dir).join("node.key");

    if key_path.exists() {
        let bytes = std::fs::read(&key_path)
            .map_err(|e| NodeError::Paths(format!("read {}: {e}", key_path.display())))?;
        if bytes.len() != 64 {
            return Err(NodeError::Paths(format!(
                "node.key has {} bytes, expected 64",
                bytes.len()
            )));
        }
        let mut arr = [0u8; 64];
        arr.copy_from_slice(&bytes);
        let ed = arknet_network::identity::ed25519::Keypair::try_from_bytes(&mut arr)
            .map_err(|e| NodeError::Paths(format!("parse node.key: {e}")))?;
        info!(path = %key_path.display(), "loaded node keypair from disk");
        Ok(arknet_network::identity::Keypair::from(ed))
    } else {
        let keypair = Keypair::generate_ed25519();
        let ed = keypair
            .clone()
            .try_into_ed25519()
            .map_err(|e| NodeError::Paths(format!("ed25519 conversion: {e}")))?;
        std::fs::create_dir_all(key_path.parent().unwrap())
            .map_err(|e| NodeError::Paths(format!("create keys dir: {e}")))?;
        std::fs::write(&key_path, ed.to_bytes())
            .map_err(|e| NodeError::Paths(format!("write {}: {e}", key_path.display())))?;
        info!(path = %key_path.display(), "generated new node keypair");
        Ok(keypair)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_parse_from_args() {
        assert_eq!("compute".parse::<Role>().unwrap(), Role::Compute);
    }

    #[test]
    fn keypair_persists_across_loads() {
        let tmp = tempfile::tempdir().unwrap();
        paths::ensure_layout(tmp.path()).unwrap();
        let kp1 = load_or_generate_keypair(tmp.path()).unwrap();
        let kp2 = load_or_generate_keypair(tmp.path()).unwrap();
        let pk1 = kp1.public().to_peer_id();
        let pk2 = kp2.public().to_peer_id();
        assert_eq!(pk1, pk2, "same data dir must produce same PeerId");
    }
}
