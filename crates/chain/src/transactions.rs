//! On-chain transaction types and signed wire format.
//!
//! Authoritative shape: [`docs/PROTOCOL_SPEC.md`](../../../docs/PROTOCOL_SPEC.md)
//! §11 (lifecycle), §9.3 (stake ops), §13 (governance). Phase 1 Week 1-2
//! lands the types + borsh forms + hash; state application logic lands in
//! Week 3-4.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use arknet_common::types::{
    Address, Amount, Gas, Hash256, Height, NodeId, Nonce, PoolId, PubKey, Signature, TeeCapability,
    Timestamp, TxHash, DOMAIN_TX,
};
use arknet_crypto::hash::blake3;

use crate::errors::{ChainError, Result};
use crate::receipt::ReceiptBatch;

/// Role selector for a stake operation.
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
#[repr(u8)]
pub enum StakeRole {
    /// L1 consensus validator.
    Validator = 0x01,
    /// L2 request router.
    Router = 0x02,
    /// L2 output verifier.
    Verifier = 0x03,
    /// L2 inference compute node.
    Compute = 0x04,
}

/// All stake lifecycle operations. Mirrors PROTOCOL_SPEC §9.3.
#[derive(Clone, PartialEq, Eq, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum StakeOp {
    /// Lock stake for a role (optionally tied to a pool; delegator optional).
    Deposit {
        /// Target node.
        node_id: NodeId,
        /// Role being staked for.
        role: StakeRole,
        /// Optional pool this stake is pinned to.
        pool_id: Option<PoolId>,
        /// Amount in ark_atom.
        amount: Amount,
        /// If set, this stake is a delegation from another address.
        delegator: Option<Address>,
    },
    /// Begin withdrawal (starts unbonding period).
    Withdraw {
        /// Node being unstaked from.
        node_id: NodeId,
        /// Role.
        role: StakeRole,
        /// Pool (if the stake was pool-pinned).
        pool_id: Option<PoolId>,
        /// Amount to withdraw.
        amount: Amount,
    },
    /// Finalize a completed unbonding after the 14-day period.
    Complete {
        /// Node being finalized on.
        node_id: NodeId,
        /// Role.
        role: StakeRole,
        /// Pool (if any).
        pool_id: Option<PoolId>,
        /// Opaque unbonding-id returned by `Withdraw`.
        unbond_id: u64,
    },
    /// Move stake between nodes (1-day cooldown enforced by state layer).
    Redelegate {
        /// Source node.
        from: NodeId,
        /// Destination node.
        to: NodeId,
        /// Role.
        role: StakeRole,
        /// Amount.
        amount: Amount,
    },
}

/// Voting options on a governance proposal.
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
#[repr(u8)]
pub enum VoteChoice {
    /// In favor.
    Yes = 0x01,
    /// Against.
    No = 0x02,
    /// Abstain (counts toward quorum, not toward yes/no).
    Abstain = 0x03,
    /// No-with-veto (burns the proposal deposit if threshold met).
    NoWithVeto = 0x04,
}

/// Governance proposal body. See PROTOCOL_SPEC §13.
#[derive(Clone, PartialEq, Eq, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct Proposal {
    /// Unique proposal identifier (allocated by state at submission).
    pub proposal_id: u64,
    /// Proposer address.
    pub proposer: Address,
    /// Deposit (10,000 ARK by genesis constant).
    pub deposit: Amount,
    /// Human-readable title.
    pub title: String,
    /// Markdown body (capped by size bound on the tx).
    pub body: String,
    /// Timestamp when discussion phase ends (`start + 7d`).
    pub discussion_ends: Timestamp,
    /// Timestamp when voting phase ends (`discussion_ends + 7d`).
    pub voting_ends: Timestamp,
    /// Optional hard-fork activation height.
    pub activation: Option<Height>,
}

/// On-chain model manifest carried by a `RegisterModel` transaction.
///
/// This is a minimal copy of the registry manifest (see `arknet-model-manager`)
/// so the chain crate stays free of model-manager deps.
#[derive(Clone, PartialEq, Eq, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct OnChainModelManifest {
    /// Canonical model identifier, e.g. `"meta-llama/Llama-3-8B-Instruct"`.
    pub model_id: String,
    /// SHA-256 digest of the GGUF artifact.
    pub sha256: Hash256,
    /// Expected byte size of the artifact.
    pub size_bytes: u64,
    /// Mirrors — free-form URLs; verified against `sha256` by pullers.
    pub mirrors: Vec<String>,
    /// License identifier (SPDX short form).
    pub license: String,
}

