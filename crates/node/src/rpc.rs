//! HTTP RPC endpoint — Phase 0 internal + OpenAI-compatible surface.
//!
//! Endpoints:
//! - `POST /v1/chat/completions` — OpenAI-compatible (streaming + non-streaming).
//! - `POST /v1/inference` — Phase 0 internal format (SSE InferenceEvent blobs).
//! - `GET  /v1/models` — list loaded model handles.
//! - `POST /v1/models/load` — load a model by manifest.
//! - `GET  /v1/candidates/:model` — compute-node discovery for SDK direct p2p.
//! - `GET  /health` — JSON `{ "status": "ok", ... }`.
//! - `GET  /v1/status` — chain status.
//! - `POST /v1/tx` — submit a signed transaction.
//!
//! Bound to `127.0.0.1` by default — no auth in Phase 0. Public
//! binding + wallet-session tokens come with Phase 4.

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
use axum::response::IntoResponse;
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
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(list_models))
        .route("/v1/models/load", post(load_model))
        .route("/v1/inference", post(infer))
        .route("/v1/status", get(status))
        .route("/v1/account/:address", get(get_account))
        .route("/v1/tx", post(submit_tx))
        .route("/v1/candidates/:model", get(list_candidates))
        .route("/v1/gateways", get(list_gateways))
        .route("/v1/block/:height", get(get_block))
        .route("/v1/tx/:hash", get(get_tx))
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
    quant: Option<String>,
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
        req.quant.as_deref(),
    )?;
    state.register_manifest(&model_ref, manifest.clone());

    let runtime = temp_runtime_with_manifest(state, &model_ref, manifest).await?;
    let handle = runtime.inference.load(&model_ref).await?;

    if let Some(network) = state.runtime.network.as_ref() {
        let model_refs: Vec<String> = state.manifests.lock().keys().cloned().collect();
        let operator = crate::compute_role::local_operator(&state.runtime.data_dir);
        let supports_tee = state.runtime.cfg.tee.enabled;
        crate::compute_role::announce_models(network, model_refs, operator, supports_tee).await;
    }

    Ok(LoadResponse {
        model_ref: model_ref.to_string(),
        digest_hex: hex::encode(handle.digest().as_bytes()),
        description: handle.description(),
    })
}

// ─── /v1/chat/completions (OpenAI-compatible) ───────────────────────

