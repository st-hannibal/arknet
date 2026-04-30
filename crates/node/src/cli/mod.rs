//! `arknet` CLI — top-level command surface.
//!
//! Nested subcommands (Cargo / Docker / kubectl style). Every command
//! routes through `cli::dispatch` which is what `main.rs` calls.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::errors::Result;
use crate::logging::LogFormat;

pub mod config_cmd;
pub mod health;
pub mod init;
pub mod model_cmd;
pub mod start;
pub mod status;
pub mod tee;
pub mod wallet;

/// Top-level arguments shared across every subcommand.
#[derive(Parser, Debug)]
#[command(
    name = "arknet",
    version,
    about = "arknet — decentralized AI inference protocol node",
    long_about = None
)]
pub struct Cli {
    /// Override the data directory. Default: ~/.arknet/
    /// (or the ARKNET_HOME env var if set).
    #[arg(long, global = true, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,

    /// Log output format. Default: pretty.
    #[arg(long, global = true, value_enum, default_value_t = CliLogFormat::Pretty)]
    pub log_format: CliLogFormat,

    /// Explicit log filter (overrides config; RUST_LOG env var wins
    /// above this if set).
    #[arg(long, global = true, value_name = "FILTER")]
    pub log_level: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

/// Serialized log-format variant for clap.
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
pub enum CliLogFormat {
    /// Human-readable colored output.
    Pretty,
    /// Line-delimited JSON.
    Json,
}

impl From<CliLogFormat> for LogFormat {
    fn from(v: CliLogFormat) -> Self {
        match v {
            CliLogFormat::Pretty => LogFormat::Pretty,
            CliLogFormat::Json => LogFormat::Json,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Initialize the data directory and default config.
    Init(init::InitArgs),

    /// Start the node — boots the configured role and serves metrics + RPC.
    Start(start::StartArgs),

    /// Print a one-shot status report from the running node's endpoints.
    Status(status::StatusArgs),

    /// Check whether the running node's `/health` endpoint reports OK.
    /// Exit 0 on ok, 1 on degraded/unreachable.
    Health(health::HealthArgs),

    /// Inspect or validate the node config without starting the node.
    #[command(subcommand)]
    Config(config_cmd::ConfigCmd),

    /// Drive the model-manager + inference engine from the CLI.
    #[command(subcommand)]
    Model(model_cmd::ModelCmd),

    /// Wallet — create keys, check balance, send ARK.
    #[command(subcommand)]
    Wallet(wallet::WalletCmd),

    /// TEE — manage enclave keys and register TEE capability.
    #[command(subcommand)]
    Tee(tee::TeeCmd),
}

/// Dispatch a parsed [`Cli`] to the matching handler.
pub async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init(args) => init::run(args, cli.data_dir.as_deref()).await,
        Command::Start(args) => start::run(args, cli.data_dir.as_deref()).await,
        Command::Status(args) => status::run(args, cli.data_dir.as_deref()).await,
        Command::Health(args) => health::run(args, cli.data_dir.as_deref()).await,
        Command::Config(cmd) => config_cmd::run(cmd, cli.data_dir.as_deref()).await,
        Command::Model(cmd) => model_cmd::run(cmd, cli.data_dir.as_deref()).await,
        Command::Wallet(cmd) => wallet::run(cmd, cli.data_dir.as_deref()).await,
        Command::Tee(cmd) => tee::run(cmd, cli.data_dir.as_deref()).await,
    }
}
