//! Translation between malachite consensus messages and
//! `arknet-network` gossip.
//!
//! Malachite's wire types (`SignedVote`, `SignedProposal`,
//! `ConsensusMsg`, `Round`, `NilOrVal<ValueId>`) are not borsh-derivable
//! — they use `derive_where` macros and embed foreign types. Rather
//! than patching upstream, this module defines small, borsh-native
//! **wire structs** that mirror those messages and provides encode /
//! decode helpers. The bridge never gossips malachite types directly;
//! it always passes through the wire structs.
//!
//! # Topic map (PROTOCOL_SPEC §6)
//!
//! - `arknet/consensus/vote/1` — [`WireVote`] (prevote + precommit)
//! - `arknet/block/prop/1`    — [`WireProposal`] (proposal + block body)
//!
//! # Invariants
//!
//! - Canonical encoding. The bytes produced by [`encode_vote`] /
//!   [`encode_proposal`] are what the signer signs and what peers
//!   hash on gossipsub, so divergence between signer and wire bytes
//!   would silently invalidate every signature.
//! - Domain separation. Topic names carry a `/1` version suffix; a
//!   breaking schema change ships on `/2` without renaming `/1`.

use borsh::{BorshDeserialize, BorshSerialize};

use arknet_chain::block::Block;
use arknet_common::types::{Address, BlockHash};
use malachitebft_core_types::{
    NilOrVal, Round, SignedMessage, SignedProposal, SignedVote, VoteType,
};
use malachitebft_signing_ed25519::{PublicKey, Signature as EdSignature};

use crate::height::Height;
use crate::proposal::ChainProposal;
use crate::validators::ChainAddress;
use crate::value::{BlockId, ChainValue};
use crate::vote::ChainVote;

/// Topic name for vote gossip. Mirrors [`arknet_network::gossip::consensus_vote`]
/// but as a plain string constant so the bridge doesn't need to depend on
/// libp2p types.
pub const TOPIC_CONSENSUS_VOTE: &str = "arknet/consensus/vote/1";

/// Topic name for proposal gossip.
pub const TOPIC_BLOCK_PROP: &str = "arknet/block/prop/1";

/// Wire-format vote. Borsh-derivable; exactly matches what
/// [`crate::signing`] signs.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct WireVote {
    /// Height of the vote.
    pub height: u64,
    /// Round, encoded as `i64` (`-1` for `Round::Nil`).
    pub round: i64,
    /// Vote type: `1 = Prevote`, `2 = Precommit`.
    pub vote_type: u8,
    /// Value-id tag: `0 = Nil`, `1 = Val(..)`.
    pub value_tag: u8,
    /// When `value_tag == 1`, the 32-byte block hash; zeroed otherwise.
    pub value: [u8; 32],
    /// Operator address (20 bytes).
    pub validator_address: [u8; 20],
    /// Ed25519 signature (64 bytes).
    pub signature: [u8; 64],
}

/// Wire-format proposal. Carries the full block body so malachite can
/// both queue the `SignedProposal` and produce the matching
/// `ProposedValue` on the receiving side — ProposalOnly mode sends no
/// separate parts.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct WireProposal {
    /// Height of the proposal.
    pub height: u64,
    /// Round, encoded as `i64` (`-1` for `Round::Nil`).
    pub round: i64,
    /// Proof-of-lock round, encoded as `i64` (`-1` for `Round::Nil`).
    pub pol_round: i64,
    /// Proposer address (20 bytes).
    pub validator_address: [u8; 20],
    /// Borsh-encoded [`arknet_chain::Block`]. Opaque at this layer —
    /// decoded by [`WireProposal::block`].
    pub block: Vec<u8>,
    /// Ed25519 signature (64 bytes) over the canonical proposal bytes
    /// (see [`crate::signing`]).
    pub signature: [u8; 64],
}

/// Errors from wire encoding / decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// Borsh decode failed (malformed peer or version drift).
    Decode(String),
    /// Wire encoding violates an internal invariant (e.g. value tag
    /// 1 but all-zero hash).
    Invariant(&'static str),
    /// Value tag byte was neither 0 (Nil) nor 1 (Val).
    BadValueTag(u8),
    /// Vote type byte was neither 1 (Prevote) nor 2 (Precommit).
    BadVoteType(u8),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode(s) => write!(f, "wire decode: {s}"),
            Self::Invariant(s) => write!(f, "wire invariant: {s}"),
            Self::BadValueTag(b) => write!(f, "wire value tag: unexpected byte 0x{b:02x}"),
            Self::BadVoteType(b) => write!(f, "wire vote type: unexpected byte 0x{b:02x}"),
        }
    }
}

impl std::error::Error for WireError {}