async fn chat_completions(
    State(state): State<RpcState>,
    Json(req): Json<arknet_rpc::openai::ChatCompletionRequest>,
) -> std::result::Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    use arknet_rpc::openai::*;

    if req.messages.is_empty() {
        let (status, body) = error_response(
            StatusCode::BAD_REQUEST,
            "messages array is empty",
            "invalid_request_error",
        );
        return Err((status, Json(serde_json::to_value(body.0).unwrap())));
    }

    if req.prefer_tee && !state.runtime.cfg.tee.enabled {
        let (status, body) = error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "prefer_tee requested but this node has no TEE capability. \
             Route through a TEE-capable gateway or remove prefer_tee.",
            "tee_unavailable",
        );
        return Err((status, Json(serde_json::to_value(body.0).unwrap())));
    }

    let model_ref = ModelRef::parse(&req.model).map_err(|e| {
        let (status, body) = error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid model: {e}"),
            "invalid_request_error",
        );
        (status, Json(serde_json::to_value(body.0).unwrap()))
    })?;

    // Local inference: load model on this node and run inference.
    let manifest = state.manifest_for(&model_ref).ok_or_else(|| {
        let (status, body) = error_response(
            StatusCode::NOT_FOUND,
            format!("model not loaded: {model_ref}. POST /v1/models/load first.",),
            "model_not_found",
        );
        (status, Json(serde_json::to_value(body.0).unwrap()))
    })?;

    let prompt = req
        .messages
        .iter()
        .map(|m| format!("{}: {}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n");

    let stop = req.stop.map(|s| s.into_vec()).unwrap_or_default();

    let runtime = temp_runtime_with_manifest(&state, &model_ref, manifest)
        .await
        .map_err(|e| {
            let (status, body) = error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string(),
                "server_error",
            );
            (status, Json(serde_json::to_value(body.0).unwrap()))
        })?;
    let handle = runtime.inference.load(&model_ref).await.map_err(|e| {
        let (status, body) = error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("model load failed: {e}"),
            "server_error",
        );
        (status, Json(serde_json::to_value(body.0).unwrap()))
    })?;

    let inf_stream = runtime
        .inference
        .infer(
            &handle,
            InferenceRequest {
                prompt,
                max_tokens: req.max_tokens,
                mode: InferenceMode::Serving,
                sampling: SamplingParams {
                    temperature: req.temperature as f32,
                    top_p: req.top_p as f32,
                    ..SamplingParams::default()
                },
                stop,
            },
        )
        .await
        .map_err(|e| {
            let (status, body) = error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string(),
                "server_error",
            );
            (status, Json(serde_json::to_value(body.0).unwrap()))
        })?;

    let request_id = gen_request_id();
    let model_name = req.model.clone();

    if req.stream {
        let mut first = true;
        let sse_stream = inf_stream.map(move |ev| {
            let chunk = match ev {
                Ok(ref event) => {
                    let (role, content, finish) = match event {
                        arknet_inference::InferenceEvent::Token(t) => {
                            let r = if first {
                                Some("assistant".into())
                            } else {
                                None
                            };
                            first = false;
                            (r, Some(t.text.clone()), None)
                        }
                        arknet_inference::InferenceEvent::Stop(reason) => {
                            let fr = match reason {
                                StopReason::MaxTokens => "length",
                                _ => "stop",
                            };
                            (None, None, Some(fr.to_string()))
                        }
                    };
                    ChatCompletionChunk {
                        id: request_id.clone(),
                        object: "chat.completion.chunk",
                        created: unix_now(),
                        model: model_name.clone(),
                        choices: vec![ChunkChoice {
                            index: 0,
                            delta: Delta { role, content },
                            finish_reason: finish,
                        }],
                    }
                }
                Err(ref e) => ChatCompletionChunk {
                    id: request_id.clone(),
                    object: "chat.completion.chunk",
                    created: unix_now(),
                    model: model_name.clone(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: Delta {
                            role: None,
                            content: Some(format!("[error: {e}]")),
                        },
                        finish_reason: Some("stop".into()),
                    }],
                },
            };
            let data = serde_json::to_string(&chunk).unwrap_or_default();
            Ok::<_, std::convert::Infallible>(Event::default().data(data))
        });

        Ok(Sse::new(sse_stream)
            .keep_alive(KeepAlive::default())
            .into_response())
    } else {
        use futures::TryStreamExt;
        let events: Vec<_> = inf_stream.try_collect().await.map_err(|e| {
            let (status, body) = error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string(),
                "server_error",
            );
            (status, Json(serde_json::to_value(body.0).unwrap()))
        })?;

        let mut text = String::new();
        let mut finish = "stop".to_string();
        let mut completion_tokens = 0u32;
        for ev in &events {
            match ev {
                arknet_inference::InferenceEvent::Token(t) => {
                    text.push_str(&t.text);
                    completion_tokens += 1;
                }
                arknet_inference::InferenceEvent::Stop(StopReason::MaxTokens) => {
                    finish = "length".into();
                }
                _ => {}
            }
        }

        let resp = ChatCompletionResponse {
            id: request_id,
            object: "chat.completion",
            created: unix_now(),
            model: model_name,
            choices: vec![Choice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".into(),
                    content: text,
                },
                finish_reason: Some(finish),
            }],
            usage: Usage {
                prompt_tokens: 0,
                completion_tokens,
                total_tokens: completion_tokens,
            },
        };
        Ok(Json(resp).into_response())
    }
}

