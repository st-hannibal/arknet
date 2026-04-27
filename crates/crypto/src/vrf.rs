//! Verifiable Random Function (VRF).
//!
//! Used for unpredictable, publicly verifiable verifier selection — the
//! verifier chosen for a given inference receipt must be unknowable to
//! compute nodes until the block containing the receipt is finalized.
//!
//! # Phase 0 construction
//!
//! At Phase 0 we ship a minimal VRF built on top of Ed25519:
//!
//! ```text
//! proof(sk, input)  = sign(sk, sha256(input))   // 64 bytes
//! hash(proof)       = sha256(proof)             // 32 bytes, uniform-looking
//! ```
//!
//! This is **not** an RFC-9381 ECVRF — it's sufficient for Phase 0's
//! single-verifier selection requirement (unpredictability + public
//! verifiability from `(vk, input, proof)`), but a replacement with a
//! dedicated ECVRF-Ristretto255 construction lands in Phase 1 via the
//! [`Vrf`] trait.
//!
//! # Security caveat
//!
//! Do **not** use the output of this VRF as a source of CSPRNG bytes for
//! key generation — only as a public coin for verifier election.

use crate::errors::Result;
use crate::hash::{sha256, Sha256Digest};
use crate::keys::{SigningKey, VerifyingKey};
use crate::signatures::{sign, verify};
use arknet_common::{PubKey, Signature};

/// A VRF proof. On the Phase 0 construction this is exactly an Ed25519 signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VrfProof(pub Signature);

/// VRF output (the "random" hash derived from the proof).
pub type VrfOutput = Sha256Digest;

/// Produce `(proof, output)` for the given input, using the signing key.
///
/// Deterministic — same `(sk, input)` always yields the same proof.
pub fn prove(sk: &SigningKey, input: &[u8]) -> (VrfProof, VrfOutput) {
    let msg = sha256(input);
    let sig = sign(sk, msg.as_bytes());
    let output = sha256(&sig.bytes);
    (VrfProof(sig), output)
}

/// Verify a VRF proof against a public key and input, returning the
/// derived output.
pub fn verify_proof(vk: &VerifyingKey, input: &[u8], proof: &VrfProof) -> Result<VrfOutput> {
    let msg = sha256(input);
    verify(&vk.to_pubkey(), msg.as_bytes(), &proof.0)?;
    Ok(sha256(&proof.0.bytes))
}

/// Convenience: verify using a scheme-tagged `PubKey`.
pub fn verify_with_pubkey(pk: &PubKey, input: &[u8], proof: &VrfProof) -> Result<VrfOutput> {
    let vk = VerifyingKey::from_pubkey(pk)?;
    verify_proof(&vk, input, proof)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prove_is_deterministic() {
        let sk = SigningKey::from_seed(&[0x11; 32]);
        let (p1, o1) = prove(&sk, b"input");
        let (p2, o2) = prove(&sk, b"input");
        assert_eq!(p1, p2);
        assert_eq!(o1, o2);
    }

    #[test]
    fn different_inputs_produce_different_outputs() {
        let sk = SigningKey::from_seed(&[0x22; 32]);
        let (_, o1) = prove(&sk, b"input-a");
        let (_, o2) = prove(&sk, b"input-b");
        assert_ne!(o1, o2);
    }

    #[test]
    fn verify_succeeds_with_correct_key() {
        let sk = SigningKey::generate();
        let vk = sk.verifying_key();
        let (proof, expected) = prove(&sk, b"block-hash-42");
        let got = verify_proof(&vk, b"block-hash-42", &proof).unwrap();
        assert_eq!(got, expected);
    }

    #[test]
    fn verify_fails_with_wrong_key() {
        let sk = SigningKey::generate();
        let wrong = SigningKey::generate();
        let (proof, _) = prove(&sk, b"input");
        let res = verify_proof(&wrong.verifying_key(), b"input", &proof);
        assert!(res.is_err());
    }

    #[test]
    fn verify_fails_with_tampered_input() {
        let sk = SigningKey::generate();
        let vk = sk.verifying_key();
        let (proof, _) = prove(&sk, b"original");
        let res = verify_proof(&vk, b"tampered", &proof);
        assert!(res.is_err());
    }

    #[test]
    fn verify_fails_with_tampered_proof() {
        let sk = SigningKey::generate();
        let vk = sk.verifying_key();
        let (mut proof, _) = prove(&sk, b"x");
        proof.0.bytes[0] ^= 1;
        let res = verify_proof(&vk, b"x", &proof);
        assert!(res.is_err());
    }

    #[test]
    fn verify_with_pubkey_works() {
        let sk = SigningKey::generate();
        let pk = sk.verifying_key().to_pubkey();
        let (proof, expected) = prove(&sk, b"any");
        let got = verify_with_pubkey(&pk, b"any", &proof).unwrap();
        assert_eq!(got, expected);
    }

    proptest::proptest! {
        #[test]
        fn prove_verify_roundtrip(seed: [u8; 32], input: Vec<u8>) {
            let sk = SigningKey::from_seed(&seed);
            let vk = sk.verifying_key();
            let (proof, out) = prove(&sk, &input);
            let got = verify_proof(&vk, &input, &proof).unwrap();
            proptest::prop_assert_eq!(out, got);
        }
    }
}
