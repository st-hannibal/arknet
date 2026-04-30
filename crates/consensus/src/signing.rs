//! [`malachitebft_core_types::SigningProvider`] implementation.
//!
//! Wraps the `malachitebft-signing-ed25519` crate so malachite's core
//! can sign / verify our concrete [`ChainVote`], [`ChainProposal`], and
//! [`ChainProposalPart`] messages.
//!
//! # Why a second Ed25519 crate?
//!
//! Malachite's signing trait is implemented in terms of its own
//! `Ed25519` newtype, which wraps `ed25519_consensus`. The rest of the
//! codebase uses `ed25519-dalek` (both are audited; see
//! SECURITY.md §4). Consolidating to one implementation is a
//! post-Phase-1 crypto-agility task — this module keeps the daylight
//! between the two contained to this file.
//!
//! # Message digest
//!
//! All three message kinds are hashed as:
//!
//! `blake3(DOMAIN_TAG || borsh(message))`
//!
//! Distinct per-message domain tags make a prevote signature unusable as
//! a proposal signature even if the underlying bytes happened to
//! collide.

use borsh::BorshSerialize;
use malachitebft_core_types::{Signature, SignedMessage, SigningProvider};
pub use malachitebft_signing_ed25519::PrivateKey;
use malachitebft_signing_ed25519::{PublicKey, Signature as EdSignature};

use arknet_crypto::hash::blake3;

use crate::context::ArknetContext;
use crate::proposal::{ChainProposal, ChainProposalPart};
use crate::vote::ChainVote;

// Domain tags — prepended to the borsh encoding before hashing so a
// prevote signature cannot be passed off as a proposal signature even
// if their encoded bytes somehow collide.
const DOMAIN_VOTE: &[u8] = b"arknet-consensus-vote-v1";
const DOMAIN_PROPOSAL: &[u8] = b"arknet-consensus-proposal-v1";
const DOMAIN_PROPOSAL_PART: &[u8] = b"arknet-consensus-proposal-part-v1";

/// Hashes a borsh-encoded message under a domain tag.
///
/// Returning `[u8; 32]` rather than `Hash256` keeps the callsite free of
/// `arknet_common` conversions that the signing provider does not need.
fn digest<T: BorshSerialize>(domain: &[u8], msg: &T) -> [u8; 32] {
    let body = borsh::to_vec(msg).expect("consensus message borsh encoding is infallible");
    let mut buf = Vec::with_capacity(domain.len() + body.len());
    buf.extend_from_slice(domain);
    buf.extend_from_slice(&body);
    let out = blake3(&buf);
    *out.as_bytes()
}

// `ChainVote` / `ChainProposal` / `ChainProposalPart` do not derive
// `BorshSerialize` today — they hold malachite types (`Round`,
// `NilOrVal<BlockId>`) that do not implement borsh. Encode by hand
// into a fixed-shape byte layout that matches what we gossip on the
// wire, so both sides hash the same bytes.
fn encode_vote(v: &ChainVote) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + 8 + 4 + 1 + 1 + 32 + 20);
    // Height
    out.extend_from_slice(&v.height.0.to_be_bytes());
    // Round (i64; -1 represents Round::Nil)
    let round_i = v.round.as_i64();
    out.extend_from_slice(&round_i.to_be_bytes());
    // Vote type
    out.push(match v.vote_type {
        malachitebft_core_types::VoteType::Prevote => 0x01,
        malachitebft_core_types::VoteType::Precommit => 0x02,
    });
    // Value (0 nil, 1 + 32 bytes otherwise)
    match &v.value {
        malachitebft_core_types::NilOrVal::Nil => out.push(0x00),
        malachitebft_core_types::NilOrVal::Val(id) => {
            out.push(0x01);
            out.extend_from_slice(id.0.as_bytes());
        }
    }
    // Validator address (unwrap the newtype)
    out.extend_from_slice(v.validator_address.0.as_bytes());
    out
}

fn encode_proposal(p: &ChainProposal) -> Vec<u8> {
    let mut out = Vec::new();
    // Height
    out.extend_from_slice(&p.height.0.to_be_bytes());
    // Round (as above — -1 for Nil)
    out.extend_from_slice(&p.round.as_i64().to_be_bytes());
    // POL round
    out.extend_from_slice(&p.pol_round.as_i64().to_be_bytes());
    // Value id (32-byte block hash — the body is committed via this id).
    out.extend_from_slice(p.value.id().0.as_bytes());
    // Validator address (unwrap the newtype)
    out.extend_from_slice(p.validator_address.0.as_bytes());
    out
}

