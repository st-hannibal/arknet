//! Ed25519 wallet for signing inference requests.
//!
//! The wallet holds a [`SigningKey`] and derives the on-chain [`Address`]
//! using the same `blake3(pubkey_bytes)[0..20]` rule as the chain.
//!
//! # File format
//!
//! The on-disk format is 64 bytes: `32 secret || 32 public` (Ed25519).
//! This is identical to the node key file at `<data-dir>/keys/node.key`.
//!
//! # Default path
//!
//! `~/.arknet/wallet.key`, overridable via `ARKNET_WALLET_PATH`.

use std::path::{Path, PathBuf};

use arknet_common::types::{Address, PubKey, Signature};
use arknet_crypto::keys::SigningKey;

use crate::errors::{Result, SdkError};

/// Default wallet directory relative to the user's home.
const DEFAULT_WALLET_DIR: &str = ".arknet";

/// Default wallet file name.
const DEFAULT_WALLET_FILE: &str = "wallet.key";

/// Environment variable that overrides the default wallet path.
const WALLET_PATH_ENV: &str = "ARKNET_WALLET_PATH";

/// Ed25519 wallet for signing arknet transactions and inference requests.
///
/// Holds a signing key and its derived address. The address is computed
/// as `blake3(pubkey_bytes)[0..20]`, matching `derive_user_address` in
/// `arknet-compute`.
pub struct Wallet {
    signing_key: SigningKey,
    address: Address,
}

impl Wallet {
    /// Generate a new wallet with a random Ed25519 keypair.
    pub fn create() -> Self {
        let signing_key = SigningKey::generate();
        let address = derive_address(&signing_key);
        Self {
            signing_key,
            address,
        }
    }

    /// Load a wallet from a 64-byte key file (32 secret || 32 public).
    ///
    /// Returns an error if the file doesn't exist, has the wrong size,
    /// or the public key doesn't match the secret key.
    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read(path)
            .map_err(|e| SdkError::Wallet(format!("failed to read {}: {e}", path.display())))?;
        if data.len() != 64 {
            return Err(SdkError::Wallet(format!(
                "wallet file must be 64 bytes, got {}",
                data.len()
            )));
        }
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&data[..32]);
        let signing_key = SigningKey::import(&secret);

        // Verify the stored public key matches the derived one.
        let mut stored_pub = [0u8; 32];
        stored_pub.copy_from_slice(&data[32..64]);
        let derived_pub = signing_key.verifying_key().to_bytes();
        if stored_pub != derived_pub {
            return Err(SdkError::Wallet(
                "stored public key does not match secret key".into(),
            ));
        }

        let address = derive_address(&signing_key);
        // Zeroize the secret copy on the stack.
        zeroize::Zeroize::zeroize(&mut secret);

        Ok(Self {
            signing_key,
            address,
        })
    }

    /// Save the wallet to a 64-byte key file (32 secret || 32 public).
    ///
    /// Creates parent directories if they don't exist. Sets file
    /// permissions to owner-only on Unix.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                SdkError::Wallet(format!(
                    "failed to create directory {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&self.signing_key.export());
        buf[32..].copy_from_slice(&self.signing_key.verifying_key().to_bytes());

        std::fs::write(path, buf)
            .map_err(|e| SdkError::Wallet(format!("failed to write {}: {e}", path.display())))?;

        // Best-effort: restrict permissions on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }

        // Zeroize the buffer.
        zeroize::Zeroize::zeroize(&mut buf);

        Ok(())
    }

    /// Create a wallet deterministically from a 32-byte seed.
    ///
    /// Useful for test fixtures and HKDF-derived wallets.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let signing_key = SigningKey::from_seed(seed);
        let address = derive_address(&signing_key);
        Self {
            signing_key,
            address,
        }
    }

    /// The 20-byte on-chain address for this wallet.
    pub fn address(&self) -> &Address {
        &self.address
    }

    /// Sign an arbitrary message with the wallet's Ed25519 key.
    ///
    /// Returns a scheme-tagged [`Signature`] suitable for on-wire use.
    pub fn sign(&self, message: &[u8]) -> Signature {
        arknet_crypto::signatures::sign(&self.signing_key, message)
    }

    /// The scheme-tagged public key for this wallet.
    pub fn public_key(&self) -> PubKey {
        self.signing_key.verifying_key().to_pubkey()
    }

    /// Access the underlying signing key (for advanced usage and tests).
    #[cfg(test)]
    pub(crate) fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }

    /// Resolve the default wallet path.
    ///
    /// Checks `ARKNET_WALLET_PATH` env var first, then falls back to
    /// `~/.arknet/wallet.key`.
    pub fn default_path() -> Result<PathBuf> {
        if let Ok(path) = std::env::var(WALLET_PATH_ENV) {
            return Ok(PathBuf::from(path));
        }
        let home = dirs_next::home_dir()
            .ok_or_else(|| SdkError::Wallet("could not determine home directory".into()))?;
        Ok(home.join(DEFAULT_WALLET_DIR).join(DEFAULT_WALLET_FILE))
    }

    /// Load the wallet from the default path (env var or `~/.arknet/wallet.key`).
    pub fn load_default() -> Result<Self> {
        let path = Self::default_path()?;
        Self::load(&path)
    }
}

