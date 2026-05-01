//! `arknet wallet` — key management, balance queries, transfers, staking.

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
    /// Stake ARK for a role (validator, router, compute, verifier).
    Stake(StakeArgs),
    /// Begin unstaking ARK (starts 14-day unbonding period).
    Unstake(UnstakeArgs),
    /// Finalize a completed unbonding and reclaim ARK.
    CompleteUnbond(CompleteUnbondArgs),
    /// Move staked ARK from one node to another.
    Redelegate(RedelegateArgs),
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

#[derive(Args, Debug)]
pub struct StakeArgs {
    /// Amount in ark_atom to stake.
    #[arg(long)]
    pub amount: u64,
    /// Role to stake for: validator, router, compute, verifier.
    #[arg(long)]
    pub role: String,
    /// RPC endpoint of a running node.
    #[arg(long, default_value = "http://127.0.0.1:26657")]
    pub rpc: String,
}

#[derive(Args, Debug)]
pub struct UnstakeArgs {
    /// Amount in ark_atom to unstake.
    #[arg(long)]
    pub amount: u64,
    /// Role to unstake from: validator, router, compute, verifier.
    #[arg(long)]
    pub role: String,
    /// RPC endpoint of a running node.
    #[arg(long, default_value = "http://127.0.0.1:26657")]
    pub rpc: String,
}

#[derive(Args, Debug)]
pub struct CompleteUnbondArgs {
    /// Unbonding ID returned by the unstake transaction.
    #[arg(long)]
    pub unbond_id: u64,
    /// Role the stake was in: validator, router, compute, verifier.
    #[arg(long)]
    pub role: String,
    /// RPC endpoint of a running node.
    #[arg(long, default_value = "http://127.0.0.1:26657")]
    pub rpc: String,
}

#[derive(Args, Debug)]
pub struct RedelegateArgs {
    /// Amount in ark_atom to move.
    #[arg(long)]
    pub amount: u64,
    /// Role: validator, router, compute, verifier.
    #[arg(long)]
    pub role: String,
    /// Destination node public key (64-char hex).
    #[arg(long)]
    pub to_node: String,
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
        WalletCmd::Stake(args) => stake(&root, &args).await,
        WalletCmd::Unstake(args) => unstake(&root, &args).await,
        WalletCmd::CompleteUnbond(args) => complete_unbond(&root, &args).await,
        WalletCmd::Redelegate(args) => redelegate(&root, &args).await,
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

/// Load wallet key material from disk. Returns (key_bytes, pubkey_32, address_20).
pub fn load_key(data_dir: &Path) -> Result<([u8; 64], [u8; 32], [u8; 20])> {
    let key_path = paths::keys_dir(data_dir).join("node.key");
    if !key_path.exists() {
        return Err(NodeError::Paths(
            "no wallet found — run `arknet wallet create` first".into(),
        ));
    }
    let key_bytes: [u8; 64] = std::fs::read(&key_path)
        .map_err(|e| NodeError::Paths(format!("read key: {e}")))?
        .try_into()
        .map_err(|v: Vec<u8>| {
            NodeError::Paths(format!("node.key has {} bytes, expected 64", v.len()))
        })?;
    let mut pubkey = [0u8; 32];
    pubkey.copy_from_slice(&key_bytes[32..64]);
    let digest = arknet_crypto::hash::blake3(&pubkey);
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&digest.as_bytes()[..20]);
    Ok((key_bytes, pubkey, addr))
}

/// Sign a transaction and submit it to a node's RPC endpoint.
pub async fn sign_and_submit(
    key_bytes: &[u8; 64],
    pubkey: &[u8; 32],
    tx: arknet_chain::transactions::Transaction,
    rpc: &str,
) -> Result<String> {
    let tx_hash = tx.hash();
    let signer = arknet_common::types::PubKey::ed25519(*pubkey);
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
        .post(format!("{}/v1/tx", rpc))
        .json(&serde_json::json!({ "tx_hex": tx_hex }))
        .send()
        .await
        .map_err(|e| NodeError::Config(format!("submit tx: {e}")))?;

    if resp.status().is_success() {
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let hash = body
            .get("tx_hash_hex")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        Ok(hash)
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(NodeError::Config(format!("tx rejected ({status}): {body}")))
    }
}

/// Parse a role string into a StakeRole.
fn parse_role(s: &str) -> Result<arknet_chain::transactions::StakeRole> {
    use arknet_chain::transactions::StakeRole;
    match s.to_lowercase().as_str() {
        "validator" => Ok(StakeRole::Validator),
        "router" => Ok(StakeRole::Router),
        "compute" => Ok(StakeRole::Compute),
        "verifier" => Ok(StakeRole::Verifier),
        _ => Err(NodeError::Config(format!(
            "unknown role '{s}' — use: validator, router, compute, verifier"
        ))),
    }
}