// ─── Encoders ────────────────────────────────────────────────────────────

/// Serialize a malachite-signed vote to bytes suitable for gossip.
pub fn encode_signed_vote(sv: &SignedVote<crate::context::ArknetContext>) -> Vec<u8> {
    let msg = &sv.message;
    let wire = WireVote {
        height: msg.height.0,
        round: msg.round.as_i64(),
        vote_type: match msg.vote_type {
            VoteType::Prevote => 0x01,
            VoteType::Precommit => 0x02,
        },
        value_tag: match &msg.value {
            NilOrVal::Nil => 0x00,
            NilOrVal::Val(_) => 0x01,
        },
        value: match &msg.value {
            NilOrVal::Nil => [0u8; 32],
            NilOrVal::Val(id) => *id.0.as_bytes(),
        },
        validator_address: *msg.validator_address.0.as_bytes(),
        signature: sv.signature.to_bytes(),
    };
    borsh::to_vec(&wire).expect("wire vote encoding is infallible")
}

/// Serialize a malachite-signed proposal + block body for gossip.
pub fn encode_signed_proposal(sp: &SignedProposal<crate::context::ArknetContext>) -> Vec<u8> {
    let msg = &sp.message;
    let block_bytes = borsh::to_vec(&msg.value.block).expect("block borsh encoding is infallible");
    let wire = WireProposal {
        height: msg.height.0,
        round: msg.round.as_i64(),
        pol_round: msg.pol_round.as_i64(),
        validator_address: *msg.validator_address.0.as_bytes(),
        block: block_bytes,
        signature: sp.signature.to_bytes(),
    };
    borsh::to_vec(&wire).expect("wire proposal encoding is infallible")
}

// ─── Decoders ────────────────────────────────────────────────────────────

/// Parse gossip bytes back into a malachite-signed vote. Signature is
/// not verified here — that's the engine's job via the signing provider.
pub fn decode_signed_vote(
    bytes: &[u8],
) -> Result<SignedVote<crate::context::ArknetContext>, WireError> {
    let wire: WireVote = borsh::from_slice(bytes).map_err(|e| WireError::Decode(e.to_string()))?;
    let vote_type = match wire.vote_type {
        0x01 => VoteType::Prevote,
        0x02 => VoteType::Precommit,
        other => return Err(WireError::BadVoteType(other)),
    };
    let value = match wire.value_tag {
        0x00 => {
            if wire.value != [0u8; 32] {
                return Err(WireError::Invariant("nil tag with non-zero value bytes"));
            }
            NilOrVal::Nil
        }
        0x01 => NilOrVal::Val(BlockId(BlockHash::new(wire.value))),
        other => return Err(WireError::BadValueTag(other)),
    };
    let chain_vote = ChainVote {
        height: Height(wire.height),
        round: Round::from(wire.round),
        value,
        vote_type,
        validator_address: ChainAddress(Address::new(wire.validator_address)),
    };
    Ok(SignedMessage::new(
        chain_vote,
        EdSignature::from_bytes(wire.signature),
    ))
}

/// Parse gossip bytes back into a malachite-signed proposal. The block
/// body is decoded eagerly so the engine has a ready-to-use
/// [`ChainValue`].
pub fn decode_signed_proposal(
    bytes: &[u8],
) -> Result<SignedProposal<crate::context::ArknetContext>, WireError> {
    let wire: WireProposal =
        borsh::from_slice(bytes).map_err(|e| WireError::Decode(e.to_string()))?;
    let block: Block = borsh::from_slice(&wire.block)
        .map_err(|e| WireError::Decode(format!("block body: {e}")))?;
    let chain_proposal = ChainProposal {
        height: Height(wire.height),
        round: Round::from(wire.round),
        value: ChainValue::new(block),
        pol_round: Round::from(wire.pol_round),
        validator_address: ChainAddress(Address::new(wire.validator_address)),
    };
    Ok(SignedMessage::new(
        chain_proposal,
        EdSignature::from_bytes(wire.signature),
    ))
}

// ─── Outbound routing ────────────────────────────────────────────────────

/// Classifies a `SignedConsensusMsg` into (topic, wire bytes) ready for
/// [`arknet_network::NetworkHandle::publish`].
///
/// The engine's effect handler calls this for every
/// [`malachitebft_core_consensus::Effect::PublishConsensusMsg`].
pub fn outbound_message(
    msg: &malachitebft_core_consensus::SignedConsensusMsg<crate::context::ArknetContext>,
) -> (&'static str, Vec<u8>) {
    use malachitebft_core_consensus::SignedConsensusMsg;
    match msg {
        SignedConsensusMsg::Vote(sv) => (TOPIC_CONSENSUS_VOTE, encode_signed_vote(sv)),
        SignedConsensusMsg::Proposal(sp) => (TOPIC_BLOCK_PROP, encode_signed_proposal(sp)),
    }
}

