//! Safe wrapper around `llama_context`.
//!
//! A [`Context`] is the per-request runtime state: it owns the KV cache
//! and the inputs/outputs for `llama_decode`. Unlike [`Model`],
//! contexts are NOT safe to share across threads — creation is
//! expensive but short-lived, and each request owns its own.
//!
//! # Concurrency
//!
//! `Context` is `Send` (can be moved to another thread) but not `Sync`
//! (cannot be shared). The underlying `llama_context` is not
//! thread-safe; serialize access through ownership rather than a lock.

use std::ptr::NonNull;

use crate::backend;
use crate::errors::{InferenceError, Result};
use crate::model::Model;
use crate::sys;

/// Parameters for creating a context over a model.
///
/// Build one manually only if you need to override defaults; most
/// callers should use [`ContextParams::serving`] or
/// [`ContextParams::deterministic`].
#[derive(Clone, Debug)]
pub struct ContextParams {
    /// Context length in tokens. `0` = use the model's trained context length.
    pub n_ctx: u32,
    /// Logical batch size for `llama_decode`.
    pub n_batch: u32,
    /// Physical sub-batch size (how many tokens are actually forwarded
    /// through the network at once).
    pub n_ubatch: u32,
    /// Threads used during token generation (one-at-a-time decode).
    pub n_threads: i32,
    /// Threads used when ingesting a full prompt batch.
    pub n_threads_batch: i32,
    /// Whether to offload the KV cache to the GPU.
    pub offload_kqv: bool,
    /// When true, only produce embeddings — not logits. Phase 0 leaves
    /// this `false`; embeddings are Phase 1.
    pub embeddings: bool,
}

impl ContextParams {
    /// Deterministic / verifier preset: single thread, no GPU KV offload.
    pub fn deterministic(n_ctx: u32) -> Self {
        Self {
            n_ctx,
            n_batch: n_ctx.max(512),
            n_ubatch: 512,
            n_threads: 1,
            n_threads_batch: 1,
            offload_kqv: false,
            embeddings: false,
        }
    }

    /// Serving preset: caller-chosen thread count, GPU KV offload
    /// allowed (ignored when building a CPU-only binary).
    pub fn serving(n_ctx: u32, n_threads: i32) -> Self {
        Self {
            n_ctx,
            n_batch: n_ctx.max(512),
            n_ubatch: 512,
            n_threads,
            n_threads_batch: n_threads,
            offload_kqv: true,
            embeddings: false,
        }
    }
}

/// Safe handle to a llama.cpp inference context.
///
/// Drop frees the context via `llama_free`. Keep the owning [`Model`]
/// alive for at least as long as the `Context` — we bundle a lifetime
/// on `Context` to enforce that at compile time.
#[derive(Debug)]
pub struct Context<'model> {
    ptr: NonNull<sys::llama_context>,
    _model: std::marker::PhantomData<&'model Model>,
}

// SAFETY: llama_context owns its own GPU memory and KV cache. It is
// safe to move to another thread (`Send`) but must not be used
// concurrently from multiple threads — so no `Sync`.
unsafe impl Send for Context<'_> {}

impl<'model> Context<'model> {
    /// Create a new inference context over `model`.
    pub fn new(model: &'model Model, params: ContextParams) -> Result<Self> {
        backend::init_once();

        // SAFETY: pure getter.
        let mut native = unsafe { sys::llama_context_default_params() };
        native.n_ctx = params.n_ctx;
        native.n_batch = params.n_batch;
        native.n_ubatch = params.n_ubatch;
        native.n_threads = params.n_threads;
        native.n_threads_batch = params.n_threads_batch;
        native.offload_kqv = params.offload_kqv;
        native.embeddings = params.embeddings;

        // SAFETY: `model.as_ptr()` is non-null and outlives the context
        // by the lifetime annotation on `Self`.
        let raw = unsafe { sys::llama_init_from_model(model.as_ptr(), native) };
        let ptr = NonNull::new(raw).ok_or_else(|| {
            InferenceError::ContextInit("llama_init_from_model returned NULL".into())
        })?;

        Ok(Self {
            ptr,
            _model: std::marker::PhantomData,
        })
    }

