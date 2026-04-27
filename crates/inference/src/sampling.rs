//! Sampling via llama.cpp's sampler chains.
//!
//! The engine builds one [`Sampler`] per request. In
//! [`InferenceMode::Deterministic`] the sampler is a single greedy
//! step (argmax), regardless of what [`SamplingParams`] asked for —
//! this is the invariant that makes verification work.
//!
//! # Layout
//!
//! A llama.cpp sampler is a "chain": transforms run in order, then
//! the final step picks a token. Typical serving chain:
//!
//! ```text
//! top_k(40) → top_p(0.95) → temp(0.8) → dist(seed)
//! ```
//!
//! Deterministic chain:
//!
//! ```text
//! greedy
//! ```

use std::ptr::NonNull;

use crate::backend;
use crate::config::{InferenceMode, SamplingParams};
use crate::context::Context;
use crate::errors::{InferenceError, Result};
use crate::sys;
use crate::tokenizer::Token;

/// Owned llama.cpp sampler chain. Dropped via `llama_sampler_free`.
pub struct Sampler {
    ptr: NonNull<sys::llama_sampler>,
}

// SAFETY: a sampler chain owns its internal state exclusively; one
// request, one sampler, never shared. `Send` allows moving between
// threads (e.g. onto a blocking task), `Sync` is not needed.
unsafe impl Send for Sampler {}

impl Sampler {
    /// Build the sampler for a given mode. Deterministic mode ignores
    /// `params` and always constructs a greedy sampler.
    pub fn new(mode: InferenceMode, params: &SamplingParams) -> Result<Self> {
        backend::init_once();

        // SAFETY: pure getter; llama.cpp zero-fills the struct.
        let chain_params = unsafe { sys::llama_sampler_chain_default_params() };
        // SAFETY: llama_sampler_chain_init allocates and returns non-null
        // on success; we immediately wrap in NonNull.
        let raw_chain = unsafe { sys::llama_sampler_chain_init(chain_params) };
        let chain = NonNull::new(raw_chain).ok_or_else(|| {
            InferenceError::ContextInit("llama_sampler_chain_init returned NULL".into())
        })?;

        match mode {
            InferenceMode::Deterministic => {
                // SAFETY: greedy constructor takes no args.
                let greedy = unsafe { sys::llama_sampler_init_greedy() };
                if greedy.is_null() {
                    // Free the chain we just built before bailing.
                    unsafe { sys::llama_sampler_free(chain.as_ptr()) };
                    return Err(InferenceError::ContextInit(
                        "llama_sampler_init_greedy returned NULL".into(),
                    ));
                }
                // SAFETY: add transfers ownership of `greedy` to `chain`.
                unsafe { sys::llama_sampler_chain_add(chain.as_ptr(), greedy) };
            }
            InferenceMode::Serving => {
                // Greedy shortcut: if temperature is 0, always argmax.
                if params.temperature <= 0.0 || params.top_k == 1 {
                    // SAFETY: same as above.
                    let greedy = unsafe { sys::llama_sampler_init_greedy() };
                    if greedy.is_null() {
                        unsafe { sys::llama_sampler_free(chain.as_ptr()) };
                        return Err(InferenceError::ContextInit(
                            "llama_sampler_init_greedy returned NULL".into(),
                        ));
                    }
                    unsafe { sys::llama_sampler_chain_add(chain.as_ptr(), greedy) };
                } else {
                    // Classic chain: top_k → top_p → temp → dist.
                    // top_k and top_p may be disabled (0) — llama.cpp
                    // tolerates that and treats it as identity.
                    if params.top_k > 0 {
                        let s = unsafe { sys::llama_sampler_init_top_k(params.top_k as i32) };
                        if s.is_null() {
                            unsafe { sys::llama_sampler_free(chain.as_ptr()) };
                            return Err(InferenceError::ContextInit(
                                "llama_sampler_init_top_k returned NULL".into(),
                            ));
                        }
                        unsafe { sys::llama_sampler_chain_add(chain.as_ptr(), s) };
                    }
                    if params.top_p > 0.0 && params.top_p < 1.0 {
                        let s = unsafe { sys::llama_sampler_init_top_p(params.top_p, 1) };
                        if s.is_null() {
                            unsafe { sys::llama_sampler_free(chain.as_ptr()) };
                            return Err(InferenceError::ContextInit(
                                "llama_sampler_init_top_p returned NULL".into(),
                            ));
                        }
                        unsafe { sys::llama_sampler_chain_add(chain.as_ptr(), s) };
                    }
                    let temp_s = unsafe { sys::llama_sampler_init_temp(params.temperature) };
                    if temp_s.is_null() {
                        unsafe { sys::llama_sampler_free(chain.as_ptr()) };
                        return Err(InferenceError::ContextInit(
                            "llama_sampler_init_temp returned NULL".into(),
                        ));
                    }
                    unsafe { sys::llama_sampler_chain_add(chain.as_ptr(), temp_s) };
                    let dist_s = unsafe { sys::llama_sampler_init_dist(params.seed as u32) };
                    if dist_s.is_null() {
                        unsafe { sys::llama_sampler_free(chain.as_ptr()) };
                        return Err(InferenceError::ContextInit(
                            "llama_sampler_init_dist returned NULL".into(),
                        ));
                    }
                    unsafe { sys::llama_sampler_chain_add(chain.as_ptr(), dist_s) };
                }
            }
        }

        Ok(Self { ptr: chain })
    }

    /// Sample one token from the last logits of `ctx`.
    ///
    /// `idx = -1` means "use the last logit row", which is what decode
    /// loops want after calling `llama_decode` on a one-token batch.
    pub fn sample(&mut self, ctx: &Context<'_>) -> Token {
        // SAFETY: `self.ptr` and `ctx.as_ptr()` are live for the call.
        unsafe { sys::llama_sampler_sample(self.ptr.as_ptr(), ctx.as_ptr(), -1) }
    }

    /// Notify the sampler that `token` was accepted (for state-tracking
    /// samplers like repetition penalty). Harmless for greedy.
    pub fn accept(&mut self, token: Token) {
        // SAFETY: `self.ptr` is live; llama.cpp tolerates tokens not in vocab
        // but we only pass values that came from `sample`.
        unsafe { sys::llama_sampler_accept(self.ptr.as_ptr(), token) };
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        // SAFETY: we own the chain and every sampler added to it; free
        // recursively releases children.
        unsafe { sys::llama_sampler_free(self.ptr.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_mode_builds_without_error() {
        let s = Sampler::new(InferenceMode::Deterministic, &SamplingParams::default());
        assert!(s.is_ok());
    }

    #[test]
    fn serving_mode_with_greedy_preset_builds() {
        let s = Sampler::new(InferenceMode::Serving, &SamplingParams::GREEDY);
        assert!(s.is_ok());
    }

    #[test]
    fn serving_mode_with_full_chain_builds() {
        let s = Sampler::new(
            InferenceMode::Serving,
            &SamplingParams {
                temperature: 0.8,
                top_k: 40,
                top_p: 0.95,
                seed: 12345,
            },
        );
        assert!(s.is_ok());
    }
}
