//! Data availability layer integration.
//!
//! Posts block bodies to an external DA layer (Celestia or EigenDA) so
//! validators can prune old blocks locally while keeping full data
//! recoverable. Phase 1-2 used inline storage; this module enables the
//! offload path.
//!
//! # Celestia integration
//!
//! Uses the Celestia light node's HTTP blob API (`/blob.Submit`,
//! `/blob.Get`). The light node handles sampling and header sync;
//! arknet only needs to post and retrieve blobs via HTTP.

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::errors::{ChainError, Result};
use crate::receipt::{DaLayer, DaReference};

/// Configuration for the DA layer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaConfig {
    /// Which DA layer to use. `"inline"` disables offload (default).
    #[serde(default = "default_layer")]
    pub layer: String,
    /// Celestia light-node RPC endpoint (e.g. `http://localhost:26658`).
    #[serde(default)]
    pub endpoint: String,
    /// Celestia namespace (hex-encoded, 10 bytes). Blobs are scoped to this.
    #[serde(default = "default_namespace")]
    pub namespace: String,
    /// Bearer token for the Celestia RPC (from `celestia light auth`).
    #[serde(default)]
    pub auth_token: String,
}

fn default_layer() -> String {
    "inline".into()
}

fn default_namespace() -> String {
    "00000000000000617264".into() // "ark" zero-padded to 10 bytes
}

impl Default for DaConfig {
    fn default() -> Self {
        Self {
            layer: default_layer(),
            endpoint: String::new(),
            namespace: default_namespace(),
            auth_token: String::new(),
        }
    }
}

impl DaConfig {
    /// Whether DA offload is enabled (non-inline layer with a configured endpoint).
    pub fn is_enabled(&self) -> bool {
        self.layer != "inline" && !self.endpoint.is_empty()
    }

    /// Parse the configured layer string into a [`DaLayer`].
    pub fn da_layer(&self) -> DaLayer {
        match self.layer.as_str() {
            "celestia" => DaLayer::Celestia,
            "eigenda" => DaLayer::EigenDa,
            _ => DaLayer::Inline,
        }
    }
}

/// Async DA client. Submits and retrieves blobs from the configured
/// DA layer. Currently supports Celestia; EigenDA is a future extension.
#[derive(Clone)]
pub struct DaClient {
    config: DaConfig,
    http: reqwest::Client,
}

impl DaClient {
    /// Create a new DA client. Returns `None` if DA is not enabled.
    pub fn new(config: DaConfig) -> Option<Self> {
        if !config.is_enabled() {
            return None;
        }
        Some(Self {
            config,
            http: reqwest::Client::new(),
        })
    }

    /// Post a block body to the DA layer. Returns a [`DaReference`]
    /// that can be stored on L1 for later retrieval.
    pub async fn submit_block(&self, height: u64, block_bytes: &[u8]) -> Result<DaReference> {
        match self.config.da_layer() {
            DaLayer::Celestia => self.celestia_submit(height, block_bytes).await,
            DaLayer::EigenDa => Err(ChainError::Da("EigenDA not yet implemented".into())),
            DaLayer::Inline => Err(ChainError::Da("inline DA does not submit".into())),
        }
    }

    /// Retrieve a block body from the DA layer using a stored reference.
    pub async fn get_block(&self, da_ref: &DaReference) -> Result<Vec<u8>> {
        match da_ref.layer {
            DaLayer::Celestia => self.celestia_get(da_ref).await,
            DaLayer::EigenDa => Err(ChainError::Da("EigenDA not yet implemented".into())),
            DaLayer::Inline => Err(ChainError::Da("inline DA does not retrieve".into())),
        }
    }

    async fn celestia_submit(&self, height: u64, data: &[u8]) -> Result<DaReference> {
        use base64::Engine;

        let blob_b64 = base64::engine::general_purpose::STANDARD.encode(data);
        let ns_hex = &self.config.namespace;

        let body = serde_json::json!({
            "id": 1,
            "jsonrpc": "2.0",
            "method": "blob.Submit",
            "params": [
                [{
                    "namespace": ns_hex,
                    "data": blob_b64,
                    "share_version": 0,
                }],
                0.002  // gas price
            ]
        });

        let resp = self
            .http
            .post(&self.config.endpoint)
            .header("Content-Type", "application/json")
            .bearer_auth(&self.config.auth_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| ChainError::Da(format!("celestia submit: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ChainError::Da(format!("celestia response body: {e}")))?;

        if !status.is_success() {
            return Err(ChainError::Da(format!(
                "celestia submit HTTP {status}: {text}"
            )));
        }

        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| ChainError::Da(format!("celestia response parse: {e}")))?;

        let da_height = parsed["result"].as_u64().unwrap_or(0);

        let commitment = arknet_crypto::hash::sha256(data);

        debug!(
            height,
            da_height,
            bytes = data.len(),
            "block posted to celestia"
        );

        Ok(DaReference {
            layer: DaLayer::Celestia,
            commitment: commitment.0,
            height: da_height,
        })
    }

    async fn celestia_get(&self, da_ref: &DaReference) -> Result<Vec<u8>> {
        use base64::Engine;

        let ns_hex = &self.config.namespace;
        let commitment_b64 = base64::engine::general_purpose::STANDARD.encode(da_ref.commitment);

        let body = serde_json::json!({
            "id": 1,
            "jsonrpc": "2.0",
            "method": "blob.Get",
            "params": [
                da_ref.height,
                ns_hex,
                commitment_b64,
            ]
        });

        let resp = self
            .http
            .post(&self.config.endpoint)
            .header("Content-Type", "application/json")
            .bearer_auth(&self.config.auth_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| ChainError::Da(format!("celestia get: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ChainError::Da(format!("celestia get body: {e}")))?;

        if !status.is_success() {
            return Err(ChainError::Da(format!(
                "celestia get HTTP {status}: {text}"
            )));
        }

        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| ChainError::Da(format!("celestia get parse: {e}")))?;

        let blob_b64 = parsed["result"]["data"]
            .as_str()
            .ok_or_else(|| ChainError::Da("celestia get: missing result.data".into()))?;

        let data = base64::engine::general_purpose::STANDARD
            .decode(blob_b64)
            .map_err(|e| ChainError::Da(format!("celestia get base64 decode: {e}")))?;

        let actual_hash = arknet_crypto::hash::sha256(&data);
        if actual_hash.0 != da_ref.commitment {
            return Err(ChainError::Da(format!(
                "celestia get: commitment mismatch (expected {}, got {})",
                hex::encode(da_ref.commitment),
                hex::encode(actual_hash.0),
            )));
        }

        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_inline() {
        let cfg = DaConfig::default();
        assert!(!cfg.is_enabled());
        assert_eq!(cfg.da_layer(), DaLayer::Inline);
    }

    #[test]
    fn celestia_config_is_enabled() {
        let cfg = DaConfig {
            layer: "celestia".into(),
            endpoint: "http://localhost:26658".into(),
            namespace: default_namespace(),
            auth_token: "test-token".into(),
        };
        assert!(cfg.is_enabled());
        assert_eq!(cfg.da_layer(), DaLayer::Celestia);
    }

    #[test]
    fn celestia_without_endpoint_is_disabled() {
        let cfg = DaConfig {
            layer: "celestia".into(),
            endpoint: String::new(),
            ..Default::default()
        };
        assert!(!cfg.is_enabled());
    }
}
