//! Wire types for the router ↔ compute inference protocol.
//!
//! Borsh-encoded. The same types are used over libp2p `request_response`
//! (Phase 1+) and in unit tests that drive the compute role in-process.
//!
//! # Versioning
//!
//! Top-level protocol id is `/arknet/inference/1` — a breaking change to
//! these types is a new `/2` path. Field order here is part of the wire
//! contract; do not reorder.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use arknet_common::types::{Address, Hash256, JobId, Nonce, PubKey, Signature, Timestamp};

/// X25519 + ChaCha20-Poly1305 encrypted envelope for confidential
/// inference. The user generates an ephemeral X25519 keypair, performs
/// ECDH against the enclave's pubkey, and encrypts the prompt. The
/// compute node (running inside a TEE) decrypts using the enclave's
/// private key — the host OS never sees plaintext.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct EncryptedEnvelope {
    /// User's ephemeral X25519 public key (32 bytes).
    pub ephemeral_pubkey: Vec<u8>,
    /// 12-byte nonce for ChaCha20-Poly1305.
    pub nonce: Vec<u8>,
    /// Encrypted prompt bytes (ciphertext + 16-byte Poly1305 tag).
    pub ciphertext: Vec<u8>,
}

/// A request for a single inference job, signed by the user.
///
/// `user_pubkey` is the signing key; `signature` covers the borsh encoding
/// of every other field in this struct. `user_address` is derived as
/// `blake3(user_pubkey.bytes)[0..20]` — the same derivation used by
/// `arknet-chain`'s `derive_address_from_signer`.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct InferenceJobRequest {
    /// Canonical model identifier (e.g. `"local/stories260K"`).
    pub model_ref: String,
    /// Expected model digest — compute refuses to serve if mismatch.
    pub model_hash: Hash256,
    /// Prompt text. Phase 1 is plaintext; Phase 2 adds X25519 envelope.
    pub prompt: String,
    /// Max new tokens.
    pub max_tokens: u32,
    /// Deterministic-mode seed (ignored in serving mode).
    pub seed: u64,
    /// `true` → force `InferenceMode::Deterministic` on the compute node.
    pub deterministic: bool,
    /// Stop strings.
    pub stop_strings: Vec<String>,
    /// Per-user replay nonce. Stored in a bounded LRU on the compute node.
    pub nonce: Nonce,
    /// Unix ms — rejected if older than [`REQUEST_MAX_SKEW_MS`].
    pub timestamp_ms: Timestamp,
    /// User signing pubkey.
    pub user_pubkey: PubKey,
    /// Signature over borsh encoding of every prior field.
    pub signature: Signature,
    /// `true` → route only to TEE-capable nodes. The router rejects the
    /// request if no TEE candidate is available (no silent downgrade).
    pub prefer_tee: bool,
    /// When `prefer_tee` is true and the user wants confidential inference,
    /// the prompt is encrypted to the enclave's pubkey and sent here
    /// instead of in `prompt`. The `prompt` field is empty in this case.
    pub encrypted_prompt: Option<EncryptedEnvelope>,
    /// Session key delegation certificate. When present, `user_pubkey`
    /// is the session key and `signature` is signed by that session key.
    /// The compute node verifies the delegation chain back to the main
    /// wallet for billing.
    pub delegation: Option<DelegationCert>,
}

/// Derived user address = `blake3(pubkey.bytes)[0..20]`.
///
/// Kept out of the signed body so the wire format doesn't duplicate
/// information already carried by `user_pubkey`.
pub fn derive_user_address(pubkey: &PubKey) -> Address {
    let digest = blake3::hash(&pubkey.bytes);
    let bytes = digest.as_bytes();
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes[..20]);
    Address::new(out)
}

/// How old a request can be before it's rejected as stale (30s).
///
/// Small enough that replay is useless; large enough that mild clock
/// skew doesn't break honest clients.
pub const REQUEST_MAX_SKEW_MS: u64 = 30_000;

