//! Public Rust SDK for arknet.
//!
//! Pure p2p — the SDK joins the arknet gossip mesh, discovers compute
//! nodes via `PoolOffer` messages, and connects directly for inference.
//! No HTTP anywhere.
//!
//! # Session keys
//!
//! Create a [`session::SessionKey`] from a [`wallet::Wallet`] to limit
//! exposure: the session key has a spending cap and expiry.
//!
//! # Example
//!
//! ```rust,no_run
//! # async fn demo() -> arknet_sdk::Result<()> {
//! use std::time::Duration;
//!
//! let wallet = arknet_sdk::wallet::Wallet::create();
//! let session = arknet_sdk::session::SessionKey::create(
//!     &wallet, 100_000_000, Duration::from_secs(3600),
//! )?;
//! let client = arknet_sdk::Client::connect(arknet_sdk::ConnectOptions {
//!     session: Some(session),
//!     ..Default::default()
//! }).await?;
//! let response = client.infer(arknet_sdk::InferRequest {
//!     model: "Qwen/Qwen3-0.6B-Q8_0".into(),
//!     prompt: "Hello!".into(),
//!     max_tokens: 64,
//!     ..Default::default()
//! }).await?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod candidate_table;
pub mod discovery;
pub mod errors;
pub mod p2p;
pub mod session;
pub mod wallet;

pub use errors::{Result, SdkError};

/// Hardcoded fallback seed multiaddrs (validator nodes).
const FALLBACK_SEED_MULTIADDRS: &[&str] =
    &["/dns4/arknet.arkengel.com/tcp/26656/p2p/12D3KooWFKNZj7VaophcMVbA7QCRexAm7tg9dnADSJ8SxW4sLE1f"];

/// arknet SDK client. Joins the gossip mesh, discovers compute nodes,
/// and sends signed inference requests over p2p.
pub struct Client {
    swarm: discovery::SdkSwarmHandle,
    wallet: Option<wallet::Wallet>,
    session: Option<session::SessionKey>,
}

impl Client {
    /// Connect to the arknet mesh via bootstrap peers.
    ///
    /// Discovers compute nodes via gossip. Waits up to
    /// `discovery_timeout` (default 30s) for the first `PoolOffer`.
    pub async fn connect(opts: ConnectOptions) -> Result<Self> {
        let bootstrap_peers: Vec<arknet_network::Multiaddr> = if opts.seeds.is_empty() {
            FALLBACK_SEED_MULTIADDRS
                .iter()
                .filter_map(|s| s.parse().ok())
                .collect()
        } else {
            opts.seeds.iter().filter_map(|s| s.parse().ok()).collect()
        };

        let config = discovery::SdkConfig {
            network_id: opts.network_id,
            bootstrap_peers,
            discovery_timeout: opts.discovery_timeout,
        };

        let (swarm, _join) = discovery::start(config).await?;

        Ok(Self {
            swarm,
            wallet: opts.wallet,
            session: opts.session,
        })
    }

    /// Attach a wallet after construction.
    pub fn with_wallet(mut self, wallet: wallet::Wallet) -> Self {
        self.wallet = Some(wallet);
        self
    }

    /// Attach a session key after construction.
    pub fn with_session(mut self, session: session::SessionKey) -> Self {
        self.session = Some(session);
        self
    }

    /// Reference to the attached wallet, if any.
    pub fn wallet(&self) -> Option<&wallet::Wallet> {
        self.wallet.as_ref()
    }

    /// Reference to the attached session key, if any.
    pub fn session(&self) -> Option<&session::SessionKey> {
        self.session.as_ref()
    }

    /// The candidate table populated from gossip.
    pub fn candidates(&self) -> &candidate_table::CandidateTable {
        self.swarm.candidates()
    }

    /// Send an inference request to a compute node.
    ///
    /// Discovers candidates from gossip, builds a signed request (using
    /// session key if available, otherwise wallet), and connects directly
    /// via p2p. Retries up to `max_retries` candidates on busy/error.
    pub async fn infer(&self, req: InferRequest) -> Result<Vec<u8>> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        // Build the signing identity.
        let (pubkey, delegation) = if let Some(session) = &self.session {
            if session.is_expired() {
                return Err(SdkError::Session("session key expired".into()));
            }
            (session.public_key(), Some(session.delegation().clone()))
        } else if let Some(w) = &self.wallet {
            (w.public_key(), None)
        } else {
            return Err(SdkError::NoWallet);
        };

        let model_hash = req.model_hash.unwrap_or([0u8; 32]);
        let mut job_req = arknet_compute::wire::InferenceJobRequest {
            model_ref: req.model.clone(),
            model_hash,
            prompt: req.prompt.clone(),
            max_tokens: req.max_tokens,
            seed: req.seed.unwrap_or(0),
            deterministic: req.deterministic,
            stop_strings: req.stop_strings.clone(),
            nonce: now_ms,
            timestamp_ms: now_ms,
            user_pubkey: pubkey,
            signature: arknet_common::Signature::ed25519([0u8; 64]),
            prefer_tee: req.prefer_tee,
            encrypted_prompt: None,
            delegation,
        };

        let signing_bytes = job_req.signing_bytes();
        job_req.signature = if let Some(session) = &self.session {
            session.sign(&signing_bytes)
        } else if let Some(w) = &self.wallet {
            w.sign(&signing_bytes)
        } else {
            return Err(SdkError::NoWallet);
        };

