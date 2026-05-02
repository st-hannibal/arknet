//! PyO3 bindings for the arknet Rust SDK.
//!
//! Exposes [`Wallet`], [`SessionKey`], and [`Client`] to Python,
//! wrapping the async Rust SDK with a synchronous interface.

#![allow(clippy::useless_conversion)]
#![allow(clippy::too_many_arguments)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

// ---------------------------------------------------------------------------
// Error conversion
// ---------------------------------------------------------------------------

fn to_py_err(e: ark_sdk::SdkError) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

// ---------------------------------------------------------------------------
// Wallet
// ---------------------------------------------------------------------------

/// Ed25519 wallet for signing arknet transactions.
///
/// ```python
/// w = Wallet.create()
/// w.save()
/// print(w.address)  # "0x..."
/// ```
#[pyclass]
struct Wallet {
    inner: Arc<ark_sdk::wallet::Wallet>,
}

#[pymethods]
impl Wallet {
    /// Generate a new wallet with a random Ed25519 keypair.
    #[staticmethod]
    fn create() -> Self {
        Self {
            inner: Arc::new(ark_sdk::wallet::Wallet::create()),
        }
    }

    /// Load a wallet from disk.
    #[staticmethod]
    #[pyo3(signature = (path=None))]
    fn load(path: Option<String>) -> PyResult<Self> {
        let p = match path {
            Some(s) => PathBuf::from(s),
            None => ark_sdk::wallet::Wallet::default_path().map_err(to_py_err)?,
        };
        let w = ark_sdk::wallet::Wallet::load(&p).map_err(to_py_err)?;
        Ok(Self { inner: Arc::new(w) })
    }

    /// Save the wallet to disk.
    #[pyo3(signature = (path=None))]
    fn save(&self, path: Option<String>) -> PyResult<()> {
        let p = match path {
            Some(s) => PathBuf::from(s),
            None => ark_sdk::wallet::Wallet::default_path().map_err(to_py_err)?,
        };
        self.inner.save(&p).map_err(to_py_err)
    }

    /// Create a session key authorized by this wallet.
    #[pyo3(signature = (spending_limit, expires_secs))]
    fn create_session(&self, spending_limit: u64, expires_secs: u64) -> PyResult<SessionKey> {
        let session = ark_sdk::session::SessionKey::create(
            &self.inner,
            spending_limit as u128,
            Duration::from_secs(expires_secs),
        )
        .map_err(to_py_err)?;
        Ok(SessionKey {
            inner: Arc::new(std::sync::Mutex::new(session)),
        })
    }

    /// The on-chain address as a ``"0x..."`` hex string.
    #[getter]
    fn address(&self) -> String {
        format!("0x{}", self.inner.address().to_hex())
    }

    /// The Ed25519 public key as a hex string.
    #[getter]
    fn public_key_hex(&self) -> String {
        hex::encode(&self.inner.public_key().bytes)
    }

    fn __repr__(&self) -> String {
        format!("Wallet(address='{}')", self.address())
    }

    fn __str__(&self) -> String {
        self.address()
    }
}

// ---------------------------------------------------------------------------
// SessionKey
// ---------------------------------------------------------------------------

/// Ephemeral session key with spending limit and expiry.
///
/// ```python
/// session = wallet.create_session(spending_limit=100_000_000, expires_secs=3600)
/// client = Client.connect(session=session)
/// ```
#[pyclass]
struct SessionKey {
    inner: Arc<std::sync::Mutex<ark_sdk::session::SessionKey>>,
}

#[pymethods]
impl SessionKey {
    /// Whether the session key has expired.
    #[getter]
    fn is_expired(&self) -> bool {
        self.inner.lock().expect("session lock").is_expired()
    }

    /// Remaining spending limit (ark_atom).
    #[getter]
    fn remaining_limit(&self) -> u64 {
        self.inner.lock().expect("session lock").remaining_limit() as u64
    }

