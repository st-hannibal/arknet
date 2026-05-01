//! Public Rust SDK for arknet.
//!
//! Provides an async `Client` with OpenAI-compatible methods:
//! - `chat_completion` — non-streaming completions
//! - `chat_completion_stream` — streaming SSE completions
//! - `list_models` — query the on-chain model registry
//!
//! # Example
//!
//! ```rust,no_run
//! # async fn demo() -> arknet_sdk::Result<()> {
//! let client = arknet_sdk::Client::new("http://127.0.0.1:3000")?;
//! let resp = client.chat_completion(arknet_sdk::ChatRequest {
//!     model: "meta-llama/Llama-3-8B".into(),
//!     messages: vec![arknet_sdk::Message {
//!         role: "user".into(),
//!         content: "Hello!".into(),
//!     }],
//!     max_tokens: Some(64),
//!     ..Default::default()
//! }).await?;
//! println!("{}", resp.choices[0].message.content);
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod errors;

use serde::{Deserialize, Serialize};

pub use errors::{Result, SdkError};

/// arknet SDK client. Wraps an HTTP connection to a node's
/// OpenAI-compatible API surface.
pub struct Client {
    base_url: String,
    http: reqwest::Client,
    api_key: Option<String>,
}

impl Client {
    /// Create a new client pointing at an arknet node.
    ///
    /// `base_url` should be the node's HTTP root, e.g.
    /// `http://127.0.0.1:3000`. The client appends `/v1/...` paths.
    /// Create a new client pointing at an arknet node.
    ///
    /// Reads wallet address from `ARKNET_WALLET` env var if not
    /// provided explicitly via [`ConnectOptions`].
    pub fn new(base_url: &str) -> Result<Self> {
        let base_url = base_url.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| SdkError::Http(e.to_string()))?;
        let api_key = std::env::var("ARKNET_WALLET").ok();
        Ok(Self {
            base_url,
            http,
            api_key,
        })
    }

    /// Auto-discover a gateway from the on-chain registry.
    ///
    /// Contacts each seed URL's `/v1/gateways`, picks the best
    /// reachable gateway (HTTPS preferred), and returns a connected client.
    pub async fn connect(opts: ConnectOptions) -> Result<Self> {
        let seeds = if opts.seeds.is_empty() {
            vec!["https://api.arknet.arkengel.com".to_string()]
        } else {
            opts.seeds
        };
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| SdkError::Http(e.to_string()))?;

        for seed in &seeds {
            let url = format!("{}/v1/gateways", seed.trim_end_matches('/'));
            let resp = match http.get(&url).send().await {
                Ok(r) if r.status().is_success() => r,
                _ => continue,
            };
            let body: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(_) => continue,
            };
            let gateways = body["gateways"].as_array().cloned().unwrap_or_default();
            // Sort HTTPS first.
            let mut sorted = gateways;
            sorted.sort_by_key(|g| {
                if g["https"].as_bool() == Some(true) {
                    0
                } else {
                    1
                }
            });
            for gw in &sorted {
                let is_https = gw["https"].as_bool() == Some(true);
                if opts.require_https && !is_https {
                    continue;
                }
                if let Some(gw_url) = gw["url"].as_str() {
                    return Self::new(gw_url);
                }
            }
        }
        Err(SdkError::Http("no reachable gateway found".into()))
    }

    /// Non-streaming chat completion.
    pub async fn chat_completion(&self, req: ChatRequest) -> Result<ChatResponse> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let mut builder = self.http.post(&url).json(&req);
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| SdkError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(SdkError::Api { status, body });
        }

        resp.json::<ChatResponse>()
            .await
            .map_err(|e| SdkError::Http(e.to_string()))
    }

    /// List models from the on-chain registry.
    pub async fn list_models(&self) -> Result<ModelsResponse> {
        let url = format!("{}/v1/models", self.base_url);
        let mut builder = self.http.get(&url);
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| SdkError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(SdkError::Api { status, body });
        }

        resp.json::<ModelsResponse>()
            .await
            .map_err(|e| SdkError::Http(e.to_string()))
    }
}

// ─── Request / response types ───────────────────────────────────────

