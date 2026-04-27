//! Tokenizer backed by the model's vocabulary.
//!
//! Always uses llama.cpp's internal tokenizer — one source of truth,
//! guaranteed byte-identical to what the model was trained with.
//!
//! # Conventions
//!
//! - [`Tokenizer::encode`] uses `add_special = true` by default so the
//!   BOS token (and any model-specific prefixes) is prepended. Override
//!   to `false` when continuing an existing sequence.
//! - [`Tokenizer::decode`] strips special tokens by default so output
//!   text is what a user would see, not the raw training stream.

use std::os::raw::c_char;

use crate::errors::{InferenceError, Result};
use crate::model::Model;
use crate::sys;

/// Integer token id. Matches llama.cpp's `llama_token`.
pub type Token = i32;

/// Tokenizer tied to a specific loaded [`Model`].
///
/// Holds a raw vocab pointer whose lifetime is bound to the model.
pub struct Tokenizer<'model> {
    vocab: *const sys::llama_vocab,
    _model: std::marker::PhantomData<&'model Model>,
}

// SAFETY: vocab is read-only in llama.cpp and safe to call concurrently
// from multiple threads.
unsafe impl Send for Tokenizer<'_> {}
unsafe impl Sync for Tokenizer<'_> {}

impl<'model> Tokenizer<'model> {
    /// Build a tokenizer over `model`. Cheap — just extracts the vocab
    /// pointer.
    pub fn new(model: &'model Model) -> Self {
        // SAFETY: `llama_model_get_vocab` is a pure getter returning a
        // pointer into the model; it remains valid for the lifetime
        // of the borrowed `Model`.
        let vocab = unsafe { sys::llama_model_get_vocab(model.as_ptr()) };
        Self {
            vocab,
            _model: std::marker::PhantomData,
        }
    }

    /// BOS token id (beginning of sentence), if the model defines one.
    pub fn bos(&self) -> Token {
        // SAFETY: pure getter over a non-null vocab pointer.
        unsafe { sys::llama_vocab_bos(self.vocab) }
    }

    /// EOS token id (end of sentence), if the model defines one.
    pub fn eos(&self) -> Token {
        // SAFETY: pure getter over a non-null vocab pointer.
        unsafe { sys::llama_vocab_eos(self.vocab) }
    }

    /// Vocabulary size (number of distinct token ids).
    pub fn vocab_size(&self) -> i32 {
        // SAFETY: pure getter.
        unsafe { sys::llama_vocab_n_tokens(self.vocab) }
    }

    /// Encode `text` into a sequence of token ids.
    ///
    /// When `add_special` is true, the model's BOS token is prepended.
    pub fn encode(&self, text: &str, add_special: bool) -> Result<Vec<Token>> {
        if text.is_empty() && !add_special {
            return Ok(Vec::new());
        }

        // First pass: ask llama.cpp how many tokens will be produced.
        // It returns a negative number when the buffer is too small —
        // the absolute value is the required length.
        let text_bytes = text.as_bytes();
        let mut guess_cap = text_bytes.len() + 16;
        let mut tokens = vec![0 as Token; guess_cap];

        loop {
            // SAFETY: all four pointers are valid for the duration of
            // the call; `tokens` is owned by us, `text_bytes` by the
            // caller. llama.cpp does not retain them.
            let n = unsafe {
                sys::llama_tokenize(
                    self.vocab,
                    text_bytes.as_ptr() as *const c_char,
                    text_bytes.len() as i32,
                    tokens.as_mut_ptr(),
                    tokens.len() as i32,
                    add_special,
                    true, // parse_special
                )
            };
            if n >= 0 {
                tokens.truncate(n as usize);
                return Ok(tokens);
            }
            // Buffer too small: llama.cpp returns -needed. Grow.
            let needed = n.unsigned_abs() as usize;
            if needed <= guess_cap {
                return Err(InferenceError::Tokenize(format!(
                    "tokenize returned -{needed} with buffer {guess_cap}; cannot grow"
                )));
            }
            guess_cap = needed;
            tokens.resize(guess_cap, 0);
        }
    }