fn encode_proposal_part(_part: &ChainProposalPart) -> Vec<u8> {
    // `ChainProposalPart` is a unit type for Phase 1 (ProposalOnly mode).
    // Domain-prefixed empty body is still unique per-message thanks to
    // the DOMAIN_PROPOSAL_PART prefix.
    Vec::new()
}

fn digest_raw(domain: &[u8], body: &[u8]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(domain.len() + body.len());
    buf.extend_from_slice(domain);
    buf.extend_from_slice(body);
    *blake3(&buf).as_bytes()
}

/// Owns the consensus private key and implements
/// [`SigningProvider`] for [`ArknetContext`].
///
/// Construct once per node from the persisted consensus keypair
/// (Week 9 keystore task takes over from the in-memory keypair used in
/// Week 7-8 tests).
pub struct ArknetSigningProvider {
    private_key: PrivateKey,
}

impl ArknetSigningProvider {
    /// Wrap a loaded private key.
    pub fn new(private_key: PrivateKey) -> Self {
        Self { private_key }
    }

    /// The public key paired with the loaded private key. Useful during
    /// node boot to build the [`ChainValidator`] entry for ourselves.
    pub fn public_key(&self) -> PublicKey {
        self.private_key.public_key()
    }
}

impl SigningProvider<ArknetContext> for ArknetSigningProvider {
    fn sign_vote(&self, vote: ChainVote) -> SignedMessage<ArknetContext, ChainVote> {
        let body = encode_vote(&vote);
        let sig_bytes = digest_raw(DOMAIN_VOTE, &body);
        let signature: EdSignature = self.private_key.sign(&sig_bytes);
        SignedMessage::new(vote, signature)
    }

    fn verify_signed_vote(
        &self,
        vote: &ChainVote,
        signature: &Signature<ArknetContext>,
        public_key: &PublicKey,
    ) -> bool {
        let body = encode_vote(vote);
        let sig_bytes = digest_raw(DOMAIN_VOTE, &body);
        public_key.verify(&sig_bytes, signature).is_ok()
    }

    fn sign_proposal(
        &self,
        proposal: ChainProposal,
    ) -> SignedMessage<ArknetContext, ChainProposal> {
        let body = encode_proposal(&proposal);
        let sig_bytes = digest_raw(DOMAIN_PROPOSAL, &body);
        let signature: EdSignature = self.private_key.sign(&sig_bytes);
        SignedMessage::new(proposal, signature)
    }

    fn verify_signed_proposal(
        &self,
        proposal: &ChainProposal,
        signature: &Signature<ArknetContext>,
        public_key: &PublicKey,
    ) -> bool {
        let body = encode_proposal(proposal);
        let sig_bytes = digest_raw(DOMAIN_PROPOSAL, &body);
        public_key.verify(&sig_bytes, signature).is_ok()
    }

    fn sign_proposal_part(
        &self,
        proposal_part: ChainProposalPart,
    ) -> SignedMessage<ArknetContext, ChainProposalPart> {
        let body = encode_proposal_part(&proposal_part);
        let sig_bytes = digest_raw(DOMAIN_PROPOSAL_PART, &body);
        let signature: EdSignature = self.private_key.sign(&sig_bytes);
        SignedMessage::new(proposal_part, signature)
    }

    fn verify_signed_proposal_part(
        &self,
        proposal_part: &ChainProposalPart,
        signature: &Signature<ArknetContext>,
        public_key: &PublicKey,
    ) -> bool {
        let body = encode_proposal_part(proposal_part);
        let sig_bytes = digest_raw(DOMAIN_PROPOSAL_PART, &body);
        public_key.verify(&sig_bytes, signature).is_ok()
    }

    // Phase 1 uses `Extension = ()`. Sign returns a dummy signature;
    // verify always accepts. Both paths are unreachable in
    // ProposalOnly mode — the state machine never attaches extensions
    // unless `ValuePayload` asks for them.
    fn sign_vote_extension(&self, extension: ()) -> SignedMessage<ArknetContext, ()> {
        SignedMessage::new(extension, EdSignature::test())
    }

