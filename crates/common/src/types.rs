//! Core primitive types used throughout arknet.
//!
//! These types are the on-the-wire and on-chain vocabulary. Layout must stay
//! stable: any breaking change is a protocol hard fork.
//!
//! # Crypto agility
//!
//! Signatures, public keys, and KEM keys carry a [`SignatureScheme`] /
//! [`KemScheme`] / [`VrfScheme`] tag in their first byte. At launch only the
//! `0x01` variants (Ed25519, X25519, Ristretto255 VRF, BLS12-381 threshold)
//! are implemented, but the wire format reserves the remaining space so a
//! governance-scheduled post-quantum migration can ship without breaking
//! transaction encoding.
//!
//! See `docs/SECURITY.md` §4 (Cryptographic Primitives) and §12 (PQ Migration).

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::errors::{CommonError, Result};

// ─── Hashes & identifiers ─────────────────────────────────────────────────

/// 256-bit digest. SHA-256 on-chain, BLAKE3 for fast local hashing.
pub type Hash256 = [u8; 32];

/// 20-byte account address. Derived as `blake3(pubkey_bytes)[0..20]`.
///
/// Addresses are displayed in bech32 as `ark1…` (mainnet) / `arktest1…` (testnet).
/// See `docs/PROTOCOL_SPEC.md` §2.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Debug,
    Default,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct Address(pub [u8; 20]);

impl Address {
    /// Construct from raw bytes.
    pub const fn new(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// Borrow the underlying byte array.
    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Hex-encode without `0x` prefix.
    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    /// Parse a hex-encoded address (with or without `0x` prefix).
    pub fn from_hex(s: &str) -> Result<Self> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        let bytes = hex::decode(s).map_err(|e| CommonError::InvalidArgument(e.to_string()))?;
        if bytes.len() != 20 {
            return Err(CommonError::InvalidArgument(format!(
                "expected 20-byte address, got {} bytes",
                bytes.len()
            )));
        }
        let mut out = [0u8; 20];
        out.copy_from_slice(&bytes);
        Ok(Self(out))
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x{}", self.to_hex())
    }
}

/// Node identifier — 32-byte hash of the node's consensus pubkey.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Debug,
    Default,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct NodeId(pub [u8; 32]);

impl NodeId {
    /// Construct from raw bytes.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the underlying byte array.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "node:{}", hex::encode(self.0))
    }
}

/// Inference job identifier. Unique per job.
///
/// Derived as `blake3(user_pubkey || router_id || nonce || timestamp_ms)`.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Debug,
    Default,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct JobId(pub [u8; 32]);

impl JobId {
    /// Construct from raw bytes.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "job:{}", hex::encode(self.0))
    }
}

/// Computation pool identifier — `hash(model_id || quantization)[0..16]`.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Debug,
    Default,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct PoolId(pub [u8; 16]);

impl PoolId {
    /// Construct from raw bytes.
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

impl std::fmt::Display for PoolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pool:{}", hex::encode(self.0))
    }
}

/// Payment channel identifier.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Debug,
    Default,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct ChannelId(pub [u8; 32]);