    /// Render a single token id as its UTF-8 text fragment.
    ///
    /// Individual tokens may be partial codepoints (BPE splits UTF-8
    /// mid-sequence for some languages); callers that need full
    /// codepoints should buffer via [`PieceBuffer`].
    pub fn token_to_piece(&self, token: Token) -> Vec<u8> {
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is stack-owned for the call; llama.cpp writes
        // up to `length` bytes without NUL terminating.
        let written = unsafe {
            sys::llama_token_to_piece(
                self.vocab,
                token,
                buf.as_mut_ptr() as *mut c_char,
                buf.len() as i32,
                0,     // lstrip
                false, // special
            )
        };
        if written <= 0 {
            return Vec::new();
        }
        buf[..written as usize].to_vec()
    }

    /// Decode a token sequence back into text. Special tokens are
    /// stripped by default.
    pub fn decode(&self, tokens: &[Token]) -> Result<String> {
        if tokens.is_empty() {
            return Ok(String::new());
        }
        // Rough upper bound — llama.cpp will report if too small.
        let mut cap = tokens.len() * 8 + 32;
        let mut out = vec![0u8; cap];

        loop {
            // SAFETY: all pointers are owned and live for the call.
            let n = unsafe {
                sys::llama_detokenize(
                    self.vocab,
                    tokens.as_ptr(),
                    tokens.len() as i32,
                    out.as_mut_ptr() as *mut c_char,
                    out.len() as i32,
                    true,  // remove_special
                    false, // unparse_special
                )
            };
            if n >= 0 {
                out.truncate(n as usize);
                return String::from_utf8(out)
                    .map_err(|e| InferenceError::Tokenize(format!("invalid utf-8: {e}")));
            }
            let needed = n.unsigned_abs() as usize;
            if needed <= cap {
                return Err(InferenceError::Tokenize(format!(
                    "detokenize returned -{needed} with buffer {cap}; cannot grow"
                )));
            }
            cap = needed;
            out.resize(cap, 0);
        }
    }
}

/// Streaming helper that buffers incomplete UTF-8 across tokens.
///
/// llama.cpp emits partial codepoints for multi-byte characters. A
/// naive per-token decode produces invalid UTF-8; this buffer holds
/// pending bytes until a full codepoint arrives.
#[derive(Default)]
pub struct PieceBuffer {
    pending: Vec<u8>,
}

impl PieceBuffer {
    /// Create an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes from [`Tokenizer::token_to_piece`] and return any
    /// fully-decoded text. Partial bytes stay buffered.
    pub fn feed(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        // Find the longest valid UTF-8 prefix.
        match std::str::from_utf8(&self.pending) {
            Ok(s) => {
                let out = s.to_owned();
                self.pending.clear();
                out
            }
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                let out = std::str::from_utf8(&self.pending[..valid_up_to])
                    .expect("valid_up_to guarantees this slice decodes")
                    .to_owned();
                self.pending.drain(..valid_up_to);
                out
            }
        }
    }

    /// Flush any remaining bytes. Replaces invalid sequences with U+FFFD.
    pub fn flush(&mut self) -> String {
        let out = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piece_buffer_passes_through_ascii() {
        let mut buf = PieceBuffer::new();
        assert_eq!(buf.feed(b"hello "), "hello ");
        assert_eq!(buf.feed(b"world"), "world");
    }

    #[test]
    fn piece_buffer_assembles_split_multibyte() {
        // "é" is 0xC3 0xA9. Feed it one byte at a time.
        let mut buf = PieceBuffer::new();
        assert_eq!(buf.feed(&[0xC3]), "");
        assert_eq!(buf.feed(&[0xA9]), "é");
    }

    #[test]
    fn piece_buffer_flushes_invalid_tail() {
        let mut buf = PieceBuffer::new();
        buf.feed(&[0xC3]); // leading byte of a 2-byte seq, no follow-up
        let flushed = buf.flush();
        assert!(flushed.contains('\u{FFFD}'));
    }
}
