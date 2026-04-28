//! `arknet model {list, pull, load, verify, bench}` — operator commands
//! that drive the model manager + inference engine directly.
//!
//! These run as standalone CLI invocations (no running daemon). Each
//! command opens its own `NodeRuntime`, does its work, and exits.
//! Sharing a runtime with a running `arknet start` is a Phase 1
//! improvement once the HTTP endpoint lands and CLI commands can hit
//! it instead of spinning up their own engine.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use arknet_common::config::NodeConfig;
use arknet_crypto::hash::Sha256Digest;
use arknet_inference::{
    InferenceEvent, InferenceMode, InferenceRequest, SamplingParams, StopReason,
};
use arknet_model_manager::{GgufQuant, MockRegistry, ModelId, ModelManifest, ModelRef};
use clap::{Args, Subcommand};
use futures::StreamExt;
use tokio::io::AsyncReadExt;
use url::Url;

use crate::errors::{NodeError, Result};
use crate::paths;
use crate::runtime::NodeRuntime;

#[derive(Subcommand, Debug)]
pub enum ModelCmd {
    /// List entries in the model-manager cache.
    List,
    /// Pull a model by reference and verify it into the cache.
    Pull(PullArgs),
    /// Load a model into the inference engine and print its metadata.
    Load(LoadArgs),
    /// Re-verify a cached model against its manifest digest.
    Verify(VerifyArgs),
    /// Run a throughput benchmark for a loaded model.
    Bench(BenchArgs),
}

#[derive(Args, Debug)]
pub struct PullArgs {
    /// Model reference, e.g. `test-org/Stories260K-F32`.
    pub model_ref: String,
    /// Manifest URL (Phase 0: mock registry accepts a direct URL + digest).
    #[arg(long)]
    pub url: String,
    /// Expected SHA-256 of the downloaded file (64-char hex).
    #[arg(long)]
    pub sha256: String,
    /// Declared file size in bytes.
    #[arg(long)]
    pub size: u64,
    /// Declared GGUF quantization tag (default F32 for tests).
    #[arg(long, default_value = "F32")]
    pub quant: String,
}

#[derive(Args, Debug)]
pub struct LoadArgs {
    /// Model reference previously pulled into the cache.
    pub model_ref: String,
    /// Re-supply the manifest details used at pull time. Phase 0 doesn't
    /// persist a (ref → digest) map, so every command that resolves a
    /// ref needs the same manifest inputs. Phase 1's on-chain registry
    /// removes this.
    #[arg(long)]
    pub url: String,
    /// SHA-256 digest (64 hex chars).
    #[arg(long)]
    pub sha256: String,
    /// Declared file size in bytes.
    #[arg(long)]
    pub size: u64,
    /// Declared GGUF quantization tag.
    #[arg(long, default_value = "F32")]
    pub quant: String,
}

#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// Model reference to re-verify.
    pub model_ref: String,
}

#[derive(Args, Debug)]
pub struct BenchArgs {
    /// Model reference.
    pub model_ref: String,
    /// Manifest URL (Phase 0 workaround — see LoadArgs for details).
    #[arg(long)]
    pub url: String,
    /// SHA-256 digest (64 hex chars).
    #[arg(long)]
    pub sha256: String,
    /// Declared file size in bytes.
    #[arg(long)]
    pub size: u64,
    /// Declared GGUF quantization tag.
    #[arg(long, default_value = "F32")]
    pub quant: String,
    /// Tokens to generate during the timed portion.
    #[arg(long, default_value_t = 128)]
    pub tokens: u32,
    /// Prompt to use.
    #[arg(long, default_value = "Once upon a time")]
    pub prompt: String,
}

pub async fn run(cmd: ModelCmd, data_dir: Option<&Path>) -> Result<()> {
    let root = paths::resolve(data_dir)?;
    paths::ensure_layout(&root)?;

    let toml_path = paths::node_toml(&root);
    let cfg = if toml_path.exists() {
        NodeConfig::load(&toml_path)?
    } else {
        NodeConfig::load_env_only()?
    };

    // For commands that need a registry entry, we build a one-off
    // MockRegistry around the CLI args and pass that to the runtime.
    // `list` and `bench` reach straight into the cache without needing
    // a manifest.
    match cmd {
        ModelCmd::List => run_list(root).await,
        ModelCmd::Pull(args) => run_pull(root, cfg, args).await,
        ModelCmd::Load(args) => run_load(root, cfg, args).await,
        ModelCmd::Verify(args) => run_verify(root, cfg, args).await,
        ModelCmd::Bench(args) => run_bench(root, cfg, args).await,
    }
}

