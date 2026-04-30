//! `arknet tee` — TEE capability management.

use std::path::Path;

use clap::{Args, Subcommand};

use crate::errors::{NodeError, Result};
use crate::paths;

#[derive(Subcommand, Debug)]
pub enum TeeCmd {
    /// Generate an enclave keypair and print the public key.
    Keygen(KeygenArgs),
    /// Show the enclave public key (for users to encrypt prompts to).
    Pubkey(PubkeyArgs),
    /// Register TEE capability on-chain by submitting a
    /// RegisterTeeCapability transaction.
    Register(RegisterArgs),
}

#[derive(Args, Debug)]
pub struct KeygenArgs {}

#[derive(Args, Debug)]
pub struct PubkeyArgs {}

#[derive(Args, Debug)]
pub struct RegisterArgs {
    /// RPC endpoint of a running validator node.
    #[arg(long, default_value = "http://127.0.0.1:26657")]
    pub rpc: String,
    /// TEE platform: "intel-tdx" or "amd-sev-snp".
    #[arg(long)]
    pub platform: String,
    /// Path to the raw attestation quote file.
    #[arg(long)]
    pub quote_file: std::path::PathBuf,
}

/// Dispatch `arknet tee <subcommand>`.
pub async fn run(cmd: TeeCmd, data_dir: Option<&Path>) -> Result<()> {
    match cmd {
        TeeCmd::Keygen(_) => keygen(data_dir).await,
        TeeCmd::Pubkey(_) => show_pubkey(data_dir).await,
        TeeCmd::Register(args) => register(args, data_dir).await,
    }
}

/// Path to the enclave keypair file.
fn enclave_key_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("keys").join("enclave.key")
}

/// Generate and persist an enclave X25519 keypair.
async fn keygen(data_dir: Option<&Path>) -> Result<()> {
    let dir = paths::resolve(data_dir)?;
    let key_path = enclave_key_path(&dir);

    if key_path.exists() {
        println!("Enclave key already exists at {}", key_path.display());
        return show_pubkey(data_dir).await;
    }

    let keys_dir = key_path.parent().unwrap();
    std::fs::create_dir_all(keys_dir).map_err(|e| NodeError::Config(format!("mkdir keys: {e}")))?;

    let secret = arknet_crypto::keys::KeyExchangeSecret::generate();
    let pubkey = secret.public_key();

    // Persist: 32-byte secret key.
    std::fs::write(&key_path, secret.export())
        .map_err(|e| NodeError::Config(format!("write enclave key: {e}")))?;

    println!("Enclave keypair generated:");
    println!("  Private: {}", key_path.display());
    println!("  Public:  {}", hex::encode(pubkey.as_bytes()));
    println!();
    println!("Users encrypt prompts to this public key for confidential inference.");
    println!("The private key MUST stay on this machine (inside the TEE).");

    Ok(())
}

/// Load and display the enclave public key.
async fn show_pubkey(data_dir: Option<&Path>) -> Result<()> {
    let dir = paths::resolve(data_dir)?;
    let key_path = enclave_key_path(&dir);

    if !key_path.exists() {
        return Err(NodeError::Config(format!(
            "no enclave key at {}. Run `arknet tee keygen` first.",
            key_path.display()
        )));
    }

    let bytes = std::fs::read(&key_path)
        .map_err(|e| NodeError::Config(format!("read enclave key: {e}")))?;
    if bytes.len() != 32 {
        return Err(NodeError::Config(format!(
            "enclave key file is {} bytes, expected 32",
            bytes.len()
        )));
    }
    let mut secret_bytes = [0u8; 32];
    secret_bytes.copy_from_slice(&bytes);
    let secret = arknet_crypto::keys::KeyExchangeSecret::from_bytes(secret_bytes);
    let pubkey = secret.public_key();

    println!("{}", hex::encode(pubkey.as_bytes()));
    Ok(())
}

