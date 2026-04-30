//! Transaction application: `SignedTransaction` → state mutation.
//!
//! Lenient rejection model (Cosmos-style): invalid txs are discarded as
//! `TxOutcome::Rejected(reason)` without poisoning the block. The proposer
//! chooses whether to include a tx; the state layer only answers "does it
//! apply cleanly?"
//!
//! # Supported transactions
//!
//! - [`Transaction::Transfer`] — nonce, balance, fee burn.
//! - [`Transaction::StakeOp`] — deposit, withdraw, complete, redelegate.
//! - [`Transaction::ReceiptBatch`] — Merkle-verified receipt anchoring.
//! - [`Transaction::RegisterModel`] — on-chain model registry (10K ARK deposit).
//! - [`Transaction::EscrowLock`] / [`Transaction::EscrowSettle`] — escrow lifecycle.
//! - [`Transaction::RewardMint`] — block reward distribution (proposer-only).
//! - [`Transaction::GovProposal`] / [`Transaction::GovVote`] — governance.
//! - [`Transaction::Dispute`] — verifier-submitted slashing evidence.
//!
//! # Fee model
//!
//! Per PROTOCOL_SPEC §7.2: the EIP-1559 base fee is **burned** (subtracted
//! from the sender's balance, credited to nobody). The `fee` field is the
//! gas budget priced at 1 ark_atom/gas.

use arknet_common::types::{Address, Amount, Gas, Height, JobId, Nonce};

use crate::errors::{ChainError, Result};
use crate::state::BlockCtx;
use crate::transactions::{SignedTransaction, Transaction};

/// Outcome of a single `apply_tx` call. Lenient: rejection is a normal
/// return, not a [`ChainError`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TxOutcome {
    /// State mutated cleanly.
    Applied {
        /// Gas consumed (for block gas accounting).
        gas_used: Gas,
    },
    /// Tx was not applied; state is unchanged.
    Rejected(RejectReason),
}

/// Why a transaction was rejected. Used by mempool / proposer to filter bad
/// txs without halting consensus.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RejectReason {
    /// Sender account has insufficient balance for `amount + fee`.
    InsufficientBalance {
        /// Amount the sender owns.
        have: Amount,
        /// Amount required (amount + fee).
        need: Amount,
    },
    /// Nonce mismatch — sender replayed or skipped ahead.
    NonceMismatch {
        /// Nonce expected by state.
        expected: Nonce,
        /// Nonce the tx carried.
        got: Nonce,
    },
    /// Fee is below the protocol floor (must cover base transfer gas).
    FeeTooLow {
        /// Minimum fee required.
        min: Gas,
        /// Fee the tx offered.
        got: Gas,
    },
    /// Self-transfer (`from == to`) — disallowed to keep the transfer flow
    /// simple and avoid nonce-only traffic that mutates nothing.
    SelfTransfer,
    /// No stake entry exists for the (node, role, pool, delegator) tuple.
    /// Surfaced by Withdraw / Redelegate.
    StakeNotFound,
    /// Withdraw / Redelegate asked for more than the entry holds.
    StakeExceeded {
        /// Amount requested.
        requested: Amount,
        /// Amount available in the entry.
        available: Amount,
    },
    /// `StakeOp::Complete` called before the unbonding window elapsed.
    UnbondingNotComplete {
        /// Current block height.
        current: Height,
        /// Earliest height at which Complete may land.
        completes_at: Height,
    },
    /// `StakeOp::Complete` targets a non-existent unbonding id.
    UnbondingNotFound,
    /// Redelegate rejected during the 1-day cooldown.
    RedelegateCooldown {
        /// Blocks still to wait.
        blocks_remaining: Height,
    },
    /// Third-party delegation (delegator != sender) — reserved for Phase 2.
    ThirdPartyDelegation,
    /// Redelegate source and destination are the same node.
    RedelegateSameNode,
    /// Receipt batch was empty or had zero-receipt contents.
    EmptyReceiptBatch,
    /// Receipt batch's on-wire `merkle_root` didn't match the root we
    /// recomputed from `receipts` — corrupt or crafted batch.
    ReceiptMerkleMismatch,
    /// One of the batch's `job_id`s is already present in the receipt
    /// ledger from a prior block (§6 "seen exactly once" invariant).
    ReceiptDoubleAnchor {
        /// Borsh-encoded `job_id` bytes.
        job_id_hex: String,
    },
    /// Dispute's `claimed_output_hash == reexec_output_hash` — nothing
    /// to slash.
    DisputeOutputMatches,
    /// Dispute references a `job_id` that was never anchored.
    DisputeReceiptNotFound,
    /// Transaction variant is not yet live in this phase — see the phase
    /// plan.
    NotYetImplemented(&'static str),
}

/// Minimum gas cost of a `Transfer` transaction. Matches EVM's 21,000 base
/// gas — not binding beyond that reference.
pub const BASE_TRANSFER_GAS: Gas = 21_000;

/// Apply a signed transaction against the buffered block context.
///
/// Returns [`TxOutcome::Applied`] or [`TxOutcome::Rejected`] as appropriate.
/// Errors ([`ChainError`]) are reserved for unrecoverable issues (DB I/O,
/// encoding) — they abort the whole block.
pub fn apply_tx(ctx: &mut BlockCtx<'_>, tx: &SignedTransaction) -> Result<TxOutcome> {
    // Height is sourced from META; the stake handlers fall back to 0
    // on a fresh chain (block 0) which is correct — bootstrap-epoch
    // checks treat block 0 as inside the window.
    let height = ctx.current_height()?.unwrap_or(0);
    match &tx.tx {
        Transaction::Transfer {
            from,
            to,
            amount,
            nonce,
            fee,
        } => apply_transfer(ctx, from, to, *amount, *nonce, *fee),
        Transaction::StakeOp(op) => {
            let sender = derive_address_from_signer(&tx.signer);
            crate::stake_apply::apply_stake_op(ctx, op, &sender, height)
        }
        Transaction::ReceiptBatch(batch) => apply_receipt_batch(ctx, batch, height),
        Transaction::Dispute(d) => apply_dispute(ctx, d, height),
        Transaction::EscrowLock {
            from,
            job_id,
            amount,
            nonce,
            fee,
        } => apply_escrow_lock(ctx, from, job_id, *amount, *nonce, *fee, height),
        Transaction::EscrowSettle {
            job_id,
            batch_id: _,
            compute_addr,
            verifier_addr,
            router_addr,
            treasury_addr,
        } => apply_escrow_settle(
            ctx,
            job_id,
            compute_addr,
            verifier_addr,
            router_addr,
            treasury_addr,
            height,
        ),
        Transaction::RewardMint {
            job_id,
            total_reward,
            compute_addr,
            verifier_addr,
            router_addr,
            treasury_addr,
            output_tokens: _,
        } => apply_reward_mint(
            ctx,
            job_id,
            *total_reward,
            compute_addr,
            verifier_addr,
            router_addr,
            treasury_addr,
        ),
        Transaction::RegisterModel {
            manifest,
            registrar,
            deposit,
        } => apply_register_model(ctx, manifest, registrar, *deposit),
        Transaction::GovProposal(p) => apply_gov_proposal(ctx, &tx.signer, p, height),
        Transaction::GovVote {
            proposal_id,
            voter,
            choice,
        } => apply_gov_vote(ctx, *proposal_id, voter, *choice),
        Transaction::RegisterTeeCapability {
            node_id,
            operator,
            capability,
        } => apply_register_tee(ctx, node_id, operator, capability),
        Transaction::RegisterGateway {
            node_id,
            operator,
            url,
            https,
        } => apply_register_gateway(ctx, node_id, operator, url, *https, height),
        Transaction::UnregisterGateway { node_id, .. } => apply_unregister_gateway(ctx, node_id),
    }
}

