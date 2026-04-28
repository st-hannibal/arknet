//! `arknet model {list, pull, load, verify, bench}` — operator-facing
//! commands that drive the model manager + inference engine directly.
//!
//! Phase 0 Day 1: stubs. Days 7-8 fill these in.

use std::path::Path;

use clap::{Args, Subcommand};

use crate::errors::{NodeError, Result};

#[derive(Subcommand, Debug)]
pub enum ModelCmd {
    /// List entries in the model-manager cache.
    List,
    /// Pull a model by reference and verify it into the cache.
    Pull(PullArgs),
    /// Load a model into the inference engine and print its metadata.
    Load(LoadArgs),
    /// Re-verify a cached model against its manifest digest.
    Verify(VerifyArgs),
    /// Run a throughput benchmark for a loaded model.
    Bench(BenchArgs),
}

#[derive(Args, Debug)]
pub struct PullArgs {
    /// Model reference, e.g. `meta-llama/Llama-3-7B-Instruct-Q4_K_M`.
    pub model_ref: String,
}

#[derive(Args, Debug)]
pub struct LoadArgs {
    /// Model reference.
    pub model_ref: String,
}

#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// Model reference.
    pub model_ref: String,
}

#[derive(Args, Debug)]
pub struct BenchArgs {
    /// Model reference.
    pub model_ref: String,
    /// Tokens to generate during the timed portion.
    #[arg(long, default_value_t = 128)]
    pub tokens: u32,
    /// Prompt to use. A short default is provided.
    #[arg(long, default_value = "Once upon a time")]
    pub prompt: String,
}

pub async fn run(cmd: ModelCmd, _data_dir: Option<&Path>) -> Result<()> {
    match cmd {
        ModelCmd::List => Err(NodeError::NotImplemented("model list — Day 7")),
        ModelCmd::Pull(_) => Err(NodeError::NotImplemented("model pull — Day 7")),
        ModelCmd::Load(_) => Err(NodeError::NotImplemented("model load — Day 7")),
        ModelCmd::Verify(_) => Err(NodeError::NotImplemented("model verify — Day 7")),
        ModelCmd::Bench(_) => Err(NodeError::NotImplemented("model bench — Day 8")),
    }
}
