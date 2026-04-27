//! Cryptographic hashing primitives.
//!
//! Two algorithms are used throughout arknet:
//!
//! - **SHA-256** — consensus-critical hashes (Merkle roots, block hashes,
//!   receipt commitments, on-chain identifiers). Chosen for ubiquity, FIPS
//!   approval, and hardware acceleration. Quantum-safe (Grover's reduces
//!   128-bit preimage strength, still above comfort threshold).
//! - **BLAKE3** — fast, keyed hashing for non-consensus paths (peer IDs,
//!   local caches, deterministic random seeds). ~10× faster than SHA-256
//!   on modern hardware. Quantum-safe.
//!
//! Both produce 32-byte digests that share the [`Hash256`][arknet_common::Hash256]
//! type alias from `arknet-common`. A call site that needs to distinguish
//! the two should use [`Sha256Digest`] / [`Blake3Digest`] newtype wrappers.
//!
//! # Security
//!
//! Never mix the two. SHA-256 is the only hash that enters consensus.
//! BLAKE3 is for fast local work only.

use arknet_common::Hash256;
use blake3::Hasher as Blake3Hasher;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

fn serialize_hash256_as_hex<S: Serializer>(h: &Hash256, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&hex::encode(h))
}

fn deserialize_hash256_from_hex<'de, D: Deserializer<'de>>(d: D) -> Result<Hash256, D::Error> {
    let s = String::deserialize(d)?;
    let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
    bytes
        .try_into()
        .map_err(|_| serde::de::Error::custom("expected 32-byte hex string"))
}

// ─── SHA-256 ─────────────────────────────────────────────────────────────

/// Typed SHA-256 output. Distinct from [`Blake3Digest`] to prevent mix-ups.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
pub struct Sha256Digest(
    #[serde(
        serialize_with = "serialize_hash256_as_hex",
        deserialize_with = "deserialize_hash256_from_hex"
    )]
    pub Hash256,
);

impl Sha256Digest {
    /// Raw bytes.
    pub const fn as_bytes(&self) -> &Hash256 {
        &self.0
    }

    /// Consume into raw bytes.
    pub const fn into_bytes(self) -> Hash256 {
        self.0
    }
}

impl std::fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sha256:{}", hex::encode(self.0))
    }
}

/// Compute SHA-256 of `input`.
///
/// Deterministic. Consensus-critical when used on on-chain state.
pub fn sha256(input: &[u8]) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(input);
    Sha256Digest(hasher.finalize().into())
}

/// Streaming SHA-256 hasher. Use for large / chunked inputs (model files, etc.).
#[derive(Default)]
pub struct Sha256Stream(Sha256);

impl Sha256Stream {
    /// Create a new empty hasher.
    pub fn new() -> Self {
        Self(Sha256::new())
    }

    /// Append bytes.
    pub fn update(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update(bytes);
        self
    }

    /// Finalize and return the digest.
    pub fn finalize(self) -> Sha256Digest {
        Sha256Digest(self.0.finalize().into())
    }
}

// ─── BLAKE3 ───────────────────────────────────────────────────────────────

/// Typed BLAKE3 output. Distinct from [`Sha256Digest`] to prevent mix-ups.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
pub struct Blake3Digest(
    #[serde(
        serialize_with = "serialize_hash256_as_hex",
        deserialize_with = "deserialize_hash256_from_hex"
    )]
    pub Hash256,
);

impl Blake3Digest {
    /// Raw bytes.
    pub const fn as_bytes(&self) -> &Hash256 {
        &self.0
    }

    /// Consume into raw bytes.
    pub const fn into_bytes(self) -> Hash256 {
        self.0
    }
}

impl std::fmt::Display for Blake3Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "blake3:{}", hex::encode(self.0))
    }
}

/// Compute BLAKE3 of `input`.
pub fn blake3(input: &[u8]) -> Blake3Digest {
    Blake3Digest(*blake3::hash(input).as_bytes())
}

/// Keyed BLAKE3 — use when a per-node or per-session secret should scope
/// the hash space (MAC-style). Always pass a 32-byte key.
pub fn blake3_keyed(key: &[u8; 32], input: &[u8]) -> Blake3Digest {
    Blake3Digest(*blake3::keyed_hash(key, input).as_bytes())
}

