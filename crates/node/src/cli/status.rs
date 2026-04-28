//! `arknet status` — scrape the running node's metrics endpoint.
//!
//! Phase 0 Day 1: stub. Day 10 fills this in once the HTTP endpoint is live.

use std::path::Path;

use clap::Args;

use crate::errors::{NodeError, Result};

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Override the metrics endpoint URL. Default: reads config.
    #[arg(long)]
    pub endpoint: Option<String>,
}

pub async fn run(_args: StatusArgs, _data_dir: Option<&Path>) -> Result<()> {
    Err(NodeError::NotImplemented("arknet status — Day 10"))
}
