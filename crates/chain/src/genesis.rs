//! Genesis configuration — the one-time state the chain starts from.
//!
//! Format is TOML. Canonical schema is in PROTOCOL_SPEC §9.5. The loader
//! enforces arknet's fair-launch invariant: **no premine**. Any non-empty
//! `initial_accounts` section is rejected with [`ChainError::Codec`] so a
//! misconfiguration cannot silently seed a whale.
//!
//! Genesis validators are hardcoded by name in the TOML and enter the
//! chain with zero bonded stake. They retain their active slots through
//! the bootstrap epoch (PROTOCOL_SPEC §9.4); after that, each must have
//! acquired the standard minimum stake through block rewards or be
//! evicted.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use arknet_common::types::{Address, Amount, Height, PubKey, SignatureScheme, Timestamp};

use crate::errors::{ChainError, Result};
use crate::validator::ValidatorInfo;

/// Top-level genesis document.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenesisConfig {
    /// Chain identifier — e.g. `"arknet-devnet-1"`.
    pub chain_id: String,
    /// Height of the genesis block. Must be 0 for a fresh chain.
    pub initial_height: Height,
    /// Timestamp of the genesis block, ms since Unix epoch.
    pub initial_timestamp_ms: Timestamp,
    /// Hardcoded initial validator set.
    pub validators: Vec<GenesisValidator>,
    /// **Fair-launch invariant**: must be empty at genesis. The loader
    /// returns an error if this field is populated. Accepted here only to
    /// keep the TOML parser flexible; future non-fair-launch forks would
    /// lift the check in their fork-specific loader.
    #[serde(default)]
    pub initial_accounts: Vec<InitialAccount>,
    /// Initial protocol parameters.
    pub params: GenesisParams,
    /// Coinbase message embedded in block 0. Timestamped proof that
    /// the chain was not pre-mined before this date.
    #[serde(default)]
    pub genesis_message: String,
}

/// Entry in the hardcoded initial validator set.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenesisValidator {
    /// Human-readable moniker (informational).
    pub name: String,
    /// Hex-encoded consensus public key bytes. Scheme defaults to Ed25519.
    pub pubkey_hex: String,
    /// Operator address (bech32 or 0x-hex).
    pub operator_hex: String,
    /// Voting power at genesis. Usually 1 for equal weighting.
    #[serde(default = "default_voting_power")]
    pub voting_power: u64,
}

fn default_voting_power() -> u64 {
    1
}

/// Accepted only because `serde` needs a concrete type for the (always
/// empty) `initial_accounts` array. The loader rejects non-empty vectors.
/// Uses `u64` because the `toml` crate does not support `u128`; any non-zero
/// value would fail the fair-launch invariant check anyway.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InitialAccount {
    /// Address (bech32 or 0x-hex).
    pub address_hex: String,
    /// Amount in `ark_atom`.
    pub balance: u64,
}

/// Protocol parameters seeded from the genesis file.
///
/// Note: `base_fee` is represented as `u64` in the TOML (the `toml` crate
/// does not serialize `u128`). This is fine at genesis since base fees
/// that exceed `u64::MAX` ark_atom per gas unit would represent an
/// economically impossible state. Values are widened to [`Amount`]
/// (`u128`) via [`Self::base_fee_amount`] when flowing into the state
/// layer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenesisParams {
    /// Initial EIP-1559 base fee (`ark_atom/gas`) — fits in `u64` at genesis.
    pub base_fee: u64,
    /// Initial block gas target.
    pub gas_target: u64,
    /// Initial block gas limit.
    pub gas_limit: u64,
}

impl GenesisParams {
    /// Base fee widened to the chain's canonical [`Amount`] type.
    pub fn base_fee_amount(&self) -> Amount {
        self.base_fee as Amount
    }
}

impl Default for GenesisParams {
    fn default() -> Self {
        Self {
            base_fee: 1_000_000_000, // 1 gwei equivalent
            gas_target: 15_000_000,
            gas_limit: 30_000_000,
        }
    }
}

/// Load a genesis file from disk. Performs schema validation and enforces
/// the fair-launch invariant before returning.
pub fn load_genesis(path: &Path) -> Result<GenesisConfig> {
    let toml_text = fs::read_to_string(path)
        .map_err(|e| ChainError::Codec(format!("read genesis {}: {e}", path.display())))?;
    let cfg: GenesisConfig = toml::from_str(&toml_text)
        .map_err(|e| ChainError::Codec(format!("parse genesis {}: {e}", path.display())))?;

    validate_genesis(&cfg)?;
    Ok(cfg)
}

/// Validate a parsed genesis — chain_id non-empty, validators non-empty,
/// and the fair-launch invariant (`initial_accounts.is_empty()`).
pub fn validate_genesis(cfg: &GenesisConfig) -> Result<()> {
    if cfg.chain_id.is_empty() {
        return Err(ChainError::Codec("genesis: chain_id is empty".into()));
    }
    if cfg.validators.is_empty() {
        return Err(ChainError::Codec(
            "genesis: must list at least one validator".into(),
        ));
    }
    if !cfg.initial_accounts.is_empty() {
        return Err(ChainError::Codec(format!(
            "genesis: fair-launch invariant violated — initial_accounts must be empty, got {} entries",
            cfg.initial_accounts.len()
        )));
    }
    if cfg.initial_height != 0 {
        return Err(ChainError::Codec(format!(
            "genesis: initial_height must be 0, got {}",
            cfg.initial_height
        )));
    }
    if cfg.params.gas_target == 0 {
        return Err(ChainError::Codec("genesis: gas_target must be > 0".into()));
    }
    if cfg.params.gas_limit < cfg.params.gas_target {
        return Err(ChainError::Codec(
            "genesis: gas_limit must be ≥ gas_target".into(),
        ));
    }
    for v in &cfg.validators {
        decode_pubkey(&v.pubkey_hex)?;
        decode_address(&v.operator_hex)?;
    }
    Ok(())
}