        let encoded =
            borsh::to_vec(&job_req).map_err(|e| SdkError::Wire(format!("encode request: {e}")))?;

        // Discover candidates from gossip.
        let candidates = self.swarm.candidates().eligible_for(&req.model, now_ms);
        if candidates.is_empty() {
            return Err(SdkError::Discovery(format!(
                "no compute nodes serving model '{}'",
                req.model
            )));
        }

        // Try candidates in order (sorted by capacity + stake).
        let max_retries = req.max_retries.unwrap_or(3).min(candidates.len() as u32);
        for candidate in candidates.iter().take(max_retries as usize) {
            if candidate.multiaddrs.is_empty() {
                continue;
            }

            for addr in &candidate.multiaddrs {
                match p2p::P2pClient::connect(addr).await {
                    Ok(mut client) => match client.infer(encoded.clone()).await {
                        Ok(resp) => {
                            if let Ok(events) = borsh::from_slice::<
                                Vec<arknet_compute::wire::InferenceJobEvent>,
                            >(&resp)
                            {
                                if events.iter().any(|e| {
                                    matches!(
                                        e,
                                        arknet_compute::wire::InferenceJobEvent::Busy { .. }
                                    )
                                }) {
                                    break;
                                }
                            }
                            return Ok(resp);
                        }
                        Err(_) => continue,
                    },
                    Err(_) => continue,
                }
            }
        }

        Err(SdkError::AllComputeNodesBusy)
    }

    /// Publish a signed escrow transaction via gossip.
    pub async fn submit_escrow(
        &self,
        job_req: &arknet_compute::wire::InferenceJobRequest,
        amount: u128,
    ) -> Result<()> {
        let w = self.wallet.as_ref().ok_or(SdkError::NoWallet)?;
        let job_id = {
            let mut hasher = blake3::Hasher::new();
            hasher.update(b"arknet-job-id-v1");
            hasher.update(&job_req.billing_address().0);
            hasher.update(&job_req.nonce.to_le_bytes());
            hasher.update(&job_req.timestamp_ms.to_le_bytes());
            let digest = hasher.finalize();
            let mut out = [0u8; 32];
            out.copy_from_slice(digest.as_bytes());
            arknet_common::types::JobId::new(out)
        };

        let tx = arknet_chain::transactions::Transaction::EscrowLock {
            from: job_req.billing_address(),
            job_id,
            amount,
            nonce: job_req.nonce,
            fee: 21_000,
        };
        let tx_bytes =
            borsh::to_vec(&tx).map_err(|e| SdkError::Wire(format!("escrow encode: {e}")))?;
        let sig = w.sign(&tx_bytes);
        let signed = arknet_chain::transactions::SignedTransaction {
            tx,
            signer: w.public_key(),
            signature: sig,
        };
        let signed_bytes =
            borsh::to_vec(&signed).map_err(|e| SdkError::Wire(format!("signed tx encode: {e}")))?;

        self.swarm.publish_tx(signed_bytes).await
    }

    /// Shut down the SDK swarm.
    pub fn shutdown(&self) {
        self.swarm.shutdown();
    }
}

/// Parameters for an inference request.
#[derive(Clone, Debug, Default)]
pub struct InferRequest {
    /// Model identifier (e.g. `"Qwen/Qwen3-0.6B-Q8_0"`).
    pub model: String,
    /// Prompt text.
    pub prompt: String,
    /// Maximum tokens to generate.
    pub max_tokens: u32,
    /// Expected model hash (optional; zeros if unknown).
    pub model_hash: Option<[u8; 32]>,
    /// Deterministic mode seed.
    pub seed: Option<u64>,
    /// Force deterministic mode.
    pub deterministic: bool,
    /// Stop sequences.
    pub stop_strings: Vec<String>,
    /// Route only to TEE-capable nodes.
    pub prefer_tee: bool,
    /// Max candidates to try before giving up.
    pub max_retries: Option<u32>,
}

/// Options for [`Client::connect`].
pub struct ConnectOptions {
    /// Seed multiaddrs (e.g. `"/ip4/1.2.3.4/tcp/26656/p2p/12D3KooW..."`).
    /// Falls back to hardcoded seeds if empty.
    pub seeds: Vec<String>,
    /// Network id (must match the chain). Defaults to `"mainnet"`.
    pub network_id: String,
    /// How long to wait for the first PoolOffer.
    pub discovery_timeout: std::time::Duration,
    /// Wallet for signing (used if no session key).
    pub wallet: Option<wallet::Wallet>,
    /// Session key for signing (preferred over wallet).
    pub session: Option<session::SessionKey>,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            seeds: Vec::new(),
            network_id: "mainnet".into(),
            discovery_timeout: std::time::Duration::from_secs(30),
            wallet: None,
            session: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_options_defaults() {
        let opts = ConnectOptions::default();
        assert!(opts.seeds.is_empty());
        assert_eq!(opts.network_id, "mainnet");
    }

    #[test]
    fn fallback_seeds_are_valid_multiaddrs() {
        for s in FALLBACK_SEED_MULTIADDRS {
            let parsed: std::result::Result<arknet_network::Multiaddr, _> = s.parse();
            assert!(parsed.is_ok(), "invalid fallback seed: {s}");
        }
    }

    #[test]
    fn infer_request_defaults() {
        let req = InferRequest::default();
        assert!(req.model.is_empty());
        assert_eq!(req.max_tokens, 0);
        assert!(!req.prefer_tee);
        assert!(!req.deterministic);
    }
}
