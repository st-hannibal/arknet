//! Single-request inference session.
//!
//! A [`Session`] wraps one [`Context`], one [`Tokenizer`] view, and
//! one [`Sampler`]. Given a prompt and an event sink, it drives the
//! decode loop to completion. The loop is synchronous — wrap the call
//! in `tokio::task::spawn_blocking` if you need async integration
//! (the engine facade does exactly that).
//!
//! # Determinism
//!
//! When the owning config is [`InferenceMode::Deterministic`] the
//! session is byte-identical across runs: same prompt → same token
//! ids → same decoded text. This is the contract that underpins
//! on-chain verification.

use crate::config::InferenceMode;
use crate::context::Context;
use crate::errors::{InferenceError, Result};
use crate::events::{InferenceEvent, StopReason, TokenEvent};
use crate::sampling::Sampler;
use crate::sys;
use crate::tokenizer::{PieceBuffer, Token, Tokenizer};

/// Per-request options.
#[derive(Clone, Debug)]
pub struct SessionRequest {
    /// Prompt text.
    pub prompt: String,
    /// Maximum new tokens to generate after the prompt.
    pub max_tokens: u32,
    /// Mode gate. Deterministic forces greedy + single-thread.
    pub mode: InferenceMode,
    /// Stop strings — generation ends when any appears in the
    /// decoded output.
    pub stop: Vec<String>,
}

/// Simple synchronous sink for events produced during generation.
///
/// Implementors can buffer into a `Vec` (for tests) or forward into
/// a `tokio::sync::mpsc` channel (for streaming). The function
/// returns `false` to signal cancellation; the session returns
/// [`StopReason::Cancelled`] on the next event boundary.
pub trait EventSink {
    /// Called once per event. Returning `false` cancels generation.
    fn accept(&mut self, event: InferenceEvent) -> bool;
}

impl EventSink for Vec<InferenceEvent> {
    fn accept(&mut self, event: InferenceEvent) -> bool {
        self.push(event);
        true
    }
}

/// Outcome of [`Session::run`].
#[derive(Clone, Debug)]
pub struct SessionOutcome {
    /// All generated token ids (excluding the prompt).
    pub generated_tokens: Vec<Token>,
    /// Concatenated decoded text (excluding the prompt).
    pub text: String,
    /// Why generation ended.
    pub reason: StopReason,
}

/// Single-use decode driver.
pub struct Session<'model, 'ctx> {
    ctx: &'ctx mut Context<'model>,
    tokenizer: Tokenizer<'model>,
    sampler: Sampler,
}

impl<'model, 'ctx> Session<'model, 'ctx> {
    /// Build a new session. The caller owns `ctx` and `sampler`; we
    /// borrow a tokenizer view from the model.
    pub fn new(
        ctx: &'ctx mut Context<'model>,
        tokenizer: Tokenizer<'model>,
        sampler: Sampler,
    ) -> Self {
        Self {
            ctx,
            tokenizer,
            sampler,
        }
    }

    /// Drive the full request to completion, pushing events into `sink`.
    pub fn run(
        mut self,
        req: &SessionRequest,
        sink: &mut impl EventSink,
    ) -> Result<SessionOutcome> {
        let add_bos = true; // typical for a fresh context
        let prompt_tokens = self.tokenizer.encode(&req.prompt, add_bos)?;

        if prompt_tokens.is_empty() {
            return Err(InferenceError::Tokenize(
                "prompt tokenized to zero tokens".into(),
            ));
        }
        if prompt_tokens.len() as u32 + req.max_tokens > self.ctx.n_ctx() {
            return Err(InferenceError::Tokenize(format!(
                "prompt ({} tokens) + max_tokens ({}) exceed context ({})",
                prompt_tokens.len(),
                req.max_tokens,
                self.ctx.n_ctx()
            )));
        }

        // ── Ingest prompt ──
        self.ingest_prompt(&prompt_tokens)?;

        // ── Generate ──
        let mut generated: Vec<Token> = Vec::with_capacity(req.max_tokens as usize);
        let mut piece_buf = PieceBuffer::new();
        let mut full_text = String::new();
        let mut reason = StopReason::MaxTokens;

        for index in 0..req.max_tokens {
            let token = self.sampler.sample(self.ctx);
            self.sampler.accept(token);

            // EOS check — llama.cpp's eos() returns -1 if the model has no EOS.
            let eos = self.tokenizer.eos();
            if eos >= 0 && token == eos {
                reason = StopReason::EndOfStream;
                break;
            }

            let piece_bytes = self.tokenizer.token_to_piece(token);
            let fragment = piece_buf.feed(&piece_bytes);
            full_text.push_str(&fragment);
            generated.push(token);

            // Stop-string check — done in accumulated text so tokens that
            // split a stop string are still caught.
            let matched_stop = req.stop.iter().find(|s| full_text.ends_with(s.as_str()));
            if let Some(stop_str) = matched_stop {
                reason = StopReason::StopString(stop_str.clone());
                if !sink.accept(InferenceEvent::Token(TokenEvent {
                    index,
                    token_id: token,
                    text: fragment,
                })) {
                    reason = StopReason::Cancelled;
                }
                break;
            }

            if !sink.accept(InferenceEvent::Token(TokenEvent {
                index,
                token_id: token,
                text: fragment,
            })) {
                reason = StopReason::Cancelled;
                break;
            }

            // Feed this token back for the next step.
            self.decode_single(token)?;
        }

        // Flush any trailing partial codepoint.
        let tail = piece_buf.flush();
        if !tail.is_empty() {
            full_text.push_str(&tail);
        }

        sink.accept(InferenceEvent::Stop(reason.clone()));

        Ok(SessionOutcome {
            generated_tokens: generated,
            text: full_text,
            reason,
        })
    }

    /// Feed the full prompt through `llama_decode` in one batch.
    fn ingest_prompt(&mut self, tokens: &[Token]) -> Result<()> {
        // SAFETY: `llama_batch_get_one` is a helper that constructs a
        // batch view over an existing token slice. The batch borrows
        // `tokens`; llama.cpp reads but does not retain the pointer.
        let batch =
            unsafe { sys::llama_batch_get_one(tokens.as_ptr() as *mut _, tokens.len() as i32) };
        // SAFETY: `self.ctx` is a live context.
        let rc = unsafe { sys::llama_decode(self.ctx.as_ptr(), batch) };
        if rc != 0 {
            return Err(InferenceError::Decode { code: rc });
        }
        Ok(())
    }

    /// Feed a single generated token back into the KV cache.
    fn decode_single(&mut self, token: Token) -> Result<()> {
        // SAFETY: `one` is a local whose address is valid until the
        // function returns; `llama_batch_get_one` does not retain.
        let mut one = [token];
        let batch = unsafe { sys::llama_batch_get_one(one.as_mut_ptr(), one.len() as i32) };
        // SAFETY: `self.ctx` is live.
        let rc = unsafe { sys::llama_decode(self.ctx.as_ptr(), batch) };
        if rc != 0 {
            return Err(InferenceError::Decode { code: rc });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec_eventsink_accepts_tokens() {
        let mut sink: Vec<InferenceEvent> = Vec::new();
        let kept = sink.accept(InferenceEvent::Token(TokenEvent {
            index: 0,
            token_id: 42,
            text: "hi".into(),
        }));
        assert!(kept);
        assert_eq!(sink.len(), 1);
    }
}