/// Gas cost per receipt anchored. Phase-1 flat: `20_000 * receipts`.
pub const RECEIPT_ANCHOR_GAS_PER_RECEIPT: Gas = 20_000;

/// Gas charged for a successful dispute application.
pub const DISPUTE_GAS: Gas = 100_000;

/// Apply a `Transaction::ReceiptBatch`. Validates the batch shape
/// (non-empty, Merkle root matches the recomputed root, no replayed
/// `job_id`s), then records each receipt's `job_id` in
/// `CF_RECEIPTS_SEEN` so a future dispute can look it up. Economic
/// rewards (block-reward minting against `total_tokens`) land with
/// Week 12 + treasury emission.
fn apply_receipt_batch(
    ctx: &mut BlockCtx<'_>,
    batch: &crate::receipt::ReceiptBatch,
    height: Height,
) -> Result<TxOutcome> {
    if batch.receipts.is_empty() {
        return Ok(TxOutcome::Rejected(RejectReason::EmptyReceiptBatch));
    }
    // Recompute Merkle root with the same layout used by
    // `arknet_receipts::compute_merkle_root`. Kept inline here so the
    // chain crate doesn't depend on the receipts crate (which already
    // depends on chain).
    let leaves: Vec<[u8; 32]> = batch
        .receipts
        .iter()
        .map(|r| {
            let body = borsh::to_vec(r).expect("receipt borsh encoding is infallible");
            let mut buf = Vec::with_capacity(body.len() + 25);
            buf.extend_from_slice(b"arknet-receipt-leaf-v1");
            buf.extend_from_slice(&body);
            *arknet_crypto::hash::sha256(&buf).as_bytes()
        })
        .collect();
    let tree = arknet_crypto::merkle::MerkleTree::new(leaves.iter().map(|l| l.as_slice()))
        .map_err(|e| ChainError::Codec(format!("receipt merkle: {e}")))?;
    let recomputed = *tree.root().as_bytes();
    if recomputed != batch.merkle_root {
        return Ok(TxOutcome::Rejected(RejectReason::ReceiptMerkleMismatch));
    }

    // Dedup check + mark are interleaved: consult the overlay (which
    // sees prior marks *this call*), reject on any hit, then mark.
    // This also catches duplicate `job_id`s inside a single batch.
    //
    // On rejection we'd ideally roll back any marks we already buffered
    // — but the overlay only commits when the caller calls `ctx.commit()`,
    // and our caller (consensus) drops the ctx on any rejection up to
    // the block boundary. `apply_tx` returning `Rejected` keeps the
    // block moving but since rejections here are "reject the whole tx"
    // the dirtied overlay is immaterial; `apply_tx` is called with a
    // fresh snapshot from the block builder.
    for r in &batch.receipts {
        if ctx.is_receipt_seen(&r.job_id)? {
            return Ok(TxOutcome::Rejected(RejectReason::ReceiptDoubleAnchor {
                job_id_hex: hex::encode(r.job_id.0),
            }));
        }
        ctx.mark_receipt_seen(&r.job_id, height)?;
    }

    // Bootstrap emission: during bootstrap, every anchored receipt
    // queues a pending reward even without an escrow lock. This
    // solves the fair-launch chicken-and-egg problem — compute nodes
    // earn ARK from block emission for serving free-tier requests,
    // bootstrapping the token supply from zero.
    //
    // Uses `in_bootstrap_epoch` (both height AND validator-count
    // gates) so bootstrap emission stops once 100 validators are
    // active, not just after the 6-month time window.
    let active_count = ctx
        .state()
        .iter_validators()
        .map(|v| v.len() as u32)
        .unwrap_or(0);
    let in_bootstrap = crate::bootstrap::in_bootstrap_epoch(height, active_count);
    if in_bootstrap {
        let epoch = height / crate::bootstrap::EPOCH_LENGTH_BLOCKS;
        let treasury = bootstrap_treasury_address();
        for r in &batch.receipts {
            let compute_addr = node_id_to_address(&r.compute_node);
            let router_addr = node_id_to_address(&r.router_node);
            let tee_mult = if r.verification_tier == crate::receipt::VerificationTier::Tee {
                crate::pending_reward::TEE_MULTIPLIER_TEE
            } else {
                crate::pending_reward::TEE_MULTIPLIER_NONE
            };
            let pr = crate::pending_reward::PendingReward {
                job_id: r.job_id,
                output_tokens: r.output_token_count,
                user_payment: 0,
                epoch,
                compute_addr,
                verifier_addr: treasury,
                router_addr,
                treasury_addr: treasury,
                tee_multiplier_bps: tee_mult,
                https_multiplier_bps: crate::gateway_entry::HTTP_MULTIPLIER_BPS,
            };
            let pr_bytes = borsh::to_vec(&pr)
                .map_err(|e| ChainError::Codec(format!("pending_reward encode: {e}")))?;
            ctx.set_pending_reward(&r.job_id, &pr_bytes)?;
        }
    }

    let gas_used = RECEIPT_ANCHOR_GAS_PER_RECEIPT.saturating_mul(batch.receipts.len() as u64);
    Ok(TxOutcome::Applied { gas_used })
}

