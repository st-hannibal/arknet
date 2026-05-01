//! gRPC + REST + OpenAI-compatible API for arknet.
//!
//! This crate defines the **wire types** and **axum handlers** for the
//! OpenAI-compatible surface (`/v1/chat/completions`, `/v1/models`).
//! The actual inference dispatch lives in the node crate; these handlers
//! transform between the OpenAI JSON shape and the internal event stream.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod openai;
