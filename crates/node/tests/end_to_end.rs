//! End-to-end smoke: boot the node's metrics + RPC servers in-process
//! and drive them over HTTP.
//!
//! This exercises the full wiring (NodeRuntime → MetricsRegistry →
//! axum servers → CancellationToken shutdown) without spawning a
//! separate binary. Skips when the stories260K fixture isn't present.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use arknet_common::config::NodeConfig;
use arknet_crypto::hash::sha256;

// We import node::* through the crate's binary-target name; because
// `arknet-node` compiles as a bin, tests reach into submodules via
// path-based declarations. The simplest way to exercise the public
// surface is to re-hoist what we need into this test binary.
//
// This test lives outside the binary crate and can't import its
// private modules directly. We drive the public `arknet` binary
// instead — it's the honest integration path.
const STORIES260K_SHA256: &str = "270cba1bd5109f42d03350f60406024560464db173c0e387d91f0426d3bd256d";

fn fixture_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ARKNET_TEST_STORIES260K") {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    // Platform temp dir — Windows returns something like
    // `C:\Users\runneradmin\AppData\Local\Temp\`, which is absolute.
    let default = std::env::temp_dir()
        .join("arknet-test-fixtures")
        .join("stories260K.gguf");
    if default.exists() && verify(&default) {
        return Some(default);
    }
    None
}

fn verify(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    hex::encode(sha256(&bytes).as_bytes()) == STORIES260K_SHA256
}

fn arknet_binary() -> PathBuf {
    // Locate the freshly-compiled `arknet` binary. cargo sets
    // CARGO_BIN_EXE_arknet for tests in the same package.
    PathBuf::from(env!("CARGO_BIN_EXE_arknet"))
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn write_config(path: &Path, data_dir: &Path, rpc_port: u16, metrics_port: u16) {
    let cfg = format!(
        r#"[node]
name = "e2e-node"
network = "devnet"
data_dir = '{data_dir}'
log_level = "warn"

[roles]
compute = true

[network]
p2p_listen     = "0.0.0.0:26656"
rpc_listen     = "127.0.0.1:{rpc_port}"
metrics_listen = "127.0.0.1:{metrics_port}"

[telemetry]
prometheus_enabled = true
"#,
        data_dir = data_dir.display(),
        rpc_port = rpc_port,
        metrics_port = metrics_port,
    );
    std::fs::write(path, cfg).unwrap();

    // Sanity: the written file must load through the shared loader.
    let _ = NodeConfig::load(path).expect("generated config loads");
}

async fn wait_reachable(addr: SocketAddr, path: &str) -> bool {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .unwrap();
    let url = format!("http://{addr}{path}");
    for _ in 0..40 {
        if client.get(&url).send().await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_boots_serves_endpoints_and_runs_inference() {
    let Some(fixture) = fixture_path() else {
        eprintln!("stories260K fixture unavailable; skipping");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    let rpc_port = free_port();
    let metrics_port = free_port();

    // Pre-seed: copy the fixture into the content-addressed cache so
    // `/v1/models/load` hits a cache-hit path and doesn't need network.
    let (prefix, rest) = STORIES260K_SHA256.split_at(2);
    let cache_path = data_dir
        .join("models")
        .join("objects")
        .join(prefix)
        .join(format!("{rest}.gguf"));
    std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
    std::fs::copy(&fixture, &cache_path).unwrap();

    // Emit a full node.toml with custom ports and init the rest of
    // the layout (logs/, keys/). Layout setup is idempotent so we
    // can do this without running `arknet init`.
    std::fs::create_dir_all(data_dir.join("keys")).unwrap();
    std::fs::create_dir_all(data_dir.join("logs")).unwrap();
    write_config(
        &data_dir.join("node.toml"),
        &data_dir,
        rpc_port,
        metrics_port,
    );

    // Boot the binary.
    let bin = arknet_binary();
    let mut child = tokio::process::Command::new(&bin)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("start")
        .arg("--role")
        .arg("compute")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let rpc_addr: SocketAddr = format!("127.0.0.1:{rpc_port}").parse().unwrap();
    let metrics_addr: SocketAddr = format!("127.0.0.1:{metrics_port}").parse().unwrap();

    let rpc_up = wait_reachable(rpc_addr, "/health").await;
    let metrics_up = wait_reachable(metrics_addr, "/metrics").await;
    assert!(rpc_up, "rpc /health never became reachable");
    assert!(metrics_up, "metrics /metrics never became reachable");

    let client = reqwest::Client::new();

    // /health
    let health: serde_json::Value = client
        .get(format!("http://{rpc_addr}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["status"], "ok");

    // /v1/models/load
    let load_resp = client
        .post(format!("http://{rpc_addr}/v1/models/load"))
        .json(&serde_json::json!({
            "model_ref": "test-org/Stories260K-F32",
            "url": "https://huggingface.co/ggml-org/models/resolve/main/tinyllamas/stories260K.gguf",
            "sha256": STORIES260K_SHA256,
            "size_bytes": 1_185_376,
            "quant": "F32",
        }))
        .send()
        .await
        .unwrap();
    assert!(
        load_resp.status().is_success(),
        "load failed: {:?}",
        load_resp.text().await
    );

    // /v1/inference — read the SSE body as text; we just verify token
    // events surfaced, don't parse the full event stream.
    let stream_text = client
        .post(format!("http://{rpc_addr}/v1/inference"))
        .json(&serde_json::json!({
            "model_ref": "test-org/Stories260K-F32",
            "prompt": "Once upon a time",
            "max_tokens": 8,
            "mode": "deterministic",
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        stream_text.contains("event: inference"),
        "no inference events in stream: {stream_text}"
    );

    // /metrics reflects at least the install counter.
    let metrics_text = client
        .get(format!("http://{metrics_addr}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics_text.contains("arknet_node_starts_total"));

    // Graceful shutdown: send SIGTERM (portable across macOS/Linux)
    // and wait for the child to exit.
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id().unwrap() as i32, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        child.kill().await.ok();
    }

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("node did not exit promptly")
        .unwrap();
    // Not strictly required that status.code() == 0 — SIGTERM typically
    // produces 143 (128 + 15). Either is acceptable for a graceful exit.
    eprintln!("node exited with {status:?}");
}