impl ChannelId {
    /// Construct from raw bytes.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

// ─── Numeric types ────────────────────────────────────────────────────────

/// Token amount in atomic units. 1 ARK = [`ATOMS_PER_ARK`] `ark_atom` (9 decimals).
///
/// Always `u128`. Never represent token amounts as floats.
pub type Amount = u128;

/// Block height.
pub type Height = u64;

/// Unix timestamp in milliseconds.
pub type Timestamp = u64;

/// Atomic units per whole ARK token.
pub const ATOMS_PER_ARK: Amount = 1_000_000_000;

/// Protocol-level hard cap on ARK supply (1B ARK).
pub const ARK_SUPPLY_CAP: Amount = 1_000_000_000 * ATOMS_PER_ARK;

// ─── Crypto scheme tags ───────────────────────────────────────────────────

/// Signature scheme identifier. First byte of every encoded [`Signature`] / [`PubKey`].
///
/// Versioned from day one so a post-quantum migration is a protocol upgrade,
/// not a wire-format rewrite. See `docs/SECURITY.md` §12.
#[repr(u8)]
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Debug,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
#[borsh(use_discriminant = true)]
pub enum SignatureScheme {
    /// Ed25519 (EdDSA over Curve25519). Genesis default.
    Ed25519 = 0x01,
    /// Reserved for ML-DSA / Dilithium (NIST FIPS 204). Post-quantum.
    Dilithium = 0x02,
    /// Reserved for Falcon (NIST FIPS 205 draft). Post-quantum, smaller sigs than Dilithium.
    Falcon = 0x03,
    /// Reserved for SLH-DSA / SPHINCS+ (NIST FIPS 205). Hash-based, stateless.
    Sphincs = 0x04,
    /// Reserved for hybrid Ed25519 + Dilithium (belt-and-braces during migration).
    HybridEd25519Dilithium = 0x05,
}

impl SignatureScheme {
    /// Schemes currently implemented and accepted by consensus.
    pub const fn is_active(&self) -> bool {
        matches!(self, SignatureScheme::Ed25519)
    }

    /// Expected public-key length in bytes for this scheme.
    pub const fn pubkey_len(&self) -> usize {
        match self {
            SignatureScheme::Ed25519 => 32,
            SignatureScheme::Dilithium => 1312,
            SignatureScheme::Falcon => 897,
            SignatureScheme::Sphincs => 32,
            SignatureScheme::HybridEd25519Dilithium => 32 + 1312,
        }
    }

    /// Expected signature length in bytes for this scheme.
    pub const fn signature_len(&self) -> usize {
        match self {
            SignatureScheme::Ed25519 => 64,
            SignatureScheme::Dilithium => 2420,
            SignatureScheme::Falcon => 666,
            SignatureScheme::Sphincs => 17088,
            SignatureScheme::HybridEd25519Dilithium => 64 + 2420,
        }
    }
}

/// Key-encapsulation-mechanism (KEM) identifier. Used for prompt encryption.
#[repr(u8)]
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Debug,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
#[borsh(use_discriminant = true)]
pub enum KemScheme {
    /// X25519 elliptic-curve Diffie-Hellman. Genesis default.
    X25519 = 0x01,
    /// Reserved for ML-KEM / Kyber (NIST FIPS 203). Post-quantum.
    Kyber = 0x02,
    /// Reserved for hybrid X25519 + Kyber during migration.
    HybridX25519Kyber = 0x03,
}

impl KemScheme {
    /// Schemes currently implemented.
    pub const fn is_active(&self) -> bool {
        matches!(self, KemScheme::X25519)
    }
}

/// Verifiable Random Function scheme — used for unpredictable verifier selection.
#[repr(u8)]
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Debug,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
#[borsh(use_discriminant = true)]
pub enum VrfScheme {
    /// ECVRF over Ristretto255. Genesis default.
    Ristretto255 = 0x01,
    /// Reserved for a future lattice-based VRF construction.
    LatticeVrf = 0x02,
}

impl VrfScheme {
    /// Schemes currently implemented.
    pub const fn is_active(&self) -> bool {
        matches!(self, VrfScheme::Ristretto255)
    }
}

// ─── Versioned public keys & signatures ───────────────────────────────────

/// A scheme-tagged public key.
///
/// On-chain encoding is `scheme_byte || key_bytes`. Length of `key_bytes`
/// is determined by [`SignatureScheme::pubkey_len`].
#[derive(
    Clone, PartialEq, Eq, Hash, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub struct PubKey {
    /// Scheme identifier.
    pub scheme: SignatureScheme,
    /// Raw key bytes (length must match `scheme.pubkey_len()`).
    pub bytes: Vec<u8>,
}

impl PubKey {
    /// Construct a new public key. Returns an error if `bytes.len()` doesn't match the scheme.
    pub fn new(scheme: SignatureScheme, bytes: Vec<u8>) -> Result<Self> {
        if bytes.len() != scheme.pubkey_len() {
            return Err(CommonError::InvalidArgument(format!(
                "pubkey length for {:?} must be {}, got {}",
                scheme,
                scheme.pubkey_len(),
                bytes.len()
            )));
        }
        Ok(Self { scheme, bytes })
    }

