//! Tracing / logging setup.
//!
//! Three output layers, composable from config and `RUST_LOG`:
//!
//! - **Console** — pretty human-readable output to stderr. Always on.
//! - **JSON** — structured logs to stderr. Opt-in via [`LogFormat::Json`].
//! - **File** — Phase 1 will add rotating file output via `tracing-appender`.
//!   Phase 0 ships console + JSON only; file output is deferred to avoid
//!   pulling in the extra dep before we need it.
//!
//! `EnvFilter` precedence: `RUST_LOG` env > config `log_level` string.
//! Default: `info,llama=warn` — quiets llama.cpp's chatty INFO at startup.

use serde::{Deserialize, Serialize};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Output format for the console / stderr layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable, color-on-TTY.
    #[default]
    Pretty,
    /// One JSON object per line. For log aggregators (Loki, Datadog, etc.).
    Json,
}

/// Defaults that apply when neither `RUST_LOG` nor config sets a filter.
///
/// `info` for our own crates, `warn` for the llama.cpp log target to
/// avoid drowning operators in `llama_model_loader` startup chatter.
pub const DEFAULT_FILTER: &str = "info,llama=warn";

/// Install the global tracing subscriber.
///
/// Safe to call at most once per process. Subsequent calls will return
/// an error from `try_init`, which we surface as a log but don't panic.
pub fn init(format: LogFormat, level_override: Option<&str>) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| match level_override {
            Some(s) => EnvFilter::try_new(s),
            None => EnvFilter::try_new(DEFAULT_FILTER),
        })
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let registry = tracing_subscriber::registry().with(filter);

    match format {
        LogFormat::Pretty => {
            if let Err(e) = registry
                .with(fmt::layer().with_target(true).with_writer(std::io::stderr))
                .try_init()
            {
                // Already initialized (e.g. in tests). Log and continue.
                eprintln!("arknet: tracing already initialized: {e}");
            }
        }
        LogFormat::Json => {
            if let Err(e) = registry
                .with(
                    fmt::layer()
                        .json()
                        .with_target(true)
                        .with_writer(std::io::stderr),
                )
                .try_init()
            {
                eprintln!("arknet: tracing already initialized: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_format_default_is_pretty() {
        assert_eq!(LogFormat::default(), LogFormat::Pretty);
    }

    #[test]
    fn log_format_deserializes_lowercase() {
        let pretty: LogFormat = serde_json::from_str("\"pretty\"").unwrap();
        let json: LogFormat = serde_json::from_str("\"json\"").unwrap();
        assert_eq!(pretty, LogFormat::Pretty);
        assert_eq!(json, LogFormat::Json);
    }
}
