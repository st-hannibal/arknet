//! Graceful shutdown.
//!
//! Returns a `CancellationToken` that fires when the process receives
//! SIGINT (Ctrl-C) or, on Unix, SIGTERM. The token is cheap to clone —
//! each long-lived task holds its own copy and drops it when its role
//! body exits.

use tokio_util::sync::CancellationToken;

/// Spawn a background task that signals `CancellationToken::cancel`
/// when a termination signal arrives. Returns the token; callers keep
/// it for their own `.cancelled()` awaits.
pub fn install() -> CancellationToken {
    let token = CancellationToken::new();
    let child = token.clone();

    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => {
                    // Fall back to Ctrl-C only.
                    let _ = tokio::signal::ctrl_c().await;
                    child.cancel();
                    return;
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        child.cancel();
    });

    token
}
