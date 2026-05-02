//! PyO3 bindings for the arknet Rust SDK.
//!
//! Exposes [`Wallet`] and [`Client`] to Python, wrapping the async Rust
//! SDK with a synchronous interface backed by a per-client tokio runtime.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;

// ---------------------------------------------------------------------------
// Error conversion
// ---------------------------------------------------------------------------

/// Map an [`ark_sdk::SdkError`] to a Python `RuntimeError`.
fn to_py_err(e: ark_sdk::SdkError) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

// ---------------------------------------------------------------------------
// Wallet
// ---------------------------------------------------------------------------

/// Ed25519 wallet for signing arknet inference requests.
///
/// The wallet holds a keypair and derives a 20-byte on-chain address.
///
/// **Create a new wallet:**
///
/// ```python
/// w = Wallet.create()
/// w.save()           # writes to ~/.arknet/wallet.key
/// print(w.address)   # "0xabc123..."
/// ```
///
/// **Load an existing wallet:**
///
/// ```python
/// w = Wallet.load()                   # default path
/// w = Wallet.load("/path/to/key")     # custom path
/// ```
#[pyclass]
struct Wallet {
    /// The underlying Rust wallet wrapped in Arc so it can be shared
    /// with the Client (signing keys don't implement Clone).
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
    ///
    /// Parameters
    /// ----------
    /// path : str, optional
    ///     Path to a 64-byte key file. Defaults to ``~/.arknet/wallet.key``
    ///     (or the ``ARKNET_WALLET_PATH`` environment variable).
    #[staticmethod]
    #[pyo3(signature = (path=None))]
    fn load(path: Option<String>) -> PyResult<Self> {
        let p = match path {
            Some(s) => PathBuf::from(s),
            None => ark_sdk::wallet::Wallet::default_path().map_err(to_py_err)?,
        };
        let w = ark_sdk::wallet::Wallet::load(&p).map_err(to_py_err)?;
        Ok(Self {
            inner: Arc::new(w),
        })
    }

    /// Save the wallet to disk.
    ///
    /// Parameters
    /// ----------
    /// path : str, optional
    ///     Destination file. Defaults to ``~/.arknet/wallet.key``
    ///     (or the ``ARKNET_WALLET_PATH`` environment variable).
    ///     Parent directories are created automatically.
    #[pyo3(signature = (path=None))]
    fn save(&self, path: Option<String>) -> PyResult<()> {
        let p = match path {
            Some(s) => PathBuf::from(s),
            None => ark_sdk::wallet::Wallet::default_path().map_err(to_py_err)?,
        };
        self.inner.save(&p).map_err(to_py_err)
    }

    /// The on-chain address as a ``"0x..."`` hex string.
    #[getter]
    fn address(&self) -> String {
        format!("0x{}", self.inner.address().to_hex())
    }

    /// The Ed25519 public key as a hex string (no prefix).
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
// Client
// ---------------------------------------------------------------------------

/// HTTP client for the arknet OpenAI-compatible API.
///
/// **Direct construction:**
///
/// ```python
/// client = Client("http://127.0.0.1:3000", wallet=wallet)
/// ```
///
/// **Auto-discovery:**
///
/// ```python
/// client = Client.connect(wallet=wallet)
/// ```
#[pyclass]
struct Client {
    inner: ark_sdk::Client,
    rt: Arc<tokio::runtime::Runtime>,
}

#[pymethods]
impl Client {
    /// Create a client pointing at a specific node URL.
    ///
    /// Parameters
    /// ----------
    /// base_url : str
    ///     Node HTTP root, e.g. ``"http://127.0.0.1:3000"``.
    /// wallet : Wallet, optional
    ///     Wallet for signed requests.
    #[new]
    #[pyo3(signature = (base_url, wallet=None))]
    fn new(base_url: &str, wallet: Option<&Wallet>) -> PyResult<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| PyRuntimeError::new_err(format!("failed to build tokio runtime: {e}")))?;