/// Streaming event returned by the compute node.
///
/// Mirrors `arknet_inference::InferenceEvent` but carries the `JobId`
/// so a router multiplexing multiple jobs can demux. One `Stop` variant
/// terminates the stream.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum InferenceJobEvent {
    /// A decoded token text + index.
    Token {
        /// Job this event belongs to.
        job_id: JobId,
        /// Zero-based index in the output sequence.
        index: u32,
        /// Decoded UTF-8 fragment.
        text: String,
    },
    /// Generation ended for the reason carried.
    Stop {
        /// Job this event belongs to.
        job_id: JobId,
        /// Terminal reason.
        reason: StopKind,
    },
    /// Compute-side error — final event on the stream.
    Error {
        /// Job this event belongs to.
        job_id: JobId,
        /// Error message (opaque; for logging, not consensus).
        message: String,
    },
    /// Compute node is at capacity — SDK should try the next candidate.
    Busy {
        /// Job this event belongs to.
        job_id: JobId,
    },
}

/// Why generation ended. Mirror of `arknet_inference::StopReason`
/// intentionally reencoded into borsh-friendly shape.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum StopKind {
    /// Hit `max_tokens`.
    MaxTokens,
    /// Model's EOS token produced.
    EndOfStream,
    /// Caller-provided stop string matched.
    StopString(String),
    /// Caller (router / client) cancelled.
    Cancelled,
}

impl From<arknet_inference::StopReason> for StopKind {
    fn from(s: arknet_inference::StopReason) -> Self {
        match s {
            arknet_inference::StopReason::MaxTokens => StopKind::MaxTokens,
            arknet_inference::StopReason::EndOfStream => StopKind::EndOfStream,
            arknet_inference::StopReason::StopString(s) => StopKind::StopString(s),
            arknet_inference::StopReason::Cancelled => StopKind::Cancelled,
        }
    }
}

/// `InferenceJobRequest` payload for signing. The signature covers the
/// borsh encoding of this struct so a signer can't be tricked into
/// signing a semantically different request.
#[derive(Clone, Debug, BorshSerialize)]
pub struct InferenceRequestSigningBody<'a> {
    /// Domain separator — must not appear inside any other signed body.
    pub domain: &'static [u8],
    /// Model identifier.
    pub model_ref: &'a str,
    /// Model digest.
    pub model_hash: &'a Hash256,
    /// Prompt.
    pub prompt: &'a str,
    /// Max new tokens.
    pub max_tokens: u32,
    /// Deterministic seed.
    pub seed: u64,
    /// Deterministic flag.
    pub deterministic: bool,
    /// Stop strings.
    pub stop_strings: &'a [String],
    /// Nonce.
    pub nonce: Nonce,
    /// Timestamp.
    pub timestamp_ms: Timestamp,
    /// Pubkey.
    pub user_pubkey: &'a PubKey,
    /// TEE preference flag.
    pub prefer_tee: bool,
    /// Encrypted prompt (if present).
    pub encrypted_prompt: &'a Option<EncryptedEnvelope>,
    /// Delegation certificate (if present).
    pub delegation: &'a Option<DelegationCert>,
}

/// Domain tag for inference-request signatures. Never reused by any
/// other signed body in arknet.
pub const INFERENCE_REQUEST_DOMAIN: &[u8] = b"arknet-inference-req-v1";

impl InferenceJobRequest {
    /// Bytes a signer covers with their signature. Use this to both
    /// sign and verify.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let body = InferenceRequestSigningBody {
            domain: INFERENCE_REQUEST_DOMAIN,
            model_ref: &self.model_ref,
            model_hash: &self.model_hash,
            prompt: &self.prompt,
            max_tokens: self.max_tokens,
            seed: self.seed,
            deterministic: self.deterministic,
            stop_strings: &self.stop_strings,
            nonce: self.nonce,
            timestamp_ms: self.timestamp_ms,
            user_pubkey: &self.user_pubkey,
            prefer_tee: self.prefer_tee,
            encrypted_prompt: &self.encrypted_prompt,
            delegation: &self.delegation,
        };
        borsh::to_vec(&body).expect("borsh encoding of signing body is infallible")
    }

    /// The billing address for this request. When a delegation cert is
    /// present the main wallet pays; otherwise the direct signer pays.
    pub fn billing_address(&self) -> Address {
        match &self.delegation {
            Some(cert) => cert.main_wallet_address,
            None => derive_user_address(&self.user_pubkey),
        }
    }
}

