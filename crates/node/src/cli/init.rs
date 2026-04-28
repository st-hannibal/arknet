//! `arknet init` — create data directory + default `node.toml`.
//!
//! Phase 0 Day 1: stub. Day 2 fills this in.

use std::path::Path;

use clap::Args;

use crate::errors::{NodeError, Result};

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Overwrite an existing node.toml if present.
    #[arg(long)]
    pub force: bool,
}

pub async fn run(_args: InitArgs, _data_dir: Option<&Path>) -> Result<()> {
    Err(NodeError::NotImplemented("arknet init — Day 2"))
}
