//! Tendermint BFT consensus for the arknet L1.
//!
//! Phase 1 Week 7-8 binds the `malachitebft-core-*` crates to our
//! on-chain types. The layout mirrors the trait obligations malachite
//! imposes on a host application:
//!
//! - [`context`] — [`context::ArknetContext`], the type parameter every
//!   malachite-generic call takes.
//! - [`height`] — [`height::Height`] newtype implementing
//!   `malachitebft_core_types::Height`.
//! - [`value`] — [`value::ChainValue`] wraps [`arknet_chain::Block`] so
//!   malachite can treat a block as the proposed value.
//! - [`vote`] — [`vote::ChainVote`]: prevote / precommit.
//! - [`proposal`] — [`proposal::ChainProposal`] (+ `ChainProposalPart`
//!   stub for ProposalOnly mode).
//! - [`validators`] — [`validators::ChainValidator`] /
//!   [`validators::ChainValidatorSet`].
//! - [`signing`] — [`signing::ArknetSigningProvider`] implements
//!   malachite's `SigningProvider` on top of our Ed25519 keypair.
//! - [`errors`] — [`errors::ConsensusError`] (shared Result type).
//!
//! Later weeks add `mempool`, `block_builder`, `commit`,
//! `network_bridge`, and `engine` — those tie the rest of Phase 1
//! (state store, libp2p) into the consensus loop.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod context;
pub mod errors;
pub mod height;
pub mod proposal;
pub mod signing;
pub mod validators;
pub mod value;
pub mod vote;

pub use context::ArknetContext;
pub use errors::{ConsensusError, Result};
pub use height::Height;
pub use proposal::{ChainProposal, ChainProposalPart};
pub use signing::ArknetSigningProvider;
pub use validators::{ChainValidator, ChainValidatorSet};
pub use value::{BlockId, ChainValue};
pub use vote::ChainVote;
