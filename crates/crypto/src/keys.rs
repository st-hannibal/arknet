//! Key generation, encoding, and lifecycle for Ed25519 (signing) and X25519 (KEX).
//!
//! # Security
//!
//! - Secret keys are stored in [`SigningKey`] / [`KeyExchangeSecret`], both of
//!   which implement [`Zeroize`] on drop.
//! - Secret bytes are **never** logged, serialized to JSON, or included in
//!   `Debug`/`Display` output.
//! - Public keys wrap the scheme-tagged [`PubKey`] from `arknet-common` so
//!   crypto-agility propagates downstream.

use arknet_common::{PubKey, SignatureScheme};
use ed25519_dalek::{
    SigningKey as EdSigningKey, VerifyingKey as EdVerifyingKey, SECRET_KEY_LENGTH,
};
use rand_core::{OsRng, RngCore};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::errors::CryptoError;

// ─── Ed25519 signing key ──────────────────────────────────────────────────

/// Ed25519 signing key (secret). Zeroized on drop.
///
/// Treat as a secret: do not log, clone freely, or serialize to JSON.
/// Use the `borsh`-friendly [`export`][Self::export] / [`import`][Self::import]
/// helpers for on-disk persistence, and wrap the result in an encrypted
/// container (see [`crate::kdf`]).
pub struct SigningKey(EdSigningKey);

impl SigningKey {
    /// Generate a fresh signing key from the OS CSPRNG.
    pub fn generate() -> Self {
        let mut bytes = [0u8; SECRET_KEY_LENGTH];
        OsRng.fill_bytes(&mut bytes);
        let key = EdSigningKey::from_bytes(&bytes);
        bytes.zeroize();
        Self(key)
    }

    /// Derive the signing key from a 32-byte seed (used for deterministic tests
    /// and HKDF-derived subkeys — **not** for user-facing key generation).
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self(EdSigningKey::from_bytes(seed))
    }

    /// Corresponding verifying (public) key.
    pub fn verifying_key(&self) -> VerifyingKey {
        VerifyingKey(self.0.verifying_key())
    }

    /// Export the raw 32 secret-key bytes.
    ///
    /// # Security
    /// The returned buffer must be zeroized by the caller when done.
    /// Use [`Zeroizing`][zeroize::Zeroizing] from the `zeroize` crate.
    pub fn export(&self) -> [u8; SECRET_KEY_LENGTH] {
        self.0.to_bytes()
    }

    /// Import a signing key from raw 32 secret-key bytes.
    pub fn import(bytes: &[u8; SECRET_KEY_LENGTH]) -> Self {
        Self(EdSigningKey::from_bytes(bytes))
    }

    /// Access the underlying dalek key (for in-crate use in `signatures.rs`).
    pub(crate) fn inner(&self) -> &EdSigningKey {
        &self.0
    }
}

impl Drop for SigningKey {
    fn drop(&mut self) {
        // `ed25519_dalek::SigningKey` is `ZeroizeOnDrop`, so this is defense in depth.
        // Explicit drop order keeps the pattern obvious to readers.
    }
}

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak secret material via Debug.
        f.debug_struct("SigningKey")
            .field("scheme", &"Ed25519")
            .field("secret", &"<redacted>")
            .finish()
    }
}

// ─── Ed25519 verifying key ────────────────────────────────────────────────

/// Ed25519 verifying (public) key. Safe to display, clone, store.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct VerifyingKey(EdVerifyingKey);

impl VerifyingKey {
    /// Import from raw 32 bytes. Returns an error if the bytes don't decode
    /// to a valid curve point.
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<Self, CryptoError> {
        EdVerifyingKey::from_bytes(bytes)
            .map(Self)
            .map_err(|e| CryptoError::InvalidKey(e.to_string()))
    }

