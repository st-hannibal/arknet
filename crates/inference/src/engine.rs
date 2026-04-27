//! `InferenceEngine` — public facade.
//!
//! Every downstream role (compute, verifier, RPC) enters inference
//! through this type. It:
//!
//! 1. Asks the model manager to ensure the model is local + verified.
//! 2. Loads the llama.cpp model with appropriate params.
//! 3. Caches loaded models by their on-disk digest so repeat requests
//!    are free.
//! 4. Spawns decode work onto `tokio::task::spawn_blocking` and streams
//!    events back via a bounded channel.

use std::collections::HashMap;
use std::sync::Arc;

use arknet_crypto::hash::Sha256Digest;
use arknet_model_manager::{ModelManager, ModelRef};
use futures::Stream;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::PollSender;
use tracing::{debug, info};

use crate::config::{InferenceConfig, InferenceMode, SamplingParams};
use crate::context::{Context, ContextParams};
use crate::errors::{InferenceError, Result};
use crate::events::InferenceEvent;
use crate::model::{Model, ModelLoadParams};
use crate::sampling::Sampler;
use crate::session::{EventSink, Session, SessionRequest};
use crate::tokenizer::Tokenizer;

/// Shared handle to a loaded model.
///
/// `ModelHandle` is cheap to clone and can be held across await points.
/// The underlying [`Model`] is created once and kept in the engine's
/// cache until the engine is dropped.
#[derive(Clone)]
pub struct ModelHandle {
    inner: Arc<LoadedModel>,
}

struct LoadedModel {
    /// On-disk digest — the stable identity we key the cache on.
    digest: Sha256Digest,
    model: Model,
}

impl ModelHandle {
    /// SHA-256 of the on-disk model bytes. Stable identity.
    pub fn digest(&self) -> Sha256Digest {
        self.inner.digest
    }

    /// Description reported by llama.cpp.
    pub fn description(&self) -> String {
        self.inner.model.description()
    }
}

/// Public inference entry point.
///
/// Cheap to clone — the inner state is reference-counted.
#[derive(Clone)]
pub struct InferenceEngine {
    cfg: InferenceConfig,
    mm: ModelManager,
    cache: Arc<Mutex<HashMap<Sha256Digest, Arc<LoadedModel>>>>,
}

impl InferenceEngine {
    /// Build a new engine. The model manager is held for the lifetime
    /// of the engine.
    pub fn new(cfg: InferenceConfig, mm: ModelManager) -> Self {
        crate::backend::init_once();
        Self {
            cfg,
            mm,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Ensure the model is local (via the model manager), load it
    /// through llama.cpp, and return a handle.
    ///
    /// Cache semantics: if a model with the same on-disk digest has
    /// already been loaded in this engine, the cached handle is
    /// returned without re-loading.
    pub async fn load(&self, r: &ModelRef) -> Result<ModelHandle> {
        let sandbox = self.mm.ensure_local(r).await?;
        let path = sandbox.path().to_path_buf();
        let digest = file_digest(&path).await?;

        if let Some(existing) = self.cache.lock().get(&digest).cloned() {
            debug!(model=%r, "engine cache hit");
            return Ok(ModelHandle { inner: existing });
        }

        info!(model=%r, path=%path.display(), "loading model into llama.cpp");

        // Loading is CPU-heavy (multi-GB reads + mmap setup); push to a
        // blocking thread so the async runtime isn't stalled.
        let model = tokio::task::spawn_blocking(move || {
            Model::load_from_file(&path, ModelLoadParams::default())
        })
        .await
        .map_err(|e| InferenceError::ModelLoad(format!("blocking join: {e}")))??;

        let loaded = Arc::new(LoadedModel { digest, model });
        self.cache.lock().insert(digest, loaded.clone());
        Ok(ModelHandle { inner: loaded })
    }

    /// Drive a single request to completion, streaming events as they
    /// are produced.
    ///
    /// Determinism: when `req.mode == InferenceMode::Deterministic`
    /// the sampler is forced to greedy and the context is
    /// single-threaded. Two calls with the same prompt and mode
    /// produce byte-identical token streams.
    pub async fn infer(
        &self,
        model: &ModelHandle,
        req: InferenceRequest,
    ) -> Result<impl Stream<Item = Result<InferenceEvent>> + Send + 'static> {
        // Bounded channel so a slow consumer back-pressures the decoder.
        let (tx, rx) = mpsc::channel::<Result<InferenceEvent>>(64);

        // Pick mode-appropriate context params.
        let context_params = match req.mode {
            InferenceMode::Deterministic => {
                ContextParams::deterministic(self.cfg.max_context_tokens)
            }
            InferenceMode::Serving => {
                ContextParams::serving(self.cfg.max_context_tokens, self.cfg.serving_threads as i32)
            }
        };

        let model_arc = model.inner.clone();
        let sampling = req.sampling.clone();
        let session_req = SessionRequest {
            prompt: req.prompt,
            max_tokens: req.max_tokens,
            mode: req.mode,
            stop: req.stop,
        };

        tokio::task::spawn_blocking(move || {
            let mut tx = PollSender::new(tx);
            let result = run_sync(&model_arc, context_params, &sampling, &session_req, &mut tx);
            if let Err(e) = result {
                let _ = tx.get_ref().and_then(|s| {
                    // Best-effort error push; ignore if the receiver is gone.
                    s.try_send(Err(e)).ok()
                });
            }
        });

        Ok(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

/// Request shape for [`InferenceEngine::infer`].
#[derive(Clone, Debug)]
pub struct InferenceRequest {
    /// Prompt text.
    pub prompt: String,
    /// Max new tokens.
    pub max_tokens: u32,
    /// Mode gate — deterministic is the verifier path.
    pub mode: InferenceMode,
    /// Sampling knobs (ignored in `Deterministic` mode).
    pub sampling: SamplingParams,
    /// Stop strings.
    pub stop: Vec<String>,
}

/// Synchronous run under `spawn_blocking` — builds a fresh context +
/// tokenizer + sampler per request and drains into `sink`.
fn run_sync(
    loaded: &LoadedModel,
    ctx_params: ContextParams,
    sampling: &SamplingParams,
    req: &SessionRequest,
    sink: &mut impl EventSink,
) -> Result<()> {
    let mut ctx = Context::new(&loaded.model, ctx_params)?;
    let tokenizer = Tokenizer::new(&loaded.model);
    let sampler = Sampler::new(req.mode, sampling)?;
    let session = Session::new(&mut ctx, tokenizer, sampler);
    session.run(req, sink)?;
    Ok(())
}

/// `EventSink` that pushes each event into an async mpsc channel.
/// Closes on full channel or on cancellation.
impl EventSink for PollSender<Result<InferenceEvent>> {
    fn accept(&mut self, event: InferenceEvent) -> bool {
        // Attempt a non-blocking send; drop the event if the channel is
        // closed, which our caller treats as cancellation on the next
        // event boundary.
        self.get_ref()
            .map(|s| s.try_send(Ok(event)).is_ok())
            .unwrap_or(false)
    }
}

async fn file_digest(path: &std::path::Path) -> Result<Sha256Digest> {
    use arknet_crypto::hash::Sha256Stream;
    use tokio::fs::File;
    use tokio::io::AsyncReadExt;

    let mut file = File::open(path).await?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut hasher = Sha256Stream::new();
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}