// ---------------------------------------------------------------------------
// Session key delegation
// ---------------------------------------------------------------------------

/// Domain tag for delegation certificate signatures.
pub const DELEGATION_DOMAIN: &[u8] = b"arknet-session-delegation-v1";

/// A delegation certificate authorizing a session key to act on behalf
/// of a main wallet, with spending and time constraints.
///
/// The main wallet signs this once; the session key uses it for every
/// inference request. If the session key leaks, the attacker can only
/// spend up to `spending_limit` and only until `expiry_ms`.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct DelegationCert {
    /// The session key's Ed25519 public key.
    pub session_pubkey: PubKey,
    /// Maximum spending allowed by this session (ark_atom).
    pub spending_limit: u128,
    /// Unix ms after which this delegation is invalid.
    pub expiry_ms: Timestamp,
    /// The main wallet's on-chain address.
    pub main_wallet_address: Address,
    /// The main wallet's public key (needed for sig verification).
    pub main_wallet_pubkey: PubKey,
    /// Signature by the main wallet over [`DelegationSigningBody`].
    pub main_wallet_signature: Signature,
}

/// Deterministic signing body for [`DelegationCert`].
#[derive(Clone, Debug, BorshSerialize)]
pub struct DelegationSigningBody<'a> {
    /// Domain separator.
    pub domain: &'a [u8],
    /// Session public key being authorized.
    pub session_pubkey: &'a PubKey,
    /// Spending cap (ark_atom).
    pub spending_limit: u128,
    /// Expiry timestamp (unix ms).
    pub expiry_ms: Timestamp,
    /// Main wallet address.
    pub main_wallet_address: &'a Address,
}

impl DelegationCert {
    /// Bytes the main wallet must sign to produce `main_wallet_signature`.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let body = DelegationSigningBody {
            domain: DELEGATION_DOMAIN,
            session_pubkey: &self.session_pubkey,
            spending_limit: self.spending_limit,
            expiry_ms: self.expiry_ms,
            main_wallet_address: &self.main_wallet_address,
        };
        borsh::to_vec(&body).expect("borsh encoding of delegation body is infallible")
    }
}

/// Verify a delegation certificate: signature valid, not expired,
/// address matches pubkey.
pub fn verify_delegation(cert: &DelegationCert, now_ms: Timestamp) -> Result<(), String> {
    if now_ms > cert.expiry_ms {
        return Err(format!(
            "delegation expired: now={now_ms} > expiry={}",
            cert.expiry_ms
        ));
    }
    let expected_addr = derive_user_address(&cert.main_wallet_pubkey);
    if expected_addr != cert.main_wallet_address {
        return Err("delegation: main_wallet_address does not match main_wallet_pubkey".into());
    }
    let signing_bytes = cert.signing_bytes();
    arknet_crypto::signatures::verify(
        &cert.main_wallet_pubkey,
        &signing_bytes,
        &cert.main_wallet_signature,
    )
    .map_err(|e| format!("delegation signature invalid: {e}"))
}

// ---------------------------------------------------------------------------
// Pool offer
// ---------------------------------------------------------------------------

