//! Cryptographic primitives for arknet.
//!
//! All primitives wrap audited crates. **Do not implement custom crypto here.**
//!
//! Modules:
//! - [`hash`] — SHA-256 + BLAKE3 with typed digests.
//! - [`keys`] — Ed25519 signing + X25519 key exchange with zeroize-on-drop.
//! - [`signatures`] — sign / verify / batch verify over scheme-tagged types.
//! - [`merkle`] — domain-separated binary Merkle tree over SHA-256.
//! - [`kdf`] — Argon2id for passphrase-protected storage.
//! - [`vrf`] — verifiable random function (Phase 0 Ed25519 construction).
//! - [`threshold`] — threshold crypto trait (impl lands in Phase 2).
//!
//! See [`docs/SECURITY.md`] §4 for the crypto inventory and §12 for the
//! post-quantum migration plan.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod errors;
pub mod hash;
pub mod kdf;
pub mod keys;
pub mod merkle;
pub mod signatures;
pub mod threshold;
pub mod vrf;

// Ergonomic re-exports.
pub use errors::{CryptoError, Result};
pub use hash::{
    blake3, blake3_keyed, sha256, Blake3Digest, Blake3Stream, Sha256Digest, Sha256Stream,
};
pub use keys::{KeyExchangePublic, KeyExchangeSecret, SharedSecret, SigningKey, VerifyingKey};
pub use merkle::{verify_proof as verify_merkle_proof, MerkleProof, MerkleTree};
pub use signatures::{sign, verify, verify_batch};
pub use vrf::{prove as vrf_prove, verify_proof as vrf_verify, VrfOutput, VrfProof};
