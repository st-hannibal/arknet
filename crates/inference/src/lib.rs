//! arknet inference engine.
//!
//! A thin, safe wrapper over llama.cpp that produces a deterministic
//! stream of tokens suitable for on-chain verification. The public
//! entry point is [`InferenceEngine`][not-yet]; every downstream role
//! (compute, verifier) drives inference through it.
//!
//! [not-yet]: #status
//!
//! # Phase 0 scope
//!
//! - Vendored llama.cpp (git submodule at a pinned SHA).
//! - Hand-rolled FFI via `bindgen` (raw symbols confined to `sys`).
//! - Safe `Model` / `Context` / `Tokenizer` wrappers with RAII drops.
//! - Single-request (batch = 1) decode loop.
//! - Two modes: [`InferenceMode::Serving`] and [`InferenceMode::Deterministic`].
//! - [`CheckpointableSession`] trait (stub implementation).
//!
//! # Status
//!
//! Day 1 ships the scaffold: CMake build of llama.cpp, `bindgen`
//! FFI, and the public type surface. The decode loop and engine
//! facade land over Days 2-10.
//!
//! # Determinism
//!
//! The verifier path lives in [`InferenceMode::Deterministic`] and is
//! the contract that makes on-chain slashing work. That mode forces:
//! greedy sampling, 1 thread, CPU only. Any change there requires a
//! protocol version bump.
//!
//! # Safety
//!
//! The `sys` module is the only `unsafe` surface in this crate.
//! Everything it exposes is wrapped before leaving the module
//! boundary.

#![warn(missing_docs)]

mod sys;

pub mod checkpoint;
pub mod config;
pub mod errors;
pub mod events;

pub use checkpoint::{CheckpointableSession, Phase0CheckpointStub};
pub use config::{InferenceConfig, InferenceMode, SamplingParams};
pub use errors::{InferenceError, Result};
pub use events::{InferenceEvent, StopReason, TokenEvent};