/// A decoded inbound gossip message. The bridge does not peek at the
/// signature or content semantics — it just hands the decoded malachite
/// message to the engine.
///
/// Both variants are boxed so the enum fits the clippy
/// `large_enum_variant` budget — a full proposal carries an entire
/// block body so the size imbalance with `Vote` matters.
#[derive(Debug)]
pub enum InboundMsg {
    /// A vote that should be fed to the state machine as
    /// `Input::Vote`.
    Vote(Box<SignedVote<crate::context::ArknetContext>>),
    /// A proposal. The engine will emit BOTH `Input::Proposal(...)`
    /// and `Input::ProposedValue(..., ValueOrigin::Consensus)` — the
    /// bridge only produces the signed proposal.
    Proposal(Box<SignedProposal<crate::context::ArknetContext>>),
}

/// Parse an inbound gossip message. `None` means the topic is not a
/// consensus topic and the engine should ignore it.
pub fn classify_inbound(topic: &str, data: &[u8]) -> Option<Result<InboundMsg, WireError>> {
    match topic {
        TOPIC_CONSENSUS_VOTE => {
            Some(decode_signed_vote(data).map(|sv| InboundMsg::Vote(Box::new(sv))))
        }
        TOPIC_BLOCK_PROP => {
            Some(decode_signed_proposal(data).map(|sp| InboundMsg::Proposal(Box::new(sp))))
        }
        _ => None,
    }
}

