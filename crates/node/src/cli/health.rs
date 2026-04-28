//! `arknet health` — one-shot probe of the running node's /health endpoint.
//!
//! Exits 0 when status is "ok", 1 otherwise or when the endpoint is
//! unreachable. Useful in shell scripts (systemd ExecStartPost, k8s
//! liveness probes after the HTTP server comes up).

use std::path::Path;
use std::time::Duration;

use arknet_common::config::NodeConfig;
use clap::Args;
use serde::Deserialize;

use crate::errors::{NodeError, Result};
use crate::paths;

#[derive(Args, Debug)]
pub struct HealthArgs {
    /// Override the health endpoint URL. Default: reads config.
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Request timeout in milliseconds.
    #[arg(long, default_value_t = 2000)]
    pub timeout_ms: u64,
}

#[derive(Deserialize)]
struct HealthBody {
    status: String,
    uptime_seconds: f64,
    version: String,
}

pub async fn run(args: HealthArgs, data_dir: Option<&Path>) -> Result<()> {
    let endpoint = resolve_endpoint(args.endpoint.as_deref(), data_dir)?;
    let url = format!("{endpoint}/health");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(args.timeout_ms))
        .build()?;

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("unhealthy: cannot reach {url}: {e}");
            std::process::exit(1);
        }
    };

    if !resp.status().is_success() {
        eprintln!("unhealthy: {url} returned {}", resp.status());
        std::process::exit(1);
    }

    let body: HealthBody = resp.json().await?;
    println!(
        "{} / {} / uptime {:.1}s",
        body.status, body.version, body.uptime_seconds
    );
    if body.status != "ok" {
        std::process::exit(1);
    }
    Ok(())
}

/// Resolve the RPC base URL: CLI flag > node.toml `rpc_listen`.
///
/// `rpc_listen` is a SocketAddr (e.g. `127.0.0.1:8080`), so we prepend
/// `http://` to build a URL. No HTTPS in Phase 0 — the endpoint binds
/// to localhost only.
fn resolve_endpoint(flag: Option<&str>, data_dir: Option<&Path>) -> Result<String> {
    if let Some(s) = flag {
        return Ok(trim_trailing_slash(s));
    }
    let root = paths::resolve(data_dir)?;
    let toml_path = paths::node_toml(&root);
    let addr = if toml_path.exists() {
        NodeConfig::load(&toml_path)?.network.rpc_listen
    } else {
        NodeConfig::load_env_only()?.network.rpc_listen
    };
    Ok(format!("http://{addr}"))
}

fn trim_trailing_slash(s: &str) -> String {
    let t = s.trim_end_matches('/');
    t.to_string()
}

#[allow(dead_code)] // reserved for NodeError::Config-style failures.
fn _keep_err_in_scope(e: NodeError) -> String {
    e.to_string()
}
