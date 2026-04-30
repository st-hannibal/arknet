//! `arknet wallet` — key management, balance queries, transfers.

use std::path::Path;

use clap::{Args, Subcommand};

use crate::errors::{NodeError, Result};
use crate::paths;

#[derive(Subcommand, Debug)]
pub enum WalletCmd {
    /// Create a new wallet keypair (or show existing).
    Create(CreateArgs),
    /// Show the wallet address and public key.
    Address(AddressArgs),
    /// Query the account balance from a running node.
    Balance(BalanceArgs),
    /// Send ARK to another address.
    Send(SendArgs),
}

#[derive(Args, Debug)]
pub struct CreateArgs {}

#[derive(Args, Debug)]
pub struct AddressArgs {}

#[derive(Args, Debug)]
pub struct BalanceArgs {
    /// RPC endpoint of a running node.
    #[arg(long, default_value = "http://127.0.0.1:26657")]
    pub rpc: String,
    /// Address to query (defaults to this node's address).
    #[arg(long)]
    pub address: Option<String>,
}

#[derive(Args, Debug)]
pub struct SendArgs {
    /// Recipient address (40-char hex).
    #[arg(long)]
    pub to: String,
    /// Amount in ark_atom.
    #[arg(long)]
    pub amount: u64,
    /// Fee in ark_atom (gas budget).
    #[arg(long, default_value = "21000")]
    pub fee: u64,
    /// RPC endpoint of a running node.
    #[arg(long, default_value = "http://127.0.0.1:26657")]
    pub rpc: String,
}

pub async fn run(cmd: WalletCmd, data_dir: Option<&Path>) -> Result<()> {
    let root = paths::resolve(data_dir)?;
    paths::ensure_layout(&root)?;

    match cmd {
        WalletCmd::Create(_) => create_wallet(&root),
        WalletCmd::Address(_) => show_address(&root),
        WalletCmd::Balance(args) => query_balance(&root, &args).await,
        WalletCmd::Send(args) => send_transfer(&root, &args).await,
    }
}

fn create_wallet(data_dir: &Path) -> Result<()> {
    let key_path = paths::keys_dir(data_dir).join("node.key");

    if key_path.exists() {
        eprintln!("Wallet already exists at {}", key_path.display());
        return show_address(data_dir);
    }

    let keypair = arknet_network::Keypair::generate_ed25519();
    let ed = keypair
        .clone()
        .try_into_ed25519()
        .map_err(|e| NodeError::Paths(format!("ed25519 conversion: {e}")))?;

    std::fs::create_dir_all(key_path.parent().unwrap())
        .map_err(|e| NodeError::Paths(format!("create keys dir: {e}")))?;
    std::fs::write(&key_path, ed.to_bytes())
        .map_err(|e| NodeError::Paths(format!("write {}: {e}", key_path.display())))?;

    eprintln!("Wallet created at {}", key_path.display());
    show_address(data_dir)
}

fn show_address(data_dir: &Path) -> Result<()> {
    let key_path = paths::keys_dir(data_dir).join("node.key");
    if !key_path.exists() {
        return Err(NodeError::Paths(
            "no wallet found — run `arknet wallet create` first".into(),
        ));
    }

    let bytes = std::fs::read(&key_path)
        .map_err(|e| NodeError::Paths(format!("read {}: {e}", key_path.display())))?;
    if bytes.len() != 64 {
        return Err(NodeError::Paths(format!(
            "node.key has {} bytes, expected 64",
            bytes.len()
        )));
    }

    let pubkey = &bytes[32..64];
    let digest = arknet_crypto::hash::blake3(pubkey);
    let address = &digest.as_bytes()[..20];

    println!("Address:    0x{}", hex::encode(address));
    println!("Public key: {}", hex::encode(pubkey));
    println!("Key file:   {}", key_path.display());

    Ok(())
}