async fn run_list(root: std::path::PathBuf) -> Result<()> {
    let cache_dir = paths::models_dir(&root).join("objects");
    if !cache_dir.exists() {
        println!("cache empty (no objects directory)");
        return Ok(());
    }

    let mut count = 0u64;
    let mut total_bytes = 0u64;
    let mut dir = tokio::fs::read_dir(&cache_dir).await?;
    while let Some(shard) = dir.next_entry().await? {
        if !shard.file_type().await?.is_dir() {
            continue;
        }
        let mut sub = tokio::fs::read_dir(shard.path()).await?;
        while let Some(f) = sub.next_entry().await? {
            let name = f.file_name().to_string_lossy().to_string();
            let Some(rest) = name.strip_suffix(".gguf") else {
                continue;
            };
            let prefix = shard.file_name().to_string_lossy().to_string();
            let digest_hex = format!("{prefix}{rest}");
            let size = f.metadata().await?.len();
            count += 1;
            total_bytes += size;
            let mib = size as f64 / (1024.0 * 1024.0);
            println!("  sha256:{digest_hex}  {mib:>9.2} MiB");
        }
    }

    if count == 0 {
        println!("cache empty");
    } else {
        let gib = total_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        println!();
        println!("{count} object(s), {gib:.2} GiB total");
    }
    Ok(())
}

async fn run_pull(root: std::path::PathBuf, cfg: NodeConfig, args: PullArgs) -> Result<()> {
    let model_ref = ModelRef::parse(&args.model_ref).map_err(NodeError::ModelRef)?;
    let manifest = build_manifest(&model_ref, &args.url, &args.sha256, args.size, &args.quant)?;
    let rt = open_runtime_with_manifest(root, cfg, &model_ref, manifest).await?;
    let sandbox = rt.model_manager.ensure_local(&model_ref).await?;
    println!("pulled + verified: {}", sandbox.path().display());
    Ok(())
}

async fn run_load(root: std::path::PathBuf, cfg: NodeConfig, args: LoadArgs) -> Result<()> {
    let model_ref = ModelRef::parse(&args.model_ref).map_err(NodeError::ModelRef)?;
    let manifest = build_manifest(&model_ref, &args.url, &args.sha256, args.size, &args.quant)?;
    let rt = open_runtime_with_manifest(root, cfg, &model_ref, manifest).await?;
    let handle = rt.inference.load(&model_ref).await?;
    println!("loaded: {}", handle.description());
    println!("digest: sha256:{}", hex::encode(handle.digest().as_bytes()));
    Ok(())
}

fn build_manifest(
    model_ref: &ModelRef,
    url: &str,
    sha256_hex: &str,
    size: u64,
    quant_str: &str,
) -> Result<ModelManifest> {
    let url = Url::parse(url).map_err(|e| NodeError::ModelRef(format!("bad url: {e}")))?;
    let digest = parse_digest(sha256_hex)?;
    let quant = GgufQuant::parse(quant_str)
        .ok_or_else(|| NodeError::ModelRef(format!("unknown quant: {quant_str}")))?;
    Ok(ModelManifest {
        id: ModelId([0u8; 32]),
        model_ref: model_ref.clone(),
        mirrors: vec![url],
        sha256: digest,
        size_bytes: size,
        quant,
        license: "unknown".into(),
    })
}

