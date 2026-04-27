//! Events produced by a running inference session.
//!
//! An inference stream is a sequence of [`InferenceEvent`] values
//! terminated by exactly one `Stop` variant (success or error).

use serde::{Deserialize, Serialize};

/// One event on the inference stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum InferenceEvent {
    /// A single generated token with its decoded text fragment.
    Token(TokenEvent),
    /// Final event — generation ended for the reason indicated.
    Stop(StopReason),
}

/// A single generated token.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenEvent {
    /// Zero-based token index in the generated sequence.
    pub index: u32,
    /// Raw token id as emitted by the model's vocab.
    pub token_id: i32,
    /// Decoded text fragment (UTF-8). Partial codepoints across tokens
    /// are buffered by the tokenizer so each `text` is always valid UTF-8.
    pub text: String,
}

/// Why generation ended.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum StopReason {
    /// Reached `max_tokens`.
    MaxTokens,
    /// End-of-stream token produced.
    EndOfStream,
    /// A caller-provided stop string matched.
    StopString(String),
    /// Caller dropped the stream.
    Cancelled,
}
