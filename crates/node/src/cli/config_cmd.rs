//! `arknet config {check, show}` — inspect or validate the node config
//! without booting the node.
//!
//! `check` loads and validates the file; exits 0 on success, 3 on
//! config error. Useful in pre-flight scripts.
//!
//! `show` prints the effective config (defaults + `node.toml` + env
//! overlays) as pretty TOML. Handy when you suspect an env var is
//! clobbering a field.

use std::path::Path;

use arknet_common::config::NodeConfig;
use clap::Subcommand;

use crate::errors::{NodeError, Result};
use crate::paths;

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Load and validate node.toml without starting the node.
    Check,
    /// Print the effective config (defaults + node.toml + env overlays).
    Show,
}

pub async fn run(cmd: ConfigCmd, data_dir: Option<&Path>) -> Result<()> {
    let root = paths::resolve(data_dir)?;
    let toml_path = paths::node_toml(&root);

    if !toml_path.exists() {
        return Err(NodeError::Config(format!(
            "no node.toml at {}; run `arknet init` first",
            toml_path.display()
        )));
    }

    let cfg = NodeConfig::load(&toml_path)?;

    match cmd {
        ConfigCmd::Check => {
            println!("OK: config at {} is valid", toml_path.display());
            if !cfg.roles.compute
                && !cfg.roles.router
                && !cfg.roles.validator
                && !cfg.roles.verifier
            {
                println!("  warning: no role is enabled — `arknet start` will refuse to boot");
            }
            Ok(())
        }
        ConfigCmd::Show => {
            let rendered = toml::to_string_pretty(&cfg).map_err(NodeError::from)?;
            println!("{rendered}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::init::{run as init_run, InitArgs};

    #[tokio::test]
    async fn check_passes_on_freshly_initialized_toml() {
        let tmp = tempfile::tempdir().unwrap();
        init_run(InitArgs { force: false }, Some(tmp.path()))
            .await
            .unwrap();
        run(ConfigCmd::Check, Some(tmp.path())).await.unwrap();
    }

    #[tokio::test]
    async fn check_errors_when_no_toml_present() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run(ConfigCmd::Check, Some(tmp.path())).await.unwrap_err();
        assert!(matches!(err, NodeError::Config(_)));
    }

    #[tokio::test]
    async fn show_roundtrips_through_pretty_toml() {
        let tmp = tempfile::tempdir().unwrap();
        init_run(InitArgs { force: false }, Some(tmp.path()))
            .await
            .unwrap();
        run(ConfigCmd::Show, Some(tmp.path())).await.unwrap();
    }
}
