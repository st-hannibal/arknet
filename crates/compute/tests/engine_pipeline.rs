//! Compute-side integration test: real inference engine, real tokens.
//!
//! Boots an [`InferenceEngine`] on the stories260K fixture (shared
//! with the inference crate's determinism test) and pushes a job
//! through [`ComputeJobRunner`]. Verifies:
//!
//! - Deterministic-mode run emits at least one token event.
//! - Stream ends with exactly one [`StopKind`] variant.
//! - Same request twice produces byte-identical token text.
//!
//! Skipped when the fixture is unavailable (CI / offline dev).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arknet_common::types::{JobId, PoolId, PubKey, Signature};
use arknet_compute::wire::{InferenceJobEvent, InferenceJobRequest, StopKind};
use arknet_compute::ComputeJobRunner;
use arknet_crypto::hash::{sha256, Sha256Digest};
use arknet_inference::{InferenceConfig, InferenceEngine};
use arknet_model_manager::{
    CacheConfig, GgufQuant, MockRegistry, ModelId, ModelManager, ModelManifest, ModelRef,
};
use futures::StreamExt;
use url::Url;

const STORIES260K_URL: &str =
    "https://huggingface.co/ggml-org/models/resolve/main/tinyllamas/stories260K.gguf";
const STORIES260K_SHA256: &str = "270cba1bd5109f42d03350f60406024560464db173c0e387d91f0426d3bd256d";

fn fixture_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("STORIES260K_FIXTURE_PATH") {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    let default = std::env::temp_dir()
        .join("arknet-test-fixtures")
        .join("stories260K.gguf");
    if default.exists() {
        return Some(default);
    }
    std::fs::create_dir_all(default.parent()?).ok()?;
    let status = std::process::Command::new("curl")
        .args(["-sL", "--fail", "-o", default.to_str()?, STORIES260K_URL])
        .status()
        .ok()?;
    if status.success() && verify_fixture(&default) {
        Some(default)
    } else {
        None
    }
}

fn verify_fixture(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    hex::encode(sha256(&bytes).as_bytes()) == STORIES260K_SHA256
}

fn parse_digest(hex_s: &str) -> Sha256Digest {
    let bytes = hex::decode(hex_s).unwrap();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Sha256Digest(arr)
}

async fn build_engine() -> Option<(InferenceEngine, ModelRef)> {
    let path = fixture_path()?;
    let digest = parse_digest(STORIES260K_SHA256);
    let model_ref = ModelRef::parse("test-org/Stories260K-F32").ok()?;
    let size = std::fs::metadata(&path).ok()?.len();

    let manifest = ModelManifest {
        id: ModelId([0u8; 32]),
        model_ref: model_ref.clone(),
        mirrors: vec![Url::from_file_path(&path).ok()?],
        sha256: digest,
        size_bytes: size,
        quant: GgufQuant::F32,
        license: "mit".into(),
    };
    let mut tbl = HashMap::new();
    tbl.insert(model_ref.to_string(), manifest);
    let registry = Arc::new(MockRegistry::from_manifests(tbl));

    // Pre-seed the cache with the fixture.
    let cache_root = tempfile::tempdir().ok()?;
    let digest_hex = hex::encode(digest.as_bytes());
    let (prefix, rest) = digest_hex.split_at(2);
    let target_dir = cache_root.path().join("objects").join(prefix);
    std::fs::create_dir_all(&target_dir).ok()?;
    let target_file = target_dir.join(format!("{rest}.gguf"));
    std::fs::copy(&path, &target_file).ok()?;

    let cfg = CacheConfig::with_root(cache_root.path().to_path_buf()).with_max_bytes(1 << 30);
    let mm = ModelManager::open(cfg, registry).await.ok()?;
    // Leak the tempdir so it outlives the engine.
    std::mem::forget(cache_root);

    let engine = InferenceEngine::new(
        InferenceConfig {
            max_context_tokens: 512,
            serving_threads: 1,
        },
        mm,
    );
    Some((engine, model_ref))
}

fn request_for(prompt: &str, nonce: u64) -> InferenceJobRequest {
    InferenceJobRequest {
        model_ref: "test-org/Stories260K-F32".into(),
        model_hash: [0; 32],
        prompt: prompt.into(),
        max_tokens: 8,
        seed: 0,
        deterministic: true,
        stop_strings: vec![],
        nonce,
        timestamp_ms: 1_000,
        user_pubkey: PubKey::ed25519([0xab; 32]),
        signature: Signature::ed25519([0; 64]),
    }
}

async fn collect_text(
    runner: &ComputeJobRunner,
    model_ref: &ModelRef,
    req: InferenceJobRequest,
    job_id: JobId,
    now_ms: u64,
) -> (String, Option<StopKind>) {
    let stream = runner
        .run(req, model_ref, PoolId::new([0; 16]), job_id, now_ms)
        .await
        .expect("job runs");
    let mut stream = std::pin::pin!(stream);
    let mut text = String::new();
    let mut stop = None;
    while let Some(ev) = stream.next().await {
        match ev {
            InferenceJobEvent::Token { text: t, .. } => text.push_str(&t),
            InferenceJobEvent::Stop { reason, .. } => {
                stop = Some(reason);
                break;
            }
            InferenceJobEvent::Error { message, .. } => {
                panic!("unexpected inflight error: {message}");
            }
        }
    }
    (text, stop)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compute_runner_streams_deterministic_tokens() {
    let Some((engine, model_ref)) = build_engine().await else {
        eprintln!("fixture unavailable; skipping");
        return;
    };
    let runner = ComputeJobRunner::new(engine);

    let req_a = request_for("Once upon a time", 1);
    let req_b = request_for("Once upon a time", 2);

    let (text_a, stop_a) =
        collect_text(&runner, &model_ref, req_a, JobId::new([1; 32]), 1_000).await;
    let (text_b, stop_b) =
        collect_text(&runner, &model_ref, req_b, JobId::new([2; 32]), 1_000).await;

    assert!(!text_a.is_empty(), "expected tokens, got empty text");
    assert!(stop_a.is_some());
    assert!(stop_b.is_some());
    assert_eq!(
        text_a, text_b,
        "deterministic mode must produce identical token text across runs"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compute_runner_rejects_replayed_nonce() {
    let Some((engine, model_ref)) = build_engine().await else {
        eprintln!("fixture unavailable; skipping");
        return;
    };
    let runner = ComputeJobRunner::new(engine);
    let req = request_for("once", 42);

    // First call: ok.
    let stream = runner
        .run(
            req.clone(),
            &model_ref,
            PoolId::new([0; 16]),
            JobId::new([1; 32]),
            1_000,
        )
        .await
        .expect("first call ok");
    // Drain so the decode thread actually finishes.
    let mut stream = std::pin::pin!(stream);
    while stream.next().await.is_some() {}

    // Second call with the same (addr, nonce): replayed.
    let result = runner
        .run(
            req,
            &model_ref,
            PoolId::new([0; 16]),
            JobId::new([2; 32]),
            1_000,
        )
        .await;
    match result {
        Ok(_) => panic!("replayed nonce must fail"),
        Err(e) => {
            assert!(matches!(e, arknet_compute::ComputeError::BadRequest(_)));
        }
    }
}