/// Evidence attached to a [`Transaction::Dispute`].
///
/// The verifier re-executed `job_id` deterministically, derived its
/// own `reexec_output_hash`, and found it diverged from the compute
/// node's `claimed_output_hash` carried on the anchored receipt. The
/// chain cross-references the `job_id` against the receipt ledger,
/// and on mismatch routes through [`arknet_staking::apply_slash`]
/// with [`arknet_staking::Offense::FailedDeterministicVerification`].
///
/// `verifier` identifies the submitter; `reexec_proof` is the
/// verifier's own [`ComputeProof::HashChain`] rebuilt during
/// re-execution so an external light client can re-check the claim.
#[derive(Clone, PartialEq, Eq, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct Dispute {
    /// Job under dispute — must match an already-anchored receipt.
    pub job_id: arknet_common::types::JobId,
    /// Compute node that signed the disputed receipt (slash target).
    pub compute_node: NodeId,
    /// The output hash the receipt claimed.
    pub claimed_output_hash: Hash256,
    /// The output hash the verifier derived on re-execution.
    pub reexec_output_hash: Hash256,
    /// Verifier's node id.
    pub verifier: NodeId,
    /// Verifier's payout address (reporter cut per §10 split).
    pub reporter: Address,
    /// VRF proof that this verifier was selected for `job_id`. §11.
    pub vrf_proof: Vec<u8>,
    /// Deterministic re-execution hash chain.
    pub reexec_proof: crate::receipt::ComputeProof,
}

/// Top-level transaction enum.
///
/// All variants are consensus-relevant. Application logic is implemented
/// in Phase 1 Week 3-4 (`chain/apply.rs`). Ordering of variants is stable
/// because `borsh` tags them by position.
#[derive(Clone, PartialEq, Eq, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum Transaction {
    /// Transfer ARK between accounts.
    Transfer {
        /// Sender.
        from: Address,
        /// Recipient.
        to: Address,
        /// Amount (ark_atom).
        amount: Amount,
        /// Sender nonce.
        nonce: Nonce,
        /// Gas budget (priced via fee market + EIP-1559 base fee).
        fee: Gas,
    },
    /// Any staking lifecycle operation.
    StakeOp(StakeOp),
    /// Anchor a verified L2 receipt batch onto L1.
    ReceiptBatch(ReceiptBatch),
    /// Register a new model in the on-chain registry.
    RegisterModel {
        /// Manifest body.
        manifest: OnChainModelManifest,
        /// Address performing the registration (pays the deposit).
        registrar: Address,
        /// Deposit (10K ARK by genesis constant).
        deposit: Amount,
    },
    /// Submit a governance proposal.
    GovProposal(Proposal),
    /// Cast a vote on an active proposal.
    GovVote {
        /// Proposal under vote.
        proposal_id: u64,
        /// Voter address (must have stake or delegation weight).
        voter: Address,
        /// Choice.
        choice: VoteChoice,
    },
    /// Verifier-submitted dispute against an already-anchored receipt.
    ///
    /// Triggers slashing on the compute node that produced `output_hash`
    /// for `job_id` if a re-execution shows it diverged from the
    /// deterministic ground truth. §11 + §10.
    ///
    /// Appended to the enum rather than inserted so the borsh
    /// discriminants of existing variants remain stable.
    Dispute(Dispute),
    /// Lock user payment in escrow before inference dispatch.
    ///
    /// §11 lifecycle: CREATED → ESCROWED. Funds are held until
    /// settlement or timeout-triggered refund.
    EscrowLock {
        /// User locking the funds.
        from: Address,
        /// Job this escrow covers.
        job_id: arknet_common::types::JobId,
        /// Amount to lock (ark_atom).
        amount: Amount,
        /// Sender nonce (for replay protection).
        nonce: Nonce,
        /// Gas budget.
        fee: Gas,
    },
    /// Settle a locked escrow — release funds to the reward
    /// distribution. Submitted by the router after the receipt is
    /// verified and the dispute window has passed.
    EscrowSettle {
        /// Job whose escrow to settle.
        job_id: arknet_common::types::JobId,
        /// Receipt batch id that proves the job was completed and
        /// verified. Must be present in `CF_RECEIPTS_SEEN`.
        batch_id: Hash256,
        /// Compute node operator address.
        compute_addr: Address,
        /// Verifier address.
        verifier_addr: Address,
        /// Router address.
        router_addr: Address,
        /// Treasury address.
        treasury_addr: Address,
    },
    /// Mint block rewards for a settled receipt. Emitted by the
    /// proposer as part of the block body — not user-submitted.
    ///
    /// §8.2: "next block MUST include matching REWARD_MINT." A
    /// proposer that omits a valid RewardMint is slashable under
    /// `CensoringMints`.
    RewardMint {
        /// Job the reward covers.
        job_id: arknet_common::types::JobId,
        /// Total reward amount (user payment + block emission).
        total_reward: Amount,
        /// Compute node operator address.
        compute_addr: Address,
        /// Verifier address.
        verifier_addr: Address,
        /// Router address.
        router_addr: Address,
        /// Treasury address.
        treasury_addr: Address,
        /// Output token count (for audit trail).
        output_tokens: u32,
    },
    /// Register (or update) a compute node's TEE capability on-chain.
    ///
    /// The `capability` carries a platform-specific attestation quote
    /// and an enclave-bound public key. Users encrypt prompts to the
    /// enclave key for confidential inference — the host OS never sees
    /// plaintext.
    ///
    /// At genesis the chain validates structural well-formedness
    /// (non-empty quote, bounded size). Full cryptographic verification
    /// against Intel/AMD root CAs is activated by governance once the
    /// verification library is audited.
    ///
    /// Faking a TEE attestation (claiming TEE but serving outside an
    /// enclave) is slashable under `FakeTeeAttestation` — 100% of stake.
    RegisterTeeCapability {
        /// Node registering TEE support.
        node_id: NodeId,
        /// Operator address (signer).
        operator: Address,
        /// TEE attestation + enclave-bound pubkey.
        capability: TeeCapability,
    },
    /// Register a node as a public gateway (discoverable RPC endpoint).
    ///
    /// Operators expose their RPC port to serve user inference requests.
    /// HTTPS gateways earn a 1.2x reward multiplier on routed jobs.
    /// Users can request `require_https: true` to only route through
    /// HTTPS gateways (no silent downgrade).
    RegisterGateway {
        /// Node registering as a gateway.
        node_id: NodeId,
        /// Operator address (signer).
        operator: Address,
        /// Public RPC URL (e.g. `"https://rpc.mynode.com"` or `"http://203.0.113.42:26657"`).
        url: String,
        /// `true` if the URL uses HTTPS (TLS-terminated).
        https: bool,
    },
    /// Remove this node from the public gateway registry.
    UnregisterGateway {
        /// Node to remove.
        node_id: NodeId,
        /// Operator address (signer).
        operator: Address,
    },
}