/// Apply a `Transaction::Dispute`. §10-§11: if the claimed and
/// re-executed output hashes diverge, slash the compute node. The
/// Week-9 `apply_slash` pathway is called for the real ark_atom
/// movement; here we only gate acceptance.
fn apply_dispute(
    ctx: &mut BlockCtx<'_>,
    d: &crate::transactions::Dispute,
    _height: Height,
) -> Result<TxOutcome> {
    if d.claimed_output_hash == d.reexec_output_hash {
        return Ok(TxOutcome::Rejected(RejectReason::DisputeOutputMatches));
    }
    if !ctx.is_receipt_seen(&d.job_id)? {
        return Ok(TxOutcome::Rejected(RejectReason::DisputeReceiptNotFound));
    }

    // Dispute acceptance gate. The actual slashing (drain + burn /
    // reporter / treasury split) is dispatched from `commit_block`
    // via `arknet_staking::apply_slash`.
    Ok(TxOutcome::Applied {
        gas_used: DISPUTE_GAS,
    })
}

/// Derive the 20-byte account [`Address`] from the signer's public key.
///
/// Matches the derivation used by the genesis loader +
/// [`crate::genesis::genesis_to_validator_info`]: `blake3(pubkey_bytes)[..20]`.
/// Gas for escrow lock/settle/refund.
pub const ESCROW_LOCK_GAS: Gas = 50_000;
/// Gas for escrow settle.
pub const ESCROW_SETTLE_GAS: Gas = 50_000;
/// Gas for reward mint (per recipient line).
pub const REWARD_MINT_GAS: Gas = 30_000;

/// Lock user funds in escrow for a job.
#[allow(clippy::too_many_arguments)]
fn apply_escrow_lock(
    ctx: &mut BlockCtx<'_>,
    sender: &Address,
    job_id: &JobId,
    amount: Amount,
    nonce: Nonce,
    fee: Gas,
    height: Height,
) -> Result<TxOutcome> {
    if amount == 0 {
        return Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
            "zero-amount escrow",
        )));
    }
    // Check for duplicate escrow.
    if ctx.get_escrow(job_id)?.is_some() {
        return Ok(TxOutcome::Rejected(RejectReason::ReceiptDoubleAnchor {
            job_id_hex: hex::encode(job_id.0),
        }));
    }
    // Debit sender.
    let mut acct = ctx.get_account(sender)?.unwrap_or_default();
    if acct.nonce != nonce {
        return Ok(TxOutcome::Rejected(RejectReason::NonceMismatch {
            expected: acct.nonce,
            got: nonce,
        }));
    }
    let total = amount.saturating_add(fee as Amount);
    if acct.balance < total {
        return Ok(TxOutcome::Rejected(RejectReason::InsufficientBalance {
            have: acct.balance,
            need: total,
        }));
    }
    acct.balance -= total;
    acct.nonce += 1;
    ctx.set_account(sender, &acct)?;

    // Create escrow record.
    let entry = crate::escrow_entry::EscrowEntry {
        job_id: *job_id,
        user: *sender,
        amount,
        created_at: height,
        state: crate::escrow_entry::EscrowState::Locked,
    };
    let bytes =
        borsh::to_vec(&entry).map_err(|e| ChainError::Codec(format!("escrow encode: {e}")))?;
    ctx.set_escrow(job_id, &bytes)?;

    Ok(TxOutcome::Applied {
        gas_used: ESCROW_LOCK_GAS,
    })
}

/// Settle a locked escrow. Distributes the user payment via the
/// 75/7/5/5/3/5 split immediately, and queues a pending reward for
/// the block-emission component (minted at the next epoch boundary).
#[allow(clippy::too_many_arguments)]
fn apply_escrow_settle(
    ctx: &mut BlockCtx<'_>,
    job_id: &JobId,
    compute_addr: &Address,
    verifier_addr: &Address,
    router_addr: &Address,
    treasury_addr: &Address,
    height: Height,
) -> Result<TxOutcome> {
    let raw = match ctx.get_escrow(job_id)? {
        Some(b) => b,
        None => {
            return Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
                "escrow not found for settle",
            )));
        }
    };
    let mut entry: crate::escrow_entry::EscrowEntry =
        borsh::from_slice(&raw).map_err(|e| ChainError::Codec(format!("escrow decode: {e}")))?;

    if entry.state != crate::escrow_entry::EscrowState::Locked {
        return Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
            "escrow not in Locked state",
        )));
    }

    // Phase 1: distribute the user payment portion immediately.
    credit_reward_split(
        ctx,
        entry.amount,
        compute_addr,
        verifier_addr,
        router_addr,
        treasury_addr,
    )?;

    // Queue the block-emission reward for epoch-boundary minting.
    // The exact per-token rate is computed at the epoch boundary once
    // total_tokens for the epoch is known (two-phase settlement).
    let epoch = height / crate::bootstrap::EPOCH_LENGTH_BLOCKS;
    // EscrowSettle doesn't carry verification_tier; default to non-TEE.
    // TEE multiplier is set correctly in bootstrap emission (receipt-based)
    // and can be upgraded here once receipts are cross-referenced.
    let pending = crate::pending_reward::PendingReward {
        job_id: *job_id,
        output_tokens: 0,
        user_payment: entry.amount,
        epoch,
        compute_addr: *compute_addr,
        verifier_addr: *verifier_addr,
        router_addr: *router_addr,
        treasury_addr: *treasury_addr,
        tee_multiplier_bps: crate::pending_reward::TEE_MULTIPLIER_NONE,
        https_multiplier_bps: crate::gateway_entry::HTTP_MULTIPLIER_BPS,
    };
    let pr_bytes = borsh::to_vec(&pending)
        .map_err(|e| ChainError::Codec(format!("pending_reward encode: {e}")))?;
    ctx.set_pending_reward(job_id, &pr_bytes)?;

    entry.state = crate::escrow_entry::EscrowState::Settled;
    let bytes =
        borsh::to_vec(&entry).map_err(|e| ChainError::Codec(format!("escrow encode: {e}")))?;
    ctx.set_escrow(job_id, &bytes)?;

    Ok(TxOutcome::Applied {
        gas_used: ESCROW_SETTLE_GAS,
    })
}

