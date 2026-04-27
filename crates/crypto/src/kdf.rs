//! Argon2id key derivation for passphrase-protected storage.
//!
//! Used to derive symmetric keys from user passphrases for encrypting
//! on-disk operator keys (see [`docs/SECURITY.md`] §5).
//!
//! Defaults follow OWASP Argon2id recommendations for interactive use:
//! - **m = 19,456 KiB** (~19 MiB)
//! - **t = 2** iterations
//! - **p = 1** lane
//!
//! These are deliberately slow (~100ms on a modern laptop) to resist
//! offline brute-force of leaked keystores.

use argon2::{Algorithm, Argon2, Params, Version};
use zeroize::Zeroizing;

use crate::errors::{CryptoError, Result};

/// Length of a KDF output key in bytes.
pub const DERIVED_KEY_LEN: usize = 32;

/// Length of the required salt.
pub const SALT_LEN: usize = 32;

/// Derive a 32-byte key from a passphrase + salt using Argon2id.
///
/// # Security
/// - Pass a unique 32-byte random salt per keystore (store it alongside the
///   ciphertext).
/// - The returned buffer is wrapped in [`Zeroizing`] and will scrub on drop.
/// - The passphrase slice must already be zeroized by the caller when done.
pub fn derive_key(
    passphrase: &[u8],
    salt: &[u8; SALT_LEN],
) -> Result<Zeroizing<[u8; DERIVED_KEY_LEN]>> {
    if passphrase.is_empty() {
        return Err(CryptoError::InvalidInput(
            "passphrase must not be empty".into(),
        ));
    }

    // OWASP 2024 recommended params for interactive use.
    let params = Params::new(19_456, 2, 1, Some(DERIVED_KEY_LEN))
        .map_err(|e| CryptoError::Kdf(e.to_string()))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut out = Zeroizing::new([0u8; DERIVED_KEY_LEN]);
    argon
        .hash_password_into(passphrase, salt, out.as_mut())
        .map_err(|e| CryptoError::Kdf(e.to_string()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Use reduced-memory params in tests to keep them fast.
    fn derive_fast(
        passphrase: &[u8],
        salt: &[u8; SALT_LEN],
    ) -> Result<Zeroizing<[u8; DERIVED_KEY_LEN]>> {
        let params = Params::new(8, 1, 1, Some(DERIVED_KEY_LEN))
            .map_err(|e| CryptoError::Kdf(e.to_string()))?;
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

        let mut out = Zeroizing::new([0u8; DERIVED_KEY_LEN]);
        argon
            .hash_password_into(passphrase, salt, out.as_mut())
            .map_err(|e| CryptoError::Kdf(e.to_string()))?;
        Ok(out)
    }

    #[test]
    fn derive_key_is_deterministic() {
        let salt = [0x42; SALT_LEN];
        let pw = b"correct horse battery staple";
        let a = derive_fast(pw, &salt).unwrap();
        let b = derive_fast(pw, &salt).unwrap();
        assert_eq!(a.as_slice(), b.as_slice());
    }

    #[test]
    fn different_salts_yield_different_keys() {
        let pw = b"same password";
        let key_a = derive_fast(pw, &[0x11; SALT_LEN]).unwrap();
        let key_b = derive_fast(pw, &[0x22; SALT_LEN]).unwrap();
        assert_ne!(key_a.as_slice(), key_b.as_slice());
    }

    #[test]
    fn different_passphrases_yield_different_keys() {
        let salt = [0x42; SALT_LEN];
        let a = derive_fast(b"pass1", &salt).unwrap();
        let b = derive_fast(b"pass2", &salt).unwrap();
        assert_ne!(a.as_slice(), b.as_slice());
    }

    #[test]
    fn empty_passphrase_is_rejected() {
        let salt = [0; SALT_LEN];
        assert!(derive_key(&[], &salt).is_err());
    }

    #[test]
    fn derived_key_length_is_32() {
        let salt = [0; SALT_LEN];
        let key = derive_fast(b"x", &salt).unwrap();
        assert_eq!(key.len(), 32);
    }

    // Real-parameters smoke test (slow — run only when specifically requested).
    #[test]
    #[ignore = "slow: runs full OWASP Argon2id params"]
    fn real_params_works() {
        let salt = [0u8; SALT_LEN];
        let _ = derive_key(b"test", &salt).unwrap();
    }
}
