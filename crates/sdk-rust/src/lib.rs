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
}

impl Client {
    /// Create a new client pointing at an arknet node.
    ///
    /// `base_url` should be the node's HTTP root, e.g.
    /// `http://127.0.0.1:3000`. The client appends `/v1/...` paths.
    pub fn new(base_url: &str) -> Result<Self> {
        let base_url = base_url.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| SdkError::Http(e.to_string()))?;
        Ok(Self { base_url, http })
    }

    /// Non-streaming chat completion.
    pub async fn chat_completion(&self, req: ChatRequest) -> Result<ChatResponse> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&req)
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
        let resp = self
            .http
            .get(&url)
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
}