/// Options for [`Client::connect`] auto-discovery.
#[derive(Clone, Debug, Default)]
pub struct ConnectOptions {
    /// Seed URLs to discover gateways. Defaults to the arknet seed list.
    pub seeds: Vec<String>,
    /// Only connect to HTTPS gateways.
    pub require_https: bool,
}

/// Chat completion request.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ChatRequest {
    /// Model identifier.
    pub model: String,
    /// Conversation messages.
    pub messages: Vec<Message>,
    /// Maximum tokens to generate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Whether to stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Stop sequences.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    /// Route only to TEE-capable nodes (confidential inference).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefer_tee: Option<bool>,
    /// Route only through HTTPS gateways.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_https: Option<bool>,
}

/// A chat message.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Message {
    /// Role: "system", "user", or "assistant".
    pub role: String,
    /// Message content.
    pub content: String,
}

/// Chat completion response.
#[derive(Clone, Debug, Deserialize)]
pub struct ChatResponse {
    /// Request identifier.
    pub id: String,
    /// Completions.
    pub choices: Vec<ChatChoice>,
    /// Token usage.
    pub usage: Option<TokenUsage>,
}

/// A single chat choice.
#[derive(Clone, Debug, Deserialize)]
pub struct ChatChoice {
    /// Index.
    pub index: u32,
    /// Generated message.
    pub message: Message,
    /// Why generation stopped.
    pub finish_reason: Option<String>,
}

/// Token counts.
#[derive(Clone, Debug, Deserialize)]
pub struct TokenUsage {
    /// Input tokens.
    pub prompt_tokens: u32,
    /// Output tokens.
    pub completion_tokens: u32,
    /// Total.
    pub total_tokens: u32,
}

/// Models list response.
#[derive(Clone, Debug, Deserialize)]
pub struct ModelsResponse {
    /// Model entries.
    pub data: Vec<ModelInfo>,
}

/// A model entry.
#[derive(Clone, Debug, Deserialize)]
pub struct ModelInfo {
    /// Model identifier.
    pub id: String,
    /// Owner.
    pub owned_by: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_trims_trailing_slash() {
        let c = Client::new("http://localhost:3000/").unwrap();
        assert_eq!(c.base_url, "http://localhost:3000");
    }

    #[test]
    fn chat_request_serializes() {
        let req = ChatRequest {
            model: "test".into(),
            messages: vec![Message {
                role: "user".into(),
                content: "hi".into(),
            }],
            max_tokens: Some(10),
            ..Default::default()
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"model\":\"test\""));
        assert!(json.contains("\"max_tokens\":10"));
        assert!(!json.contains("stream"));
    }

    #[test]
    fn chat_response_deserializes() {
        let json = r#"{
            "id": "chatcmpl-test",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 1,
                "total_tokens": 6
            }
        }"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].message.content, "hello");
    }

    #[test]
    fn models_response_deserializes() {
        let json = r#"{
            "object": "list",
            "data": [
                {"id": "llama-3-8b", "object": "model", "created": 0, "owned_by": "user"}
            ]
        }"#;
        let resp: ModelsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.data.len(), 1);
        assert_eq!(resp.data[0].id, "llama-3-8b");
    }

    #[test]
    fn api_key_from_env() {
        std::env::set_var("ARKNET_WALLET", "ark1fromenv");
        let c = Client::new("http://localhost:1234").unwrap();
        assert_eq!(c.api_key.as_deref(), Some("ark1fromenv"));
        std::env::remove_var("ARKNET_WALLET");
    }

    #[test]
    fn prefer_tee_serialized_when_set() {
        let req = ChatRequest {
            model: "test".into(),
            messages: vec![],
            prefer_tee: Some(true),
            ..Default::default()
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"prefer_tee\":true"));
    }

    #[test]
    fn prefer_tee_omitted_when_none() {
        let req = ChatRequest {
            model: "test".into(),
            messages: vec![],
            ..Default::default()
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("prefer_tee"));
    }

    #[test]
    fn require_https_serialized_when_set() {
        let req = ChatRequest {
            model: "test".into(),
            messages: vec![],
            require_https: Some(true),
            ..Default::default()
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"require_https\":true"));
    }

    #[test]
    fn connect_options_defaults() {
        let opts = ConnectOptions::default();
        assert!(opts.seeds.is_empty());
        assert!(!opts.require_https);
    }
}