async fn run_verify(root: std::path::PathBuf, _cfg: NodeConfig, args: VerifyArgs) -> Result<()> {
    let _model_ref = ModelRef::parse(&args.model_ref).map_err(NodeError::ModelRef)?;

    // Phase 0: we don't persist a (ref → digest) map, so `verify`
    // needs either the digest directly or a trusted manifest source.
    // For now, `verify` walks the cache and recomputes SHA-256 for
    // each object to make sure nothing bit-rotted. Slow on large
    // caches but cheap + correct.
    let objects = paths::models_dir(&root).join("objects");
    if !objects.exists() {
        println!("cache empty");
        return Ok(());
    }

    let mut ok = 0u64;
    let mut bad = 0u64;
    let mut dir = tokio::fs::read_dir(&objects).await?;
    while let Some(shard) = dir.next_entry().await? {
        if !shard.file_type().await?.is_dir() {
            continue;
        }
        let mut sub = tokio::fs::read_dir(shard.path()).await?;
        while let Some(f) = sub.next_entry().await? {
            let path = f.path();
            let name = f.file_name().to_string_lossy().to_string();
            let Some(rest) = name.strip_suffix(".gguf") else {
                continue;
            };
            let prefix = shard.file_name().to_string_lossy().to_string();
            let digest_hex = format!("{prefix}{rest}");

            let expected = parse_digest(&digest_hex)?;
            let mut file = tokio::fs::File::open(&path).await?;
            let mut buf = vec![0u8; 64 * 1024];
            let mut hasher = arknet_crypto::hash::Sha256Stream::new();
            loop {
                let n = file.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            let actual = hasher.finalize();
            if actual == expected {
                ok += 1;
                println!("  OK  sha256:{digest_hex}");
            } else {
                bad += 1;
                println!(
                    "  BAD sha256:{digest_hex}  (actual {})",
                    hex::encode(actual.as_bytes())
                );
            }
        }
    }
    println!();
    println!("{ok} ok, {bad} corrupted");
    if bad > 0 {
        return Err(NodeError::Config(format!(
            "{bad} cache object(s) failed verification"
        )));
    }
    Ok(())
}

async fn run_bench(root: std::path::PathBuf, cfg: NodeConfig, args: BenchArgs) -> Result<()> {
    let model_ref = ModelRef::parse(&args.model_ref).map_err(NodeError::ModelRef)?;
    let manifest = build_manifest(&model_ref, &args.url, &args.sha256, args.size, &args.quant)?;
    let rt = open_runtime_with_manifest(root, cfg, &model_ref, manifest).await?;

    let load_started = Instant::now();
    let handle = rt.inference.load(&model_ref).await?;
    let load_ms = load_started.elapsed().as_secs_f64() * 1000.0;

    let gen_started = Instant::now();
    let mut stream = rt
        .inference
        .infer(
            &handle,
            InferenceRequest {
                prompt: args.prompt.clone(),
                max_tokens: args.tokens,
                mode: InferenceMode::Serving,
                sampling: SamplingParams::GREEDY,
                stop: Vec::new(),
            },
        )
        .await?;

    let mut tokens_seen = 0u32;
    let mut stop_reason = None;
    while let Some(ev) = stream.next().await {
        match ev? {
            InferenceEvent::Token(_) => {
                tokens_seen += 1;
            }
            InferenceEvent::Stop(r) => stop_reason = Some(r),
        }
    }
    let gen_secs = gen_started.elapsed().as_secs_f64();

    let tps = if gen_secs > 0.0 {
        tokens_seen as f64 / gen_secs
    } else {
        0.0
    };

    let reason = stop_reason
        .map(pretty_stop)
        .unwrap_or_else(|| "(stream ended without Stop)".into());
    println!();
    println!("bench results ({})", model_ref);
    println!("  load:           {load_ms:>7.1} ms");
    println!("  tokens:         {tokens_seen}");
    println!("  gen time:       {gen_secs:>7.3} s");
    println!("  throughput:     {tps:>7.1} tokens/s");
    println!("  stop reason:    {reason}");
    Ok(())
}

fn pretty_stop(r: StopReason) -> String {
    match r {
        StopReason::MaxTokens => "max_tokens".into(),
        StopReason::EndOfStream => "eos".into(),
        StopReason::StopString(s) => format!("stop-string {s:?}"),
        StopReason::Cancelled => "cancelled".into(),
    }
}

fn parse_digest(hex_s: &str) -> Result<Sha256Digest> {
    let bytes =
        hex::decode(hex_s).map_err(|e| NodeError::ModelRef(format!("bad sha256 hex: {e}")))?;
    if bytes.len() != 32 {
        return Err(NodeError::ModelRef(format!(
            "sha256 must be 32 bytes / 64 hex chars, got {}",
            bytes.len()
        )));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(Sha256Digest(arr))
}

/// Build a runtime with a MockRegistry that knows exactly one manifest.
async fn open_runtime_with_manifest(
    root: std::path::PathBuf,
    cfg: NodeConfig,
    model_ref: &ModelRef,
    manifest: ModelManifest,
) -> Result<NodeRuntime> {
    use std::collections::HashMap;

    let mut tbl = HashMap::new();
    tbl.insert(model_ref.to_string(), manifest);
    let registry = Arc::new(MockRegistry::from_manifests(tbl));

    let cache_cfg = arknet_model_manager::CacheConfig::with_root(paths::models_dir(&root));
    let model_manager = arknet_model_manager::ModelManager::open(cache_cfg, registry).await?;
    let inference_cfg = arknet_inference::InferenceConfig::default();
    let inference = arknet_inference::InferenceEngine::new(inference_cfg, model_manager.clone());

    Ok(NodeRuntime {
        cfg: Arc::new(cfg),
        metrics: crate::metrics::MetricsRegistry::install()?,
        model_manager,
        inference,
        data_dir: root,
    })
}