async fn send_transfer(data_dir: &Path, args: &SendArgs) -> Result<()> {
    let (key_bytes, pubkey, from_bytes) = load_key(data_dir)?;

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

    let hash = sign_and_submit(&key_bytes, &pubkey, tx, &args.rpc).await?;
    println!("Transaction submitted!");
    println!("  Hash: 0x{hash}");
    println!("  From: 0x{}", hex::encode(from_bytes));
    println!("  To:   0x{to_hex}");
    println!("  Amount: {} ark_atom", args.amount);
    println!("  Fee:    {} ark_atom", args.fee);
    Ok(())
}

async fn stake(data_dir: &Path, args: &StakeArgs) -> Result<()> {
    let (key_bytes, pubkey, _addr) = load_key(data_dir)?;
    let role = parse_role(&args.role)?;
    let node_id = arknet_common::types::NodeId::new(pubkey);

    let tx = arknet_chain::transactions::Transaction::StakeOp(
        arknet_chain::transactions::StakeOp::Deposit {
            node_id,
            role,
            pool_id: None,
            amount: args.amount as u128,
            delegator: None,
        },
    );

    let hash = sign_and_submit(&key_bytes, &pubkey, tx, &args.rpc).await?;
    println!("Stake submitted!");
    println!("  Hash:   0x{hash}");
    println!("  Role:   {}", args.role);
    println!("  Amount: {} ark_atom", args.amount);
    Ok(())
}

async fn unstake(data_dir: &Path, args: &UnstakeArgs) -> Result<()> {
    let (key_bytes, pubkey, _addr) = load_key(data_dir)?;
    let role = parse_role(&args.role)?;
    let node_id = arknet_common::types::NodeId::new(pubkey);

    let tx = arknet_chain::transactions::Transaction::StakeOp(
        arknet_chain::transactions::StakeOp::Withdraw {
            node_id,
            role,
            pool_id: None,
            amount: args.amount as u128,
        },
    );

    let hash = sign_and_submit(&key_bytes, &pubkey, tx, &args.rpc).await?;
    println!("Unstake submitted! (14-day unbonding period starts now)");
    println!("  Hash:   0x{hash}");
    println!("  Role:   {}", args.role);
    println!("  Amount: {} ark_atom", args.amount);
    Ok(())
}

async fn complete_unbond(data_dir: &Path, args: &CompleteUnbondArgs) -> Result<()> {
    let (key_bytes, pubkey, _addr) = load_key(data_dir)?;
    let role = parse_role(&args.role)?;
    let node_id = arknet_common::types::NodeId::new(pubkey);

    let tx = arknet_chain::transactions::Transaction::StakeOp(
        arknet_chain::transactions::StakeOp::Complete {
            node_id,
            role,
            pool_id: None,
            unbond_id: args.unbond_id,
        },
    );

    let hash = sign_and_submit(&key_bytes, &pubkey, tx, &args.rpc).await?;
    println!("Unbonding finalized!");
    println!("  Hash:      0x{hash}");
    println!("  Unbond ID: {}", args.unbond_id);
    Ok(())
}

async fn redelegate(data_dir: &Path, args: &RedelegateArgs) -> Result<()> {
    let (key_bytes, pubkey, _addr) = load_key(data_dir)?;
    let role = parse_role(&args.role)?;
    let from_node = arknet_common::types::NodeId::new(pubkey);

    let to_hex = args.to_node.strip_prefix("0x").unwrap_or(&args.to_node);
    if to_hex.len() != 64 {
        return Err(NodeError::Config(format!(
            "destination node pubkey must be 64 hex chars, got {}",
            to_hex.len()
        )));
    }
    let to_bytes: [u8; 32] = hex::decode(to_hex)
        .map_err(|e| NodeError::Config(format!("bad node pubkey hex: {e}")))?
        .try_into()
        .map_err(|_| NodeError::Config("node pubkey must be 32 bytes".into()))?;
    let to_node = arknet_common::types::NodeId::new(to_bytes);

    let tx = arknet_chain::transactions::Transaction::StakeOp(
        arknet_chain::transactions::StakeOp::Redelegate {
            from: from_node,
            to: to_node,
            role,
            amount: args.amount as u128,
        },
    );

    let hash = sign_and_submit(&key_bytes, &pubkey, tx, &args.rpc).await?;
    println!("Redelegation submitted! (1-day cooldown)");
    println!("  Hash:   0x{hash}");
    println!("  To:     0x{to_hex}");
    println!("  Role:   {}", args.role);
    println!("  Amount: {} ark_atom", args.amount);
    Ok(())
}
