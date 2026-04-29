//! Phase-0 HTTP RPC endpoint.
//!
//! Deliberately minimal and **not** OpenAI-compatible. OpenAI-compat
//! with streaming SSE / function-calling / tool use is Phase 2 scope,
//! and I don't want to lock the wire format now only to break it later
//! for external integrators.
//!
//! Endpoints:
//! - `POST /v1/inference` — body: InferRequest JSON.
//!   Response: SSE stream, each event is an InferenceEvent JSON
//!   blob. Terminates on Stop or channel close.
//! - `GET  /v1/models` — list loaded model handles (from the engine cache).
//! - `POST /v1/models/load` — body: LoadRequest, returns LoadResponse.
//!   Phase 0 uses the same manifest flag surface as `arknet model load`.
//! - `GET  /health` — JSON `{ "status": "ok", ... }`.
//!
//! Bound to `127.0.0.1` by default — no auth in Phase 0. Public
//! binding + wallet-session tokens come with the real API in Phase 2.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use arknet_crypto::hash::Sha256Digest;
use arknet_inference::{InferenceMode, InferenceRequest, SamplingParams, StopReason};
use arknet_model_manager::{GgufQuant, MockRegistry, ModelId, ModelManifest, ModelRef};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{Stream, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use url::Url;

use crate::errors::{NodeError, Result};
use crate::runtime::NodeRuntime;

/// Shared HTTP state: one runtime per process; `load` mutates a small
/// manifest map so subsequent inference requests can be driven by ref
/// alone.
#[derive(Clone)]
pub struct RpcState {
    runtime: NodeRuntime,
    /// In-process ref → manifest map. The node's top-level runtime is
    /// built with an empty MockRegistry because that's resolved once
    /// at open time. To let `/v1/models/load` add entries after
    /// startup, we keep a parallel map here and rebuild a one-off
    /// runtime per request. Not scalable; Phase 1's on-chain registry
    /// replaces this entirely.
    manifests: Arc<Mutex<HashMap<String, ModelManifest>>>,
    started: Instant,
}

impl RpcState {
    pub fn new(runtime: NodeRuntime) -> Self {
        Self {
            runtime,
            manifests: Arc::new(Mutex::new(HashMap::new())),
            started: Instant::now(),
        }
    }

    fn register_manifest(&self, model_ref: &ModelRef, manifest: ModelManifest) {
        self.manifests
            .lock()
            .insert(model_ref.to_string(), manifest);
    }

    fn manifest_for(&self, model_ref: &ModelRef) -> Option<ModelManifest> {
        self.manifests.lock().get(&model_ref.to_string()).cloned()
    }
}

/// Start the RPC server and run it until `shutdown` fires.
pub async fn serve(bind: SocketAddr, state: RpcState, shutdown: CancellationToken) -> Result<()> {
    let listener = TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    info!(%addr, "rpc: HTTP endpoint bound");

    let app = Router::new()
        .route("/health", get(health))
        .route("/peers", get(list_peers))
        .route("/v1/models", get(list_models))
        .route("/v1/models/load", post(load_model))
        .route("/v1/inference", post(infer))
        .route("/v1/status", get(status))
        .route("/v1/tx", post(submit_tx))
        .with_state(state);

    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown.cancelled().await;
    });

    if let Err(e) = server.await {
        warn!(error = %e, "rpc server exited with error");
        return Err(NodeError::Config(format!("rpc server: {e}")));
    }
    info!("rpc: server stopped cleanly");
    Ok(())
}

// ─── /health ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    uptime_seconds: f64,
    version: &'static str,
}

async fn health(State(state): State<RpcState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        uptime_seconds: state.started.elapsed().as_secs_f64(),
        version: env!("CARGO_PKG_VERSION"),
    })
}

// ─── /peers ──────────────────────────────────────────────────────────

#[derive(Serialize)]
struct PeersResponse {
    /// Our own libp2p peer id (helpful when wiring up a test cluster).
    local_peer_id: Option<String>,
    /// Currently-connected peer ids.
    connected: Vec<String>,
}

async fn list_peers(State(state): State<RpcState>) -> Json<PeersResponse> {
    let Some(net) = state.runtime.network.as_ref() else {
        return Json(PeersResponse {
            local_peer_id: None,
            connected: Vec::new(),
        });
    };
    let connected = match net.connected_peers().await {
        Ok(peers) => peers.into_iter().map(|p| p.to_string()).collect(),
        Err(e) => {
            warn!(error = %e, "failed to query connected peers");
            Vec::new()
        }
    };
    Json(PeersResponse {
        local_peer_id: Some(net.local_peer_id().to_string()),
        connected,
    })
}

