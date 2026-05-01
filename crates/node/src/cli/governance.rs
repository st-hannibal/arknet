//! `arknet governance` — submit proposals and vote.

use std::path::Path;

use clap::{Args, Subcommand};

use crate::errors::{NodeError, Result};
use crate::paths;

use super::wallet::{load_key, sign_and_submit};

#[derive(Subcommand, Debug)]
pub enum GovernanceCmd {
    /// Submit a governance proposal (requires 10,000 ARK deposit).
    Propose(ProposeArgs),
    /// Vote on an active proposal.
    Vote(VoteArgs),
}

#[derive(Args, Debug)]
pub struct ProposeArgs {
    /// Proposal title (short summary).
    #[arg(long)]
    pub title: String,
    /// Proposal body (markdown description). Use @file.md to read from file.
    #[arg(long)]
    pub body: String,
    /// Deposit in ark_atom (default: 10,000 ARK = 10_000_000_000_000).
    #[arg(long, default_value = "10000000000000")]
    pub deposit: u64,
    /// RPC endpoint of a running node.
    #[arg(long, default_value = "http://127.0.0.1:26657")]
    pub rpc: String,
}

#[derive(Args, Debug)]
pub struct VoteArgs {
    /// Proposal ID to vote on.
    #[arg(long)]
    pub proposal: u64,
    /// Vote choice: yes, no, abstain, no-with-veto.
    #[arg(long)]
    pub choice: String,
    /// RPC endpoint of a running node.
    #[arg(long, default_value = "http://127.0.0.1:26657")]
    pub rpc: String,
}

pub async fn run(cmd: GovernanceCmd, data_dir: Option<&Path>) -> Result<()> {
    let root = paths::resolve(data_dir)?;
    paths::ensure_layout(&root)?;

    match cmd {
        GovernanceCmd::Propose(args) => propose(&root, &args).await,
        GovernanceCmd::Vote(args) => vote(&root, &args).await,
    }
}

/// Parse a vote choice string.
fn parse_choice(s: &str) -> Result<arknet_chain::transactions::VoteChoice> {
    use arknet_chain::transactions::VoteChoice;
    match s.to_lowercase().replace('-', "").as_str() {
        "yes" => Ok(VoteChoice::Yes),
        "no" => Ok(VoteChoice::No),
        "abstain" => Ok(VoteChoice::Abstain),
        "nowithveto" | "veto" => Ok(VoteChoice::NoWithVeto),
        _ => Err(NodeError::Config(format!(
            "unknown vote choice '{s}' — use: yes, no, abstain, no-with-veto"
        ))),
    }
}

async fn propose(data_dir: &Path, args: &ProposeArgs) -> Result<()> {
    let (key_bytes, pubkey, addr) = load_key(data_dir)?;
    let proposer = arknet_common::types::Address::new(addr);

    let body = if let Some(path) = args.body.strip_prefix('@') {
        std::fs::read_to_string(path)
            .map_err(|e| NodeError::Config(format!("read proposal body from {path}: {e}")))?
    } else {
        args.body.clone()
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let one_week_ms = 7 * 24 * 60 * 60 * 1000;

    let proposal = arknet_chain::transactions::Proposal {
        proposal_id: 0, // allocated by the chain
        proposer,
        deposit: args.deposit as u128,
        title: args.title.clone(),
        body,
        discussion_ends: now_ms + one_week_ms,
        voting_ends: now_ms + 2 * one_week_ms,
        activation: None,
    };

    let tx = arknet_chain::transactions::Transaction::GovProposal(proposal);
    let hash = sign_and_submit(&key_bytes, &pubkey, tx, &args.rpc).await?;
    println!("Proposal submitted!");
    println!("  Hash:    0x{hash}");
    println!("  Title:   {}", args.title);
    println!("  Deposit: {} ark_atom", args.deposit);
    println!("  Discussion ends: ~7 days");
    println!("  Voting ends:     ~14 days");
    Ok(())
}

async fn vote(data_dir: &Path, args: &VoteArgs) -> Result<()> {
    let (key_bytes, pubkey, addr) = load_key(data_dir)?;
    let voter = arknet_common::types::Address::new(addr);
    let choice = parse_choice(&args.choice)?;

    let tx = arknet_chain::transactions::Transaction::GovVote {
        proposal_id: args.proposal,
        voter,
        choice,
    };

    let hash = sign_and_submit(&key_bytes, &pubkey, tx, &args.rpc).await?;
    println!("Vote submitted!");
    println!("  Hash:     0x{hash}");
    println!("  Proposal: {}", args.proposal);
    println!("  Choice:   {}", args.choice);
    Ok(())
}