/// Mint block rewards for a settled job. The proposer includes this
/// tx in the block body; the amount is drawn from the epoch emission
/// budget (enforced by the caller in commit_block, not here — the
/// apply layer just credits the accounts).
fn apply_reward_mint(
    ctx: &mut BlockCtx<'_>,
    _job_id: &JobId,
    total_reward: Amount,
    compute_addr: &Address,
    verifier_addr: &Address,
    router_addr: &Address,
    treasury_addr: &Address,
) -> Result<TxOutcome> {
    if total_reward == 0 {
        return Ok(TxOutcome::Applied {
            gas_used: REWARD_MINT_GAS,
        });
    }
    credit_reward_split(
        ctx,
        total_reward,
        compute_addr,
        verifier_addr,
        router_addr,
        treasury_addr,
    )?;

    Ok(TxOutcome::Applied {
        gas_used: REWARD_MINT_GAS * 6,
    })
}

/// Credit the 75/7/5/5/3/5 reward split to the given addresses.
/// `burned` (3%) is dropped — not credited anywhere.
/// `delegators` (5%) is TODO: pro-rata split across delegators of
/// the compute node. For now it goes to the compute address as a
/// simplification; Phase 2 wires the delegation registry.
/// Gas for model registration.
pub const REGISTER_MODEL_GAS: Gas = 200_000;
/// Minimum deposit for model registration (10,000 ARK).
pub const MODEL_DEPOSIT: Amount = 10_000 * 1_000_000_000;

/// Register a model on-chain. Debits the deposit from the registrar,
/// persists the manifest in CF_MODELS, and rejects duplicates.
fn apply_register_model(
    ctx: &mut BlockCtx<'_>,
    manifest: &crate::transactions::OnChainModelManifest,
    registrar: &Address,
    deposit: Amount,
) -> Result<TxOutcome> {
    if deposit < MODEL_DEPOSIT {
        return Ok(TxOutcome::Rejected(RejectReason::FeeTooLow {
            min: MODEL_DEPOSIT as Gas,
            got: deposit as Gas,
        }));
    }
    if manifest.model_id.is_empty() {
        return Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
            "empty model_id",
        )));
    }
    if ctx.get_model(&manifest.model_id)?.is_some() {
        return Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
            "model already registered",
        )));
    }
    let mut acct = ctx.get_account(registrar)?.unwrap_or_default();
    if acct.balance < deposit {
        return Ok(TxOutcome::Rejected(RejectReason::InsufficientBalance {
            have: acct.balance,
            need: deposit,
        }));
    }
    acct.balance -= deposit;
    ctx.set_account(registrar, &acct)?;

    let bytes =
        borsh::to_vec(manifest).map_err(|e| ChainError::Codec(format!("model encode: {e}")))?;
    ctx.set_model(&manifest.model_id, &bytes)?;

    Ok(TxOutcome::Applied {
        gas_used: REGISTER_MODEL_GAS,
    })
}

/// Gas for TEE capability registration.
pub const REGISTER_TEE_GAS: Gas = 200_000;

/// Minimum TEE quote size (bytes). Intel TDX quotes are ~4-5 KB,
/// AMD SEV-SNP reports ~1-4 KB. Anything smaller is structurally invalid.
pub const MIN_TEE_QUOTE_BYTES: usize = 128;

/// Structural validation of a TEE attestation quote.
///
/// Checks platform-specific minimum size and header bytes. This is NOT
/// cryptographic verification — it only catches obvious garbage. Full
/// root-of-trust verification (Intel PCS / AMD VCEK) runs when the
/// verification library is deployed.
fn validate_tee_quote_structure(
    platform: arknet_common::types::TeePlatform,
    quote: &[u8],
) -> std::result::Result<(), &'static str> {
    if quote.len() < MIN_TEE_QUOTE_BYTES {
        return Err("TEE quote too short");
    }
    if quote.len() > arknet_common::types::MAX_TEE_QUOTE_BYTES {
        return Err("TEE quote exceeds size limit");
    }
    match platform {
        arknet_common::types::TeePlatform::IntelTdx => {
            // Intel TDX quotes start with version 4 (little-endian u16).
            if quote.len() < 4 || quote[0] != 4 || quote[1] != 0 {
                return Err("Intel TDX quote: invalid version header");
            }
        }
        arknet_common::types::TeePlatform::AmdSevSnp => {
            // AMD SEV-SNP attestation reports start with version 2.
            if quote.len() < 4 || (quote[0] != 2 && quote[0] != 3) {
                return Err("AMD SEV-SNP report: invalid version byte");
            }
        }
        arknet_common::types::TeePlatform::ArmCca => {
            // ARM CCA reserved — accept any well-sized quote.
        }
    }
    Ok(())
}

/// Register (or update) a compute node's TEE capability on-chain.
///
/// Validates:
/// - Quote passes structural validation (min size, platform header).
/// - Quote within [`MAX_TEE_QUOTE_BYTES`].
/// - Enclave pubkey scheme is active (Ed25519 at genesis).
///
/// Full cryptographic verification of the attestation quote against
/// Intel/AMD root CAs is activated once the verification library is
/// deployed.
fn apply_register_tee(
    ctx: &mut BlockCtx<'_>,
    node_id: &arknet_common::types::NodeId,
    _operator: &Address,
    capability: &arknet_common::types::TeeCapability,
) -> Result<TxOutcome> {
    if capability.quote.is_empty() {
        return Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
            "empty TEE attestation quote",
        )));
    }
    if let Err(reason) = validate_tee_quote_structure(capability.platform, &capability.quote) {
        return Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(reason)));
    }
    if !capability.enclave_pubkey.scheme.is_active() {
        return Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
            "enclave pubkey uses inactive scheme",
        )));
    }

    let bytes =
        borsh::to_vec(capability).map_err(|e| ChainError::Codec(format!("tee encode: {e}")))?;
    ctx.set_tee_capability(node_id, &bytes)?;

    Ok(TxOutcome::Applied {
        gas_used: REGISTER_TEE_GAS,
    })
}

/// Gas for gateway registration.
pub const REGISTER_GATEWAY_GAS: Gas = 100_000;
/// Gas for gateway unregistration.
pub const UNREGISTER_GATEWAY_GAS: Gas = 50_000;