    /// Construct an Ed25519 pubkey from a fixed-size array.
    pub fn ed25519(bytes: [u8; 32]) -> Self {
        Self {
            scheme: SignatureScheme::Ed25519,
            bytes: bytes.to_vec(),
        }
    }
}

/// A scheme-tagged signature.
///
/// On-chain encoding is `scheme_byte || sig_bytes`.
#[derive(
    Clone, PartialEq, Eq, Hash, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub struct Signature {
    /// Scheme identifier.
    pub scheme: SignatureScheme,
    /// Raw signature bytes (length must match `scheme.signature_len()`).
    pub bytes: Vec<u8>,
}

impl Signature {
    /// Construct a new signature. Returns an error if `bytes.len()` doesn't match the scheme.
    pub fn new(scheme: SignatureScheme, bytes: Vec<u8>) -> Result<Self> {
        if bytes.len() != scheme.signature_len() {
            return Err(CommonError::InvalidArgument(format!(
                "signature length for {:?} must be {}, got {}",
                scheme,
                scheme.signature_len(),
                bytes.len()
            )));
        }
        Ok(Self { scheme, bytes })
    }

    /// Construct an Ed25519 signature from a fixed-size array.
    pub fn ed25519(bytes: [u8; 64]) -> Self {
        Self {
            scheme: SignatureScheme::Ed25519,
            bytes: bytes.to_vec(),
        }
    }
}

// ─── Role bitmap ──────────────────────────────────────────────────────────

/// Bitmap of active roles on a node. Multiple roles can be enabled simultaneously.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Debug,
    Default,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct RoleBitmap(pub u8);

impl RoleBitmap {
    /// Empty bitmap (no roles enabled).
    pub const NONE: RoleBitmap = RoleBitmap(0);
    /// L1 validator role.
    pub const VALIDATOR: RoleBitmap = RoleBitmap(0b0001);
    /// L2 router role.
    pub const ROUTER: RoleBitmap = RoleBitmap(0b0010);
    /// L2 compute role.
    pub const COMPUTE: RoleBitmap = RoleBitmap(0b0100);
    /// L2 verifier role.
    pub const VERIFIER: RoleBitmap = RoleBitmap(0b1000);

    /// `true` if the given role is enabled.
    pub const fn has(&self, other: RoleBitmap) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Set a role bit (returns a new bitmap).
    pub const fn with(self, other: RoleBitmap) -> Self {
        Self(self.0 | other.0)
    }

    /// Clear a role bit (returns a new bitmap).
    pub const fn without(self, other: RoleBitmap) -> Self {
        Self(self.0 & !other.0)
    }