/// Convert a [`GenesisValidator`] into the on-chain [`ValidatorInfo`]
/// record. `NodeId` is derived as `blake3(pubkey_bytes)[0..32]` to match
/// PROTOCOL_SPEC §3.
pub fn genesis_to_validator_info(gv: &GenesisValidator) -> Result<ValidatorInfo> {
    let pubkey = decode_pubkey(&gv.pubkey_hex)?;
    let operator = decode_address(&gv.operator_hex)?;
    let node_id_bytes = {
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(arknet_crypto::hash::blake3(&pubkey.bytes).as_bytes());
        bytes
    };
    Ok(ValidatorInfo {
        node_id: arknet_common::types::NodeId::new(node_id_bytes),
        consensus_key: pubkey,
        operator,
        bonded_stake: 0,
        voting_power: gv.voting_power,
        is_genesis: true,
        jailed: false,
    })
}

fn decode_pubkey(hex_s: &str) -> Result<PubKey> {
    let clean = hex_s.strip_prefix("0x").unwrap_or(hex_s);
    let bytes =
        hex::decode(clean).map_err(|e| ChainError::Codec(format!("genesis pubkey hex: {e}")))?;
    if bytes.len() != SignatureScheme::Ed25519.pubkey_len() {
        return Err(ChainError::Codec(format!(
            "genesis pubkey: Ed25519 expects {} bytes, got {}",
            SignatureScheme::Ed25519.pubkey_len(),
            bytes.len()
        )));
    }
    PubKey::new(SignatureScheme::Ed25519, bytes)
        .map_err(|e| ChainError::Codec(format!("genesis pubkey: {e}")))
}

fn decode_address(hex_s: &str) -> Result<Address> {
    Address::from_hex(hex_s).map_err(|e| ChainError::Codec(format!("genesis operator: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fair_launch_toml() -> String {
        r#"
chain_id = "arknet-devnet-1"
initial_height = 0
initial_timestamp_ms = 1700000000000

[[validators]]
name = "v1"
pubkey_hex = "1111111111111111111111111111111111111111111111111111111111111111"
operator_hex = "0x2222222222222222222222222222222222222222"
voting_power = 1

[[validators]]
name = "v2"
pubkey_hex = "3333333333333333333333333333333333333333333333333333333333333333"
operator_hex = "0x4444444444444444444444444444444444444444"
voting_power = 1

[params]
base_fee = 1000000000
gas_target = 15000000
gas_limit = 30000000
"#
        .to_string()
    }

    fn write_and_load(text: &str) -> Result<GenesisConfig> {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("genesis.toml");
        fs::write(&p, text).unwrap();
        load_genesis(&p)
    }

    #[test]
    fn fair_launch_genesis_loads() {
        let cfg = write_and_load(&fair_launch_toml()).unwrap();
        assert_eq!(cfg.chain_id, "arknet-devnet-1");
        assert_eq!(cfg.validators.len(), 2);
        assert!(cfg.initial_accounts.is_empty());
    }

    #[test]
    fn rejects_any_initial_accounts() {
        let text = format!(
            "{}\n{}",
            fair_launch_toml(),
            r#"
[[initial_accounts]]
address_hex = "0x5555555555555555555555555555555555555555"
balance = 1000000000000000
"#,
        );
        let err = write_and_load(&text).unwrap_err();
        match err {
            ChainError::Codec(msg) => assert!(
                msg.contains("fair-launch invariant"),
                "message should flag the invariant, got: {msg}"
            ),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_validator_set() {
        let text = r#"
chain_id = "arknet-devnet-1"
initial_height = 0
initial_timestamp_ms = 1700000000000

[params]
base_fee = 1000000000
gas_target = 15000000
gas_limit = 30000000
"#;
        let err = write_and_load(text).unwrap_err();
        assert!(matches!(err, ChainError::Codec(_)));
    }

    #[test]
    fn rejects_nonzero_initial_height() {
        let mut text = fair_launch_toml();
        text = text.replace("initial_height = 0", "initial_height = 42");
        let err = write_and_load(&text).unwrap_err();
        match err {
            ChainError::Codec(msg) => assert!(msg.contains("initial_height")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_gas_relationship() {
        let text = fair_launch_toml().replace("gas_limit = 30000000", "gas_limit = 5000000");
        let err = write_and_load(&text).unwrap_err();
        assert!(matches!(err, ChainError::Codec(_)));
    }

    #[test]
    fn rejects_bad_pubkey_length() {
        let text = fair_launch_toml().replace(
            "pubkey_hex = \"1111111111111111111111111111111111111111111111111111111111111111\"",
            "pubkey_hex = \"1111\"",
        );
        let err = write_and_load(&text).unwrap_err();
        assert!(matches!(err, ChainError::Codec(_)));
    }

    #[test]
    fn converts_genesis_validator_to_info() {
        let cfg = write_and_load(&fair_launch_toml()).unwrap();
        let info = genesis_to_validator_info(&cfg.validators[0]).unwrap();
        assert!(info.is_genesis);
        assert_eq!(info.bonded_stake, 0);
        assert_eq!(info.voting_power, 1);
        assert!(!info.jailed);
    }

    #[test]
    fn rejects_missing_file() {
        let path = Path::new("/nonexistent/arknet-genesis-test.toml");
        let err = load_genesis(path).unwrap_err();
        assert!(matches!(err, ChainError::Codec(_)));
    }
}
