//! Sign & verify, routed through the scheme-tagged [`Signature`] / [`PubKey`]
//! types from `arknet-common`.
//!
//! Only Ed25519 is active at Phase 0. Other schemes in [`SignatureScheme`]
//! return [`CryptoError::SchemeNotSupported`] until their dedicated
//! implementation lands (Phase 3+ for PQ schemes). See `docs/SECURITY.md` §12.

use arknet_common::{PubKey, Signature, SignatureScheme};
use ed25519_dalek::{Signature as EdSignature, Signer, Verifier, SIGNATURE_LENGTH};

use crate::errors::{CryptoError, Result};
use crate::keys::{SigningKey, VerifyingKey};

/// Sign a message with a signing key.
///
/// The returned [`Signature`] is scheme-tagged for on-wire / on-chain use.
pub fn sign(key: &SigningKey, message: &[u8]) -> Signature {
    let raw = key.inner().sign(message);
    Signature::ed25519(raw.to_bytes())
}

/// Verify a scheme-tagged [`Signature`] against a scheme-tagged [`PubKey`].
///
/// Returns [`CryptoError::SchemeNotSupported`] if the scheme isn't Ed25519 at
/// Phase 0. Returns [`CryptoError::SignatureInvalid`] on any verification
/// failure (including mismatched keys, modified message, corrupted bytes).
///
/// # Security
/// The failure path is kept opaque — no hint about *why* the signature failed.
/// Attackers must not be able to distinguish "wrong key" from "wrong message"
/// from "malformed bytes".
pub fn verify(pubkey: &PubKey, message: &[u8], sig: &Signature) -> Result<()> {
    if pubkey.scheme != sig.scheme {
        return Err(CryptoError::SignatureInvalid);
    }

    match pubkey.scheme {
        SignatureScheme::Ed25519 => verify_ed25519(pubkey, message, sig),
        other => Err(CryptoError::SchemeNotSupported(format!(
            "{other:?} not implemented at Phase 0"
        ))),
    }
}

fn verify_ed25519(pubkey: &PubKey, message: &[u8], sig: &Signature) -> Result<()> {
    let vk = VerifyingKey::from_pubkey(pubkey)?;

    if sig.bytes.len() != SIGNATURE_LENGTH {
        return Err(CryptoError::SignatureInvalid);
    }
    let mut raw = [0u8; SIGNATURE_LENGTH];
    raw.copy_from_slice(&sig.bytes);
    let ed_sig = EdSignature::from_bytes(&raw);

    vk.inner()
        .verify(message, &ed_sig)
        .map_err(|_| CryptoError::SignatureInvalid)
}