/// Recover the public key bytes a peer's signature was produced under
/// from a `SignedVote`'s validator address. Used by the effect handler
/// for `VerifySignature` lookups.
///
/// Returns `None` when the address is absent from the active validator
/// set (malformed peer or byzantine vote from an ejected validator).
pub fn pubkey_for_address(
    validators: &crate::validators::ChainValidatorSet,
    address: &ChainAddress,
) -> Option<PublicKey> {
    use malachitebft_core_types::{Validator as _, ValidatorSet as _};
    validators.get_by_address(address).map(|v| *v.public_key())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ArknetContext;
    use crate::height::Height;
    use crate::signing::ArknetSigningProvider;
    use arknet_chain::block::{Block, BlockHeader};
    use arknet_common::types::{Address, BlockHash, NodeId, StateRoot};
    use malachitebft_core_types::{Context as _, NilOrVal, Round, SigningProvider, VoteType};
    use malachitebft_signing_ed25519::Ed25519;

    fn keypair() -> (ArknetSigningProvider, PublicKey) {
        use rand::rngs::OsRng;
        let sk = Ed25519::generate_keypair(OsRng);
        let pk = sk.public_key();
        (ArknetSigningProvider::new(sk), pk)
    }

    fn sample_block() -> Block {
        Block {
            header: BlockHeader {
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
            },
            txs: Vec::new(),
            receipts: Vec::new(),
        }
    }

    #[test]
    fn vote_roundtrip_nil() {
        let (signer, _pk) = keypair();
        let vote = ArknetContext.new_prevote(
            Height(7),
            Round::new(2),
            NilOrVal::Nil,
            ChainAddress(Address::new([3; 20])),
        );
        let signed = signer.sign_vote(vote);
        let bytes = encode_signed_vote(&signed);
        let decoded = decode_signed_vote(&bytes).unwrap();
        assert_eq!(decoded.message, signed.message);
        assert_eq!(decoded.signature.to_bytes(), signed.signature.to_bytes());
    }

    #[test]
    fn vote_roundtrip_val() {
        let (signer, _pk) = keypair();
        let id = BlockId(BlockHash::new([0xcd; 32]));
        let vote = ArknetContext.new_precommit(
            Height(42),
            Round::new(0),
            NilOrVal::Val(id),
            ChainAddress(Address::new([5; 20])),
        );
        let signed = signer.sign_vote(vote);
        let bytes = encode_signed_vote(&signed);
        let decoded = decode_signed_vote(&bytes).unwrap();
        assert_eq!(decoded.message, signed.message);
        assert_eq!(decoded.signature.to_bytes(), signed.signature.to_bytes());
        assert_eq!(decoded.message.vote_type, VoteType::Precommit);
    }

    #[test]
    fn proposal_roundtrip() {
        let (signer, _pk) = keypair();
        let prop = ChainProposal {
            height: Height(3),
            round: Round::new(1),
            value: ChainValue::new(sample_block()),
            pol_round: Round::Nil,
            validator_address: ChainAddress(Address::new([7; 20])),
        };
        let signed = signer.sign_proposal(prop);
        let bytes = encode_signed_proposal(&signed);
        let decoded = decode_signed_proposal(&bytes).unwrap();
        assert_eq!(decoded.message, signed.message);
        assert_eq!(decoded.signature.to_bytes(), signed.signature.to_bytes());
    }

    #[test]
    fn verify_after_wire_roundtrip() {
        // The big invariant: signatures must still verify after bytes
        // travel through the wire. If the canonical-bytes layout in
        // signing.rs drifts from the wire format here, this test
        // breaks loudly.
        let (signer, pk) = keypair();
        let vote = ArknetContext.new_prevote(
            Height(1),
            Round::new(0),
            NilOrVal::Val(BlockId(BlockHash::new([0xaa; 32]))),
            ChainAddress(Address::new([1; 20])),
        );
        let signed = signer.sign_vote(vote);
        let bytes = encode_signed_vote(&signed);
        let decoded = decode_signed_vote(&bytes).unwrap();
        assert!(signer.verify_signed_vote(&decoded.message, &decoded.signature, &pk));
    }

    #[test]
    fn verify_after_proposal_roundtrip() {
        let (signer, pk) = keypair();
        let prop = ChainProposal {
            height: Height(9),
            round: Round::new(0),
            value: ChainValue::new(sample_block()),
            pol_round: Round::Nil,
            validator_address: ChainAddress(Address::new([1; 20])),
        };
        let signed = signer.sign_proposal(prop);
        let bytes = encode_signed_proposal(&signed);
        let decoded = decode_signed_proposal(&bytes).unwrap();
        assert!(signer.verify_signed_proposal(&decoded.message, &decoded.signature, &pk));
    }

    #[test]
    fn classify_inbound_routes_topics() {
        let (signer, _pk) = keypair();
        let vote = ArknetContext.new_prevote(
            Height(1),
            Round::new(0),
            NilOrVal::Nil,
            ChainAddress(Address::new([1; 20])),
        );
        let sv = signer.sign_vote(vote);
        let bytes = encode_signed_vote(&sv);
        match classify_inbound(TOPIC_CONSENSUS_VOTE, &bytes) {
            Some(Ok(InboundMsg::Vote(b))) => {
                assert_eq!(b.message.height, Height(1));
            }
            other => panic!("expected Vote, got {other:?}"),
        }
        assert!(classify_inbound("arknet/tx/mempool/1", &bytes).is_none());
    }

    #[test]
    fn classify_inbound_rejects_garbage() {
        let res = classify_inbound(TOPIC_CONSENSUS_VOTE, &[0xff, 0xff]);
        match res {
            Some(Err(WireError::Decode(_))) => {}
            other => panic!("expected decode error, got {other:?}"),
        }
    }

    #[test]
    fn outbound_routes_vote_and_proposal_topics() {
        use malachitebft_core_consensus::SignedConsensusMsg;
        let (signer, _pk) = keypair();
        let vote = ArknetContext.new_prevote(
            Height(1),
            Round::new(0),
            NilOrVal::Nil,
            ChainAddress(Address::new([1; 20])),
        );
        let sv = signer.sign_vote(vote);
        let (topic, _) = outbound_message(&SignedConsensusMsg::Vote(sv));
        assert_eq!(topic, TOPIC_CONSENSUS_VOTE);

        let prop = ChainProposal {
            height: Height(1),
            round: Round::new(0),
            value: ChainValue::new(sample_block()),
            pol_round: Round::Nil,
            validator_address: ChainAddress(Address::new([1; 20])),
        };
        let sp = signer.sign_proposal(prop);
        let (topic, _) = outbound_message(&SignedConsensusMsg::Proposal(sp));
        assert_eq!(topic, TOPIC_BLOCK_PROP);
    }

    #[test]
    fn pubkey_for_address_lookup() {
        use crate::validators::ChainValidatorSet;
        use arknet_chain::validator::ValidatorInfo;
        use arknet_common::types::PubKey;

        let sk = Ed25519::generate_keypair(rand::rngs::OsRng);
        let pk_bytes = *sk.public_key().as_bytes();
        let info = ValidatorInfo {
            node_id: NodeId::new([1; 32]),
            consensus_key: PubKey::ed25519(pk_bytes),
            operator: Address::new([9; 20]),
            bonded_stake: 0,
            voting_power: 1,
            is_genesis: true,
            jailed: false,
        };
        let vs = ChainValidatorSet::from_infos(&[info]).unwrap();
        let addr = ChainAddress(Address::new([9; 20]));
        assert!(pubkey_for_address(&vs, &addr).is_some());
        assert!(pubkey_for_address(&vs, &ChainAddress(Address::new([7; 20]))).is_none());
    }
}