/// Register a node as a public gateway.
fn apply_register_gateway(
    ctx: &mut BlockCtx<'_>,
    node_id: &arknet_common::types::NodeId,
    operator: &Address,
    url: &str,
    https: bool,
    height: Height,
) -> Result<TxOutcome> {
    if url.is_empty() {
        return Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
            "empty gateway URL",
        )));
    }
    if url.len() > crate::gateway_entry::MAX_GATEWAY_URL_LEN {
        return Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
            "gateway URL too long",
        )));
    }
    if https && !url.starts_with("https://") {
        return Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
            "https flag set but URL does not start with https://",
        )));
    }

    let entry = crate::gateway_entry::GatewayEntry {
        node_id: *node_id,
        operator: *operator,
        url: url.to_string(),
        https,
        registered_at: height,
    };
    let bytes =
        borsh::to_vec(&entry).map_err(|e| ChainError::Codec(format!("gateway encode: {e}")))?;
    ctx.set_gateway(node_id, &bytes)?;

    Ok(TxOutcome::Applied {
        gas_used: REGISTER_GATEWAY_GAS,
    })
}

/// Remove a node from the gateway registry.
fn apply_unregister_gateway(
    ctx: &mut BlockCtx<'_>,
    node_id: &arknet_common::types::NodeId,
) -> Result<TxOutcome> {
    ctx.delete_gateway(node_id)?;
    Ok(TxOutcome::Applied {
        gas_used: UNREGISTER_GATEWAY_GAS,
    })
}

/// Gas for governance proposal submission.
pub const GOV_PROPOSAL_GAS: Gas = 500_000;
/// Gas for governance vote.
pub const GOV_VOTE_GAS: Gas = 30_000;
/// Minimum deposit for a governance proposal (10,000 ARK).
pub const PROPOSAL_DEPOSIT: Amount = 10_000 * 1_000_000_000;

/// Submit a governance proposal. Debits the deposit from the
/// proposer's balance and creates the proposal record.
fn apply_gov_proposal(
    ctx: &mut BlockCtx<'_>,
    signer: &arknet_common::types::PubKey,
    proposal: &crate::transactions::Proposal,
    height: Height,
) -> Result<TxOutcome> {
    let sender = derive_address_from_signer(signer);
    if proposal.deposit < PROPOSAL_DEPOSIT {
        return Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
            "deposit below minimum",
        )));
    }
    let mut acct = ctx.get_account(&sender)?.unwrap_or_default();
    if acct.balance < proposal.deposit {
        return Ok(TxOutcome::Rejected(RejectReason::InsufficientBalance {
            have: acct.balance,
            need: proposal.deposit,
        }));
    }
    acct.balance -= proposal.deposit;
    ctx.set_account(&sender, &acct)?;

    let id = ctx.state().next_proposal_id()?;
    let record = crate::governance_entry::ProposalRecord {
        proposal: proposal.clone(),
        phase: crate::governance_entry::ProposalPhase::Discussion,
        submitted_at: height,
    };
    let bytes =
        borsh::to_vec(&record).map_err(|e| ChainError::Codec(format!("proposal encode: {e}")))?;
    ctx.set_proposal(id, &bytes)?;
    ctx.set_next_proposal_id(id + 1)?;

    Ok(TxOutcome::Applied {
        gas_used: GOV_PROPOSAL_GAS,
    })
}

/// Cast a governance vote. Records the vote keyed by
/// `(proposal_id, voter)`. Duplicate votes are rejected.
///
/// Only accepts votes when the proposal is in the `Voting` phase.
/// Votes during `Discussion`, `Passed`, `Rejected`, `RejectedWithVeto`,
/// or `Executed` are rejected.
fn apply_gov_vote(
    ctx: &mut BlockCtx<'_>,
    proposal_id: u64,
    voter: &Address,
    choice: crate::transactions::VoteChoice,
) -> Result<TxOutcome> {
    // Check proposal exists and decode to verify phase.
    let raw = match ctx.get_proposal(proposal_id)? {
        Some(bytes) => bytes,
        None => {
            return Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
                "proposal not found",
            )));
        }
    };
    let record: crate::governance_entry::ProposalRecord =
        borsh::from_slice(&raw).map_err(|e| ChainError::Codec(format!("proposal decode: {e}")))?;
    if record.phase != crate::governance_entry::ProposalPhase::Voting {
        return Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
            "proposal not in voting phase",
        )));
    }
    // Check for duplicate vote.
    let key = gov_vote_key(proposal_id, voter);
    if ctx.get_vote(&key)?.is_some() {
        return Ok(TxOutcome::Rejected(RejectReason::NotYetImplemented(
            "duplicate vote",
        )));
    }
    let choice_byte = choice as u8;
    ctx.set_vote(&key, &[choice_byte])?;

    Ok(TxOutcome::Applied {
        gas_used: GOV_VOTE_GAS,
    })
}

/// Vote key: `proposal_id(8 BE) || voter(20)`.
fn gov_vote_key(proposal_id: u64, voter: &Address) -> Vec<u8> {
    let mut k = Vec::with_capacity(28);
    k.extend_from_slice(&proposal_id.to_be_bytes());
    k.extend_from_slice(voter.as_bytes());
    k
}

fn credit_reward_split(
    ctx: &mut BlockCtx<'_>,
    total: Amount,
    compute_addr: &Address,
    verifier_addr: &Address,
    router_addr: &Address,
    treasury_addr: &Address,
) -> Result<()> {
    let compute = total * 75 / 100;
    let verifier = total * 7 / 100;
    let router = total * 5 / 100;
    let burned = total * 3 / 100;
    let delegators = total * 5 / 100;
    let treasury = total - compute - verifier - router - burned - delegators;

    // Compute + delegators (simplified: all to compute for now).
    let compute_total = compute + delegators;

    for (addr, amount) in [
        (compute_addr, compute_total),
        (verifier_addr, verifier),
        (router_addr, router),
        (treasury_addr, treasury),
    ] {
        if amount > 0 {
            let mut acct = ctx.get_account(addr)?.unwrap_or_default();
            acct.balance = acct.balance.saturating_add(amount);
            ctx.set_account(addr, &acct)?;
        }
    }
    // `burned` is intentionally not credited anywhere.
    Ok(())
}

fn derive_address_from_signer(signer: &arknet_common::types::PubKey) -> Address {
    let digest = arknet_crypto::hash::blake3(&signer.bytes);
    let mut out = [0u8; 20];
    out.copy_from_slice(&digest.as_bytes()[..20]);
    Address::new(out)
}

/// Derive a 20-byte address from a NodeId. Used for bootstrap
/// emission where we only have the node_id from the receipt.
fn node_id_to_address(node_id: &arknet_common::types::NodeId) -> Address {
    let digest = arknet_crypto::hash::blake3(node_id.as_bytes());
    let mut out = [0u8; 20];
    out.copy_from_slice(&digest.as_bytes()[..20]);
    Address::new(out)
}

