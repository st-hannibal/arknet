//! Error types for the crypto crate.

use thiserror::Error;

/// Cryptographic error hierarchy. Never leak secret material via these.
#[derive(Debug, Error)]
pub enum CryptoError {
    /// A signature verification failed.
    #[error("signature verification failed")]
    SignatureInvalid,

    /// A public key could not be parsed or is invalid.
    #[error("invalid key: {0}")]
    InvalidKey(String),

    /// A signature could not be parsed.
    #[error("invalid signature: {0}")]
    InvalidSignature(String),

    /// The requested scheme is known but not implemented at this phase.
    #[error("scheme not supported: {0}")]
    SchemeNotSupported(String),

    /// Merkle proof verification failed.
    #[error("merkle proof invalid: {0}")]
    MerkleInvalid(String),

    /// VRF proof verification failed.
    #[error("vrf proof invalid")]
    VrfInvalid,

    /// Argon2 / KDF failure.
    #[error("kdf error: {0}")]
    Kdf(String),

    /// A length / range check failed on inputs.
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

/// Crate-local result alias.
pub type Result<T, E = CryptoError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_does_not_leak_values() {
        // Sanity: error messages don't accidentally embed whole buffers.
        let e = CryptoError::InvalidKey("bad curve point".into());
        assert_eq!(e.to_string(), "invalid key: bad curve point");
    }

    #[test]
    fn signature_invalid_is_opaque() {
        let e = CryptoError::SignatureInvalid;
        // Verification failures must be opaque — no hints about which byte
        // failed, which would enable side-channel attacks on verifiers.
        assert_eq!(e.to_string(), "signature verification failed");
    }
}
