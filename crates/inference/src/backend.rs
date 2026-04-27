//! Process-global llama.cpp backend lifecycle.
//!
//! `llama_backend_init` must be called exactly once before any other
//! llama.cpp function. Subsequent calls are no-ops but the API is not
//! documented as safe to invoke concurrently, so we serialize through a
//! [`std::sync::Once`].
//!
//! We deliberately do **not** call `llama_backend_free` automatically.
//! Rust has no hook that fires reliably at process exit for static
//! destructors, and calling it at the wrong moment (e.g. from a
//! library dropped mid-shutdown) crashes llama.cpp. The OS reclaims
//! process memory on exit; llama.cpp allocations are not kernel
//! resources that need explicit release.
//!
//! # Security
//!
//! The log callback is bridged to `tracing` so every message llama.cpp
//! emits lands in the node's structured log stream. Important for
//! auditability — if a model triggers a warning at load time, we want
//! that captured in the trace.

use std::sync::Once;

use crate::sys;

static BACKEND_INIT: Once = Once::new();

/// Ensure the llama.cpp backend is initialized. Idempotent; cheap after
/// the first call (one atomic load).
///
/// Every public entry point in this crate that calls into llama.cpp
/// must invoke this first. [`Model::load_from_file`] does so; direct
/// callers of [`sys`] must do so manually.
pub fn init_once() {
    BACKEND_INIT.call_once(|| {
        // SAFETY: `llama_backend_init` is the documented entry point; it
        // takes no arguments and has no precondition beyond "call once
        // before anything else".
        unsafe {
            sys::llama_backend_init();
        }

        install_log_callback();
    });
}

fn install_log_callback() {
    // SAFETY: `llama_log_set` stores our function pointer + user data
    // for later invocation. `log_trampoline` matches the
    // `ggml_log_callback` signature (two pointers + level + user data).
    // We pass `std::ptr::null_mut()` user data since tracing is global.
    unsafe {
        sys::llama_log_set(Some(log_trampoline), std::ptr::null_mut());
    }
}

/// C-ABI trampoline that forwards llama.cpp log lines into `tracing`.
///
/// # Safety
///
/// Called by llama.cpp; `text` must be a NUL-terminated C string (the
/// ggml contract). We defensively fall back on non-UTF-8 sequences.
unsafe extern "C" fn log_trampoline(
    level: sys::ggml_log_level,
    text: *const std::os::raw::c_char,
    _user_data: *mut std::os::raw::c_void,
) {
    if text.is_null() {
        return;
    }
    // SAFETY: llama.cpp guarantees NUL-termination for log text.
    let cstr = unsafe { std::ffi::CStr::from_ptr(text) };
    let msg = cstr.to_string_lossy();
    let msg = msg.trim_end_matches(['\n', '\r']);
    if msg.is_empty() {
        return;
    }
    match level {
        sys::ggml_log_level::GGML_LOG_LEVEL_ERROR => tracing::error!(target: "llama", "{msg}"),
        sys::ggml_log_level::GGML_LOG_LEVEL_WARN => tracing::warn!(target: "llama", "{msg}"),
        sys::ggml_log_level::GGML_LOG_LEVEL_INFO => tracing::info!(target: "llama", "{msg}"),
        sys::ggml_log_level::GGML_LOG_LEVEL_DEBUG => tracing::debug!(target: "llama", "{msg}"),
        _ => tracing::trace!(target: "llama", "{msg}"),
    }
}

/// Report runtime capabilities of the linked llama.cpp build.
///
/// Useful for startup logs so operators can confirm GPU offload /
/// memory-mapping is configured as expected.
#[derive(Clone, Copy, Debug)]
pub struct BackendCapabilities {
    /// Memory-mapped model loading is available.
    pub mmap: bool,
    /// `mlock` is available for pinning model weights in RAM.
    pub mlock: bool,
    /// A GPU offload backend (CUDA / Metal / ROCm / Vulkan) is compiled in.
    pub gpu_offload: bool,
    /// Number of backend devices detected (CPU counts as 1).
    pub device_count: usize,
}

impl BackendCapabilities {
    /// Query the linked llama.cpp for its runtime caps.
    pub fn query() -> Self {
        init_once();
        // SAFETY: all four functions are pure readers with no preconditions
        // beyond backend init, which we just guaranteed.
        unsafe {
            Self {
                mmap: sys::llama_supports_mmap(),
                mlock: sys::llama_supports_mlock(),
                gpu_offload: sys::llama_supports_gpu_offload(),
                device_count: sys::llama_max_devices(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent() {
        init_once();
        init_once();
        init_once();
    }

    #[test]
    fn capabilities_are_sane() {
        let caps = BackendCapabilities::query();
        // `mmap` is supported on every platform we target; any build
        // that reports otherwise is misconfigured.
        assert!(caps.mmap, "mmap support should be compiled in");
        // At least one backend device (CPU) must exist.
        assert!(
            caps.device_count >= 1,
            "expected at least one backend device"
        );
    }
}