/// Submit a RegisterTeeCapability transaction.
async fn register(args: RegisterArgs, data_dir: Option<&Path>) -> Result<()> {
    let dir = paths::resolve(data_dir)?;
    let key_path = enclave_key_path(&dir);

    if !key_path.exists() {
        return Err(NodeError::Config(
            "no enclave key. Run `arknet tee keygen` first.".into(),
        ));
    }

    // Load enclave pubkey.
    let secret_bytes = std::fs::read(&key_path)
        .map_err(|e| NodeError::Config(format!("read enclave key: {e}")))?;
    if secret_bytes.len() != 32 {
        return Err(NodeError::Config("invalid enclave key length".into()));
    }
    let mut sb = [0u8; 32];
    sb.copy_from_slice(&secret_bytes);
    let enclave_pubkey = arknet_crypto::keys::KeyExchangeSecret::from_bytes(sb).public_key();

    // Load attestation quote.
    let quote = std::fs::read(&args.quote_file)
        .map_err(|e| NodeError::Config(format!("read quote file: {e}")))?;

    let platform = match args.platform.as_str() {
        "intel-tdx" => arknet_common::types::TeePlatform::IntelTdx,
        "amd-sev-snp" => arknet_common::types::TeePlatform::AmdSevSnp,
        "arm-cca" => arknet_common::types::TeePlatform::ArmCca,
        other => {
            return Err(NodeError::Config(format!(
                "unknown TEE platform: {other}. Use intel-tdx, amd-sev-snp, or arm-cca."
            )));
        }
    };

    // Load node signing key.
    let node_key_path = dir.join("keys").join("node.key");
    let node_key_bytes = std::fs::read(&node_key_path)
        .map_err(|e| NodeError::Config(format!("read node key: {e}")))?;
    if node_key_bytes.len() != 64 {
        return Err(NodeError::Config("invalid node key length".into()));
    }
    let signing_key =
        arknet_crypto::keys::SigningKey::from_seed(node_key_bytes[..32].try_into().unwrap());
    let signer_pubkey = signing_key.verifying_key().to_pubkey();

    // Derive operator address.
    let operator = {
        let digest = arknet_crypto::hash::blake3(&signer_pubkey.bytes);
        let mut out = [0u8; 20];
        out.copy_from_slice(&digest.as_bytes()[..20]);
        arknet_common::types::Address::new(out)
    };

    // Derive node id.
    let node_id = {
        let digest = arknet_crypto::hash::blake3(&signer_pubkey.bytes);
        let mut out = [0u8; 32];
        out.copy_from_slice(digest.as_bytes());
        arknet_common::types::NodeId::new(out)
    };

    let capability = arknet_common::types::TeeCapability {
        platform,
        quote,
        enclave_pubkey: arknet_common::types::PubKey::ed25519(*enclave_pubkey.as_bytes()),
    };

    let tx = arknet_chain::Transaction::RegisterTeeCapability {
        node_id,
        operator,
        capability,
    };

    // Sign and submit.
    let tx_hash = tx.hash();
    let signature = arknet_crypto::signatures::sign(&signing_key, tx_hash.as_bytes());
    let signed = arknet_chain::SignedTransaction {
        tx,
        signer: signer_pubkey,
        signature,
    };

    let body =
        serde_json::to_vec(&signed).map_err(|e| NodeError::Config(format!("serialize tx: {e}")))?;

    let url = format!("{}/v1/tx", args.rpc);
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| NodeError::Config(format!("submit tx: {e}")))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| NodeError::Config(format!("read response: {e}")))?;

    if status.is_success() {
        println!("TEE capability registered successfully.");
        println!("  Platform: {}", args.platform);
        println!("  Node:     {node_id}");
        println!("  Tx hash:  {tx_hash}");
    } else {
        println!("Registration failed (HTTP {status}): {text}");
    }

    Ok(())
}
