//! Safe wrapper around `llama_model`.
//!
//! A [`Model`] owns a `*mut llama_model` handle and guarantees that
//! `llama_model_free` is called exactly once when the wrapper is
//! dropped. The raw pointer never leaves the module.
//!
//! # Concurrency
//!
//! llama.cpp's model handles are thread-safe for read access after
//! loading. We mark [`Model`] as `Send + Sync` — multiple contexts
//! built from the same model may run concurrently on different
//! threads.

use std::ffi::{CStr, CString};
use std::path::Path;
use std::ptr::NonNull;

use crate::backend;
use crate::errors::{InferenceError, Result};
use crate::sys;

/// Parameters controlling how a model is loaded from disk.
///
/// Mirrors the subset of `llama_model_params` we actually expose. Start
/// from [`ModelLoadParams::default`] — the defaults come from
/// `llama_model_default_params` and are what llama.cpp's own binaries
/// use.
#[derive(Clone, Debug)]
pub struct ModelLoadParams {
    /// How many transformer layers to offload to the GPU. Negative =
    /// all layers. Zero = CPU-only (used by the verifier / deterministic
    /// path). Ignored when no GPU backend is compiled in.
    pub n_gpu_layers: i32,
    /// Memory-map the file instead of reading it into anonymous memory.
    /// Recommended for large models.
    pub use_mmap: bool,
    /// `mlock` the resident pages so the OS can't page weights out.
    /// Prevents hitches on large models; requires privilege.
    pub use_mlock: bool,
    /// Validate tensor data after loading. Off by default — the cache
    /// path already hash-verified the whole file.
    pub check_tensors: bool,
}

impl Default for ModelLoadParams {
    fn default() -> Self {
        // SAFETY: `llama_model_default_params` is a pure getter.
        backend::init_once();
        let defaults = unsafe { sys::llama_model_default_params() };
        Self {
            n_gpu_layers: defaults.n_gpu_layers,
            use_mmap: defaults.use_mmap,
            use_mlock: defaults.use_mlock,
            check_tensors: defaults.check_tensors,
        }
    }
}

impl ModelLoadParams {
    /// Verifier / deterministic preset: no GPU offload at all.
    pub fn deterministic() -> Self {
        Self {
            n_gpu_layers: 0,
            ..Self::default()
        }
    }
}

/// Safe handle to a loaded llama.cpp model.
///
/// Dropping frees the underlying C++ object via `llama_model_free`.
/// Loading is expensive (multi-GB reads); callers should cache
/// [`Model`] instances by their source path.
#[derive(Debug)]
pub struct Model {
    ptr: NonNull<sys::llama_model>,
}

// SAFETY: llama.cpp models are safe to share across threads once
// loaded — the C++ code guards its internal state for read access,
// and our wrapper only exposes read-only accessors. Writes (context
// creation, inference) go through `Context`, which is `!Sync`.
unsafe impl Send for Model {}
unsafe impl Sync for Model {}

impl Model {
    /// Load a model from a GGUF file on disk.
    ///
    /// The path is typically what [`arknet_model_manager::SandboxedModel::path`]
    /// returns — already hash-verified and quantization-checked.
    pub fn load_from_file(path: &Path, params: ModelLoadParams) -> Result<Self> {
        backend::init_once();

        let path_str = path
            .to_str()
            .ok_or_else(|| InferenceError::ModelLoad(format!("non-utf8 path: {path:?}")))?;
        let cpath = CString::new(path_str)
            .map_err(|e| InferenceError::ModelLoad(format!("path has interior NUL: {e}")))?;

        // SAFETY: get defaults from llama.cpp and then override fields we
        // care about. `llama_model_default_params` is a pure getter.
        let mut native = unsafe { sys::llama_model_default_params() };
        native.n_gpu_layers = params.n_gpu_layers;
        native.use_mmap = params.use_mmap;
        native.use_mlock = params.use_mlock;
        native.check_tensors = params.check_tensors;

        // SAFETY: `cpath` outlives the call; llama.cpp copies the string.
        // Returns a non-null pointer on success, NULL on failure.
        let raw = unsafe { sys::llama_model_load_from_file(cpath.as_ptr(), native) };
        let ptr = NonNull::new(raw).ok_or_else(|| {
            InferenceError::ModelLoad(format!(
                "llama_model_load_from_file returned NULL for {path_str}"
            ))
        })?;

        Ok(Self { ptr })
    }