impl Transaction {
    /// Domain-separated transaction hash. Pattern:
    /// `blake3(DOMAIN_TX || borsh(tx))`.
    pub fn hash(&self) -> TxHash {
        let body = borsh::to_vec(self).expect("transaction borsh encoding is infallible");
        let mut buf = Vec::with_capacity(DOMAIN_TX.len() + body.len());
        buf.extend_from_slice(DOMAIN_TX);
        buf.extend_from_slice(&body);
        TxHash::new(*blake3(&buf).as_bytes())
    }

    /// Total borsh-encoded size in bytes.
    pub fn encoded_len(&self) -> usize {
        borsh::to_vec(self)
            .map(|v| v.len())
            .expect("transaction borsh encoding is infallible")
    }
}

/// Signed wire-format transaction.
///
/// Signature covers the *transaction hash*, not the raw bytes — hashing
/// domain-separates from block signatures and lets light clients verify
/// without re-encoding.
#[derive(Clone, PartialEq, Eq, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct SignedTransaction {
    /// The transaction body.
    pub tx: Transaction,
    /// Signer's scheme-tagged public key.
    pub signer: PubKey,
    /// Signature over `tx.hash()`.
    pub signature: Signature,
}

impl SignedTransaction {
    /// Hash of the inner transaction. Matches `self.tx.hash()`.
    pub fn hash(&self) -> TxHash {
        self.tx.hash()
    }

    /// Borsh-encoded size of the full signed wire record.
    pub fn encoded_len(&self) -> usize {
        borsh::to_vec(self)
            .map(|v| v.len())
            .expect("signed tx borsh encoding is infallible")
    }
}

/// Hard size cap on any single signed transaction (1 MiB). Rejected
/// before consensus so malicious peers cannot DOS mempools with jumbo
/// txs.
pub const MAX_SIGNED_TX_BYTES: usize = 1024 * 1024;