/// Verify a batch of `(pubkey, message, signature)` triples.
///
/// Returns `Ok(())` iff **all** verify. Faster than per-signature `verify`
/// for 8+ signatures because of internal point aggregation. Short-circuits
/// on scheme mismatch; otherwise falls back to per-signature verify only
/// if any batch element can't be processed by the fast path.
///
/// # Security
/// Do not use to selectively report which signature failed — the call either
/// succeeds or fails as a whole. Caller must not leak batch-position info.
pub fn verify_batch<'a, I>(items: I) -> Result<()>
where
    I: IntoIterator<Item = (&'a PubKey, &'a [u8], &'a Signature)>,
{
    use ed25519_dalek::{verify_batch, Signature as EdSig, VerifyingKey as EdVk};

    let mut msgs: Vec<&[u8]> = Vec::new();
    let mut sigs: Vec<EdSig> = Vec::new();
    let mut vks: Vec<EdVk> = Vec::new();

    for (pk, msg, sig) in items {
        if pk.scheme != SignatureScheme::Ed25519 || sig.scheme != SignatureScheme::Ed25519 {
            return Err(CryptoError::SchemeNotSupported(
                "batch verify only supports Ed25519 at Phase 0".into(),
            ));
        }
        let vk = VerifyingKey::from_pubkey(pk)?;
        if sig.bytes.len() != SIGNATURE_LENGTH {
            return Err(CryptoError::SignatureInvalid);
        }
        let mut raw = [0u8; SIGNATURE_LENGTH];
        raw.copy_from_slice(&sig.bytes);

        msgs.push(msg);
        sigs.push(EdSig::from_bytes(&raw));
        vks.push(*vk.inner());
    }

    verify_batch(&msgs, &sigs, &vks).map_err(|_| CryptoError::SignatureInvalid)
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::SigningKey;

    #[test]
    fn sign_and_verify_roundtrip() {
        let sk = SigningKey::generate();
        let pk = sk.verifying_key().to_pubkey();
        let msg = b"arknet block 42";
        let sig = sign(&sk, msg);
        verify(&pk, msg, &sig).unwrap();
    }

    #[test]
    fn verify_fails_on_modified_message() {
        let sk = SigningKey::generate();
        let pk = sk.verifying_key().to_pubkey();
        let sig = sign(&sk, b"original");
        let res = verify(&pk, b"tampered", &sig);
        assert!(matches!(res, Err(CryptoError::SignatureInvalid)));
    }

    #[test]
    fn verify_fails_on_wrong_pubkey() {
        let sk = SigningKey::generate();
        let wrong = SigningKey::generate();
        let sig = sign(&sk, b"msg");
        let res = verify(&wrong.verifying_key().to_pubkey(), b"msg", &sig);
        assert!(matches!(res, Err(CryptoError::SignatureInvalid)));
    }

    #[test]
    fn verify_fails_on_flipped_signature_byte() {
        let sk = SigningKey::generate();
        let pk = sk.verifying_key().to_pubkey();
        let mut sig = sign(&sk, b"msg");
        sig.bytes[0] ^= 1;
        let res = verify(&pk, b"msg", &sig);
        assert!(matches!(res, Err(CryptoError::SignatureInvalid)));
    }

    #[test]
    fn verify_rejects_scheme_mismatch() {
        let sk = SigningKey::generate();
        let pk = sk.verifying_key().to_pubkey();
        let mut sig = sign(&sk, b"msg");
        sig.scheme = SignatureScheme::Dilithium; // pretend it's PQ
        let res = verify(&pk, b"msg", &sig);
        assert!(matches!(res, Err(CryptoError::SignatureInvalid)));
    }

    #[test]
    fn verify_rejects_unimplemented_scheme() {
        let fake_pk = PubKey {
            scheme: SignatureScheme::Dilithium,
            bytes: vec![0; 1312],
        };
        let fake_sig = Signature {
            scheme: SignatureScheme::Dilithium,
            bytes: vec![0; 2420],
        };
        let res = verify(&fake_pk, b"msg", &fake_sig);
        assert!(matches!(res, Err(CryptoError::SchemeNotSupported(_))));
    }

    #[test]
    fn batch_verify_ok_on_all_valid() {
        let sks: Vec<_> = (0..5).map(|_| SigningKey::generate()).collect();
        let pks: Vec<_> = sks.iter().map(|s| s.verifying_key().to_pubkey()).collect();
        let msgs: Vec<Vec<u8>> = (0..5).map(|i| format!("msg-{i}").into_bytes()).collect();
        let sigs: Vec<_> = sks
            .iter()
            .zip(msgs.iter())
            .map(|(sk, m)| sign(sk, m))
            .collect();

        let items: Vec<_> = pks
            .iter()
            .zip(msgs.iter())
            .zip(sigs.iter())
            .map(|((pk, m), s)| (pk, m.as_slice(), s))
            .collect();
        verify_batch(items).unwrap();
    }

    #[test]
    fn batch_verify_fails_if_any_signature_is_invalid() {
        let sks: Vec<_> = (0..3).map(|_| SigningKey::generate()).collect();
        let pks: Vec<_> = sks.iter().map(|s| s.verifying_key().to_pubkey()).collect();
        let msgs: Vec<Vec<u8>> = (0..3).map(|i| format!("msg-{i}").into_bytes()).collect();
        let mut sigs: Vec<_> = sks
            .iter()
            .zip(msgs.iter())
            .map(|(sk, m)| sign(sk, m))
            .collect();

        // Corrupt one signature.
        sigs[1].bytes[0] ^= 0xff;

        let items: Vec<_> = pks
            .iter()
            .zip(msgs.iter())
            .zip(sigs.iter())
            .map(|((pk, m), s)| (pk, m.as_slice(), s))
            .collect();
        let res = verify_batch(items);
        assert!(matches!(res, Err(CryptoError::SignatureInvalid)));
    }

    #[test]
    fn batch_verify_rejects_non_ed25519() {
        let sk = SigningKey::generate();
        let pk = sk.verifying_key().to_pubkey();
        let msg = b"msg";
        let sig = sign(&sk, msg);

        let fake_pk = PubKey {
            scheme: SignatureScheme::Falcon,
            bytes: vec![0; 897],
        };
        let items = vec![
            (&pk, msg.as_slice(), &sig),
            (&fake_pk, msg.as_slice(), &sig),
        ];
        let res = verify_batch(items);
        assert!(matches!(res, Err(CryptoError::SchemeNotSupported(_))));
    }

    #[test]
    fn empty_batch_verifies_trivially() {
        let items: Vec<(&PubKey, &[u8], &Signature)> = vec![];
        verify_batch(items).unwrap();
    }

    proptest::proptest! {
        #[test]
        fn sign_verify_property(seed: [u8; 32], msg: Vec<u8>) {
            let sk = SigningKey::from_seed(&seed);
            let pk = sk.verifying_key().to_pubkey();
            let sig = sign(&sk, &msg);
            proptest::prop_assert!(verify(&pk, &msg, &sig).is_ok());
        }

        #[test]
        fn bit_flip_in_message_always_fails(
            seed: [u8; 32],
            mut msg: Vec<u8>,
            flip_index in 0usize..256,
        ) {
            if msg.is_empty() { return Ok(()); }
            let sk = SigningKey::from_seed(&seed);
            let pk = sk.verifying_key().to_pubkey();
            let sig = sign(&sk, &msg);
            let idx = flip_index % msg.len();
            msg[idx] ^= 1;
            proptest::prop_assert!(verify(&pk, &msg, &sig).is_err());
        }
    }
}
