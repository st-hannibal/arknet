//! libp2p `/arknet/inference/1` request/response protocol.
//!
//! Wire shape:
//!
//! - **Request**  — borsh-encoded `InferenceJobRequest`.
//! - **Response** — borsh-encoded `InferenceResponse` (see below),
//!   which carries the whole token stream for a completed job.
//!
//! # Phase-1 streaming model
//!
//! A libp2p `request_response` exchange is one request → one
//! response. We batch the full event stream into a single response
//! struct rather than chunking over multiple messages because:
//!
//! - Max output is bounded by `max_tokens` (capped well under the
//!   protocol's `MAX_SIGNED_TX_BYTES` even at Q4 70B).
//! - True per-token streaming needs an ordered-substream primitive
//!   that `request_response` doesn't give us; doing it properly is a
//!   `libp2p::core::upgrade` custom protocol — Phase-2 scope when
//!   first-token latency becomes the bottleneck.
//!
//! The shape of [`InferenceResponse`] is forward-compatible: later
//! phases can grow a "partial / continue" variant and swap in the
//! real streaming behaviour without breaking wire format.

use std::io;

use async_trait::async_trait;
use borsh::{BorshDeserialize, BorshSerialize};
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use libp2p::request_response::Codec;
use libp2p::StreamProtocol;
use serde::{Deserialize, Serialize};

/// Wire-level `StreamProtocol` string.
pub const INFERENCE_PROTOCOL: &str = "/arknet/inference/1";

/// Hard cap on a single request byte length (matches
/// `MAX_SIGNED_TX_BYTES` — a request is bounded by the same frame cap
/// as any signed transaction).
pub const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

/// Hard cap on a single response byte length (4 MiB — enough for a
/// 4K-token Q4 output + framing, well under libp2p's mux limits).
pub const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

/// Top-level response body.
///
/// Invariants (verified by Phase-1 receiver):
/// - Exactly one terminal event (`Stop` or `Error`) must appear, and
///   it must be the last event in `events`.
/// - `events` must not exceed [`MAX_RESPONSE_BYTES`] after borsh.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct InferenceResponse {
    /// Borsh-encoded `InferenceJobEvent`s (terminal event last).
    /// Stored as raw bytes rather than the typed enum to keep this
    /// crate free of a dependency on `arknet-compute` — the
    /// dispatcher on either side decodes/encodes the events it holds.
    pub events: Vec<Vec<u8>>,
}

impl InferenceResponse {
    /// Build a response from already-encoded event bytes.
    pub fn new(events: Vec<Vec<u8>>) -> Self {
        Self { events }
    }

    /// Number of framed events.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// `true` if the response has no events — only possible when the
    /// sender bailed immediately. The receiver should treat an empty
    /// response as `RouterError::Dispatch("empty response")`.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// Borsh + length-prefix codec for the `/arknet/inference/1` protocol.
///
/// Message shape on the wire: `u32 length || borsh bytes`. The length
/// is bounded on both sides so a misbehaving peer can't allocate
/// unbounded memory.
#[derive(Clone, Debug, Default)]
pub struct InferenceCodec;

/// Raw wire types the codec operates on. Kept as byte buffers to
/// keep the `arknet-network` crate free of a dep on `arknet-compute`.
pub type WireRequest = Vec<u8>;
/// The borsh-encoded [`InferenceResponse`].
pub type WireResponse = Vec<u8>;

#[async_trait]
impl Codec for InferenceCodec {
    type Protocol = StreamProtocol;
    type Request = WireRequest;
    type Response = WireResponse;

    async fn read_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_framed(io, MAX_REQUEST_BYTES).await
    }

    async fn read_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_framed(io, MAX_RESPONSE_BYTES).await
    }

    async fn write_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_framed(io, &req, MAX_REQUEST_BYTES).await
    }

    async fn write_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
        resp: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_framed(io, &resp, MAX_RESPONSE_BYTES).await
    }
}

