//! `arknet status` — scrape the running node's Prometheus /metrics endpoint
//! and print a human-readable summary.
//!
//! Minimal Phase-0 parsing: surface the small set of metrics the node
//! registers itself. Full dashboard-style rendering is out of scope.

use std::path::Path;
use std::time::Duration;

use arknet_common::config::NodeConfig;
use clap::Args;

use crate::errors::Result;
use crate::paths;

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Override the metrics endpoint URL (default reads config's `metrics_listen`).
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Request timeout in milliseconds.
    #[arg(long, default_value_t = 2000)]
    pub timeout_ms: u64,
}

pub async fn run(args: StatusArgs, data_dir: Option<&Path>) -> Result<()> {
    let endpoint = resolve_endpoint(args.endpoint.as_deref(), data_dir)?;
    let url = format!("{endpoint}/metrics");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(args.timeout_ms))
        .build()?;

    let body = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r.text().await?,
        Ok(r) => {
            eprintln!("status: {url} returned {}", r.status());
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("status: cannot reach {url}: {e}");
            std::process::exit(1);
        }
    };

    // Fish out the two hot metrics. The Prometheus text format is
    // line-based: `name value` after comment lines that start with `#`.
    let starts = find_counter(&body, "arknet_node_starts_total");
    let uptime = find_counter(&body, "arknet_node_uptime_seconds");
    let tokens = find_counter(&body, "arknet_inference_tokens_generated_total");

    println!("arknet status ({endpoint})");
    println!("  starts:             {}", fmt_metric(starts));
    println!("  uptime (s):         {}", fmt_metric(uptime));
    println!("  inference tokens:   {}", fmt_metric(tokens));
    Ok(())
}

fn find_counter(body: &str, name: &str) -> Option<f64> {
    for line in body.lines() {
        if line.starts_with('#') {
            continue;
        }
        // Prometheus format: `name [labels] value`. We only register
        // label-free counters + one gauge; a contains() + split_whitespace
        // is enough.
        let Some((n, rest)) = line.split_once(|c: char| c.is_whitespace()) else {
            continue;
        };
        if n == name {
            if let Some(v) = rest.split_whitespace().next() {
                return v.parse::<f64>().ok();
            }
        }
    }
    None
}

fn fmt_metric(v: Option<f64>) -> String {
    match v {
        Some(n) => format!("{n:.3}"),
        None => "(unset)".into(),
    }
}

fn resolve_endpoint(flag: Option<&str>, data_dir: Option<&Path>) -> Result<String> {
    if let Some(s) = flag {
        return Ok(s.trim_end_matches('/').to_string());
    }
    let root = paths::resolve(data_dir)?;
    let toml_path = paths::node_toml(&root);
    let addr = if toml_path.exists() {
        NodeConfig::load(&toml_path)?.network.metrics_listen
    } else {
        NodeConfig::load_env_only()?.network.metrics_listen
    };
    Ok(format!("http://{addr}"))
}
