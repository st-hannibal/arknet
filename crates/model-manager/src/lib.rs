//! arknet model manager.
//!
//! The gatekeeper for every AI model loaded by a node. Never load a model
//! without going through this crate. Hash verification is mandatory.
//!
//! Phase 0 / Weeks 5-6: `registry`, `puller`, `verifier`, `cache`, `quantization`.
//! Phase 1+: `sandbox`, `bandwidth` (P2P seeding).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