impl std::fmt::Debug for Wallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wallet")
            .field("address", &self.address)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// Derive the 20-byte address from a signing key.
///
/// Matches `arknet_compute::wire::derive_user_address`: `blake3(pubkey.bytes)[0..20]`.
fn derive_address(key: &SigningKey) -> Address {
    let pubkey = key.verifying_key().to_pubkey();
    let digest = blake3::hash(&pubkey.bytes);
    let bytes = digest.as_bytes();
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes[..20]);
    Address::new(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn create_produces_valid_wallet() {
        let w = Wallet::create();
        // Address is 20 bytes.
        assert_eq!(w.address().as_bytes().len(), 20);
        // Public key is Ed25519.
        let pk = w.public_key();
        assert_eq!(pk.scheme, arknet_common::SignatureScheme::Ed25519);
        assert_eq!(pk.bytes.len(), 32);
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.key");
        let w1 = Wallet::create();
        w1.save(&path).unwrap();

        let w2 = Wallet::load(&path).unwrap();
        assert_eq!(w1.address(), w2.address());
        assert_eq!(w1.public_key(), w2.public_key());
    }

    #[test]
    fn from_seed_is_deterministic() {
        let seed = [0x42u8; 32];
        let w1 = Wallet::from_seed(&seed);
        let w2 = Wallet::from_seed(&seed);
        assert_eq!(w1.address(), w2.address());
        assert_eq!(w1.public_key(), w2.public_key());
    }

    #[test]
    fn address_matches_compute_derivation() {
        let seed = [0xAB; 32];
        let w = Wallet::from_seed(&seed);
        // Replicate the derivation manually.
        let pk = w.public_key();
        let digest = blake3::hash(&pk.bytes);
        let mut expected = [0u8; 20];
        expected.copy_from_slice(&digest.as_bytes()[..20]);
        assert_eq!(w.address().as_bytes(), &expected);
    }

    #[test]
    fn sign_produces_valid_signature() {
        let w = Wallet::create();
        let msg = b"arknet test message";
        let sig = w.sign(msg);
        let pk = w.public_key();
        arknet_crypto::signatures::verify(&pk, msg, &sig).unwrap();
    }

    #[test]
    fn load_rejects_wrong_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.key");
        std::fs::write(&path, [0u8; 32]).unwrap();
        let err = Wallet::load(&path).unwrap_err();
        assert!(err.to_string().contains("64 bytes"));
    }

    #[test]
    fn load_rejects_mismatched_pubkey() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("mismatch.key");
        let w = Wallet::create();
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&w.signing_key().export());
        // Write garbage in the public key half.
        buf[32..].copy_from_slice(&[0xFF; 32]);
        std::fs::write(&path, buf).unwrap();
        let err = Wallet::load(&path).unwrap_err();
        assert!(err.to_string().contains("does not match"));
    }

    #[test]
    fn debug_redacts_secret() {
        let w = Wallet::create();
        let s = format!("{w:?}");
        assert!(s.contains("redacted"));
    }

    #[test]
    fn default_path_uses_env_var() {
        let custom = "/tmp/custom_wallet.key";
        std::env::set_var("ARKNET_WALLET_PATH", custom);
        let path = Wallet::default_path().unwrap();
        assert_eq!(path, PathBuf::from(custom));
        std::env::remove_var("ARKNET_WALLET_PATH");
    }

    #[test]
    fn save_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("deep").join("nested").join("wallet.key");
        let w = Wallet::create();
        w.save(&path).unwrap();
        assert!(path.exists());
    }
}