async fn query_balance(data_dir: &Path, args: &BalanceArgs) -> Result<()> {
    let address_hex = match &args.address {
        Some(a) => a.strip_prefix("0x").unwrap_or(a).to_string(),
        None => {
            let key_path = paths::keys_dir(data_dir).join("node.key");
            if !key_path.exists() {
                return Err(NodeError::Paths(
                    "no wallet found — run `arknet wallet create` or pass --address".into(),
                ));
            }
            let bytes =
                std::fs::read(&key_path).map_err(|e| NodeError::Paths(format!("read key: {e}")))?;
            let pubkey = &bytes[32..64];
            let digest = arknet_crypto::hash::blake3(pubkey);
            hex::encode(&digest.as_bytes()[..20])
        }
    };

    let url = format!("{}/v1/account/{}", args.rpc, address_hex);
    let resp = reqwest::get(&url).await.map_err(|e| {
        NodeError::Config(format!(
            "cannot reach node at {} — is it running? ({})",
            args.rpc, e
        ))
    })?;

    if resp.status().is_success() {
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| NodeError::Config(format!("bad response: {e}")))?;
        let balance = body.get("balance").and_then(|v| v.as_u64()).unwrap_or(0);
        let nonce = body.get("nonce").and_then(|v| v.as_u64()).unwrap_or(0);
        println!("Address: 0x{address_hex}");
        println!(
            "Balance: {} ark_atom ({:.9} ARK)",
            balance,
            balance as f64 / 1e9
        );
        println!("Nonce:   {nonce}");
    } else if resp.status().as_u16() == 404 {
        println!("Address: 0x{address_hex}");
        println!("Balance: 0 ark_atom (account not found — no transactions yet)");
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(NodeError::Config(format!("RPC error ({status}): {body}")));
    }

    Ok(())
}

async fn send_transfer(data_dir: &Path, args: &SendArgs) -> Result<()> {
    let key_path = paths::keys_dir(data_dir).join("node.key");
    if !key_path.exists() {
        return Err(NodeError::Paths(
            "no wallet found — run `arknet wallet create` first".into(),
        ));
    }

    let key_bytes =
        std::fs::read(&key_path).map_err(|e| NodeError::Paths(format!("read key: {e}")))?;
    let pubkey = &key_bytes[32..64];
    let digest = arknet_crypto::hash::blake3(pubkey);
    let from_bytes: [u8; 20] = digest.as_bytes()[..20].try_into().unwrap();

    let to_hex = args.to.strip_prefix("0x").unwrap_or(&args.to);
    if to_hex.len() != 40 {
        return Err(NodeError::Config(format!(
            "recipient address must be 40 hex chars, got {}",
            to_hex.len()
        )));
    }
    let to_bytes: [u8; 20] = hex::decode(to_hex)
        .map_err(|e| NodeError::Config(format!("bad recipient hex: {e}")))?
        .try_into()
        .map_err(|_| NodeError::Config("recipient must be 20 bytes".into()))?;

    let from = arknet_common::types::Address::new(from_bytes);
    let to = arknet_common::types::Address::new(to_bytes);

    // Query current nonce from the running node so sequential
    // sends work. Falls back to 0 if the node is unreachable or
    // the account doesn't exist yet.
    let nonce = {
        let addr_hex = hex::encode(from_bytes);
        let url = format!("{}/v1/account/{}", args.rpc, addr_hex);
        match reqwest::get(&url).await {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                body.get("nonce").and_then(|v| v.as_u64()).unwrap_or(0)
            }
            _ => 0,
        }
    };

    let tx = arknet_chain::transactions::Transaction::Transfer {
        from,
        to,
        amount: args.amount as u128,
        nonce,
        fee: args.fee,
    };

    let tx_hash = tx.hash();
    let mut pk_arr = [0u8; 32];
    pk_arr.copy_from_slice(pubkey);
    let signer = arknet_common::types::PubKey::ed25519(pk_arr);

    let signing_key = ed25519_dalek::SigningKey::from_bytes(&key_bytes[..32].try_into().unwrap());
    let signature_bytes = {
        use ed25519_dalek::Signer;
        signing_key.sign(tx_hash.as_bytes()).to_bytes().to_vec()
    };
    let signature = arknet_common::types::Signature::new(
        arknet_common::types::SignatureScheme::Ed25519,
        signature_bytes,
    )
    .map_err(|e| NodeError::Config(format!("signature: {e}")))?;

    let stx = arknet_chain::transactions::SignedTransaction {
        tx,
        signer,
        signature,
    };
    let tx_hex =
        hex::encode(borsh::to_vec(&stx).map_err(|e| NodeError::Config(format!("encode tx: {e}")))?);

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/tx", args.rpc))
        .json(&serde_json::json!({ "tx_hex": tx_hex }))
        .send()
        .await
        .map_err(|e| NodeError::Config(format!("submit tx: {e}")))?;

    if resp.status().is_success() {
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let hash = body
            .get("tx_hash_hex")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        println!("Transaction submitted!");
        println!("  Hash: 0x{hash}");
        println!("  From: 0x{}", hex::encode(from_bytes));
        println!("  To:   0x{to_hex}");
        println!("  Amount: {} ark_atom", args.amount);
        println!("  Fee:    {} ark_atom", args.fee);
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(NodeError::Config(format!("tx rejected ({status}): {body}")));
    }

    Ok(())
}
