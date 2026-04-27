//! End-to-end integration test for the model manager.
//!
//! Spins a local axum server on an OS-chosen port, serves a synthetic
//! GGUF blob with `Range:` support, and drives `ModelManager::ensure_local`
//! through the full pipeline: resolve → pull → verify → cache → reuse.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use arknet_crypto::hash::sha256;
use arknet_model_manager::{
    CacheConfig, GgufQuant, MockRegistry, ModelId, ModelManager, ModelManifest, ModelRef,
};
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use url::Url;

/// Minimal but valid GGUF v3 body, with `general.file_type = 15` (Q4_K_M).
fn synthetic_gguf() -> Vec<u8> {
    // Using the crate-private Builder would require re-exporting it; replicate
    // the layout manually here since this test lives outside the crate.
    let mut buf = Vec::new();
    buf.extend_from_slice(b"GGUF");
    buf.extend_from_slice(&3u32.to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
    let mut meta = Vec::new();
    // general.architecture = "llama"
    write_string(&mut meta, "general.architecture");
    meta.extend_from_slice(&8u32.to_le_bytes()); // GGUF_TYPE_STRING
    write_string(&mut meta, "llama");
    // general.file_type = 15 (Q4_K_M)
    write_string(&mut meta, "general.file_type");
    meta.extend_from_slice(&4u32.to_le_bytes()); // GGUF_TYPE_UINT32
    meta.extend_from_slice(&15u32.to_le_bytes());
    buf.extend_from_slice(&2u64.to_le_bytes()); // metadata_count
    buf.extend_from_slice(&meta);

    // Pad to a reasonable size so `Range:` resumption is interesting.
    buf.extend(std::iter::repeat_n(0xCDu8, 4096));
    buf
}

fn write_string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u64).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

#[derive(Clone)]
struct ServerState {
    body: Arc<Vec<u8>>,
    request_count: Arc<AtomicU32>,
}

async fn serve(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    state.request_count.fetch_add(1, Ordering::SeqCst);

    if let Some(v) = headers.get(header::RANGE) {
        let v = v.to_str().unwrap_or("");
        if let Some(stripped) = v.strip_prefix("bytes=") {
            if let Some((start, end)) = stripped.split_once('-') {
                let start: usize = start.parse().unwrap_or(0);
                let end_exclusive = if end.is_empty() {
                    state.body.len()
                } else {
                    end.parse::<usize>()
                        .map(|e| e + 1)
                        .unwrap_or(state.body.len())
                };
                if start < state.body.len() && end_exclusive <= state.body.len() {
                    let slice = state.body[start..end_exclusive].to_vec();
                    let total = state.body.len();
                    return Response::builder()
                        .status(StatusCode::PARTIAL_CONTENT)
                        .header(
                            header::CONTENT_RANGE,
                            format!("bytes {}-{}/{}", start, end_exclusive - 1, total),
                        )
                        .header(header::CONTENT_LENGTH, slice.len())
                        .body(Body::from(slice))
                        .unwrap();
                }
            }
        }
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_LENGTH, state.body.len())
        .body(Body::from((*state.body).clone()))
        .unwrap()
}

async fn start_server(
    body: Arc<Vec<u8>>,
) -> (SocketAddr, Arc<AtomicU32>, tokio::task::JoinHandle<()>) {
    let count = Arc::new(AtomicU32::new(0));
    let state = ServerState {
        body,
        request_count: count.clone(),
    };
    let app = Router::new()
        .route("/model.gguf", get(serve))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, count, handle)
}