    /// Number of parameters (weights). Useful for bookkeeping and logs.
    pub fn n_params(&self) -> u64 {
        // SAFETY: pure getter; `self.ptr` is non-null by construction.
        unsafe { sys::llama_model_n_params(self.ptr.as_ptr()) }
    }

    /// On-disk size of the model. Reported by llama.cpp; may differ
    /// from the manifest size if metadata padding changed.
    pub fn size_bytes(&self) -> u64 {
        // SAFETY: pure getter.
        unsafe { sys::llama_model_size(self.ptr.as_ptr()) }
    }

    /// Number of transformer layers.
    pub fn n_layers(&self) -> i32 {
        // SAFETY: pure getter.
        unsafe { sys::llama_model_n_layer(self.ptr.as_ptr()) }
    }

    /// Embedding dimension.
    pub fn n_embd(&self) -> i32 {
        // SAFETY: pure getter.
        unsafe { sys::llama_model_n_embd(self.ptr.as_ptr()) }
    }

    /// Training context length (max tokens the model was trained on).
    /// A context created from this model may use fewer.
    pub fn n_ctx_train(&self) -> i32 {
        // SAFETY: pure getter.
        unsafe { sys::llama_model_n_ctx_train(self.ptr.as_ptr()) }
    }

    /// Vocabulary size (number of distinct tokens the tokenizer emits).
    pub fn n_vocab(&self) -> i32 {
        // SAFETY: `llama_model_get_vocab` returns a pointer into the
        // model; llama_vocab_n_tokens is a pure getter over it.
        unsafe {
            let vocab = sys::llama_model_get_vocab(self.ptr.as_ptr());
            sys::llama_vocab_n_tokens(vocab)
        }
    }

    /// Short human-readable description (architecture, param count).
    ///
    /// Returns an empty string if the underlying call truncates.
    pub fn description(&self) -> String {
        let mut buf = [0u8; 512];
        // SAFETY: `llama_model_desc` writes up to `buf_size - 1` bytes
        // plus a NUL terminator. `buf` is live for the call; we reborrow
        // its pointer as `*mut c_char`.
        let written = unsafe {
            sys::llama_model_desc(
                self.ptr.as_ptr(),
                buf.as_mut_ptr() as *mut std::os::raw::c_char,
                buf.len(),
            )
        };
        if written <= 0 {
            return String::new();
        }
        // SAFETY: llama.cpp NUL-terminates on success.
        unsafe { CStr::from_ptr(buf.as_ptr() as *const std::os::raw::c_char) }
            .to_string_lossy()
            .into_owned()
    }

    /// Raw pointer escape hatch for wrappers in this crate. Not `pub`.
    #[allow(dead_code)] // consumed by `Context` in Day 4.
    pub(crate) fn as_ptr(&self) -> *mut sys::llama_model {
        self.ptr.as_ptr()
    }
}

impl Drop for Model {
    fn drop(&mut self) {
        // SAFETY: we own `self.ptr`, it came from `llama_model_load_from_file`,
        // and `llama_model_free` is the documented inverse. llama.cpp
        // tolerates this from any thread.
        unsafe {
            sys::llama_model_free(self.ptr.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_params_round_trip_through_llama() {
        let p = ModelLoadParams::default();
        // Defaults we care about: mmap is on, mlock is off.
        assert!(p.use_mmap);
        assert!(!p.use_mlock);
    }

    #[test]
    fn deterministic_params_disable_gpu() {
        let p = ModelLoadParams::deterministic();
        assert_eq!(p.n_gpu_layers, 0);
    }

    #[test]
    fn load_nonexistent_file_errors() {
        let fake = Path::new("/tmp/definitely-not-a-real-gguf-file.gguf");
        let err = Model::load_from_file(fake, ModelLoadParams::default()).unwrap_err();
        assert!(matches!(err, InferenceError::ModelLoad(_)));
    }

    #[test]
    fn load_non_gguf_file_errors() {
        // Feed llama.cpp a tempfile that is NOT a GGUF. It must reject
        // without crashing.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-model.bin");
        std::fs::write(&path, b"this is not a gguf file").unwrap();

        let err = Model::load_from_file(&path, ModelLoadParams::default()).unwrap_err();
        assert!(matches!(err, InferenceError::ModelLoad(_)));
    }
}
