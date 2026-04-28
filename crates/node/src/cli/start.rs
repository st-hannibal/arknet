//! `arknet start` — boot the node with the configured role.
//!
//! Phase 0 Day 1: stub. Day 5 fills this in.

use std::path::Path;

use clap::Args;

use crate::errors::{NodeError, Result};

#[derive(Args, Debug)]
pub struct StartArgs {
    /// Role to run. Phase 0 only supports `compute`.
    #[arg(long, default_value = "compute")]
    pub role: String,
}

pub async fn run(_args: StartArgs, _data_dir: Option<&Path>) -> Result<()> {
    Err(NodeError::NotImplemented("arknet start — Day 5"))
}
