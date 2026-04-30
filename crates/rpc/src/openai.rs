//! OpenAI-compatible wire types and axum handlers.
//!
//! Implements the subset of the OpenAI API that arknet exposes:
//! - `POST /v1/chat/completions` — streaming and non-streaming
//! - `GET  /v1/models`           — list registered models
//!
//! These types mirror the OpenAI JSON schema so any client library
//! (Python `openai`, TypeScript `openai`, curl) works out of the box
//! by pointing `base_url` at the arknet node.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ─── Request types ──────────────────────────────────────────────────

/// OpenAI `POST /v1/chat/completions` request body.
#[derive(Clone, Debug, Deserialize)]
pub struct ChatCompletionRequest {
    /// Model identifier (must match an on-chain registered model).
    pub model: String,
    /// Conversation messages.
    pub messages: Vec<ChatMessage>,
    /// Maximum tokens to generate.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Sampling temperature (0.0 – 2.0).
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    /// Nucleus sampling.
    #[serde(default = "default_top_p")]
    pub top_p: f64,
    /// Whether to stream the response.
    #[serde(default)]
    pub stream: bool,
    /// Stop sequences.
    #[serde(default)]
    pub stop: Option<StopCondition>,
    /// Number of completions (arknet only supports 1).
    #[serde(default = "default_n")]
    pub n: u32,
}

fn default_max_tokens() -> u32 {
    256
}
fn default_temperature() -> f64 {
    1.0
}
fn default_top_p() -> f64 {
    1.0
}
fn default_n() -> u32 {
    1
}

/// A chat message (system / user / assistant).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Role: "system", "user", or "assistant".
    pub role: String,
    /// Message content.
    pub content: String,
}

/// Stop condition — a single string or an array of strings.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum StopCondition {
    /// Single stop string.
    Single(String),
    /// Array of stop strings.
    Multiple(Vec<String>),
}

impl StopCondition {
    /// Flatten into a `Vec<String>`.
    pub fn into_vec(self) -> Vec<String> {
        match self {
            StopCondition::Single(s) => vec![s],
            StopCondition::Multiple(v) => v,
        }
    }
}

// ─── Non-streaming response ─────────────────────────────────────────

/// OpenAI `ChatCompletion` response (non-streaming).
#[derive(Clone, Debug, Serialize)]
pub struct ChatCompletionResponse {
    /// Always "chat.completion".
    pub id: String,
    /// Always "chat.completion".
    pub object: &'static str,
    /// Unix timestamp.
    pub created: u64,
    /// Model used.
    pub model: String,
    /// Generated choices.
    pub choices: Vec<Choice>,
    /// Token usage.
    pub usage: Usage,
}

/// A single completion choice.
#[derive(Clone, Debug, Serialize)]
pub struct Choice {
    /// Index in the choices array.
    pub index: u32,
    /// Generated message.
    pub message: ChatMessage,
    /// Why generation stopped ("stop", "length", "content_filter").
    pub finish_reason: Option<String>,
}

/// Token counts.
#[derive(Clone, Debug, Serialize)]
pub struct Usage {
    /// Input tokens.
    pub prompt_tokens: u32,
    /// Output tokens.
    pub completion_tokens: u32,
    /// Total.
    pub total_tokens: u32,
}

// ─── Streaming response (SSE) ───────────────────────────────────────

/// A single streamed chunk (`data:` line in the SSE stream).
#[derive(Clone, Debug, Serialize)]
pub struct ChatCompletionChunk {
    /// Same id across all chunks of one request.
    pub id: String,
    /// Always "chat.completion.chunk".
    pub object: &'static str,
    /// Unix timestamp.
    pub created: u64,
    /// Model used.
    pub model: String,
    /// Chunk choices (one per `n`; arknet always sends one).
    pub choices: Vec<ChunkChoice>,
}

/// A choice within a streamed chunk.
#[derive(Clone, Debug, Serialize)]
pub struct ChunkChoice {
    /// Choice index.
    pub index: u32,
    /// Incremental content delta.
    pub delta: Delta,
    /// Set on the final chunk.
    pub finish_reason: Option<String>,
}