#[tokio::test]
async fn end_to_end_pull_verify_cache_reuse() {
    let body = Arc::new(synthetic_gguf());
    let (addr, count, _h) = start_server(body.clone()).await;

    let model_ref = ModelRef::parse("test-org/TinyModel-Q4_K_M").unwrap();
    let url = Url::parse(&format!("http://{addr}/model.gguf")).unwrap();
    let manifest = ModelManifest {
        id: ModelId([0u8; 32]),
        model_ref: model_ref.clone(),
        mirrors: vec![url],
        sha256: sha256(&body),
        size_bytes: body.len() as u64,
        quant: GgufQuant::Q4KM,
        license: "apache-2.0".into(),
    };

    let mut tbl = HashMap::new();
    tbl.insert(model_ref.to_string(), manifest.clone());
    let registry = Arc::new(MockRegistry::from_manifests(tbl));

    let dir = tempfile::tempdir().unwrap();
    let cfg = CacheConfig::with_root(dir.path().to_path_buf()).with_max_bytes(1 << 20);
    let mgr = ModelManager::open(cfg, registry).await.unwrap();

    // First call: cache miss, one network request.
    let first = mgr.ensure_local(&model_ref).await.unwrap();
    assert!(first.path().exists());
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // Second call: cache hit, zero additional network requests.
    let second = mgr.ensure_local(&model_ref).await.unwrap();
    assert_eq!(first.path(), second.path());
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn corrupted_download_errors_and_leaves_no_cache_entry() {
    let body = Arc::new(synthetic_gguf());
    let (addr, _count, _h) = start_server(body.clone()).await;

    // Manifest advertises a bogus digest; the streaming hasher will
    // reject the download.
    let model_ref = ModelRef::parse("test-org/TinyModel-Q4_K_M").unwrap();
    let url = Url::parse(&format!("http://{addr}/model.gguf")).unwrap();
    let bogus_digest = sha256(b"not the body");
    let manifest = ModelManifest {
        id: ModelId([0u8; 32]),
        model_ref: model_ref.clone(),
        mirrors: vec![url],
        sha256: bogus_digest,
        size_bytes: body.len() as u64,
        quant: GgufQuant::Q4KM,
        license: "apache-2.0".into(),
    };

    let mut tbl = HashMap::new();
    tbl.insert(model_ref.to_string(), manifest.clone());
    let registry = Arc::new(MockRegistry::from_manifests(tbl));

    let dir = tempfile::tempdir().unwrap();
    let cfg = CacheConfig::with_root(dir.path().to_path_buf()).with_max_bytes(1 << 20);
    let mgr = ModelManager::open(cfg, registry).await.unwrap();

    let err = mgr.ensure_local(&model_ref).await.unwrap_err();
    assert!(
        matches!(err, arknet_model_manager::ModelError::HashMismatch { .. }),
        "expected HashMismatch, got {err:?}"
    );
    assert_eq!(mgr.cache().len(), 0);
}

#[tokio::test]
async fn quant_mismatch_evicts_and_errors() {
    let body = Arc::new(synthetic_gguf()); // header says file_type = 15 (Q4_K_M)
    let (addr, _count, _h) = start_server(body.clone()).await;

    // Manifest claims F16 (file_type = 1) but the body says Q4_K_M.
    let model_ref = ModelRef::parse("test-org/TinyModel-F16").unwrap();
    let url = Url::parse(&format!("http://{addr}/model.gguf")).unwrap();
    let manifest = ModelManifest {
        id: ModelId([0u8; 32]),
        model_ref: model_ref.clone(),
        mirrors: vec![url],
        sha256: sha256(&body),
        size_bytes: body.len() as u64,
        quant: GgufQuant::F16,
        license: "apache-2.0".into(),
    };

    let mut tbl = HashMap::new();
    tbl.insert(model_ref.to_string(), manifest.clone());
    let registry = Arc::new(MockRegistry::from_manifests(tbl));

    let dir = tempfile::tempdir().unwrap();
    let cfg = CacheConfig::with_root(dir.path().to_path_buf()).with_max_bytes(1 << 20);
    let mgr = ModelManager::open(cfg, registry).await.unwrap();

    let err = mgr.ensure_local(&model_ref).await.unwrap_err();
    assert!(
        matches!(err, arknet_model_manager::ModelError::Gguf(ref m) if m.contains("quant mismatch")),
        "expected Gguf quant mismatch, got {err:?}"
    );
    assert_eq!(mgr.cache().len(), 0);
}