/// Validate size bound on a signed transaction. Returns `ChainError::Oversize`
/// on failure.
pub fn check_signed_tx_size(stx: &SignedTransaction) -> Result<()> {
    let len = stx.encoded_len();
    if len > MAX_SIGNED_TX_BYTES {
        return Err(ChainError::Oversize {
            what: "signed transaction",
            actual: len,
            max: MAX_SIGNED_TX_BYTES,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_common::types::{SignatureScheme, ATOMS_PER_ARK};

    fn sample_signer() -> (PubKey, Signature) {
        (
            PubKey::ed25519([0x11; 32]),
            Signature::new(SignatureScheme::Ed25519, vec![0x22; 64]).unwrap(),
        )
    }

    fn sample_transfer() -> Transaction {
        Transaction::Transfer {
            from: Address::new([0xaa; 20]),
            to: Address::new([0xbb; 20]),
            amount: 5 * ATOMS_PER_ARK,
            nonce: 1,
            fee: 21_000,
        }
    }

    #[test]
    fn transfer_borsh_roundtrip() {
        let tx = sample_transfer();
        let bytes = borsh::to_vec(&tx).unwrap();
        let decoded: Transaction = borsh::from_slice(&bytes).unwrap();
        assert_eq!(tx, decoded);
    }

    #[test]
    fn stake_op_borsh_roundtrip() {
        let op = StakeOp::Deposit {
            node_id: NodeId::new([1; 32]),
            role: StakeRole::Compute,
            pool_id: Some(PoolId::new([2; 16])),
            amount: 2_500 * ATOMS_PER_ARK,
            delegator: None,
        };
        let tx = Transaction::StakeOp(op.clone());
        let bytes = borsh::to_vec(&tx).unwrap();
        let decoded: Transaction = borsh::from_slice(&bytes).unwrap();
        assert_eq!(tx, decoded);
    }

    #[test]
    fn gov_proposal_borsh_roundtrip() {
        let prop = Proposal {
            proposal_id: 42,
            proposer: Address::new([0xcc; 20]),
            deposit: 10_000 * ATOMS_PER_ARK,
            title: "Raise base fee target to 60%".to_string(),
            body: "see forum thread".to_string(),
            discussion_ends: 1_700_000_000_000,
            voting_ends: 1_700_604_800_000,
            activation: None,
        };
        let tx = Transaction::GovProposal(prop);
        let bytes = borsh::to_vec(&tx).unwrap();
        let decoded: Transaction = borsh::from_slice(&bytes).unwrap();
        assert_eq!(tx, decoded);
    }

    #[test]
    fn gov_vote_borsh_roundtrip() {
        let tx = Transaction::GovVote {
            proposal_id: 42,
            voter: Address::new([0xdd; 20]),
            choice: VoteChoice::Yes,
        };
        let bytes = borsh::to_vec(&tx).unwrap();
        let decoded: Transaction = borsh::from_slice(&bytes).unwrap();
        assert_eq!(tx, decoded);
    }

    #[test]
    fn register_tee_borsh_roundtrip() {
        use arknet_common::types::{TeeCapability, TeePlatform};
        let tx = Transaction::RegisterTeeCapability {
            node_id: NodeId::new([0xaa; 32]),
            operator: Address::new([0xbb; 20]),
            capability: TeeCapability {
                platform: TeePlatform::IntelTdx,
                quote: vec![0xde; 64],
                enclave_pubkey: PubKey::ed25519([0xcc; 32]),
            },
        };
        let bytes = borsh::to_vec(&tx).unwrap();
        let decoded: Transaction = borsh::from_slice(&bytes).unwrap();
        assert_eq!(tx, decoded);
    }

    #[test]
    fn transaction_hash_is_deterministic() {
        let tx = sample_transfer();
        let h1 = tx.hash();
        let h2 = tx.hash();
        assert_eq!(h1, h2);
    }

    #[test]
    fn transaction_hash_differs_with_content() {
        let a = sample_transfer();
        let b = Transaction::Transfer {
            from: Address::new([0xaa; 20]),
            to: Address::new([0xbb; 20]),
            amount: 6 * ATOMS_PER_ARK, // different amount
            nonce: 1,
            fee: 21_000,
        };
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn signed_tx_roundtrip() {
        let tx = sample_transfer();
        let (signer, signature) = sample_signer();
        let stx = SignedTransaction {
            tx,
            signer,
            signature,
        };
        let bytes = borsh::to_vec(&stx).unwrap();
        let decoded: SignedTransaction = borsh::from_slice(&bytes).unwrap();
        assert_eq!(stx, decoded);
        assert_eq!(stx.hash(), decoded.hash());
    }

    #[test]
    fn signed_tx_size_check_accepts_normal() {
        let tx = sample_transfer();
        let (signer, signature) = sample_signer();
        let stx = SignedTransaction {
            tx,
            signer,
            signature,
        };
        assert!(check_signed_tx_size(&stx).is_ok());
    }

    #[test]
    fn signed_tx_size_check_rejects_oversize() {
        // Synthesize an oversize tx by stuffing the proposal body.
        let prop = Proposal {
            proposal_id: 0,
            proposer: Address::default(),
            deposit: 0,
            title: "x".to_string(),
            body: "x".repeat(MAX_SIGNED_TX_BYTES + 1),
            discussion_ends: 0,
            voting_ends: 0,
            activation: None,
        };
        let (signer, signature) = sample_signer();
        let stx = SignedTransaction {
            tx: Transaction::GovProposal(prop),
            signer,
            signature,
        };
        let err = check_signed_tx_size(&stx).unwrap_err();
        assert!(matches!(err, ChainError::Oversize { .. }));
    }
}