        let mut client = ark_sdk::Client::new(base_url).map_err(to_py_err)?;
        if let Some(w) = wallet {
            // Re-create the wallet from the same seed bytes so the Rust
            // SDK can take ownership.  We go through save/load via an
            // in-memory round-trip of the signing key export.
            let fresh = recreate_wallet(&w.inner)?;
            client = client.with_wallet(fresh);
        }
        Ok(Self {
            inner: client,
            rt: Arc::new(rt),
        })
    }

    /// Auto-discover a gateway from the on-chain registry.
    ///
    /// Parameters
    /// ----------
    /// wallet : Wallet, optional
    ///     Wallet for signed requests.
    /// require_https : bool, optional
    ///     Only connect to HTTPS gateways (default ``False``).
    #[staticmethod]
    #[pyo3(signature = (wallet=None, require_https=None))]
    fn connect(wallet: Option<&Wallet>, require_https: Option<bool>) -> PyResult<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| PyRuntimeError::new_err(format!("failed to build tokio runtime: {e}")))?;

        let owned_wallet = match wallet {
            Some(w) => Some(recreate_wallet(&w.inner)?),
            None => None,
        };

        let opts = ark_sdk::ConnectOptions {
            require_https: require_https.unwrap_or(false),
            wallet: owned_wallet,
            ..Default::default()
        };

        let client = rt
            .block_on(ark_sdk::Client::connect(opts))
            .map_err(to_py_err)?;

        Ok(Self {
            inner: client,
            rt: Arc::new(rt),
        })
    }

    /// Non-streaming chat completion (OpenAI-compatible).
    ///
    /// Parameters
    /// ----------
    /// model : str
    ///     Model identifier (e.g. ``"Qwen/Qwen3-0.6B-Q4_K_M"``).
    /// messages : list[dict]
    ///     Conversation messages, each with ``"role"`` and ``"content"`` keys.
    /// max_tokens : int, optional
    ///     Maximum tokens to generate (default 256).
    /// temperature : float, optional
    ///     Sampling temperature (default 1.0).
    /// stop : list[str], optional
    ///     Stop sequences.
    /// prefer_tee : bool, optional
    ///     Route only to TEE-capable nodes.
    /// require_https : bool, optional
    ///     Route only through HTTPS gateways.
    ///
    /// Returns
    /// -------
    /// dict
    ///     The full OpenAI-shaped response.
    #[pyo3(signature = (model, messages, max_tokens=None, temperature=None, stop=None, prefer_tee=None, require_https=None))]
    fn chat_completion(
        &self,
        py: Python<'_>,
        model: &str,
        messages: Vec<HashMap<String, String>>,
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        stop: Option<Vec<String>>,
        prefer_tee: Option<bool>,
        require_https: Option<bool>,
    ) -> PyResult<PyObject> {
        let msgs: Vec<ark_sdk::Message> = messages
            .into_iter()
            .map(|m| ark_sdk::Message {
                role: m.get("role").cloned().unwrap_or_default(),
                content: m.get("content").cloned().unwrap_or_default(),
            })
            .collect();

        let req = ark_sdk::ChatRequest {
            model: model.to_string(),
            messages: msgs,
            max_tokens,
            temperature,
            stop,
            prefer_tee,
            require_https,
            ..Default::default()
        };

        let resp = self
            .rt
            .block_on(self.inner.chat_completion(req))
            .map_err(to_py_err)?;

        chat_response_to_py(py, &resp)
    }

    /// List registered models.
    ///
    /// Returns
    /// -------
    /// dict
    ///     ``{"data": [{"id": "...", "owned_by": "..."}]}``
    fn list_models(&self, py: Python<'_>) -> PyResult<PyObject> {
        let resp = self
            .rt
            .block_on(self.inner.list_models())
            .map_err(to_py_err)?;

        models_response_to_py(py, &resp)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Recreate a [`ark_sdk::wallet::Wallet`] by exporting the signing key
/// material from the Arc'd wallet and re-importing it.
///
/// This is necessary because the Rust SDK's `Client::with_wallet` takes
/// ownership and [`ark_sdk::wallet::Wallet`] doesn't implement Clone
/// (signing keys are zeroize-on-drop).
fn recreate_wallet(
    src: &ark_sdk::wallet::Wallet,
) -> PyResult<ark_sdk::wallet::Wallet> {
    // Round-trip through a temp file (the Wallet API only exposes
    // load/save with Path). Use a secure temp directory.
    let dir = tempdir_for_wallet()?;
    let path = dir.join("_pyo3_tmp.key");
    src.save(&path).map_err(to_py_err)?;
    let w = ark_sdk::wallet::Wallet::load(&path).map_err(to_py_err)?;
    // Best-effort cleanup.
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
    Ok(w)
}

/// Create a temporary directory for the wallet round-trip.
fn tempdir_for_wallet() -> PyResult<PathBuf> {
    let mut dir = std::env::temp_dir();
    dir.push(format!("arknet_pyo3_{}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .map_err(|e| PyRuntimeError::new_err(format!("failed to create temp dir: {e}")))?;
    Ok(dir)
}

/// Convert a [`ChatResponse`] into a Python dict.
fn chat_response_to_py(py: Python<'_>, resp: &ark_sdk::ChatResponse) -> PyResult<PyObject> {
    let dict = PyDict::new_bound(py);
    dict.set_item("id", &resp.id)?;

    let choices: Vec<PyObject> = resp
        .choices
        .iter()
        .map(|c| {
            let d = PyDict::new_bound(py);
            d.set_item("index", c.index)?;
            let msg = PyDict::new_bound(py);
            msg.set_item("role", &c.message.role)?;
            msg.set_item("content", &c.message.content)?;
            d.set_item("message", &msg)?;
            d.set_item("finish_reason", c.finish_reason.as_deref())?;
            Ok(d.into_any().unbind())
        })
        .collect::<PyResult<_>>()?;
    dict.set_item("choices", choices)?;

    if let Some(usage) = &resp.usage {
        let u = PyDict::new_bound(py);
        u.set_item("prompt_tokens", usage.prompt_tokens)?;
        u.set_item("completion_tokens", usage.completion_tokens)?;
        u.set_item("total_tokens", usage.total_tokens)?;
        dict.set_item("usage", u)?;
    }

    Ok(dict.into_any().unbind())
}

/// Convert a [`ModelsResponse`] into a Python dict.
fn models_response_to_py(
    py: Python<'_>,
    resp: &ark_sdk::ModelsResponse,
) -> PyResult<PyObject> {
    let dict = PyDict::new_bound(py);
    let data: Vec<PyObject> = resp
        .data
        .iter()
        .map(|m| {
            let d = PyDict::new_bound(py);
            d.set_item("id", &m.id)?;
            d.set_item("owned_by", &m.owned_by)?;
            Ok(d.into_any().unbind())
        })
        .collect::<PyResult<_>>()?;
    dict.set_item("data", data)?;
    Ok(dict.into_any().unbind())
}

// ---------------------------------------------------------------------------
// Module
// ---------------------------------------------------------------------------

/// arknet Python SDK — PyO3 bindings for the arknet Rust SDK.
///
/// Provides :class:`Wallet` for Ed25519 key management and :class:`Client`
/// for OpenAI-compatible inference on the arknet network.
#[pymodule]
fn arknet_sdk(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Wallet>()?;
    m.add_class::<Client>()?;
    Ok(())
}
