//! Configuration types for the inference engine.
//!
//! [`InferenceMode`] is the critical one: `Deterministic` is the
//! verifier path. Callers must pick the right mode for their role —
//! compute nodes use `Serving`, verifiers use `Deterministic`.

use serde::{Deserialize, Serialize};

/// Top-level inference engine configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceConfig {
    /// Maximum context length (in tokens) supported across all loaded models.
    /// Concrete per-context size is chosen per request, bounded by this.
    pub max_context_tokens: u32,

    /// Worker thread count for the `Serving` mode. Ignored in
    /// `Deterministic` mode (which always uses 1).
    pub serving_threads: u32,
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            max_context_tokens: 8_192,
            serving_threads: 8,
        }
    }
}

/// Mode gate for determinism guarantees.
///
/// - [`InferenceMode::Serving`] — fast path for compute nodes. GPU allowed,
///   multi-threaded, sampler can use real temperature.
/// - [`InferenceMode::Deterministic`] — the verifier path. Greedy-only,
///   single-threaded, CPU-only. **Byte-identical across runs** is the
///   invariant this mode enforces.
///
/// Changing this mid-request is not allowed. The value picked at
/// [`InferenceRequest`](crate::InferenceRequest) construction is locked
/// until the stream ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InferenceMode {
    /// Fast path: GPU allowed, multi-thread, caller chooses sampling.
    Serving,
    /// Verification path: greedy only, single thread, CPU only.
    Deterministic,
}

/// Sampling parameters for non-deterministic generation.
///
/// Ignored when `InferenceMode::Deterministic` — that mode forces
/// greedy sampling regardless of what's passed here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SamplingParams {
    /// Softmax temperature. `0.0` means greedy (argmax). Default `0.8`.
    pub temperature: f32,
    /// Top-k truncation. `1` means greedy. `0` means disabled. Default `40`.
    pub top_k: u32,
    /// Top-p (nucleus) cumulative probability cutoff. Default `0.95`.
    pub top_p: f32,
    /// RNG seed. Fixed to `0` for verification; real requests pass a session seed.
    pub seed: u64,
}

impl Default for SamplingParams {
    fn default() -> Self {
        Self {
            temperature: 0.8,
            top_k: 40,
            top_p: 0.95,
            seed: 0,
        }
    }
}

impl SamplingParams {
    /// Greedy preset: argmax every step. What `Deterministic` mode forces.
    pub const GREEDY: SamplingParams = SamplingParams {
        temperature: 0.0,
        top_k: 1,
        top_p: 1.0,
        seed: 0,
    };
}
