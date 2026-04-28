//! `arknet config {check, show}` — config utilities.
//!
//! Phase 0 Day 1: stubs. Day 2 fills `check` in.

use std::path::Path;

use clap::Subcommand;

use crate::errors::{NodeError, Result};

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Load and validate node.toml without starting the node.
    Check,
    /// Print the effective config (defaults + node.toml + env overlays).
    Show,
}

pub async fn run(cmd: ConfigCmd, _data_dir: Option<&Path>) -> Result<()> {
    match cmd {
        ConfigCmd::Check => Err(NodeError::NotImplemented("config check — Day 2")),
        ConfigCmd::Show => Err(NodeError::NotImplemented("config show — Day 2")),
    }
}