/// Gossip message for `arknet/pool/offer/1`. A compute node publishes
/// this when it loads or unloads a model so the mesh can discover it.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct PoolOffer {
    /// libp2p PeerId as bytes.
    pub peer_id: Vec<u8>,
    /// Canonical model refs this compute node currently serves.
    pub model_refs: Vec<String>,
    /// Operator address.
    pub operator: Address,
    /// Staked amount (for ranking).
    pub total_stake: u128,
    /// TEE capability flag.
    pub supports_tee: bool,
    /// Unix millis — consumers use freshness to expire stale offers.
    pub timestamp_ms: Timestamp,
    /// How many concurrent inference slots are available right now.
    pub available_slots: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_common::types::SignatureScheme;

    fn sample() -> InferenceJobRequest {
        InferenceJobRequest {
            model_ref: "local/stories260K".into(),
            model_hash: [0xab; 32],
            prompt: "Once upon a time".into(),
            max_tokens: 16,
            seed: 42,
            deterministic: true,
            stop_strings: vec![".".into()],
            nonce: 7,
            timestamp_ms: 1_700_000_000_000,
            user_pubkey: PubKey::ed25519([0x11; 32]),
            signature: Signature::ed25519([0x22; 64]),
            prefer_tee: false,
            encrypted_prompt: None,
            delegation: None,
        }
    }

    #[test]
    fn request_borsh_roundtrip() {
        let r = sample();
        let bytes = borsh::to_vec(&r).unwrap();
        let back: InferenceJobRequest = borsh::from_slice(&bytes).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn event_borsh_roundtrip() {
        let job_id = JobId::new([0x55; 32]);
        let events = vec![
            InferenceJobEvent::Token {
                job_id,
                index: 0,
                text: "hello".into(),
            },
            InferenceJobEvent::Stop {
                job_id,
                reason: StopKind::MaxTokens,
            },
            InferenceJobEvent::Stop {
                job_id,
                reason: StopKind::StopString(".".into()),
            },
            InferenceJobEvent::Error {
                job_id,
                message: "boom".into(),
            },
        ];
        for ev in events {
            let bytes = borsh::to_vec(&ev).unwrap();
            let back: InferenceJobEvent = borsh::from_slice(&bytes).unwrap();
            assert_eq!(ev, back);
        }
    }

    #[test]
    fn signing_bytes_stable_and_domain_prefixed() {
        let r = sample();
        let b1 = r.signing_bytes();
        let b2 = r.signing_bytes();
        assert_eq!(b1, b2, "signing bytes must be deterministic");
        // The domain tag appears near the start (after its borsh length prefix).
        assert!(
            b1.windows(INFERENCE_REQUEST_DOMAIN.len())
                .any(|w| w == INFERENCE_REQUEST_DOMAIN),
            "domain tag must be present in signed bytes"
        );
    }

    #[test]
    fn derive_user_address_matches_chain_rule() {
        // Must match `arknet_chain::apply::derive_address_from_signer`:
        // blake3(pubkey.bytes)[0..20].
        let pk = PubKey::ed25519([0x33; 32]);
        let a = derive_user_address(&pk);
        let digest = blake3::hash(&pk.bytes);
        assert_eq!(a.as_bytes(), &digest.as_bytes()[..20]);
    }

    #[test]
    fn encrypted_envelope_borsh_roundtrip() {
        let env = EncryptedEnvelope {
            ephemeral_pubkey: vec![0x01; 32],
            nonce: vec![0x02; 12],
            ciphertext: vec![0x03; 100],
        };
        let bytes = borsh::to_vec(&env).unwrap();
        let back: EncryptedEnvelope = borsh::from_slice(&bytes).unwrap();
        assert_eq!(env, back);
    }

    #[test]
    fn prefer_tee_included_in_signing_bytes() {
        let mut r = sample();
        let without = r.signing_bytes();
        r.prefer_tee = true;
        let with = r.signing_bytes();
        assert_ne!(without, with, "prefer_tee must affect signing bytes");
    }

    #[test]
    fn request_max_skew_is_30s() {
        assert_eq!(REQUEST_MAX_SKEW_MS, 30_000);
    }

    #[test]
    fn signature_scheme_sanity() {
        assert!(SignatureScheme::Ed25519.is_active());
    }

    #[test]
    fn delegation_cert_borsh_roundtrip() {
        let cert = DelegationCert {
            session_pubkey: PubKey::ed25519([0xAA; 32]),
            spending_limit: 1_000_000_000,
            expiry_ms: 1_800_000_000_000,
            main_wallet_address: Address::new([0xBB; 20]),
            main_wallet_pubkey: PubKey::ed25519([0xCC; 32]),
            main_wallet_signature: Signature::ed25519([0xDD; 64]),
        };
        let bytes = borsh::to_vec(&cert).unwrap();
        let back: DelegationCert = borsh::from_slice(&bytes).unwrap();
        assert_eq!(cert, back);
    }

    #[test]
    fn delegation_signing_bytes_deterministic() {
        let cert = DelegationCert {
            session_pubkey: PubKey::ed25519([0xAA; 32]),
            spending_limit: 500,
            expiry_ms: 9999,
            main_wallet_address: Address::new([0x01; 20]),
            main_wallet_pubkey: PubKey::ed25519([0x02; 32]),
            main_wallet_signature: Signature::ed25519([0x00; 64]),
        };
        assert_eq!(cert.signing_bytes(), cert.signing_bytes());
    }

    #[test]
    fn delegation_signing_bytes_contain_domain() {
        let cert = DelegationCert {
            session_pubkey: PubKey::ed25519([0xAA; 32]),
            spending_limit: 1,
            expiry_ms: 1,
            main_wallet_address: Address::new([0x00; 20]),
            main_wallet_pubkey: PubKey::ed25519([0x00; 32]),
            main_wallet_signature: Signature::ed25519([0x00; 64]),
        };
        let bytes = cert.signing_bytes();
        assert!(
            bytes
                .windows(DELEGATION_DOMAIN.len())
                .any(|w| w == DELEGATION_DOMAIN),
            "domain tag must be present"
        );
    }

    #[test]
    fn delegation_field_affects_signing_bytes() {
        let mut r = sample();
        let without = r.signing_bytes();
        r.delegation = Some(DelegationCert {
            session_pubkey: PubKey::ed25519([0xAA; 32]),
            spending_limit: 100,
            expiry_ms: 9999,
            main_wallet_address: Address::new([0x01; 20]),
            main_wallet_pubkey: PubKey::ed25519([0x02; 32]),
            main_wallet_signature: Signature::ed25519([0x00; 64]),
        });
        let with = r.signing_bytes();
        assert_ne!(without, with, "delegation must affect signing bytes");
    }

    #[test]
    fn billing_address_uses_main_wallet_when_delegated() {
        let main_addr = Address::new([0xFF; 20]);
        let mut r = sample();
        r.delegation = Some(DelegationCert {
            session_pubkey: r.user_pubkey.clone(),
            spending_limit: 100,
            expiry_ms: 9999,
            main_wallet_address: main_addr,
            main_wallet_pubkey: PubKey::ed25519([0x02; 32]),
            main_wallet_signature: Signature::ed25519([0x00; 64]),
        });
        assert_eq!(r.billing_address(), main_addr);
    }

    #[test]
    fn billing_address_uses_signer_when_no_delegation() {
        let r = sample();
        assert_eq!(r.billing_address(), derive_user_address(&r.user_pubkey));
    }

    #[test]
    fn pool_offer_borsh_roundtrip() {
        let offer = PoolOffer {
            peer_id: vec![0x01; 38],
            model_refs: vec!["test/model".into()],
            operator: Address::new([0x10; 20]),
            total_stake: 42,
            supports_tee: false,
            timestamp_ms: 1_000,
            available_slots: 3,
        };
        let bytes = borsh::to_vec(&offer).unwrap();
        let back: PoolOffer = borsh::from_slice(&bytes).unwrap();
        assert_eq!(offer, back);
    }

    #[test]
    fn busy_event_borsh_roundtrip() {
        let ev = InferenceJobEvent::Busy {
            job_id: JobId::new([0x55; 32]),
        };
        let bytes = borsh::to_vec(&ev).unwrap();
        let back: InferenceJobEvent = borsh::from_slice(&bytes).unwrap();
        assert_eq!(ev, back);
    }
}
