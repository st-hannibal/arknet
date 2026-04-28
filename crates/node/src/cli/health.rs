//! `arknet health` — one-shot probe of the running node's /health endpoint.
//!
//! Phase 0 Day 1: stub. Day 10 fills this in.

use std::path::Path;

use clap::Args;

use crate::errors::{NodeError, Result};

#[derive(Args, Debug)]
pub struct HealthArgs {
    /// Override the health endpoint URL. Default: reads config.
    #[arg(long)]
    pub endpoint: Option<String>,
}

pub async fn run(_args: HealthArgs, _data_dir: Option<&Path>) -> Result<()> {
    Err(NodeError::NotImplemented("arknet health — Day 10"))
}
