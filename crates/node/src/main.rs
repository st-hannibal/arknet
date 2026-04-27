//! The unified `arknet` binary.
//!
//! Phase 0 / Weeks 11-12: basic CLI skeleton with `init`, `start`, `status` commands.
//! Phase 1+: wires in actual role scheduler, consensus, networking.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "arknet",
    version,
    about = "arknet — decentralized AI inference node"
)]
struct Cli {
    /// Path to node config (defaults to $HOME/.arknet/node.toml)
    #[arg(long, global = true)]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate a new node config.
    Init {
        /// Network to initialize against: mainnet | testnet | devnet.
        #[arg(long, default_value = "devnet")]
        network: String,
    },
    /// Start the node.
    Start,
    /// Show node status.
    Status,
    /// Print version info.
    Version,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Init { network } => {
            println!("arknet init --network {network}  (stub — Week 11-12)");
        }
        Command::Start => {
            println!("arknet start  (stub — Week 11-12 wires roles together)");
        }
        Command::Status => {
            println!("arknet status  (stub — Week 11-12)");
        }
        Command::Version => {
            println!("arknet {}", env!("CARGO_PKG_VERSION"));
        }
    }

    Ok(())
}