/// Deterministic treasury address for bootstrap rewards.
fn bootstrap_treasury_address() -> Address {
    let digest = arknet_crypto::hash::blake3(b"arknet-treasury-v1");
    let mut out = [0u8; 20];
    out.copy_from_slice(&digest.as_bytes()[..20]);
    Address::new(out)
}

fn apply_transfer(
    ctx: &mut BlockCtx<'_>,
    from: &Address,
    to: &Address,
    amount: Amount,
    nonce: Nonce,
    fee: Gas,
) -> Result<TxOutcome> {
    if from == to {
        return Ok(TxOutcome::Rejected(RejectReason::SelfTransfer));
    }
    if fee < BASE_TRANSFER_GAS {
        return Ok(TxOutcome::Rejected(RejectReason::FeeTooLow {
            min: BASE_TRANSFER_GAS,
            got: fee,
        }));
    }

    let mut from_acct = ctx.get_account(from)?.unwrap_or_default();
    if from_acct.nonce != nonce {
        return Ok(TxOutcome::Rejected(RejectReason::NonceMismatch {
            expected: from_acct.nonce,
            got: nonce,
        }));
    }

    // EIP-1559: fee cost = gas_budget × base_fee_per_gas. The base fee
    // is stored in CF_META and updated each block by the commit path.
    let base_fee = ctx.state().base_fee().unwrap_or(1);
    let fee_cost = (fee as Amount).saturating_mul(base_fee);
    let total: Amount = match amount.checked_add(fee_cost) {
        Some(v) => v,
        None => {
            return Ok(TxOutcome::Rejected(RejectReason::InsufficientBalance {
                have: from_acct.balance,
                need: Amount::MAX,
            }))
        }
    };
    if from_acct.balance < total {
        return Ok(TxOutcome::Rejected(RejectReason::InsufficientBalance {
            have: from_acct.balance,
            need: total,
        }));
    }

    from_acct.balance -= total;
    from_acct.nonce += 1;

    let mut to_acct = ctx.get_account(to)?.unwrap_or_default();
    to_acct.balance = to_acct.balance.saturating_add(amount);

    ctx.set_account(from, &from_acct)?;
    ctx.set_account(to, &to_acct)?;

    Ok(TxOutcome::Applied { gas_used: fee })
}