async fn read_framed<T>(io: &mut T, max: u64) -> io::Result<Vec<u8>>
where
    T: AsyncRead + Unpin + Send,
{
    let mut len_buf = [0u8; 4];
    io.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as u64;
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds cap {max}"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    io.read_exact(&mut body).await?;
    Ok(body)
}

async fn write_framed<T>(io: &mut T, body: &[u8], max: u64) -> io::Result<()>
where
    T: AsyncWrite + Unpin + Send,
{
    if body.len() as u64 > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {} exceeds cap {max}", body.len()),
        ));
    }
    let len = (body.len() as u32).to_be_bytes();
    io.write_all(&len).await?;
    io.write_all(body).await?;
    io.flush().await?;
    Ok(())
}

/// The `request_response` behaviour for the inference protocol —
/// callers plug this into the arknet composed `NetworkBehaviour`.
///
/// The behaviour is re-exported as-is; the caller maps
/// `libp2p::request_response::Event<WireRequest, WireResponse>` into
/// their own event type.
pub type InferenceBehaviour = libp2p::request_response::Behaviour<InferenceCodec>;

/// Build a [`InferenceBehaviour`] with the default cbor/borsh config.
pub fn build_inference_behaviour() -> InferenceBehaviour {
    use libp2p::request_response::{Behaviour, Config, ProtocolSupport};
    Behaviour::new(
        [(
            StreamProtocol::new(INFERENCE_PROTOCOL),
            ProtocolSupport::Full,
        )],
        Config::default().with_request_timeout(std::time::Duration::from_secs(300)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::io::Cursor;

    #[tokio::test]
    async fn codec_roundtrip_request() {
        let mut codec = InferenceCodec;
        let payload = vec![1, 2, 3, 4, 5];

        let mut buf: Vec<u8> = Vec::new();
        let proto = StreamProtocol::new(INFERENCE_PROTOCOL);
        codec
            .write_request(&proto, &mut buf, payload.clone())
            .await
            .unwrap();

        let mut reader = Cursor::new(buf);
        let back = codec.read_request(&proto, &mut reader).await.unwrap();
        assert_eq!(payload, back);
    }

    #[tokio::test]
    async fn codec_roundtrip_response() {
        let mut codec = InferenceCodec;
        let payload = vec![9; 1024];

        let mut buf: Vec<u8> = Vec::new();
        let proto = StreamProtocol::new(INFERENCE_PROTOCOL);
        codec
            .write_response(&proto, &mut buf, payload.clone())
            .await
            .unwrap();

        let mut reader = Cursor::new(buf);
        let back = codec.read_response(&proto, &mut reader).await.unwrap();
        assert_eq!(payload, back);
    }

    #[tokio::test]
    async fn codec_rejects_oversize_write_request() {
        let mut codec = InferenceCodec;
        let huge = vec![0u8; (MAX_REQUEST_BYTES + 1) as usize];
        let mut buf = Vec::new();
        let proto = StreamProtocol::new(INFERENCE_PROTOCOL);
        let err = codec
            .write_request(&proto, &mut buf, huge)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn codec_rejects_oversize_read_request() {
        let mut codec = InferenceCodec;
        let mut framed = Vec::new();
        framed.extend_from_slice(&(MAX_REQUEST_BYTES as u32 + 1).to_be_bytes());
        // no body needed — length check fires first
        let mut reader = Cursor::new(framed);
        let proto = StreamProtocol::new(INFERENCE_PROTOCOL);
        let err = codec.read_request(&proto, &mut reader).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn response_borsh_roundtrip() {
        let r = InferenceResponse::new(vec![vec![1, 2, 3], vec![4, 5, 6]]);
        let bytes = borsh::to_vec(&r).unwrap();
        let back: InferenceResponse = borsh::from_slice(&bytes).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn protocol_name_is_versioned() {
        assert!(INFERENCE_PROTOCOL.starts_with("/arknet/inference/"));
        assert!(INFERENCE_PROTOCOL.ends_with("/1"));
    }
}