    /// Effective context length (may differ from the requested value if
    /// the model constrained it).
    pub fn n_ctx(&self) -> u32 {
        // SAFETY: pure getter.
        unsafe { sys::llama_n_ctx(self.ptr.as_ptr()) }
    }

    /// Logical batch size.
    pub fn n_batch(&self) -> u32 {
        // SAFETY: pure getter.
        unsafe { sys::llama_n_batch(self.ptr.as_ptr()) }
    }

    /// Raw pointer for downstream modules (sampler, decoder).
    #[allow(dead_code)] // consumed by session.rs on Day 7
    pub(crate) fn as_ptr(&self) -> *mut sys::llama_context {
        self.ptr.as_ptr()
    }
}

impl Context<'_> {
    /// Serialize the full KV-cache + internal state into a byte buffer.
    ///
    /// The returned buffer can be fed to [`Self::restore_state`] on a
    /// context created over the **same model** (same digest) to resume
    /// inference from the point this snapshot was taken.
    ///
    /// # Security
    ///
    /// The snapshot contains the full KV cache, which includes the
    /// user's prompt tokens. Callers must handle the buffer with the
    /// same confidentiality as the prompt itself. Phase 2 encrypts
    /// checkpoint blobs with the user's session key.
    pub fn snapshot_state(&self) -> Result<Vec<u8>> {
        // SAFETY: pure query — returns the byte budget for the state.
        let size = unsafe { sys::llama_state_get_size(self.ptr.as_ptr()) };
        if size == 0 {
            return Err(InferenceError::Checkpoint("state size is 0".into()));
        }
        let mut buf = vec![0u8; size];
        // SAFETY: `buf` is sized to `llama_state_get_size`; llama.cpp
        // writes at most `size` bytes. Returns the number of bytes
        // actually written; a return of 0 indicates failure.
        let written =
            unsafe { sys::llama_state_get_data(self.ptr.as_ptr(), buf.as_mut_ptr(), size) };
        if written == 0 {
            return Err(InferenceError::Checkpoint(
                "llama_state_get_data returned 0".into(),
            ));
        }
        buf.truncate(written);
        Ok(buf)
    }

    /// Restore internal state from a buffer previously produced by
    /// [`Self::snapshot_state`].
    ///
    /// The context **must** be backed by the same model (same digest)
    /// that was used when the snapshot was taken — a mismatch will
    /// produce garbage or crash.
    pub fn restore_state(&mut self, data: &[u8]) -> Result<()> {
        // SAFETY: `data` came from `llama_state_get_data` and the
        // caller guarantees the same model. `llama_state_set_data`
        // copies at most `data.len()` bytes from `src`.
        let consumed =
            unsafe { sys::llama_state_set_data(self.ptr.as_ptr(), data.as_ptr(), data.len()) };
        if consumed == 0 {
            return Err(InferenceError::Checkpoint(
                "llama_state_set_data returned 0".into(),
            ));
        }
        Ok(())
    }
}

impl Drop for Context<'_> {
    fn drop(&mut self) {
        // SAFETY: we own the pointer, it came from `llama_init_from_model`,
        // and `llama_free` is the documented inverse.
        unsafe {
            sys::llama_free(self.ptr.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_preset_is_single_threaded() {
        let p = ContextParams::deterministic(2048);
        assert_eq!(p.n_threads, 1);
        assert_eq!(p.n_threads_batch, 1);
        assert!(!p.offload_kqv);
    }

    #[test]
    fn serving_preset_uses_requested_threads() {
        let p = ContextParams::serving(4096, 8);
        assert_eq!(p.n_threads, 8);
        assert_eq!(p.n_threads_batch, 8);
        assert!(p.offload_kqv);
    }
}