// Dead-code guard: the unused `ChainError` import would trip clippy if no
// path currently surfaces one. Keep the import reachable via a trivial
// `From` to prepare for Week 9's stake ops.
#[allow(dead_code)]
fn _chain_error_is_reachable(_: ChainError) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::Account;
    use crate::state::State;
    use arknet_common::types::{PubKey, Signature, SignatureScheme};

    fn tmp_state() -> (tempfile::TempDir, State) {
        let tmp = tempfile::tempdir().unwrap();
        let state = State::open(tmp.path()).unwrap();
        (tmp, state)
    }

    fn sign(tx: Transaction) -> SignedTransaction {
        SignedTransaction {
            tx,
            signer: PubKey::ed25519([1; 32]),
            signature: Signature::new(SignatureScheme::Ed25519, vec![2; 64]).unwrap(),
        }
    }

    fn fund(ctx: &mut BlockCtx<'_>, addr: &Address, balance: Amount) {
        ctx.set_account(addr, &Account { balance, nonce: 0 })
            .unwrap();
    }

    #[test]
    fn transfer_happy_path() {
        let (_tmp, state) = tmp_state();
        let alice = Address::new([1; 20]);
        let bob = Address::new([2; 20]);

        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &alice, 1_000_000);
            ctx.commit().unwrap();
        }

        let mut ctx = state.begin_block();
        let stx = sign(Transaction::Transfer {
            from: alice,
            to: bob,
            amount: 500,
            nonce: 0,
            fee: BASE_TRANSFER_GAS,
        });
        let outcome = apply_tx(&mut ctx, &stx).unwrap();
        assert_eq!(
            outcome,
            TxOutcome::Applied {
                gas_used: BASE_TRANSFER_GAS
            }
        );
        ctx.commit().unwrap();

        let a = state.get_account(&alice).unwrap().unwrap();
        let b = state.get_account(&bob).unwrap().unwrap();
        assert_eq!(a.balance, 1_000_000 - 500 - BASE_TRANSFER_GAS as Amount);
        assert_eq!(a.nonce, 1);
        assert_eq!(b.balance, 500);
    }

    #[test]
    fn transfer_rejects_wrong_nonce() {
        let (_tmp, state) = tmp_state();
        let alice = Address::new([1; 20]);
        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &alice, 1_000_000);
            ctx.commit().unwrap();
        }

        let mut ctx = state.begin_block();
        let stx = sign(Transaction::Transfer {
            from: alice,
            to: Address::new([2; 20]),
            amount: 1,
            nonce: 42, // sender has nonce 0
            fee: BASE_TRANSFER_GAS,
        });
        match apply_tx(&mut ctx, &stx).unwrap() {
            TxOutcome::Rejected(RejectReason::NonceMismatch {
                expected: 0,
                got: 42,
            }) => {}
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn transfer_rejects_over_balance() {
        let (_tmp, state) = tmp_state();
        let alice = Address::new([1; 20]);
        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &alice, 100);
            ctx.commit().unwrap();
        }

        let mut ctx = state.begin_block();
        let stx = sign(Transaction::Transfer {
            from: alice,
            to: Address::new([2; 20]),
            amount: 1_000_000,
            nonce: 0,
            fee: BASE_TRANSFER_GAS,
        });
        match apply_tx(&mut ctx, &stx).unwrap() {
            TxOutcome::Rejected(RejectReason::InsufficientBalance { .. }) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn transfer_rejects_below_base_fee() {
        let (_tmp, state) = tmp_state();
        let alice = Address::new([1; 20]);
        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &alice, 1_000_000);
            ctx.commit().unwrap();
        }

        let mut ctx = state.begin_block();
        let stx = sign(Transaction::Transfer {
            from: alice,
            to: Address::new([2; 20]),
            amount: 1,
            nonce: 0,
            fee: 100, // below BASE_TRANSFER_GAS
        });
        match apply_tx(&mut ctx, &stx).unwrap() {
            TxOutcome::Rejected(RejectReason::FeeTooLow { min, got: 100 }) => {
                assert_eq!(min, BASE_TRANSFER_GAS);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn transfer_rejects_self_transfer() {
        let (_tmp, state) = tmp_state();
        let alice = Address::new([1; 20]);
        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &alice, 1_000_000);
            ctx.commit().unwrap();
        }

        let mut ctx = state.begin_block();
        let stx = sign(Transaction::Transfer {
            from: alice,
            to: alice,
            amount: 1,
            nonce: 0,
            fee: BASE_TRANSFER_GAS,
        });
        assert_eq!(
            apply_tx(&mut ctx, &stx).unwrap(),
            TxOutcome::Rejected(RejectReason::SelfTransfer)
        );
    }

    #[test]
    fn rejected_tx_does_not_mutate_state() {
        let (_tmp, state) = tmp_state();
        let alice = Address::new([1; 20]);
        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &alice, 1_000_000);
            ctx.commit().unwrap();
        }
        let root_before = state.state_root();

        let mut ctx = state.begin_block();
        let stx = sign(Transaction::Transfer {
            from: alice,
            to: Address::new([2; 20]),
            amount: 1,
            nonce: 999, // bogus
            fee: BASE_TRANSFER_GAS,
        });
        let _ = apply_tx(&mut ctx, &stx).unwrap();
        ctx.commit().unwrap();

        assert_eq!(state.state_root(), root_before);
    }

    #[test]
    fn stake_deposit_happy_path_applies() {
        use crate::transactions::{StakeOp, StakeRole};

        let (_tmp, state) = tmp_state();
        // Derive the sender address from the public key bytes used by `sign`
        // so the deposit debits the correct account.
        let signer_pubkey: [u8; 32] = [1; 32];
        let sender = {
            let d = arknet_crypto::hash::blake3(&signer_pubkey);
            let mut a = [0u8; 20];
            a.copy_from_slice(&d.as_bytes()[..20]);
            Address::new(a)
        };
        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &sender, 10_000_000);
            ctx.commit().unwrap();
        }

        let mut ctx = state.begin_block();
        let stx = sign(Transaction::StakeOp(StakeOp::Deposit {
            node_id: arknet_common::types::NodeId::new([9; 32]),
            role: StakeRole::Validator,
            pool_id: None,
            amount: 2_500,
            delegator: None,
        }));
        match apply_tx(&mut ctx, &stx).unwrap() {
            TxOutcome::Applied { gas_used } => assert!(gas_used > 0),
            other => panic!("unexpected: {other:?}"),
        }
        ctx.commit().unwrap();

        let e = state
            .get_stake(
                &arknet_common::types::NodeId::new([9; 32]),
                crate::transactions::StakeRole::Validator,
                None,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(e.amount, 2_500);
    }

    fn sample_manifest() -> crate::transactions::OnChainModelManifest {
        crate::transactions::OnChainModelManifest {
            model_id: "meta-llama/Llama-3-8B".to_string(),
            sha256: [0xab; 32],
            size_bytes: 4_000_000_000,
            mirrors: vec!["https://example.com/llama3.gguf".to_string()],
            license: "Llama-3".to_string(),
        }
    }

    #[test]
    fn register_model_happy_path() {
        let (_tmp, state) = tmp_state();
        let registrar = Address::new([5; 20]);
        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &registrar, MODEL_DEPOSIT + 1_000_000);
            ctx.commit().unwrap();
        }

        let stx = sign(Transaction::RegisterModel {
            manifest: sample_manifest(),
            registrar,
            deposit: MODEL_DEPOSIT,
        });
        let mut ctx = state.begin_block();
        let outcome = apply_tx(&mut ctx, &stx).unwrap();
        assert_eq!(
            outcome,
            TxOutcome::Applied {
                gas_used: REGISTER_MODEL_GAS
            }
        );
        ctx.commit().unwrap();

        let acct = state.get_account(&registrar).unwrap().unwrap();
        assert_eq!(acct.balance, 1_000_000);

        let model = state.get_model("meta-llama/Llama-3-8B").unwrap();
        assert!(model.is_some());
        assert_eq!(model.unwrap().size_bytes, 4_000_000_000);
    }

    #[test]
    fn register_model_rejects_duplicate() {
        let (_tmp, state) = tmp_state();
        let registrar = Address::new([5; 20]);
        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &registrar, MODEL_DEPOSIT * 3);
            ctx.commit().unwrap();
        }

        let stx = sign(Transaction::RegisterModel {
            manifest: sample_manifest(),
            registrar,
            deposit: MODEL_DEPOSIT,
        });
        let mut ctx = state.begin_block();
        let _ = apply_tx(&mut ctx, &stx).unwrap();
        ctx.commit().unwrap();

        let stx2 = sign(Transaction::RegisterModel {
            manifest: sample_manifest(),
            registrar,
            deposit: MODEL_DEPOSIT,
        });
        let mut ctx = state.begin_block();
        match apply_tx(&mut ctx, &stx2).unwrap() {
            TxOutcome::Rejected(RejectReason::NotYetImplemented("model already registered")) => {}
            other => panic!("expected duplicate rejection, got {other:?}"),
        }
    }

    #[test]
    fn register_model_rejects_insufficient_balance() {
        let (_tmp, state) = tmp_state();
        let registrar = Address::new([5; 20]);
        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &registrar, 100);
            ctx.commit().unwrap();
        }

        let stx = sign(Transaction::RegisterModel {
            manifest: sample_manifest(),
            registrar,
            deposit: MODEL_DEPOSIT,
        });
        let mut ctx = state.begin_block();
        match apply_tx(&mut ctx, &stx).unwrap() {
            TxOutcome::Rejected(RejectReason::InsufficientBalance { .. }) => {}
            other => panic!("expected insufficient balance, got {other:?}"),
        }
    }

    fn valid_tdx_quote() -> Vec<u8> {
        let mut q = vec![0u8; 256];
        q[0] = 4; // TDX version = 4 (little-endian u16)
        q[1] = 0;
        q
    }

    fn valid_snp_quote() -> Vec<u8> {
        let mut q = vec![0u8; 256];
        q[0] = 2; // SEV-SNP version = 2
        q
    }

    #[test]
    fn register_tee_happy_path() {
        use arknet_common::types::{TeeCapability, TeePlatform};
        let (_tmp, state) = tmp_state();
        let node_id = arknet_common::types::NodeId::new([0xaa; 32]);
        let operator = Address::new([0xbb; 20]);
        let stx = sign(Transaction::RegisterTeeCapability {
            node_id,
            operator,
            capability: TeeCapability {
                platform: TeePlatform::IntelTdx,
                quote: valid_tdx_quote(),
                enclave_pubkey: PubKey::ed25519([0xcc; 32]),
            },
        });
        let mut ctx = state.begin_block();
        let outcome = apply_tx(&mut ctx, &stx).unwrap();
        assert_eq!(
            outcome,
            TxOutcome::Applied {
                gas_used: REGISTER_TEE_GAS
            }
        );
        ctx.commit().unwrap();

        let cap = state.get_tee_capability(&node_id).unwrap();
        assert!(cap.is_some());
        assert_eq!(cap.unwrap().platform, TeePlatform::IntelTdx);
    }

    #[test]
    fn register_tee_rejects_empty_quote() {
        use arknet_common::types::{TeeCapability, TeePlatform};
        let (_tmp, state) = tmp_state();
        let stx = sign(Transaction::RegisterTeeCapability {
            node_id: arknet_common::types::NodeId::new([0xaa; 32]),
            operator: Address::new([0xbb; 20]),
            capability: TeeCapability {
                platform: TeePlatform::AmdSevSnp,
                quote: vec![],
                enclave_pubkey: PubKey::ed25519([0xcc; 32]),
            },
        });
        let mut ctx = state.begin_block();
        match apply_tx(&mut ctx, &stx).unwrap() {
            TxOutcome::Rejected(RejectReason::NotYetImplemented("empty TEE attestation quote")) => {
            }
            other => panic!("expected empty quote rejection, got {other:?}"),
        }
    }

    #[test]
    fn register_tee_rejects_oversized_quote() {
        use arknet_common::types::{TeeCapability, TeePlatform, MAX_TEE_QUOTE_BYTES};
        let (_tmp, state) = tmp_state();
        let stx = sign(Transaction::RegisterTeeCapability {
            node_id: arknet_common::types::NodeId::new([0xaa; 32]),
            operator: Address::new([0xbb; 20]),
            capability: TeeCapability {
                platform: TeePlatform::IntelTdx,
                quote: vec![0xff; MAX_TEE_QUOTE_BYTES + 1],
                enclave_pubkey: PubKey::ed25519([0xcc; 32]),
            },
        });
        let mut ctx = state.begin_block();
        match apply_tx(&mut ctx, &stx).unwrap() {
            TxOutcome::Rejected(RejectReason::NotYetImplemented(
                "TEE quote exceeds size limit",
            )) => {}
            other => panic!("expected oversized quote rejection, got {other:?}"),
        }
    }

    #[test]
    fn register_tee_update_overwrites() {
        use arknet_common::types::{TeeCapability, TeePlatform};
        let (_tmp, state) = tmp_state();
        let node_id = arknet_common::types::NodeId::new([0xaa; 32]);
        let operator = Address::new([0xbb; 20]);

        // First registration: Intel TDX.
        let stx1 = sign(Transaction::RegisterTeeCapability {
            node_id,
            operator,
            capability: TeeCapability {
                platform: TeePlatform::IntelTdx,
                quote: valid_tdx_quote(),
                enclave_pubkey: PubKey::ed25519([0xcc; 32]),
            },
        });
        let mut ctx = state.begin_block();
        let _ = apply_tx(&mut ctx, &stx1).unwrap();
        ctx.commit().unwrap();

        // Second registration: AMD SEV-SNP (update).
        let stx2 = sign(Transaction::RegisterTeeCapability {
            node_id,
            operator,
            capability: TeeCapability {
                platform: TeePlatform::AmdSevSnp,
                quote: valid_snp_quote(),
                enclave_pubkey: PubKey::ed25519([0xdd; 32]),
            },
        });
        let mut ctx = state.begin_block();
        let _ = apply_tx(&mut ctx, &stx2).unwrap();
        ctx.commit().unwrap();

        let cap = state.get_tee_capability(&node_id).unwrap().unwrap();
        assert_eq!(cap.platform, TeePlatform::AmdSevSnp);
    }

    #[test]
    fn register_tee_rejects_bad_tdx_header() {
        use arknet_common::types::{TeeCapability, TeePlatform};
        let (_tmp, state) = tmp_state();
        // Quote is long enough but has wrong version header for TDX.
        let mut bad_quote = vec![0u8; 256];
        bad_quote[0] = 99; // not version 4
        let stx = sign(Transaction::RegisterTeeCapability {
            node_id: arknet_common::types::NodeId::new([0xaa; 32]),
            operator: Address::new([0xbb; 20]),
            capability: TeeCapability {
                platform: TeePlatform::IntelTdx,
                quote: bad_quote,
                enclave_pubkey: PubKey::ed25519([0xcc; 32]),
            },
        });
        let mut ctx = state.begin_block();
        match apply_tx(&mut ctx, &stx).unwrap() {
            TxOutcome::Rejected(RejectReason::NotYetImplemented(
                "Intel TDX quote: invalid version header",
            )) => {}
            other => panic!("expected TDX header rejection, got {other:?}"),
        }
    }

    #[test]
    fn register_tee_rejects_too_short_quote() {
        use arknet_common::types::{TeeCapability, TeePlatform};
        let (_tmp, state) = tmp_state();
        let stx = sign(Transaction::RegisterTeeCapability {
            node_id: arknet_common::types::NodeId::new([0xaa; 32]),
            operator: Address::new([0xbb; 20]),
            capability: TeeCapability {
                platform: TeePlatform::IntelTdx,
                quote: vec![4, 0, 0, 0], // valid header but too short
                enclave_pubkey: PubKey::ed25519([0xcc; 32]),
            },
        });
        let mut ctx = state.begin_block();
        match apply_tx(&mut ctx, &stx).unwrap() {
            TxOutcome::Rejected(RejectReason::NotYetImplemented("TEE quote too short")) => {}
            other => panic!("expected too-short rejection, got {other:?}"),
        }
    }

    #[test]
    fn register_model_rejects_low_deposit() {
        let (_tmp, state) = tmp_state();
        let registrar = Address::new([5; 20]);
        {
            let mut ctx = state.begin_block();
            fund(&mut ctx, &registrar, MODEL_DEPOSIT * 2);
            ctx.commit().unwrap();
        }

        let stx = sign(Transaction::RegisterModel {
            manifest: sample_manifest(),
            registrar,
            deposit: 100,
        });
        let mut ctx = state.begin_block();
        match apply_tx(&mut ctx, &stx).unwrap() {
            TxOutcome::Rejected(RejectReason::FeeTooLow { .. }) => {}
            other => panic!("expected low deposit rejection, got {other:?}"),
        }
    }
}
