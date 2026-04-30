//! Authenticated encryption for confidential inference prompts.
//!
//! Implements the user → enclave prompt encryption envelope:
//!
//! 1. User generates an ephemeral X25519 keypair.
//! 2. ECDH against the enclave's public key → shared secret.
//! 3. HKDF-SHA256(shared_secret, info="arknet-tee-prompt-v1") → 32-byte key.
//! 4. ChaCha20-Poly1305(key, random_nonce, prompt) → ciphertext.
//! 5. Envelope = (ephemeral_pubkey, nonce, ciphertext).
//!
//! The enclave decrypts by performing the same ECDH with its private key.
//! The host OS sees only encrypted blobs.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use rand::RngCore;

use crate::errors::{CryptoError, Result};
use crate::keys::{KeyExchangePublic, KeyExchangeSecret};

/// HKDF info string — domain-separates this key derivation from all
/// other uses of X25519 shared secrets in arknet.
const HKDF_INFO: &[u8] = b"arknet-tee-prompt-v1";

/// ChaCha20-Poly1305 nonce length.
pub const NONCE_LEN: usize = 12;

/// Poly1305 authentication tag overhead.
pub const TAG_LEN: usize = 16;

/// Derive a symmetric key from a raw ECDH shared secret via HKDF-SHA256.
fn derive_symmetric_key(shared: &[u8; 32]) -> [u8; 32] {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, shared);
    let mut key = [0u8; 32];
    hk.expand(HKDF_INFO, &mut key)
        .expect("32-byte output is valid for HKDF-SHA256");
    key
}

/// Encrypted prompt envelope. Sent over the wire as part of
/// `InferenceJobRequest.encrypted_prompt`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedPrompt {
    /// User's ephemeral X25519 public key (32 bytes).
    pub ephemeral_pubkey: [u8; 32],
    /// 12-byte nonce.
    pub nonce: [u8; NONCE_LEN],
    /// Ciphertext (prompt + 16-byte Poly1305 tag).
    pub ciphertext: Vec<u8>,
}

/// Encrypt a prompt to an enclave's public key.
///
/// Returns a [`SealedPrompt`] containing the ephemeral pubkey, nonce,
/// and ciphertext. The enclave decrypts with [`open_prompt`].
pub fn seal_prompt(plaintext: &[u8], enclave_pubkey: &KeyExchangePublic) -> Result<SealedPrompt> {
    if plaintext.is_empty() {
        return Err(CryptoError::InvalidInput("empty plaintext".into()));
    }

    let ephemeral = KeyExchangeSecret::generate();
    let ephemeral_pub = ephemeral.public_key();
    let shared = ephemeral.diffie_hellman(enclave_pubkey);
    let sym_key = derive_symmetric_key(shared.as_bytes());

    let cipher = ChaCha20Poly1305::new_from_slice(&sym_key)
        .map_err(|e| CryptoError::InvalidInput(format!("cipher init: {e}")))?;

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| CryptoError::InvalidInput(format!("encrypt: {e}")))?;

    Ok(SealedPrompt {
        ephemeral_pubkey: *ephemeral_pub.as_bytes(),
        nonce: nonce_bytes,
        ciphertext,
    })
}

/// Decrypt a sealed prompt using the enclave's private key.
///
/// Called inside the TEE by the enclave binary. The host OS never
/// has access to `enclave_secret`.
pub fn open_prompt(sealed: &SealedPrompt, enclave_secret: &KeyExchangeSecret) -> Result<Vec<u8>> {
    let peer = KeyExchangePublic(sealed.ephemeral_pubkey);
    let shared = enclave_secret.diffie_hellman(&peer);
    let sym_key = derive_symmetric_key(shared.as_bytes());

    let cipher = ChaCha20Poly1305::new_from_slice(&sym_key)
        .map_err(|e| CryptoError::InvalidInput(format!("cipher init: {e}")))?;

    let nonce = Nonce::from_slice(&sealed.nonce);

    cipher
        .decrypt(nonce, sealed.ciphertext.as_ref())
        .map_err(|e| CryptoError::InvalidInput(format!("decrypt: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let enclave = KeyExchangeSecret::generate();
        let enclave_pub = enclave.public_key();

        let prompt = b"What is the meaning of life?";
        let sealed = seal_prompt(prompt, &enclave_pub).unwrap();

        assert_ne!(sealed.ciphertext, prompt.to_vec());
        assert_eq!(sealed.ciphertext.len(), prompt.len() + TAG_LEN);
        assert_eq!(sealed.ephemeral_pubkey.len(), 32);
        assert_eq!(sealed.nonce.len(), NONCE_LEN);

        let decrypted = open_prompt(&sealed, &enclave).unwrap();
        assert_eq!(decrypted, prompt);
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let enclave = KeyExchangeSecret::generate();
        let enclave_pub = enclave.public_key();
        let wrong_key = KeyExchangeSecret::generate();

        let sealed = seal_prompt(b"secret", &enclave_pub).unwrap();
        let result = open_prompt(&sealed, &wrong_key);
        assert!(result.is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let enclave = KeyExchangeSecret::generate();
        let enclave_pub = enclave.public_key();

        let mut sealed = seal_prompt(b"secret", &enclave_pub).unwrap();
        sealed.ciphertext[0] ^= 0xff;

        let result = open_prompt(&sealed, &enclave);
        assert!(result.is_err());
    }

    #[test]
    fn empty_plaintext_rejected() {
        let enclave = KeyExchangeSecret::generate();
        let enclave_pub = enclave.public_key();
        assert!(seal_prompt(b"", &enclave_pub).is_err());
    }

    #[test]
    fn two_seals_produce_different_ciphertexts() {
        let enclave = KeyExchangeSecret::generate();
        let enclave_pub = enclave.public_key();
        let prompt = b"same prompt";

        let a = seal_prompt(prompt, &enclave_pub).unwrap();
        let b = seal_prompt(prompt, &enclave_pub).unwrap();

        assert_ne!(a.ephemeral_pubkey, b.ephemeral_pubkey);
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    #[test]
    fn large_prompt_works() {
        let enclave = KeyExchangeSecret::generate();
        let enclave_pub = enclave.public_key();
        let prompt = vec![0x42; 100_000];

        let sealed = seal_prompt(&prompt, &enclave_pub).unwrap();
        let decrypted = open_prompt(&sealed, &enclave).unwrap();
        assert_eq!(decrypted, prompt);
    }
}