// ─── /v1/models ──────────────────────────────────────────────────────

#[derive(Serialize)]
struct ListResponse {
    models: Vec<ListedModel>,
}

#[derive(Serialize)]
struct ListedModel {
    model_ref: String,
    digest_hex: String,
}

async fn list_models(State(state): State<RpcState>) -> Json<ListResponse> {
    let manifests = state.manifests.lock();
    let models = manifests
        .iter()
        .map(|(k, v)| ListedModel {
            model_ref: k.clone(),
            digest_hex: hex::encode(v.sha256.as_bytes()),
        })
        .collect();
    Json(ListResponse { models })
}

// ─── /v1/models/load ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct LoadRequest {
    model_ref: String,
    url: String,
    sha256: String,
    size_bytes: u64,
    #[serde(default = "default_quant")]
    quant: String,
}

fn default_quant() -> String {
    "F32".into()
}

#[derive(Serialize)]
struct LoadResponse {
    model_ref: String,
    digest_hex: String,
    description: String,
}

async fn load_model(
    State(state): State<RpcState>,
    Json(req): Json<LoadRequest>,
) -> std::result::Result<Json<LoadResponse>, (StatusCode, Json<ErrorBody>)> {
    match do_load(&state, req).await {
        Ok(r) => Ok(Json(r)),
        Err(e) => Err(http_error(e)),
    }
}

async fn do_load(state: &RpcState, req: LoadRequest) -> Result<LoadResponse> {
    let model_ref = ModelRef::parse(&req.model_ref).map_err(NodeError::ModelRef)?;
    let manifest = build_manifest(
        &model_ref,
        &req.url,
        &req.sha256,
        req.size_bytes,
        &req.quant,
    )?;
    state.register_manifest(&model_ref, manifest.clone());

    let runtime = temp_runtime_with_manifest(state, &model_ref, manifest).await?;
    let handle = runtime.inference.load(&model_ref).await?;
    Ok(LoadResponse {
        model_ref: model_ref.to_string(),
        digest_hex: hex::encode(handle.digest().as_bytes()),
        description: handle.description(),
    })
}

// ─── /v1/inference ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct InferRequestBody {
    model_ref: String,
    prompt: String,
    #[serde(default = "default_max_tokens")]
    max_tokens: u32,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    stop: Vec<String>,
}

fn default_max_tokens() -> u32 {
    64
}
fn default_mode() -> String {
    "serving".into()
}

async fn infer(
    State(state): State<RpcState>,
    Json(req): Json<InferRequestBody>,
) -> std::result::Result<
    Sse<impl Stream<Item = std::result::Result<Event, std::convert::Infallible>>>,
    (StatusCode, Json<ErrorBody>),
> {
    let model_ref =
        ModelRef::parse(&req.model_ref).map_err(|e| http_error(NodeError::ModelRef(e)))?;

    let manifest = state.manifest_for(&model_ref).ok_or_else(|| {
        http_error(NodeError::ModelRef(format!(
            "model not loaded: {}. POST /v1/models/load first.",
            model_ref
        )))
    })?;

    let mode = match req.mode.as_str() {
        "serving" => InferenceMode::Serving,
        "deterministic" => InferenceMode::Deterministic,
        other => {
            return Err(http_error(NodeError::ModelRef(format!(
                "unknown mode: {other}. Use `serving` or `deterministic`."
            ))));
        }
    };

    let runtime = temp_runtime_with_manifest(&state, &model_ref, manifest)
        .await
        .map_err(http_error)?;
    let handle = runtime.inference.load(&model_ref).await.map_err(|e| {
        http_error(NodeError::from(
            arknet_inference::InferenceError::ModelLoad(e.to_string()),
        ))
    })?;

    let stream = runtime
        .inference
        .infer(
            &handle,
            InferenceRequest {
                prompt: req.prompt,
                max_tokens: req.max_tokens,
                mode,
                sampling: if mode == InferenceMode::Deterministic {
                    SamplingParams::GREEDY
                } else {
                    SamplingParams::default()
                },
                stop: req.stop,
            },
        )
        .await
        .map_err(|e| http_error(NodeError::from(e)))?;

    let sse_stream = stream.map(|ev| match ev {
        Ok(event) => {
            let data = serde_json::to_string(&event)
                .unwrap_or_else(|e| format!("{{\"error\":\"serialize: {e}\"}}"));
            Ok(Event::default().event("inference").data(data))
        }
        Err(e) => {
            // Send a terminal error as the final SSE event.
            Ok(Event::default().event("error").data(format!(
                "{{\"error\":{}}}",
                serde_json::to_string(&e.to_string()).unwrap()
            )))
        }
    });

    Ok(Sse::new(sse_stream).keep_alive(KeepAlive::default()))
}