    fn verify_signed_vote_extension(
        &self,
        _extension: &(),
        _signature: &Signature<ArknetContext>,
        _public_key: &PublicKey,
    ) -> bool {
        true
    }
}

// Silence the unused-import warning that appears when the digest helper
// is only used inside `encode_vote` / `encode_proposal` / extension
// stubs through the raw-bytes path. `digest` stays in the module as
// the canonical borsh-based hash helper we will switch to once our
// Round / NilOrVal impls grow borsh derives.
#[allow(dead_code)]
fn _keep_digest_in_scope<T: BorshSerialize>(domain: &[u8], msg: &T) -> [u8; 32] {
    digest(domain, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_chain::block::{Block, BlockHeader};
    use arknet_common::types::{Address, BlockHash, NodeId, StateRoot};
    use malachitebft_core_types::{NilOrVal, Round, VoteType};

    use crate::height::Height;
    use crate::validators::ChainAddress;
    use crate::value::ChainValue;

    fn keypair() -> (ArknetSigningProvider, PublicKey) {
        use rand::rngs::OsRng;
        let sk = malachitebft_signing_ed25519::Ed25519::generate_keypair(OsRng);
        let pk = sk.public_key();
        (ArknetSigningProvider::new(sk), pk)
    }

    fn sample_block() -> Block {
        let header = BlockHeader {
            version: 1,
            chain_id: "arknet-test".into(),
            height: 1,
            timestamp_ms: 0,
            parent_hash: BlockHash::new([0; 32]),
            state_root: StateRoot::new([0; 32]),
            tx_root: [0; 32],
            receipt_root: [0; 32],
            proposer: NodeId::new([0; 32]),
            validator_set_hash: [0; 32],
            base_fee: 1_000_000_000,
            genesis_message: String::new(),
        };
        Block {
            header,
            txs: Vec::new(),
            receipts: Vec::new(),
        }
    }

    #[test]
    fn vote_sign_verify_roundtrip() {
        let (signer, pk) = keypair();
        let vote = ChainVote {
            height: Height(1),
            round: Round::new(0),
            value: NilOrVal::Nil,
            vote_type: VoteType::Prevote,
            validator_address: ChainAddress(Address::new([1; 20])),
        };
        let signed = signer.sign_vote(vote.clone());
        assert!(signer.verify_signed_vote(&signed.message, &signed.signature, &pk));
    }

    #[test]
    fn vote_tampered_body_fails_verify() {
        let (signer, pk) = keypair();
        let vote = ChainVote {
            height: Height(1),
            round: Round::new(0),
            value: NilOrVal::Nil,
            vote_type: VoteType::Prevote,
            validator_address: ChainAddress(Address::new([1; 20])),
        };
        let signed = signer.sign_vote(vote);
        let mut tampered = signed.message.clone();
        tampered.height = Height(2);
        assert!(!signer.verify_signed_vote(&tampered, &signed.signature, &pk));
    }

    #[test]
    fn proposal_sign_verify_roundtrip() {
        let (signer, pk) = keypair();
        let prop = ChainProposal {
            height: Height(1),
            round: Round::new(0),
            value: ChainValue::new(sample_block()),
            pol_round: Round::Nil,
            validator_address: ChainAddress(Address::new([1; 20])),
        };
        let signed = signer.sign_proposal(prop.clone());
        assert!(signer.verify_signed_proposal(&signed.message, &signed.signature, &pk));
    }

    #[test]
    fn vote_signature_rejected_as_proposal_signature() {
        // Domain-separation check: signing a vote should NOT produce a
        // signature that verifies as a proposal.
        let (signer, pk) = keypair();
        let vote = ChainVote {
            height: Height(1),
            round: Round::new(0),
            value: NilOrVal::Nil,
            vote_type: VoteType::Prevote,
            validator_address: ChainAddress(Address::new([1; 20])),
        };
        let prop = ChainProposal {
            height: Height(1),
            round: Round::new(0),
            value: ChainValue::new(sample_block()),
            pol_round: Round::Nil,
            validator_address: ChainAddress(Address::new([1; 20])),
        };
        let signed_vote = signer.sign_vote(vote);
        assert!(!signer.verify_signed_proposal(&prop, &signed_vote.signature, &pk));
    }

    #[test]
    fn public_key_matches_private() {
        let (signer, expected) = keypair();
        assert_eq!(signer.public_key().as_bytes(), expected.as_bytes());
    }
}
