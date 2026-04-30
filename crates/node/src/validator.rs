//! Validator role body.
//!
//! Loads the genesis file, opens the L1 chain state, builds the
//! consensus validator set, derives the malachite signing key from the
//! same 32-byte seed that drives libp2p PeerId derivation, and starts
//! [`arknet_consensus::engine::ConsensusEngine`]. Waits for
//! `shutdown` before returning.
//!
//! # Key unification
//!
//! The validator's ed25519 seed is the single cryptographic identity
//! of the node. The same 32 bytes produce:
//!
//! - The libp2p `PeerId` (via `Keypair::public().to_peer_id()`).
//! - The consensus public key on the chain (registered under
//!   `ValidatorInfo.consensus_key`).
//! - Every prevote / precommit / proposal signature the node sends.
//!
//! libp2p exposes the raw 32-byte seed via
//! `ed25519::SecretKey::as_ref()`; malachite's `PrivateKey`
//! consumes it through `From<[u8; 32]>`. Both underlying crates
//! (ed25519-dalek and ed25519-consensus) follow RFC 8032, so the
//! derived public keys match.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use arknet_chain::genesis::{genesis_to_validator_info, load_genesis, GenesisConfig};
use arknet_chain::State as ChainState;
use arknet_common::types::NodeId;
use arknet_consensus::engine::{ConsensusEngine, ConsensusHandle, EngineConfig, TimeoutConfig};
use arknet_consensus::signing::{ArknetSigningProvider, PrivateKey};
use arknet_consensus::validators::{ChainAddress, ChainValidatorSet};
use arknet_consensus::Height;
use arknet_network::{Keypair, NetworkHandle};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::errors::{NodeError, Result};
use crate::paths;

/// Boot the validator engine.
///
/// Returns the consensus handle (for the RPC layer) + the engine's
/// background join handle. Caller is responsible for cancelling the
/// shared `shutdown` token and awaiting the returned join handle.
pub async fn start_validator(
    data_dir: &Path,
    keypair: &Keypair,
    network: NetworkHandle,
    shutdown: CancellationToken,
) -> Result<(ConsensusHandle, JoinHandle<arknet_consensus::Result<()>>)> {
    let genesis_path = paths::genesis_toml(data_dir);
    if !genesis_path.exists() {
        return Err(NodeError::Config(format!(
            "genesis.toml not found at {} — validator role requires it",
            genesis_path.display()
        )));
    }

    let genesis: GenesisConfig =
        load_genesis(&genesis_path).map_err(|e| NodeError::Config(format!("load genesis: {e}")))?;
    info!(
        chain_id = %genesis.chain_id,
        validators = genesis.validators.len(),
        "genesis loaded"
    );

    let mut infos = Vec::with_capacity(genesis.validators.len());
    for gv in &genesis.validators {
        let info = genesis_to_validator_info(gv)
            .map_err(|e| NodeError::Config(format!("genesis_to_validator_info: {e}")))?;
        infos.push(info);
    }
    let validator_set = ChainValidatorSet::from_infos(&infos)
        .map_err(|e| NodeError::Config(format!("build validator set: {e}")))?;

    let state = ChainState::open(&paths::l1_dir(data_dir))
        .map_err(|e| NodeError::Config(format!("open chain state: {e}")))?;
    let state = Arc::new(state);

    let (consensus_pk, consensus_sk) = ed25519_from_libp2p(keypair)?;
    let local_address = derive_local_address(keypair);

    // Confirm our key is actually in the genesis set. Running a
    // validator whose key isn't registered would still gossip but
    // never be tallied — better to fail loud at boot.
    if !infos
        .iter()
        .any(|i| i.consensus_key.bytes.as_slice() == consensus_pk.as_slice())
    {
        return Err(NodeError::Config(format!(
            "this node's consensus key is not in genesis; add the matching validator entry to {}",
            genesis_path.display()
        )));
    }

    let signer = Arc::new(ArknetSigningProvider::new(consensus_sk));

    // NodeId is blake3(pubkey_bytes) per PROTOCOL_SPEC §3.
    let node_id_bytes: [u8; 32] = *arknet_crypto::hash::blake3(&consensus_pk).as_bytes();
    let local_node_id = NodeId::new(node_id_bytes);

    let engine_cfg = EngineConfig {
        chain_id: genesis.chain_id.clone(),
        version: 1,
        initial_height: Height(genesis.initial_height + 1), // genesis is height 0, first produced block is 1
        validator_set: validator_set.clone(),
        base_fee: genesis.params.base_fee_amount(),
        gas_limit: genesis.params.gas_limit,
        gas_target: genesis.params.gas_target,
        local_address: ChainAddress(local_address),
        local_node_id,
        timeouts: TimeoutConfig::default(),
        genesis_message: genesis.genesis_message.clone(),
    };

    info!(
        chain_id = %engine_cfg.chain_id,
        local_address = %engine_cfg.local_address,
        initial_height = %engine_cfg.initial_height.0,
        "validator engine starting"
    );

    let (handle, join) = ConsensusEngine::start(engine_cfg, state, network, signer, shutdown);
    Ok((handle, join))
}

/// RPC socket used by the validator role for `/v1/tx` + `/v1/status`.
#[allow(dead_code)]
pub fn rpc_listen_addr(cfg: &arknet_common::config::NodeConfig) -> Result<SocketAddr> {
    cfg.network
        .rpc_listen
        .parse()
        .map_err(|e| NodeError::Config(format!("rpc_listen: {e}")))
}

/// Project the libp2p ed25519 keypair onto a `(public bytes, malachite
/// PrivateKey)` pair. Returns an error if the inner keypair is not
/// ed25519 (libp2p's `Keypair` can wrap other schemes — we only
/// generate ed25519 in `start.rs`).
fn ed25519_from_libp2p(kp: &Keypair) -> Result<([u8; 32], PrivateKey)> {
    let ed = kp
        .clone()
        .try_into_ed25519()
        .map_err(|e| NodeError::Config(format!("consensus requires ed25519 keypair: {e}")))?;
    let pk_bytes = ed.public().to_bytes();
    // libp2p's `ed25519::Keypair::to_bytes()` lays out [secret || public].
    let keypair_bytes = ed.to_bytes();
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&keypair_bytes[..32]);
    Ok((pk_bytes, PrivateKey::from(seed)))
}

/// Derive the 20-byte arknet [`Address`] from the libp2p keypair's
/// public half. Matches `blake3(pubkey_bytes)[..20]`.
///
/// [`Address`]: arknet_common::types::Address
fn derive_local_address(kp: &Keypair) -> arknet_common::types::Address {
    let ed = kp
        .clone()
        .try_into_ed25519()
        .expect("validator role requires ed25519 keypair — check start.rs key generation");
    let pk = ed.public().to_bytes();
    let digest = *arknet_crypto::hash::blake3(&pk).as_bytes();
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&digest[..20]);
    arknet_common::types::Address::new(addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libp2p_seed_projects_to_matching_malachite_key() {
        let kp = Keypair::generate_ed25519();
        let (pk_bytes, sk) = ed25519_from_libp2p(&kp).unwrap();
        // malachite's public key derived from the same seed must match
        // the libp2p-derived bytes. RFC 8032 guarantees this across the
        // two ed25519 crates.
        assert_eq!(*sk.public_key().as_bytes(), pk_bytes);
    }

    #[test]
    fn derived_address_is_deterministic() {
        let kp = Keypair::generate_ed25519();
        let a = derive_local_address(&kp);
        let b = derive_local_address(&kp);
        assert_eq!(a, b);
    }
}