/// Delta content in a streaming chunk.
#[derive(Clone, Debug, Serialize)]
pub struct Delta {
    /// Role (only present on the first chunk).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Token text (present on every chunk except the final one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

// ─── /v1/models response ────────────────────────────────────────────

/// OpenAI `/v1/models` list response.
#[derive(Clone, Debug, Serialize)]
pub struct ModelsResponse {
    /// Always "list".
    pub object: &'static str,
    /// Model entries.
    pub data: Vec<ModelEntry>,
}

/// A single model in the models list.
#[derive(Clone, Debug, Serialize)]
pub struct ModelEntry {
    /// Model identifier.
    pub id: String,
    /// Always "model".
    pub object: &'static str,
    /// Unix timestamp of registration.
    pub created: u64,
    /// Owner (registrar address hex).
    pub owned_by: String,
}

// ─── OpenAI error shape ─────────────────────────────────────────────

/// OpenAI-shaped error response body.
#[derive(Clone, Debug, Serialize)]
pub struct OpenAiError {
    /// Error wrapper.
    pub error: OpenAiErrorInner,
}

/// Inner error payload.
#[derive(Clone, Debug, Serialize)]
pub struct OpenAiErrorInner {
    /// Human-readable message.
    pub message: String,
    /// Error type (e.g. "invalid_request_error").
    #[serde(rename = "type")]
    pub error_type: String,
    /// Offending parameter (if any).
    pub param: Option<String>,
    /// Machine-readable code (if any).
    pub code: Option<String>,
}

// ─── Helpers ────────────────────────────────────────────────────────

/// Current unix timestamp.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Generate a request id: `chatcmpl-<hex>`.
pub fn gen_request_id() -> String {
    let ts = unix_now();
    format!("chatcmpl-{ts:016x}")
}

/// Build an OpenAI error response.
pub fn error_response(
    status: axum::http::StatusCode,
    message: impl Into<String>,
    error_type: &str,
) -> (axum::http::StatusCode, axum::Json<OpenAiError>) {
    (
        status,
        axum::Json(OpenAiError {
            error: OpenAiErrorInner {
                message: message.into(),
                error_type: error_type.to_string(),
                param: None,
                code: None,
            },
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_request_deserializes_minimal() {
        let json = r#"{
            "model": "meta-llama/Llama-3-8B",
            "messages": [{"role": "user", "content": "hello"}]
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.model, "meta-llama/Llama-3-8B");
        assert_eq!(req.messages.len(), 1);
        assert!(!req.stream);
        assert_eq!(req.max_tokens, 256);
        assert_eq!(req.n, 1);
    }

    #[test]
    fn chat_request_deserializes_streaming() {
        let json = r#"{
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
            "max_tokens": 100,
            "stop": ["<|end|>"]
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).unwrap();
        assert!(req.stream);
        assert_eq!(req.max_tokens, 100);
        let stop = req.stop.unwrap().into_vec();
        assert_eq!(stop, vec!["<|end|>"]);
    }

    #[test]
    fn stop_condition_single_string() {
        let json = r#""stop""#;
        let sc: StopCondition = serde_json::from_str(json).unwrap();
        assert_eq!(sc.into_vec(), vec!["stop"]);
    }

    #[test]
    fn stop_condition_array() {
        let json = r#"["a", "b"]"#;
        let sc: StopCondition = serde_json::from_str(json).unwrap();
        assert_eq!(sc.into_vec(), vec!["a", "b"]);
    }

    #[test]
    fn response_serializes_correctly() {
        let resp = ChatCompletionResponse {
            id: gen_request_id(),
            object: "chat.completion",
            created: unix_now(),
            model: "test".into(),
            choices: vec![Choice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".into(),
                    content: "hello".into(),
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 5,
                completion_tokens: 1,
                total_tokens: 6,
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("chat.completion"));
        assert!(json.contains("hello"));
    }

    #[test]
    fn chunk_serializes_with_delta() {
        let chunk = ChatCompletionChunk {
            id: "chatcmpl-test".into(),
            object: "chat.completion.chunk",
            created: 0,
            model: "test".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content: Some("tok".into()),
                },
                finish_reason: None,
            }],
        };
        let json = serde_json::to_string(&chunk).unwrap();
        assert!(json.contains("tok"));
        assert!(!json.contains("role"));
    }

    #[test]
    fn chunk_first_has_role() {
        let chunk = ChatCompletionChunk {
            id: "chatcmpl-test".into(),
            object: "chat.completion.chunk",
            created: 0,
            model: "test".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    role: Some("assistant".into()),
                    content: None,
                },
                finish_reason: None,
            }],
        };
        let json = serde_json::to_string(&chunk).unwrap();
        assert!(json.contains("assistant"));
    }

    #[test]
    fn models_response_shape() {
        let resp = ModelsResponse {
            object: "list",
            data: vec![ModelEntry {
                id: "llama-3-8b".into(),
                object: "model",
                created: 1000,
                owned_by: "0xdead".into(),
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("llama-3-8b"));
        assert!(json.contains("\"object\":\"list\""));
    }

    #[test]
    fn error_shape() {
        let (status, body) = error_response(
            axum::http::StatusCode::NOT_FOUND,
            "model not found",
            "invalid_request_error",
        );
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        let json = serde_json::to_string(&body.0).unwrap();
        assert!(json.contains("model not found"));
        assert!(json.contains("invalid_request_error"));
    }
}
