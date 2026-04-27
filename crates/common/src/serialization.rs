//! Serialization helpers for arknet.
//!
//! - **borsh** is the *only* encoding used for on-chain bytes. Canonical,
//!   deterministic, consensus-critical. Every `struct` on the wire must
//!   derive [`BorshSerialize`] + [`BorshDeserialize`].
//! - **JSON** is used for human-facing surfaces: RPC responses, config
//!   dumps, CLI output.
//! - **Hex** is used for displaying hashes / keys in logs and error messages.
//!
//! [`BorshSerialize`]: borsh::BorshSerialize
//! [`BorshDeserialize`]: borsh::BorshDeserialize

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{de::DeserializeOwned, Serialize};

use crate::errors::{CommonError, Result};

// ─── Borsh (consensus-critical) ───────────────────────────────────────────

/// Encode a value to canonical borsh bytes.
///
/// # Security
/// Output of this function is consumed by consensus. Any non-determinism
/// here would break state agreement across validators.
pub fn to_borsh<T: BorshSerialize>(value: &T) -> Result<Vec<u8>> {
    borsh::to_vec(value).map_err(|e| CommonError::Borsh(e.to_string()))
}

/// Decode a borsh-encoded byte slice.
pub fn from_borsh<T: BorshDeserialize>(bytes: &[u8]) -> Result<T> {
    borsh::from_slice(bytes).map_err(|e| CommonError::Borsh(e.to_string()))
}

// ─── JSON (human-facing) ──────────────────────────────────────────────────

/// Encode a value as JSON.
pub fn to_json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(Into::into)
}

/// Encode a value as pretty-printed JSON (for CLI output).
pub fn to_json_pretty<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string_pretty(value).map_err(Into::into)
}

/// Decode a JSON string.
pub fn from_json<T: DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(Into::into)
}

// ─── Hex (display-only) ───────────────────────────────────────────────────

/// Encode bytes as lowercase hex with no `0x` prefix.
pub fn to_hex(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// Decode hex with or without a `0x` prefix.
pub fn from_hex(s: &str) -> Result<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    hex::decode(s).map_err(|e| CommonError::InvalidArgument(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Address, PubKey, Signature, SignatureScheme};

    #[test]
    fn borsh_roundtrip_primitive() {
        let v: u128 = 42_000_000_000;
        let bytes = to_borsh(&v).unwrap();
        let back: u128 = from_borsh(&bytes).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn borsh_roundtrip_address() {
        let a = Address::new([0xAB; 20]);
        let bytes = to_borsh(&a).unwrap();
        let back: Address = from_borsh(&bytes).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn borsh_roundtrip_pubkey() {
        let pk = PubKey::ed25519([0xCD; 32]);
        let bytes = to_borsh(&pk).unwrap();
        let back: PubKey = from_borsh(&bytes).unwrap();
        assert_eq!(pk, back);
    }

    #[test]
    fn borsh_is_deterministic() {
        let sig = Signature::ed25519([0xFF; 64]);
        let a = to_borsh(&sig).unwrap();
        let b = to_borsh(&sig).unwrap();
        assert_eq!(a, b, "borsh output must be bit-identical across calls");
    }

    #[test]
    fn borsh_encodes_scheme_byte_first() {
        // The scheme tag is the first byte of the encoded PubKey.
        // This invariant is the load-bearing reason for crypto agility:
        // validators can tell what scheme an old key uses from byte 0.
        let pk = PubKey::ed25519([0x00; 32]);
        let bytes = to_borsh(&pk).unwrap();
        assert_eq!(bytes[0], SignatureScheme::Ed25519 as u8);
    }

    #[test]
    fn borsh_rejects_truncated_input() {
        let pk = PubKey::ed25519([0xAA; 32]);
        let mut bytes = to_borsh(&pk).unwrap();
        bytes.truncate(bytes.len() - 1);
        let res: Result<PubKey> = from_borsh(&bytes);
        assert!(res.is_err(), "decoding truncated pubkey must fail");
    }

    #[test]
    fn json_roundtrip() {
        let a = Address::new([0x99; 20]);
        let s = to_json(&a).unwrap();
        let back: Address = from_json(&s).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn json_pretty_differs_from_json() {
        let a = Address::new([0x01; 20]);
        let plain = to_json(&a).unwrap();
        let pretty = to_json_pretty(&a).unwrap();
        assert_ne!(plain, pretty);
    }

    #[test]
    fn hex_roundtrip_with_and_without_prefix() {
        let bytes = vec![0xde, 0xad, 0xbe, 0xef];
        assert_eq!(to_hex(&bytes), "deadbeef");
        assert_eq!(from_hex("deadbeef").unwrap(), bytes);
        assert_eq!(from_hex("0xdeadbeef").unwrap(), bytes);
    }

    #[test]
    fn hex_rejects_garbage() {
        assert!(from_hex("not hex").is_err());
    }
}