    /// Raw 32-byte encoding.
    pub fn to_bytes(self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// Lift to a scheme-tagged [`PubKey`] for on-wire use.
    pub fn to_pubkey(self) -> PubKey {
        PubKey::ed25519(self.to_bytes())
    }

    /// Parse a scheme-tagged [`PubKey`]. Rejects non-Ed25519 schemes.
    pub fn from_pubkey(pk: &PubKey) -> Result<Self, CryptoError> {
        if pk.scheme != SignatureScheme::Ed25519 {
            return Err(CryptoError::SchemeNotSupported(format!(
                "expected Ed25519, got {:?}",
                pk.scheme
            )));
        }
        if pk.bytes.len() != 32 {
            return Err(CryptoError::InvalidKey(format!(
                "Ed25519 pubkey must be 32 bytes, got {}",
                pk.bytes.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&pk.bytes);
        Self::from_bytes(&arr)
    }

    /// Access the underlying dalek key (for in-crate use).
    pub(crate) fn inner(&self) -> &EdVerifyingKey {
        &self.0
    }
}

// ─── X25519 key exchange ──────────────────────────────────────────────────

/// X25519 Diffie-Hellman secret. Zeroized on drop.
///
/// Used for encrypting prompts end-to-end from the user to the compute node.
#[derive(ZeroizeOnDrop)]
pub struct KeyExchangeSecret([u8; 32]);

impl KeyExchangeSecret {
    /// Generate a fresh KEM secret from the OS CSPRNG.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        // Clamp per RFC 7748 — `x25519_dalek` does this internally on use,
        // so storing raw random bytes is fine.
        Self(bytes)
    }

    /// Derive the X25519 public key.
    pub fn public_key(&self) -> KeyExchangePublic {
        let secret = x25519_dalek::StaticSecret::from(self.0);
        let public = x25519_dalek::PublicKey::from(&secret);
        KeyExchangePublic(*public.as_bytes())
    }

    /// Perform ECDH and return the raw 32-byte shared secret.
    ///
    /// # Security
    /// The returned shared secret is raw DH output. **Always** run it through
    /// a KDF (HKDF-SHA-256 recommended) before using as an encryption key.
    pub fn diffie_hellman(&self, peer: &KeyExchangePublic) -> SharedSecret {
        let secret = x25519_dalek::StaticSecret::from(self.0);
        let public = x25519_dalek::PublicKey::from(peer.0);
        let shared = secret.diffie_hellman(&public);
        SharedSecret(*shared.as_bytes())
    }

    /// Import from raw 32 bytes (used for test vectors and HSM import).
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Raw 32-byte export. Must be zeroized by caller.
    pub fn export(&self) -> [u8; 32] {
        self.0
    }
}

impl std::fmt::Debug for KeyExchangeSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyExchangeSecret")
            .field("scheme", &"X25519")
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// X25519 public key. Safe to share.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct KeyExchangePublic(pub [u8; 32]);

impl KeyExchangePublic {
    /// Raw bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Raw 32-byte ECDH output. Zeroized on drop. **Always** run through a KDF.
#[derive(ZeroizeOnDrop)]
pub struct SharedSecret([u8; 32]);

impl SharedSecret {
    /// Raw bytes (for KDF input only).
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for SharedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedSecret")
            .field("value", &"<redacted>")
            .finish()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_produces_distinct_keys() {
        let a = SigningKey::generate();
        let b = SigningKey::generate();
        assert_ne!(a.export(), b.export());
    }

    #[test]
    fn verifying_key_roundtrip_via_bytes() {
        let sk = SigningKey::generate();
        let vk = sk.verifying_key();
        let bytes = vk.to_bytes();
        let vk2 = VerifyingKey::from_bytes(&bytes).unwrap();
        assert_eq!(vk, vk2);
    }

    #[test]
    fn verifying_key_roundtrip_via_pubkey() {
        let sk = SigningKey::generate();
        let pk = sk.verifying_key().to_pubkey();
        let vk = VerifyingKey::from_pubkey(&pk).unwrap();
        assert_eq!(vk, sk.verifying_key());
    }

    #[test]
    fn from_pubkey_rejects_wrong_scheme() {
        let pk = PubKey {
            scheme: SignatureScheme::Dilithium,
            bytes: vec![0; 1312],
        };
        let res = VerifyingKey::from_pubkey(&pk);
        assert!(matches!(res, Err(CryptoError::SchemeNotSupported(_))));
    }

    #[test]
    fn from_pubkey_rejects_wrong_length() {
        let pk = PubKey {
            scheme: SignatureScheme::Ed25519,
            bytes: vec![0; 31],
        };
        let res = VerifyingKey::from_pubkey(&pk);
        assert!(matches!(res, Err(CryptoError::InvalidKey(_))));
    }

    #[test]
    fn from_seed_is_deterministic() {
        let seed = [0x42u8; 32];
        let a = SigningKey::from_seed(&seed);
        let b = SigningKey::from_seed(&seed);
        assert_eq!(a.export(), b.export());
        assert_eq!(a.verifying_key(), b.verifying_key());
    }

    #[test]
    fn signing_key_debug_redacts_secret() {
        let sk = SigningKey::from_seed(&[0x11; 32]);
        let s = format!("{sk:?}");
        assert!(s.contains("redacted"));
        assert!(!s.contains(&hex::encode(sk.export())));
    }

    #[test]
    fn kem_dh_is_symmetric() {
        let alice_sk = KeyExchangeSecret::generate();
        let bob_sk = KeyExchangeSecret::generate();
        let alice_pk = alice_sk.public_key();
        let bob_pk = bob_sk.public_key();

        let alice_shared = alice_sk.diffie_hellman(&bob_pk);
        let bob_shared = bob_sk.diffie_hellman(&alice_pk);

        assert_eq!(alice_shared.as_bytes(), bob_shared.as_bytes());
    }

    #[test]
    fn kem_public_is_deterministic_from_secret() {
        let bytes = [0x77u8; 32];
        let a = KeyExchangeSecret::from_bytes(bytes);
        let b = KeyExchangeSecret::from_bytes(bytes);
        assert_eq!(a.public_key(), b.public_key());
    }

    #[test]
    fn kem_secret_debug_redacts() {
        let sk = KeyExchangeSecret::from_bytes([0xAB; 32]);
        let s = format!("{sk:?}");
        assert!(s.contains("redacted"));
        assert!(!s.contains("ababab"));
    }

    #[test]
    fn shared_secret_debug_redacts() {
        let a = KeyExchangeSecret::generate();
        let b = KeyExchangeSecret::generate();
        let shared = a.diffie_hellman(&b.public_key());
        let s = format!("{shared:?}");
        assert!(s.contains("redacted"));
    }

    #[test]
    fn import_export_roundtrip() {
        let sk = SigningKey::generate();
        let bytes = sk.export();
        let sk2 = SigningKey::import(&bytes);
        assert_eq!(sk.export(), sk2.export());
    }

    proptest::proptest! {
        #[test]
        fn seed_determinism(seed: [u8; 32]) {
            let a = SigningKey::from_seed(&seed);
            let b = SigningKey::from_seed(&seed);
            proptest::prop_assert_eq!(a.verifying_key(), b.verifying_key());
        }

        #[test]
        fn kem_symmetry(a_bytes: [u8; 32], b_bytes: [u8; 32]) {
            let a = KeyExchangeSecret::from_bytes(a_bytes);
            let b = KeyExchangeSecret::from_bytes(b_bytes);
            let shared_ab = a.diffie_hellman(&b.public_key());
            let shared_ba = b.diffie_hellman(&a.public_key());
            proptest::prop_assert_eq!(shared_ab.as_bytes(), shared_ba.as_bytes());
        }
    }
}