// ─── Internals ───────────────────────────────────────────────────────

/// Build a one-off runtime with a single registered model. Phase 1
/// removes this indirection.
async fn temp_runtime_with_manifest(
    state: &RpcState,
    model_ref: &ModelRef,
    manifest: ModelManifest,
) -> Result<NodeRuntime> {
    let mut tbl = HashMap::new();
    tbl.insert(model_ref.to_string(), manifest);
    let registry = Arc::new(MockRegistry::from_manifests(tbl));

    let cache_cfg = arknet_model_manager::CacheConfig::with_root(crate::paths::models_dir(
        &state.runtime.data_dir,
    ));
    let model_manager = arknet_model_manager::ModelManager::open(cache_cfg, registry).await?;
    let inference = arknet_inference::InferenceEngine::new(
        arknet_inference::InferenceConfig::default(),
        model_manager.clone(),
    );

    Ok(NodeRuntime {
        cfg: state.runtime.cfg.clone(),
        metrics: state.runtime.metrics.clone(),
        model_manager,
        inference,
        data_dir: state.runtime.data_dir.clone(),
        network: state.runtime.network.clone(),
        consensus: state.runtime.consensus.clone(),
        router: state.runtime.router.clone(),
        compute: state.runtime.compute.clone(),
    })
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

// ─── Errors ──────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

fn http_error(e: NodeError) -> (StatusCode, Json<ErrorBody>) {
    let status = match &e {
        NodeError::ModelRef(_) => StatusCode::BAD_REQUEST,
        NodeError::NotImplemented(_) | NodeError::RoleNotImplemented(_) => {
            StatusCode::NOT_IMPLEMENTED
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error!(error = %e, ?status, "rpc error");
    (
        status,
        Json(ErrorBody {
            error: e.to_string(),
        }),
    )
}

// ─── /v1/status ──────────────────────────────────────────────────────

#[derive(Serialize)]
struct StatusResponse {
    chain_id: String,
    height: Option<u64>,
    peer_count: Option<usize>,
    consensus_running: bool,
}

async fn status(State(state): State<RpcState>) -> Json<StatusResponse> {
    let chain_id = state.runtime.cfg.node.network.clone();
    let height = match state.runtime.consensus.as_ref() {
        Some(h) => h.current_height().await.ok().map(|h| h.0),
        None => None,
    };
    let peer_count = match state.runtime.network.as_ref() {
        Some(n) => n.connected_peers().await.ok().map(|p| p.len()),
        None => None,
    };
    Json(StatusResponse {
        chain_id,
        height,
        peer_count,
        consensus_running: state.runtime.consensus.is_some(),
    })
}

// ─── /v1/tx ──────────────────────────────────────────────────────────

/// Raw request body: hex-encoded borsh bytes of a
/// [`arknet_chain::SignedTransaction`]. Keeping it hex-encoded on the
/// wire lets operators paste signed blobs from the CLI; a binary
/// endpoint ships in Phase 2 alongside the full OpenAI-compat surface.
#[derive(Deserialize)]
struct SubmitTxRequest {
    tx_hex: String,
}

#[derive(Serialize)]
struct SubmitTxResponse {
    tx_hash_hex: String,
}

async fn submit_tx(
    State(state): State<RpcState>,
    Json(req): Json<SubmitTxRequest>,
) -> std::result::Result<Json<SubmitTxResponse>, (StatusCode, Json<ErrorBody>)> {
    let Some(consensus) = state.runtime.consensus.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorBody {
                error: "consensus engine not running on this node".into(),
            }),
        ));
    };
    let clean = req.tx_hex.strip_prefix("0x").unwrap_or(&req.tx_hex);
    let bytes = hex::decode(clean).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: format!("tx_hex: {e}"),
            }),
        )
    })?;
    let tx: arknet_chain::SignedTransaction = borsh::from_slice(&bytes).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: format!("tx decode: {e}"),
            }),
        )
    })?;
    match consensus.submit_tx(tx).await {
        Ok(hash) => Ok(Json(SubmitTxResponse {
            tx_hash_hex: hex::encode(hash.as_bytes()),
        })),
        Err(msg) => Err((StatusCode::BAD_REQUEST, Json(ErrorBody { error: msg }))),
    }
}

// `StopReason` is referenced to keep it in scope for downstream
// crates that consume the SSE payloads — surface for later phases.
#[allow(dead_code)]
fn _keep_stop_reason_in_scope(_r: StopReason) {}
