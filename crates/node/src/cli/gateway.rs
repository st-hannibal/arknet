//! `arknet gateway` — register/unregister as a public gateway.

use std::path::Path;

use clap::{Args, Subcommand};

use crate::errors::{NodeError, Result};
use crate::paths;

use super::wallet::{load_key, sign_and_submit};

#[derive(Subcommand, Debug)]
pub enum GatewayCmd {
    /// Register this node as a public gateway (discoverable RPC endpoint).
    Register(RegisterArgs),
    /// Remove this node from the gateway registry.
    Unregister(UnregisterArgs),
}

#[derive(Args, Debug)]
pub struct RegisterArgs {
    /// Public URL of this gateway (e.g. https://rpc.mynode.com).
    #[arg(long)]
    pub url: String,
    /// Whether the URL uses HTTPS (TLS-terminated). HTTPS gateways
    /// earn a 1.2x reward multiplier.
    #[arg(long, default_value = "false")]
    pub https: bool,
    /// RPC endpoint of a running node.
    #[arg(long, default_value = "http://127.0.0.1:26657")]
    pub rpc: String,
}

#[derive(Args, Debug)]
pub struct UnregisterArgs {
    /// RPC endpoint of a running node.
    #[arg(long, default_value = "http://127.0.0.1:26657")]
    pub rpc: String,
}

pub async fn run(cmd: GatewayCmd, data_dir: Option<&Path>) -> Result<()> {
    let root = paths::resolve(data_dir)?;
    paths::ensure_layout(&root)?;

    match cmd {
        GatewayCmd::Register(args) => register(&root, &args).await,
        GatewayCmd::Unregister(args) => unregister(&root, &args).await,
    }
}

async fn register(data_dir: &Path, args: &RegisterArgs) -> Result<()> {
    if args.url.is_empty() {
        return Err(NodeError::Config("--url cannot be empty".into()));
    }

    let (key_bytes, pubkey, addr) = load_key(data_dir)?;
    let node_id = arknet_common::types::NodeId::new(pubkey);
    let operator = arknet_common::types::Address::new(addr);

    let tx = arknet_chain::transactions::Transaction::RegisterGateway {
        node_id,
        operator,
        url: args.url.clone(),
        https: args.https,
    };

    let hash = sign_and_submit(&key_bytes, &pubkey, tx, &args.rpc).await?;
    println!("Gateway registered!");
    println!("  Hash:  0x{hash}");
    println!("  URL:   {}", args.url);
    println!("  HTTPS: {}", args.https);
    if args.https {
        println!("  Reward multiplier: 1.2x");
    }
    Ok(())
}

async fn unregister(data_dir: &Path, args: &UnregisterArgs) -> Result<()> {
    let (key_bytes, pubkey, addr) = load_key(data_dir)?;
    let node_id = arknet_common::types::NodeId::new(pubkey);
    let operator = arknet_common::types::Address::new(addr);

    let tx = arknet_chain::transactions::Transaction::UnregisterGateway { node_id, operator };

    let hash = sign_and_submit(&key_bytes, &pubkey, tx, &args.rpc).await?;
    println!("Gateway unregistered!");
    println!("  Hash: 0x{hash}");
    Ok(())
}
