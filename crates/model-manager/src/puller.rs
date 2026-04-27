//! HTTP puller with streaming SHA-256 verification and resumable downloads.
//!
//! The puller writes into a `.partial` file, hashes as it goes, and only
//! renames to the final path after both the byte count and the digest
//! match the manifest. Any failure leaves the partial file in place so
//! the next call can resume with a `Range:` request.
//!
//! # Security
//!
//! The content-hash check is the primary integrity gate for a model.
//! Never load a file that bypassed this step. A size-only check is
//! cheap but insufficient — a tampered mirror could serve same-size
//! bytes with different content.

use std::path::{Path, PathBuf};

use arknet_crypto::hash::{Sha256Digest, Sha256Stream};
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, RANGE};
use reqwest::{Client, StatusCode};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};
use url::Url;

use crate::errors::{ModelError, Result};
use crate::types::ModelManifest;

/// Default HTTP client timeout. Generous — models may take hours over
/// a thin connection.
const DEFAULT_TIMEOUT_SECS: u64 = 3600;

/// Reusable HTTP puller. Holds one [`reqwest::Client`] per instance so
/// TLS sessions and connection pools are preserved across calls.
#[derive(Clone, Debug)]
pub struct Puller {
    client: Client,
}

impl Default for Puller {
    fn default() -> Self {
        Self::new()
    }
}

impl Puller {
    /// Build a puller with the default tuned [`reqwest::Client`].
    pub fn new() -> Self {
        let client = Client::builder()
            .user_agent(concat!("arknet-model-manager/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .expect("reqwest client build must succeed with defaults");
        Self { client }
    }

    /// Build a puller around a caller-provided client. Useful for tests
    /// that wire up a custom resolver or TLS config.
    pub fn with_client(client: Client) -> Self {
        Self { client }
    }

    /// Download `manifest` into `dest`. Mirrors are tried in order; the
    /// first one that produces a byte stream is used. On success the
    /// file at `dest` is the verified model; on failure, the partial
    /// file is left so the next call can resume.
    pub async fn pull(&self, manifest: &ModelManifest, dest: &Path) -> Result<()> {
        let partial = partial_path(dest);
        let mut last_error: Option<ModelError> = None;

        for (idx, url) in manifest.mirrors.iter().enumerate() {
            debug!(
                mirror_idx = idx,
                %url,
                expected_size = manifest.size_bytes,
                "pulling model"
            );
            match self.pull_from_mirror(url, manifest, &partial).await {
                Ok(()) => {
                    tokio::fs::rename(&partial, dest).await?;
                    return Ok(());
                }
                Err(e) => {
                    warn!(mirror_idx = idx, %url, error = %e, "mirror failed");
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| ModelError::NoMirrors(manifest.model_ref.to_string())))
    }

    async fn pull_from_mirror(
        &self,
        url: &Url,
        manifest: &ModelManifest,
        partial: &Path,
    ) -> Result<()> {
        let already_on_disk = match tokio::fs::metadata(partial).await {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.into()),
        };

        if already_on_disk >= manifest.size_bytes {
            // Partial is already at or beyond expected size — something's
            // off. Start over with a fresh file rather than trust it.
            warn!(
                on_disk = already_on_disk,
                expected = manifest.size_bytes,
                "stale partial; truncating and restarting"
            );
            tokio::fs::remove_file(partial).await?;
        }

        let resume_from = tokio::fs::metadata(partial)
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        let mut headers = HeaderMap::new();
        if resume_from > 0 {
            let value = format!("bytes={resume_from}-");
            headers.insert(
                RANGE,
                HeaderValue::from_str(&value)
                    .map_err(|e| ModelError::Download(format!("bad Range header: {e}")))?,
            );
        }

        let resp = self.client.get(url.clone()).headers(headers).send().await?;

        let status = resp.status();
        let is_full = status == StatusCode::OK;
        let is_partial = status == StatusCode::PARTIAL_CONTENT;
        if !is_full && !is_partial {
            return Err(ModelError::Download(format!("{url}: HTTP {status}")));
        }

        // If we asked for a range but the server ignored it and returned 200,
        // we need to restart from zero — discard any partial.
        let mut write_offset = resume_from;
        if is_full && resume_from > 0 {
            warn!(%url, "server ignored Range; restarting");
            write_offset = 0;
            tokio::fs::remove_file(partial).await?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(partial)
            .await?;

        // Rebuild the hasher by replaying any already-downloaded prefix.
        let mut hasher = Sha256Stream::new();
        if write_offset > 0 {
            let existing = tokio::fs::read(partial).await?;
            if existing.len() as u64 != write_offset {
                return Err(ModelError::Download(
                    "partial file size changed mid-resume".into(),
                ));
            }
            hasher.update(&existing);
        }

        let mut stream = resp.bytes_stream();
        let mut bytes_written: u64 = write_offset;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ModelError::Download(format!("stream: {e}")))?;
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            bytes_written += chunk.len() as u64;

            if bytes_written > manifest.size_bytes {
                return Err(ModelError::SizeMismatch {
                    expected: manifest.size_bytes,
                    actual: bytes_written,
                });
            }
        }

        file.flush().await?;
        drop(file);

        if bytes_written != manifest.size_bytes {
            return Err(ModelError::SizeMismatch {
                expected: manifest.size_bytes,
                actual: bytes_written,
            });
        }

        let actual: Sha256Digest = hasher.finalize();
        if actual != manifest.sha256 {
            // A hash-mismatch means we downloaded something corrupted
            // or tampered. Delete the partial so the next attempt does
            // not waste a resume on poisoned bytes.
            let _ = tokio::fs::remove_file(partial).await;
            return Err(ModelError::HashMismatch {
                expected: hex::encode(manifest.sha256.as_bytes()),
                actual: hex::encode(actual.as_bytes()),
            });
        }

        Ok(())
    }
}

/// Canonical partial-file path for a target destination.
pub(crate) fn partial_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    dest.with_file_name(name)
}