    /// `true` if no roles are enabled.
    pub const fn is_empty(&self) -> bool {
        self.0 == 0
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atoms_per_ark_is_billion() {
        assert_eq!(ATOMS_PER_ARK, 1_000_000_000);
    }

    #[test]
    fn supply_cap_is_1b_ark() {
        assert_eq!(ARK_SUPPLY_CAP, 1_000_000_000_000_000_000u128);
    }

    #[test]
    fn address_hex_roundtrip() {
        let a = Address::new([0x42; 20]);
        let s = a.to_hex();
        assert_eq!(s.len(), 40);
        let b = Address::from_hex(&s).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn address_hex_accepts_0x_prefix() {
        let raw = "0x4242424242424242424242424242424242424242";
        let a = Address::from_hex(raw).unwrap();
        assert_eq!(a.0, [0x42; 20]);
    }

    #[test]
    fn address_from_hex_rejects_wrong_length() {
        assert!(Address::from_hex("abcd").is_err());
    }

    #[test]
    fn signature_scheme_lengths_match_specs() {
        // Values here are the authoritative spec — do not change without architecture review.
        assert_eq!(SignatureScheme::Ed25519.pubkey_len(), 32);
        assert_eq!(SignatureScheme::Ed25519.signature_len(), 64);
        assert_eq!(SignatureScheme::Dilithium.pubkey_len(), 1312);
        assert_eq!(SignatureScheme::Dilithium.signature_len(), 2420);
        assert_eq!(SignatureScheme::Falcon.pubkey_len(), 897);
        assert_eq!(SignatureScheme::Falcon.signature_len(), 666);
    }

    #[test]
    fn only_ed25519_active_at_genesis() {
        assert!(SignatureScheme::Ed25519.is_active());
        assert!(!SignatureScheme::Dilithium.is_active());
        assert!(!SignatureScheme::Falcon.is_active());
        assert!(!SignatureScheme::Sphincs.is_active());
        assert!(!SignatureScheme::HybridEd25519Dilithium.is_active());
    }

    #[test]
    fn only_x25519_kem_active_at_genesis() {
        assert!(KemScheme::X25519.is_active());
        assert!(!KemScheme::Kyber.is_active());
    }

    #[test]
    fn only_ristretto_vrf_active_at_genesis() {
        assert!(VrfScheme::Ristretto255.is_active());
        assert!(!VrfScheme::LatticeVrf.is_active());
    }

    #[test]
    fn pubkey_constructor_enforces_length() {
        assert!(PubKey::new(SignatureScheme::Ed25519, vec![0; 32]).is_ok());
        assert!(PubKey::new(SignatureScheme::Ed25519, vec![0; 31]).is_err());
        assert!(PubKey::new(SignatureScheme::Ed25519, vec![0; 33]).is_err());
    }

    #[test]
    fn signature_constructor_enforces_length() {
        assert!(Signature::new(SignatureScheme::Ed25519, vec![0; 64]).is_ok());
        assert!(Signature::new(SignatureScheme::Ed25519, vec![0; 63]).is_err());
    }

    #[test]
    fn ed25519_constructors_produce_valid_values() {
        let pk = PubKey::ed25519([0xaa; 32]);
        assert_eq!(pk.scheme, SignatureScheme::Ed25519);
        assert_eq!(pk.bytes.len(), 32);

        let sig = Signature::ed25519([0xbb; 64]);
        assert_eq!(sig.scheme, SignatureScheme::Ed25519);
        assert_eq!(sig.bytes.len(), 64);
    }

    #[test]
    fn role_bitmap_operations() {
        let mut roles = RoleBitmap::NONE;
        assert!(roles.is_empty());

        roles = roles.with(RoleBitmap::ROUTER).with(RoleBitmap::COMPUTE);
        assert!(roles.has(RoleBitmap::ROUTER));
        assert!(roles.has(RoleBitmap::COMPUTE));
        assert!(!roles.has(RoleBitmap::VALIDATOR));
        assert!(!roles.has(RoleBitmap::VERIFIER));
        assert!(!roles.is_empty());

        let without_router = roles.without(RoleBitmap::ROUTER);
        assert!(!without_router.has(RoleBitmap::ROUTER));
        assert!(without_router.has(RoleBitmap::COMPUTE));
    }

    #[test]
    fn borsh_roundtrip_pubkey() {
        let pk = PubKey::ed25519([0x11; 32]);
        let bytes = borsh::to_vec(&pk).unwrap();
        let decoded: PubKey = borsh::from_slice(&bytes).unwrap();
        assert_eq!(pk, decoded);
    }

    #[test]
    fn borsh_roundtrip_signature() {
        let sig = Signature::ed25519([0x22; 64]);
        let bytes = borsh::to_vec(&sig).unwrap();
        let decoded: Signature = borsh::from_slice(&bytes).unwrap();
        assert_eq!(sig, decoded);
    }

    #[test]
    fn borsh_roundtrip_address() {
        let a = Address::new([0x33; 20]);
        let bytes = borsh::to_vec(&a).unwrap();
        let decoded: Address = borsh::from_slice(&bytes).unwrap();
        assert_eq!(a, decoded);
    }
}
