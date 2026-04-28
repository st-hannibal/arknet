//! Async-facing engine integration test.
//!
//! Exercises `InferenceEngine::load` + `InferenceEngine::infer`
//! through a real `tokio` runtime, with streaming events consumed via
//! `futures::Stream`. Complements `tests/determinism.rs`, which
//! exercises only the synchronous `Session`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arknet_crypto::hash::{sha256, Sha256Digest};
use arknet_inference::{
    InferenceConfig, InferenceEngine, InferenceEvent, InferenceMode, InferenceRequest,
    SamplingParams, StopReason,
};
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
    // Use the platform temp dir so Windows resolves to an absolute path
    // with a drive letter (`Url::from_file_path` rejects relative paths).
    let default = std::env::temp_dir()
        .join("arknet-test-fixtures")
        .join("stories260K.gguf");
    if default.exists() {
        return Some(default);
    }
    // Attempt one fetch.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_loads_and_streams_deterministic_tokens() {
    let Some(path) = fixture_path() else {
        eprintln!("fixture unavailable; skipping");
        return;
    };

    // Build a mock model manager that resolves a ref to the already-on-disk
    // fixture. Using file:// URLs means the puller is never actually
    // invoked — the model-manager's cache check sees the file already
    // under the content-addressed digest.
    let digest = parse_digest(STORIES260K_SHA256);
    let model_ref = ModelRef::parse("test-org/Stories260K-F32").unwrap();
    let size = std::fs::metadata(&path).unwrap().len();

    let manifest = ModelManifest {
        id: ModelId([0u8; 32]),
        model_ref: model_ref.clone(),
        // Use a file:// mirror so the integration test can stay offline
        // if the fixture is already on disk.
        mirrors: vec![Url::from_file_path(&path).unwrap()],
        sha256: digest,
        size_bytes: size,
        quant: GgufQuant::F32,
        license: "mit".into(),
    };

    let mut tbl = HashMap::new();
    tbl.insert(model_ref.to_string(), manifest);
    let registry = Arc::new(MockRegistry::from_manifests(tbl));

    // Pre-seed the model-manager cache with the fixture file so the
    // puller is never called (file:// URLs aren't supported by reqwest
    // by default). Content-addressed layout: objects/<aa>/<bb...>.gguf
    let cache_root = tempfile::tempdir().unwrap();
    let digest_hex = hex::encode(digest.as_bytes());
    let (prefix, rest) = digest_hex.split_at(2);
    let target_dir = cache_root.path().join("objects").join(prefix);
    std::fs::create_dir_all(&target_dir).unwrap();
    let target_file = target_dir.join(format!("{rest}.gguf"));
    std::fs::copy(&path, &target_file).unwrap();

    let cfg = CacheConfig::with_root(cache_root.path().to_path_buf()).with_max_bytes(1 << 30);
    let mm = ModelManager::open(cfg, registry).await.unwrap();

    let engine = InferenceEngine::new(
        InferenceConfig {
            max_context_tokens: 512,
            serving_threads: 1,
        },
        mm,
    );

    let handle = engine.load(&model_ref).await.expect("engine load");
    let _desc = handle.description();

    let mut stream = engine
        .infer(
            &handle,
            InferenceRequest {
                prompt: "Once upon a time".into(),
                max_tokens: 16,
                mode: InferenceMode::Deterministic,
                sampling: SamplingParams::GREEDY,
                stop: Vec::new(),
            },
        )
        .await
        .expect("infer kicked off");

    let mut tokens: Vec<i32> = Vec::new();
    let mut text = String::new();
    let mut stop: Option<StopReason> = None;

    while let Some(event) = stream.next().await {
        match event.expect("no error") {
            InferenceEvent::Token(t) => {
                tokens.push(t.token_id);
                text.push_str(&t.text);
            }
            InferenceEvent::Stop(r) => stop = Some(r),
        }
    }

    assert!(!tokens.is_empty(), "expected non-empty generation");
    assert!(stop.is_some(), "expected a Stop event");
    eprintln!("engine streamed: {text:?}");
}
