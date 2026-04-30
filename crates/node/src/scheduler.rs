//! Role dispatch.
//!
//! Phase 0 is single-role: one role per `arknet start` invocation.
//! `compute` has a real (if idle) body; every other role logs a
//! "Phase 1" message and exits cleanly. A full role supervisor with
//! hot switching comes in Phase 1 when the other roles have real
//! bodies to supervise.

#![allow(dead_code)]

use std::fmt;
use std::str::FromStr;

use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::errors::{NodeError, Result};
use crate::runtime::NodeRuntime;

/// The four roles a node can play. Only `Compute` has a real body
/// at Phase 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Validator,
    Router,
    Compute,
    Verifier,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Role::Validator => "validator",
            Role::Router => "router",
            Role::Compute => "compute",
            Role::Verifier => "verifier",
        })
    }
}

impl FromStr for Role {
    type Err = NodeError;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "validator" => Ok(Role::Validator),
            "router" => Ok(Role::Router),
            "compute" => Ok(Role::Compute),
            "verifier" => Ok(Role::Verifier),
            other => Err(NodeError::RoleNotImplemented(other.to_string())),
        }
    }
}

/// Drive the requested role. Returns when `shutdown` fires or the
/// role body errors out. Phase 0 only implements `Compute`. Phase 1
/// Week 7-8 adds `Validator`; the consensus engine is booted by
/// `cli::start` and the role body here just waits for shutdown (the
/// engine's own tokio task already owns the loop).
pub async fn run(role: Role, rt: NodeRuntime, shutdown: CancellationToken) -> Result<()> {
    match role {
        Role::Compute => {
            // Compute role gets a real body once a runner is attached.
            // If none, fall back to the Phase-0 idle compute loop so
            // the node still starts (compute scheduling without the
            // L2 router stack is still useful for local CLI driving).
            if rt.compute.is_some() {
                crate::compute_role::run(rt, shutdown).await
            } else {
                run_compute(rt, shutdown).await
            }
        }
        Role::Router => crate::router_role::run(rt, shutdown).await,
        Role::Validator => run_validator(rt, shutdown).await,
        Role::Verifier => crate::verifier_role::run(rt, shutdown).await,
    }
}

/// Phase 1 Week 7-8 validator role.
///
/// The real work — the consensus engine — is already spawned by
/// [`crate::cli::start::run`] before this function is called. Here we
/// just park until shutdown while pushing a `current_height` gauge so
/// `/metrics` reflects chain progress without the RPC layer polling.
async fn run_validator(rt: NodeRuntime, shutdown: CancellationToken) -> Result<()> {
    let Some(consensus) = rt.consensus.clone() else {
        return Err(NodeError::Config(
            "validator role requires a ConsensusHandle — consensus engine did not boot".into(),
        ));
    };

    info!("validator role online — awaiting shutdown");
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
    tick.tick().await; // skip the immediate fire

    loop {
        tokio::select! {
            _ = tick.tick() => {
                match consensus.current_height().await {
                    Ok(h) => metrics::gauge!("arknet_consensus_height").set(h.0 as f64),
                    Err(e) => tracing::warn!(error = %e, "failed to read consensus height"),
                }
            }
            _ = shutdown.cancelled() => {
                info!("validator role shutting down cleanly");
                return Ok(());
            }
        }
    }
}

/// Phase 0 compute role.
///
/// The honest Phase 0 answer (surfaced in the design doc): there is
/// no external ingress wired until Day 9 adds the HTTP endpoint.
/// Until then, "running compute" means: the runtime is live, the
/// metrics server is up, and the operator drives everything via the
/// CLI against the same runtime the scheduler is holding. The role
/// body sits in a loop, ticking an uptime gauge, until shutdown.
async fn run_compute(_rt: NodeRuntime, shutdown: CancellationToken) -> Result<()> {
    info!("compute role online — awaiting CLI commands or HTTP requests");

    let started = std::time::Instant::now();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
    // Default `interval` fires immediately; skip that tick so the
    // first recorded uptime is > 0.
    tick.tick().await;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let uptime = started.elapsed().as_secs_f64();
                metrics::gauge!("arknet_node_uptime_seconds").set(uptime);
            }
            _ = shutdown.cancelled() => {
                info!("compute role shutting down cleanly");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_parses_lowercase() {
        assert_eq!(Role::from_str("compute").unwrap(), Role::Compute);
        assert_eq!(Role::from_str("VALIDATOR").unwrap(), Role::Validator);
    }

    #[test]
    fn role_rejects_unknown() {
        assert!(Role::from_str("bouncer").is_err());
    }

    #[test]
    fn role_display_roundtrips() {
        for r in [Role::Validator, Role::Router, Role::Compute, Role::Verifier] {
            assert_eq!(Role::from_str(&r.to_string()).unwrap(), r);
        }
    }

    #[tokio::test]
    async fn verifier_role_without_consensus_errors() {
        // Verifier now has a real body but requires a consensus handle
        // (for block events + dispute submission). Without one it
        // returns Config error.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = arknet_common::config::NodeConfig::default();
        let rt = NodeRuntime::open(tmp.path().to_path_buf(), cfg)
            .await
            .unwrap();
        let shutdown = CancellationToken::new();

        let err = run(Role::Verifier, rt, shutdown).await.unwrap_err();
        assert!(matches!(err, NodeError::Config(_)));
    }

    #[tokio::test]
    async fn router_role_without_handle_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = arknet_common::config::NodeConfig::default();
        let rt = NodeRuntime::open(tmp.path().to_path_buf(), cfg)
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        let err = run(Role::Router, rt, shutdown).await.unwrap_err();
        assert!(matches!(err, NodeError::Config(_)));
    }

    #[tokio::test]
    async fn validator_without_consensus_handle_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = arknet_common::config::NodeConfig::default();
        let rt = NodeRuntime::open(tmp.path().to_path_buf(), cfg)
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        let err = run(Role::Validator, rt, shutdown).await.unwrap_err();
        assert!(matches!(err, NodeError::Config(_)));
    }

    #[tokio::test]
    async fn compute_role_exits_on_shutdown() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = arknet_common::config::NodeConfig::default();
        let rt = NodeRuntime::open(tmp.path().to_path_buf(), cfg)
            .await
            .unwrap();
        let shutdown = CancellationToken::new();

        let task = tokio::spawn(run_compute(rt, shutdown.clone()));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("compute role did not exit promptly")
            .unwrap()
            .unwrap();
    }
}
