//! Threshold cryptography — trait-level scaffold.
//!
//! Full Shutter-style threshold encryption lands in Phase 2 for the
//! encrypted mempool. This module only defines the shape so downstream
//! code can be written against stable abstractions.
//!
//! See [`docs/ARCHITECTURE.md`] §8 (MEV Protection) and
//! [`docs/SECURITY.md`] §8 (Defense Matrix).

/// Result of distributed key generation: the group public key plus one
/// key share per participating party.
pub type DkgOutput<S> = (
    <S as ThresholdScheme>::PublicKey,
    Vec<<S as ThresholdScheme>::KeyShare>,
);

/// A threshold encryption scheme.
///
/// `t-of-n` reconstruction: any `threshold` out of `total` parties can jointly
/// decrypt a ciphertext. No individual party can decrypt alone.
///
/// Implementations land in Phase 2. At Phase 0 this trait is declared so
/// mempool / consensus code can compile against it.
pub trait ThresholdScheme {
    /// Public key type — published on-chain as the mempool encryption key.
    type PublicKey;
    /// Per-party private key share.
    type KeyShare;
    /// Ciphertext produced by the public key.
    type Ciphertext;
    /// Per-party partial decryption.
    type DecryptionShare;
    /// Error produced by all fallible operations.
    type Error;

    /// Run distributed key generation producing one `KeyShare` per party.
    ///
    /// Phase 2 implementation: Pedersen DKG (Shutter-style).
    fn dkg(total: usize, threshold: usize) -> Result<DkgOutput<Self>, Self::Error>;

    /// Encrypt a plaintext to the group public key.
    fn encrypt(pk: &Self::PublicKey, plaintext: &[u8]) -> Result<Self::Ciphertext, Self::Error>;

    /// Produce this party's decryption share for the given ciphertext.
    fn decrypt_share(
        share: &Self::KeyShare,
        ciphertext: &Self::Ciphertext,
    ) -> Result<Self::DecryptionShare, Self::Error>;

    /// Combine `>= threshold` decryption shares into the plaintext.
    fn combine(
        ciphertext: &Self::Ciphertext,
        shares: &[Self::DecryptionShare],
    ) -> Result<Vec<u8>, Self::Error>;
}