/// Streaming BLAKE3 hasher.
#[derive(Default)]
pub struct Blake3Stream(Blake3Hasher);

impl Blake3Stream {
    /// Create a new empty hasher.
    pub fn new() -> Self {
        Self(Blake3Hasher::new())
    }

    /// Append bytes.
    pub fn update(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update(bytes);
        self
    }

    /// Finalize and return the digest.
    pub fn finalize(&self) -> Blake3Digest {
        Blake3Digest(*self.0.finalize().as_bytes())
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // SHA-256 test vectors (NIST FIPS 180-4).
    #[test]
    fn sha256_of_empty_string_matches_nist() {
        let d = sha256(b"");
        let expected = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(hex::encode(d.as_bytes()), expected);
    }

    #[test]
    fn sha256_of_abc_matches_nist() {
        let d = sha256(b"abc");
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(hex::encode(d.as_bytes()), expected);
    }

    #[test]
    fn sha256_is_deterministic() {
        let a = sha256(b"arknet");
        let b = sha256(b"arknet");
        assert_eq!(a, b);
    }

    #[test]
    fn sha256_streaming_matches_oneshot() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let oneshot = sha256(data);

        let mut stream = Sha256Stream::new();
        stream
            .update(&data[..10])
            .update(&data[10..20])
            .update(&data[20..]);
        let streamed = stream.finalize();

        assert_eq!(oneshot, streamed);
    }

    // BLAKE3 test vector (official).
    #[test]
    fn blake3_of_empty_string_matches_spec() {
        let d = blake3(b"");
        let expected = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
        assert_eq!(hex::encode(d.as_bytes()), expected);
    }

    #[test]
    fn blake3_is_deterministic() {
        let a = blake3(b"arknet");
        let b = blake3(b"arknet");
        assert_eq!(a, b);
    }

    #[test]
    fn blake3_streaming_matches_oneshot() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let oneshot = blake3(data);

        let mut stream = Blake3Stream::new();
        stream
            .update(&data[..10])
            .update(&data[10..20])
            .update(&data[20..]);
        let streamed = stream.finalize();

        assert_eq!(oneshot, streamed);
    }

    #[test]
    fn blake3_keyed_differs_from_unkeyed() {
        let key = [0x42u8; 32];
        let data = b"arknet";
        assert_ne!(blake3(data).as_bytes(), blake3_keyed(&key, data).as_bytes());
    }

    #[test]
    fn blake3_keyed_different_keys_differ() {
        let k1 = [0x11u8; 32];
        let k2 = [0x22u8; 32];
        let data = b"arknet";
        assert_ne!(blake3_keyed(&k1, data), blake3_keyed(&k2, data));
    }

    #[test]
    fn sha256_and_blake3_differ() {
        let data = b"arknet";
        assert_ne!(sha256(data).as_bytes(), blake3(data).as_bytes());
    }

    #[test]
    fn digest_types_display_with_prefix() {
        let s = sha256(b"x").to_string();
        assert!(s.starts_with("sha256:"));
        let b = blake3(b"x").to_string();
        assert!(b.starts_with("blake3:"));
    }

    // Property test: hash of any input is deterministic.
    proptest::proptest! {
        #[test]
        fn sha256_determinism_property(data: Vec<u8>) {
            proptest::prop_assert_eq!(sha256(&data), sha256(&data));
        }

        #[test]
        fn blake3_determinism_property(data: Vec<u8>) {
            proptest::prop_assert_eq!(blake3(&data), blake3(&data));
        }

        #[test]
        fn sha256_collision_resistant_on_single_bit_flip(data: Vec<u8>) {
            if data.is_empty() { return Ok(()); }
            let mut flipped = data.clone();
            flipped[0] ^= 1;
            proptest::prop_assert_ne!(sha256(&data), sha256(&flipped));
        }

        #[test]
        fn blake3_collision_resistant_on_single_bit_flip(data: Vec<u8>) {
            if data.is_empty() { return Ok(()); }
            let mut flipped = data.clone();
            flipped[0] ^= 1;
            proptest::prop_assert_ne!(blake3(&data), blake3(&flipped));
        }
    }
}
