//! arknet model manager.
//!
//! The gatekeeper between a model reference and llama.cpp. Every node
//! that touches a model goes through [`ModelManager::ensure_local`] so
//! the bytes on disk have been hash-verified against the manifest and
//! cross-checked against the GGUF header's declared quantization.
//!
//! # Phase 0 scope
//!
//! - Offline [`MockRegistry`] backed by a JSON file (Phase 1 replaces
//!   this with an on-chain registry).
//! - HTTP [`Puller`] with streaming SHA-256 and resumable `Range:` downloads.
//! - Content-addressed LRU disk [`Cache`] with a per-node byte cap.
//! - Hand-rolled [`gguf`] header validator.
//! - [`sandbox`] stub that will gain landlock / seccomp in Phase 2.
//!
//! # Security
//!
//! Never load a model that bypassed [`ModelManager::ensure_local`]. The
//! hash check is the primary integrity gate; a size-only check is not
//! a substitute.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cache;
pub mod errors;
pub mod gguf;
pub mod manager;
pub mod puller;
pub mod registry;
pub mod sandbox;
pub mod types;

pub use cache::{Cache, CacheConfig, DEFAULT_CACHE_MAX_BYTES};
pub use errors::{ModelError, Result};
pub use manager::ModelManager;
pub use puller::Puller;
pub use registry::{MockRegistry, MockRegistryFile, ModelRegistry};
pub use sandbox::{prepare as sandbox_prepare, SandboxedModel};
pub use types::{GgufQuant, ModelId, ModelManifest, ModelRef};
