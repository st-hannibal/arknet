//! The unified `arknet` binary.
//!
//! This crate is the integration layer — the code that ties
//! `arknet-common`, `arknet-crypto`, `arknet-model-manager`,
//! `arknet-inference`, and the role stubs into one runnable binary.
//! Phase 0 wires things end-to-end; Phase 1 replaces the stub roles
//! with real implementations.

mod cli;
mod errors;
mod hardware;
mod logging;
mod paths;
mod runtime;

use clap::Parser;

use crate::cli::Cli;
use crate::errors::NodeError;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Install tracing before anything else so every downstream log lands
    // on the configured sink. Log format + filter come from the shared
    // CLI flags; the full config layer overrides later via `arknet start`.
    crate::logging::init(cli.log_format.into(), cli.log_level.as_deref());

    if let Err(e) = cli::dispatch(cli).await {
        // Print the full error chain to stderr and exit non-zero.
        eprintln!("arknet: {e}");
        let mut cause = std::error::Error::source(&e as &dyn std::error::Error);
        while let Some(c) = cause {
            eprintln!("  caused by: {c}");
            cause = c.source();
        }
        std::process::exit(exit_code(&e));
    }
}

/// Map error variants to distinct exit codes so shell scripts can branch.
fn exit_code(e: &NodeError) -> i32 {
    match e {
        NodeError::RoleNotImplemented(_) | NodeError::NotImplemented(_) => 2,
        NodeError::Config(_) | NodeError::CommonConfig(_) => 3,
        NodeError::Paths(_) => 4,
        _ => 1,
    }
}