// ─── /v1/inference (Phase 0 internal format) ────────────────────────

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
    quant_override: Option<&str>,
) -> Result<ModelManifest> {
    let url = Url::parse(url).map_err(|e| NodeError::ModelRef(format!("bad url: {e}")))?;
    let digest = parse_digest(sha256_hex)?;
    let quant = match quant_override {
        Some(s) => {
            GgufQuant::parse(s).ok_or_else(|| NodeError::ModelRef(format!("unknown quant: {s}")))?
        }
        None => model_ref.quant,
    };
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

// ─── /v1/account/:address ────────────────────────────────────────────

#[derive(Serialize)]
struct AccountResponse {
    address: String,
    balance: u128,
    nonce: u64,
}

async fn get_account(
    State(state): State<RpcState>,
    axum::extract::Path(address): axum::extract::Path<String>,
) -> std::result::Result<Json<AccountResponse>, (StatusCode, Json<ErrorBody>)> {
    let clean = address.strip_prefix("0x").unwrap_or(&address);
    let addr_bytes: [u8; 20] = hex::decode(clean)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorBody {
                    error: format!("bad address hex: {e}"),
                }),
            )
        })?
        .try_into()
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorBody {
                    error: "address must be 20 bytes (40 hex chars)".into(),
                }),
            )
        })?;

    let Some(consensus) = state.runtime.consensus.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorBody {
                error: "consensus not running — cannot query state".into(),
            }),
        ));
    };

    let addr = arknet_common::types::Address::new(addr_bytes);
    match consensus.get_account(&addr).await {
        Ok(Some(acct)) => Ok(Json(AccountResponse {
            address: format!("0x{clean}"),
            balance: acct.balance,
            nonce: acct.nonce,
        })),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorBody {
                error: format!("account 0x{clean} not found"),
            }),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                error: format!("state query failed: {e}"),
            }),
        )),
    }
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

// ─── /v1/candidates/:model ───────────────────────────────────────

/// One candidate compute node returned by the discovery endpoint.
#[derive(Serialize)]
struct CandidateEntry {
    /// Hex-encoded node id (or libp2p peer-id when available).
    peer_id: String,
    /// Known multiaddresses. Empty until the peer-book maps ids to addrs.
    multiaddrs: Vec<String>,
}

/// Response for `GET /v1/candidates/:model`.
#[derive(Serialize)]
struct CandidatesResponse {
    candidates: Vec<CandidateEntry>,
}

/// Return the peer ids of compute nodes serving a given model.
///
/// The SDK uses this to discover nodes and connect via p2p directly.
async fn list_candidates(
    State(state): State<RpcState>,
    axum::extract::Path(model): axum::extract::Path<String>,
) -> std::result::Result<Json<CandidatesResponse>, (StatusCode, Json<ErrorBody>)> {
    let router = state.runtime.router.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorBody {
                error: "no router role active on this node".into(),
            }),
        )
    })?;

    let now = arknet_router::failover::now_ms();
    let candidates = router.registry().eligible_for(&model, now);

    let entries: Vec<CandidateEntry> = candidates
        .into_iter()
        .map(|c| CandidateEntry {
            peer_id: format!("{}", c.node_id),
            // TODO(phase-2): resolve node_id → multiaddr from the
            // network peer-book once the mapping is available.
            multiaddrs: Vec::new(),
        })
        .collect();

    Ok(Json(CandidatesResponse {
        candidates: entries,
    }))
}

// ─── /v1/gateways ────────────────────────────────────────────────

#[derive(Serialize)]
struct GatewayListEntry {
    node_id: String,
    operator: String,
    url: String,
    https: bool,
    registered_at: u64,
}

#[derive(Serialize)]
struct GatewayListResponse {
    gateways: Vec<GatewayListEntry>,
}

async fn list_gateways(State(state): State<RpcState>) -> Json<GatewayListResponse> {
    let entries = state
        .runtime
        .consensus
        .as_ref()
        .and_then(|c| c.iter_gateways().ok())
        .unwrap_or_default();

    let gateways = entries
        .into_iter()
        .map(|e| GatewayListEntry {
            node_id: format!("{}", e.node_id),
            operator: format!("{}", e.operator),
            url: e.url,
            https: e.https,
            registered_at: e.registered_at,
        })
        .collect();

    Json(GatewayListResponse { gateways })
}

// ─── /v1/block/:height ──────────────────────────────────────────────

#[derive(Serialize)]
struct BlockResponse {
    height: u64,
    hash: String,
    parent_hash: String,
    state_root: String,
    tx_root: String,
    timestamp_ms: u64,
    tx_count: usize,
    base_fee: u128,
    proposer: String,
    genesis_message: String,
}