    /// The main wallet address this session is authorized for.
    #[getter]
    fn main_wallet_address(&self) -> String {
        let session = self.inner.lock().expect("session lock");
        format!("0x{}", session.main_wallet_address().to_hex())
    }

    fn __repr__(&self) -> String {
        format!(
            "SessionKey(main_wallet='{}', expired={})",
            self.main_wallet_address(),
            self.is_expired()
        )
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Pure p2p client for arknet inference.
///
/// ```python
/// client = Client.connect(session=session)
/// response = client.infer(model="Qwen/Qwen3-0.6B-Q8_0", prompt="Hello!")
/// ```
#[pyclass]
struct Client {
    inner: ark_sdk::Client,
    rt: Arc<tokio::runtime::Runtime>,
}

#[pymethods]
impl Client {
    /// Connect to the arknet mesh via bootstrap peers.
    #[staticmethod]
    #[pyo3(signature = (session=None, seeds=None, network_id=None, timeout_secs=None))]
    fn connect(
        session: Option<&SessionKey>,
        seeds: Option<Vec<String>>,
        network_id: Option<String>,
        timeout_secs: Option<u64>,
    ) -> PyResult<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime: {e}")))?;

        let owned_session = session
            .map(|s| {
                let guard = s.inner.lock().expect("session lock");
                recreate_session(&guard)
            })
            .transpose()?;

        let mut opts = ark_sdk::ConnectOptions {
            seeds: seeds.unwrap_or_default(),
            network_id: network_id.unwrap_or_else(|| "mainnet".into()),
            ..Default::default()
        };
        if let Some(t) = timeout_secs {
            opts.discovery_timeout = Duration::from_secs(t);
        }
        opts.session = owned_session;

        let client = rt
            .block_on(ark_sdk::Client::connect(opts))
            .map_err(to_py_err)?;

        Ok(Self {
            inner: client,
            rt: Arc::new(rt),
        })
    }

    /// Send an inference request to a compute node.
    #[pyo3(signature = (model, prompt, max_tokens=64, prefer_tee=false))]
    fn infer(
        &self,
        model: &str,
        prompt: &str,
        max_tokens: u32,
        prefer_tee: bool,
    ) -> PyResult<Vec<u8>> {
        let req = ark_sdk::InferRequest {
            model: model.into(),
            prompt: prompt.into(),
            max_tokens,
            prefer_tee,
            ..Default::default()
        };

        self.rt.block_on(self.inner.infer(req)).map_err(to_py_err)
    }

    /// Shut down the client's p2p swarm.
    fn shutdown(&self) {
        self.inner.shutdown();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn recreate_wallet(src: &ark_sdk::wallet::Wallet) -> PyResult<ark_sdk::wallet::Wallet> {
    let dir = tempdir_for_wallet()?;
    let path = dir.join("_pyo3_tmp.key");
    src.save(&path).map_err(to_py_err)?;
    let w = ark_sdk::wallet::Wallet::load(&path).map_err(to_py_err)?;
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
    Ok(w)
}

fn recreate_session(src: &ark_sdk::session::SessionKey) -> PyResult<ark_sdk::session::SessionKey> {
    // SessionKey can't be cloned (signing keys are zeroize-on-drop).
    // For now, return a placeholder error — in production the session
    // would be created fresh in connect() from a wallet.
    // TODO: expose SessionKey::from_delegation_cert + seed for proper cloning.
    Err(PyRuntimeError::new_err(
        "session keys cannot be transferred to the client yet — \
         create the session inside Client.connect() by passing a wallet instead",
    ))
}

fn tempdir_for_wallet() -> PyResult<PathBuf> {
    let mut dir = std::env::temp_dir();
    dir.push(format!("arknet_pyo3_{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| PyRuntimeError::new_err(format!("temp dir: {e}")))?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// Module
// ---------------------------------------------------------------------------

/// arknet Python SDK — pure p2p inference on the arknet network.
#[pymodule]
fn arknet_sdk(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Wallet>()?;
    m.add_class::<SessionKey>()?;
    m.add_class::<Client>()?;
    Ok(())
}
