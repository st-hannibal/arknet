//! Public Rust SDK for arknet.
//!
//! Provides an async `Client` with OpenAI-compatible methods:
//! - `chat_completion` — non-streaming completions
//! - `chat_completion_stream` — streaming SSE completions
//! - `list_models` — query the on-chain model registry
//! - `infer_p2p` — direct P2P inference via libp2p to a compute node
//!
//! # Wallet
//!
//! The [`wallet::Wallet`] type holds an Ed25519 keypair for signing
//! inference requests. Attach one to the client via [`Client::with_wallet`]
//! or [`ConnectOptions::wallet`] to enable signed/P2P operations.
//!
//! # Example
//!
//! ```rust,no_run
//! # async fn demo() -> arknet_sdk::Result<()> {
//! let wallet = arknet_sdk::wallet::Wallet::create();
//! let client = arknet_sdk::Client::new("http://127.0.0.1:3000")?
//!     .with_wallet(wallet);
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
pub mod p2p;
pub mod wallet;

use serde::{Deserialize, Serialize};

pub use errors::{Result, SdkError};

/// arknet SDK client. Wraps an HTTP connection to a node's
/// OpenAI-compatible API surface, with optional wallet for signed
/// requests and P2P direct connect.
pub struct Client {
    base_url: String,
    http: reqwest::Client,
    api_key: Option<String>,
    wallet: Option<wallet::Wallet>,
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
            wallet: None,
        })
    }

    /// Attach a [`wallet::Wallet`] to this client.
    ///
    /// Required for [`infer_p2p`](Self::infer_p2p) and signed inference
    /// requests. The wallet's address is used as the `Authorization`
    /// bearer token for HTTP requests when no `ARKNET_WALLET` env var
    /// is set.
    pub fn with_wallet(mut self, wallet: wallet::Wallet) -> Self {
        if self.api_key.is_none() {
            self.api_key = Some(self.wallet_address_hex(&wallet));
        }
        self.wallet = Some(wallet);
        self
    }

    /// Reference to the attached wallet, if any.
    pub fn wallet(&self) -> Option<&wallet::Wallet> {
        self.wallet.as_ref()
    }

    /// Hex-encode the wallet address (for use as bearer token).
    fn wallet_address_hex(&self, w: &wallet::Wallet) -> String {
        w.address().to_hex()
    }

    /// Auto-discover a gateway from the on-chain registry.
    ///
    /// Fetches the live seed list from `seeds.json` on the arknet
    /// website, then contacts each seed's `/v1/gateways`. If the
    /// seed list is unreachable, falls back to the hardcoded list.
    /// No code changes needed to add new seeds — just edit seeds.json.
    pub async fn connect(opts: ConnectOptions) -> Result<Self> {
        let seeds = if opts.seeds.is_empty() {
            fetch_seeds().await
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
                    let mut client = Self::new(gw_url)?;
                    if let Some(w) = opts.wallet {
                        client = client.with_wallet(w);
                    }
                    return Ok(client);
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

    /// Discover compute-node candidates for a model via the gateway,
    /// connect directly to one over P2P, send a signed
    /// [`InferenceJobRequest`](arknet_compute::wire::InferenceJobRequest),
    /// and return the raw borsh-encoded response bytes.
    ///
    /// # Flow
    ///
    /// 1. `GET {gateway}/v1/candidates/{model}` to discover peer multiaddrs.
    /// 2. Connect to the first reachable candidate via [`p2p::P2pClient`].
    /// 3. Build an `InferenceJobRequest`, sign it with the wallet, borsh-encode.
    /// 4. Send over the `/arknet/inference/1` protocol.
    /// 5. Return the response bytes (caller decodes as `InferenceResponse`).
    ///
    /// Requires a wallet to be attached.
    pub async fn infer_p2p(&self, req: P2pInferenceRequest) -> Result<Vec<u8>> {
        let w = self.wallet.as_ref().ok_or(SdkError::NoWallet)?;

        // 1. Discover candidates.
        let candidates = self.discover_candidates(&req.model).await?;
        if candidates.is_empty() {
            return Err(SdkError::P2p("no candidates returned by gateway".into()));
        }

        // 2. Build and sign the InferenceJobRequest.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let pubkey = w.public_key();
        let model_hash = req.model_hash.unwrap_or([0u8; 32]);

        let mut job_req = arknet_compute::wire::InferenceJobRequest {
            model_ref: req.model.clone(),
            model_hash,
            prompt: req.prompt.clone(),
            max_tokens: req.max_tokens,
            seed: req.seed.unwrap_or(0),
            deterministic: req.deterministic,
            stop_strings: req.stop_strings.clone(),
            nonce: now_ms, // Use timestamp as nonce for simplicity.
            timestamp_ms: now_ms,
            user_pubkey: pubkey,
            signature: arknet_common::Signature::ed25519([0u8; 64]), // Placeholder.
            prefer_tee: req.prefer_tee,
            encrypted_prompt: None,
        };

        // Sign the request.
        let signing_bytes = job_req.signing_bytes();
        job_req.signature = w.sign(&signing_bytes);

        // Borsh-encode.
        let encoded = borsh::to_vec(&job_req)
            .map_err(|e| SdkError::Wire(format!("failed to encode request: {e}")))?;

        // 3. Try each candidate until one succeeds.
        let mut last_err = String::from("no candidates tried");
        for addr in &candidates {
            match p2p::P2pClient::connect(addr).await {
                Ok(mut client) => match client.infer(encoded.clone()).await {
                    Ok(resp) => return Ok(resp),
                    Err(e) => {
                        last_err = format!("infer failed on {addr}: {e}");
                        continue;
                    }
                },
                Err(e) => {
                    last_err = format!("connect failed to {addr}: {e}");
                    continue;
                }
            }
        }

        Err(SdkError::P2p(format!(
            "all candidates exhausted: {last_err}"
        )))
    }

    /// Query the gateway for compute node candidates serving a model.
    ///
    /// Calls `GET {base_url}/v1/candidates/{model}` and expects a JSON
    /// response with `{ "candidates": ["/ip4/.../p2p/...", ...] }`.
    async fn discover_candidates(&self, model: &str) -> Result<Vec<String>> {
        let url = format!(
            "{}/v1/candidates/{}",
            self.base_url,
            urlencoding::encode(model)
        );
        let mut builder = self.http.get(&url);
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| SdkError::Http(format!("candidate discovery: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(SdkError::Api { status, body });
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| SdkError::Http(format!("candidate response parse: {e}")))?;

        let addrs = body["candidates"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        Ok(addrs)
    }
}

/// Parameters for a direct P2P inference request via [`Client::infer_p2p`].
#[derive(Clone, Debug, Default)]
pub struct P2pInferenceRequest {
    /// Model identifier (e.g. `"meta-llama/Llama-3-8B"`).
    pub model: String,
    /// Prompt text.
    pub prompt: String,
    /// Maximum tokens to generate.
    pub max_tokens: u32,
    /// Expected model hash (optional; zeros if unknown).
    pub model_hash: Option<[u8; 32]>,
    /// Deterministic mode seed.
    pub seed: Option<u64>,
    /// Force deterministic mode on the compute node.
    pub deterministic: bool,
    /// Stop sequences.
    pub stop_strings: Vec<String>,
    /// Route only to TEE-capable nodes.
    pub prefer_tee: bool,
}

const SEEDS_JSON_URL: &str = "https://arknet.arkengel.com/seeds.json";
const FALLBACK_SEEDS: &[&str] = &["https://api.arknet.arkengel.com"];

/// Fetch the live seed list from the static seeds.json file.
/// Falls back to the hardcoded list if unreachable.
async fn fetch_seeds() -> Vec<String> {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return FALLBACK_SEEDS.iter().map(|s| s.to_string()).collect(),
    };
    let resp = match client.get(SEEDS_JSON_URL).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return FALLBACK_SEEDS.iter().map(|s| s.to_string()).collect(),
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return FALLBACK_SEEDS.iter().map(|s| s.to_string()).collect(),
    };
    let urls: Vec<String> = body["seeds"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s["url"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if urls.is_empty() {
        FALLBACK_SEEDS.iter().map(|s| s.to_string()).collect()
    } else {
        urls
    }
}

// ─── Request / response types ───────────────────────────────────────

/// Options for [`Client::connect`] auto-discovery.
#[derive(Default)]
pub struct ConnectOptions {
    /// Seed URLs to discover gateways. Defaults to the arknet seed list.
    pub seeds: Vec<String>,
    /// Only connect to HTTPS gateways.
    pub require_https: bool,
    /// Wallet for signed requests and P2P inference.
    pub wallet: Option<wallet::Wallet>,
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