async fn get_block(
    State(state): State<RpcState>,
    axum::extract::Path(height): axum::extract::Path<u64>,
) -> std::result::Result<Json<BlockResponse>, (StatusCode, Json<ErrorBody>)> {
    let Some(consensus) = state.runtime.consensus.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorBody {
                error: "consensus engine not running on this node".into(),
            }),
        ));
    };
    match consensus.get_block(height) {
        Ok(Some(block)) => Ok(Json(BlockResponse {
            height: block.header.height,
            hash: hex::encode(block.hash().as_bytes()),
            parent_hash: hex::encode(block.header.parent_hash.as_bytes()),
            state_root: hex::encode(block.header.state_root.as_bytes()),
            tx_root: hex::encode(block.header.tx_root),
            timestamp_ms: block.header.timestamp_ms,
            tx_count: block.txs.len(),
            base_fee: block.header.base_fee,
            proposer: format!("{}", block.header.proposer),
            genesis_message: block.header.genesis_message.clone(),
        })),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorBody {
                error: format!("block at height {height} not found"),
            }),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody { error: e }),
        )),
    }
}

// ─── /v1/tx/:hash ───────────────────────────────────────────────────

#[derive(Serialize)]
struct TxLookupResponse {
    tx_hash: String,
    block_height: u64,
    #[serde(rename = "type")]
    tx_type: String,
    from: String,
}

async fn get_tx(
    State(state): State<RpcState>,
    axum::extract::Path(hash_hex): axum::extract::Path<String>,
) -> std::result::Result<Json<TxLookupResponse>, (StatusCode, Json<ErrorBody>)> {
    let Some(consensus) = state.runtime.consensus.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorBody {
                error: "consensus engine not running on this node".into(),
            }),
        ));
    };
    let clean = hash_hex.strip_prefix("0x").unwrap_or(&hash_hex);
    let hash_bytes: [u8; 32] = hex::decode(clean)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorBody {
                    error: format!("invalid tx hash hex: {e}"),
                }),
            )
        })?
        .try_into()
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorBody {
                    error: "tx hash must be 32 bytes (64 hex chars)".into(),
                }),
            )
        })?;

    let height = consensus.get_tx_height(&hash_bytes).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody { error: e }),
        )
    })?;

    let Some(height) = height else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorBody {
                error: format!("transaction 0x{clean} not found"),
            }),
        ));
    };

    let block = consensus.get_block(height).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody { error: e }),
        )
    })?;

    let (tx_type, from) = block
        .as_ref()
        .and_then(|b| {
            b.txs.iter().find_map(|tx| {
                if hex::encode(tx.hash().as_bytes()) == clean {
                    let t = classify_tx_type(&tx.tx);
                    let f = format!("{:?}", tx.signer);
                    Some((t, f))
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| ("unknown".into(), "unknown".into()));

    Ok(Json(TxLookupResponse {
        tx_hash: format!("0x{clean}"),
        block_height: height,
        tx_type,
        from,
    }))
}

fn classify_tx_type(tx: &arknet_chain::transactions::Transaction) -> String {
    use arknet_chain::transactions::Transaction;
    match tx {
        Transaction::Transfer { .. } => "Transfer",
        Transaction::StakeOp(_) => "StakeOp",
        Transaction::ReceiptBatch(_) => "ReceiptBatch",
        Transaction::RegisterModel { .. } => "RegisterModel",
        Transaction::GovProposal(_) => "GovProposal",
        Transaction::GovVote { .. } => "GovVote",
        Transaction::Dispute(_) => "Dispute",
        Transaction::EscrowLock { .. } => "EscrowLock",
        Transaction::EscrowSettle { .. } => "EscrowSettle",
        Transaction::RewardMint { .. } => "RewardMint",
        Transaction::RegisterTeeCapability { .. } => "RegisterTeeCapability",
        Transaction::RegisterGateway { .. } => "RegisterGateway",
        Transaction::UnregisterGateway { .. } => "UnregisterGateway",
    }
    .into()
}

// `StopReason` is referenced to keep it in scope for downstream
// crates that consume the SSE payloads — surface for later phases.
#[allow(dead_code)]
fn _keep_stop_reason_in_scope(_r: StopReason) {}
