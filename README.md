<p align="center">
  <strong>arknet</strong><br>
  <em>Decentralized AI inference. One binary. Fair launch.</em>
</p>

<p align="center">
  <a href="https://arknet.arkengel.com">Website</a> ·
  <a href="https://arknet.arkengel.com/docs">Docs</a> ·
  <a href="https://arknet.arkengel.com/tokenomics">Tokenomics</a> ·
  <a href="https://arknet.arkengel.com/explorer.html">Explorer</a> ·
  <a href="https://github.com/st-hannibal/arknet/discussions">Forum</a>
</p>

---

Anyone with a computer earns **ARK** for serving AI models. Any developer queries AI through a peer-to-peer mesh — install the SDK, connect, infer.

**Use the network** (Python):
```python
from arknet_sdk import Wallet, Client

wallet = Wallet.create()
wallet.save()

session = wallet.create_session(spending_limit=100_000_000, expires_secs=3600)
client = Client.connect(session=session)
response = client.infer(
    model="Qwen/Qwen3-0.6B-Q8_0",
    prompt="Hello from arknet",
    max_tokens=64,
)
```

**Use the network** (Rust):
```rust
use std::time::Duration;
use arknet_sdk::{Client, ConnectOptions, InferRequest, wallet::Wallet, session::SessionKey};

let wallet = Wallet::create();
let session = SessionKey::create(&wallet, 100_000_000, Duration::from_secs(3600))?;
let client = Client::connect(ConnectOptions {
    session: Some(session),
    ..Default::default()
}).await?;
let response = client.infer(InferRequest {
    model: "Qwen/Qwen3-0.6B-Q8_0".into(),
    prompt: "Hello from arknet".into(),
    max_tokens: 64,
    ..Default::default()
}).await?;
```

Your wallet address is your identity. The SDK joins the p2p mesh as a lightweight libp2p peer, discovers compute nodes via gossip, and sends inference requests directly over Noise-encrypted channels. No HTTP. No gateway. No middleman.

## How it works

```
┌──────────┐  bootstrap  ┌───────────┐  gossip   ┌──────────┐
│  User /  │ ──────────► │ Validator │ ◄──────── │ Compute  │
│  SDK     │             │ (consensus│  "I have  │  (runs   │
│          │             │  + relay) │  model X" │  model)  │
│          │ ◄──────────────────────────────────► │          │
│          │    direct p2p (Noise-encrypted)      │          │
└──────────┘    via relay, then hole-punch        └──────────┘
                     ┌───────────┐
                     │ Verifier  │
                     │ (checks   │
                     │  output)  │
                     └───────────┘
```

1. **Compute nodes** gossip their loaded models on the `arknet/pool/offer/1` topic (heartbeat every 60s).
2. **SDK** bootstraps from seed validators, subscribes to gossip, and discovers which compute nodes have which models.
3. **SDK connects** to the compute node through the validator relay (libp2p circuit relay), then DCUtR hole-punches a direct connection.
4. **Inference** travels directly between the SDK and compute node — the validator never sees the prompt.
5. **Verifier** re-executes 5% of jobs deterministically — cheaters get slashed.
6. **Validator** finalizes the receipt on L1, emission mints ARK.

Two node types: **Validator** (consensus + relay + seed) and **Compute** (inference + gossip announce). That's it.

## As a developer

Install the SDK, create a wallet, create a session key, connect, infer. During the bootstrap period (first 6 months or 100 validators), inference is **free**.

```bash
pip install arknet-sdk    # or: cargo add arknet-sdk
```
```python
from arknet_sdk import Wallet, Client

wallet = Wallet.create()
wallet.save()
session = wallet.create_session(spending_limit=100_000_000, expires_secs=3600)
client = Client.connect(session=session)

response = client.infer(
    model="Qwen/Qwen3-0.6B-Q8_0",
    prompt="Hello from arknet",
    max_tokens=64,
)
print(response.text)
```

**Session keys** authorize bounded spending from your wallet. Your main key signs a `DelegationCert` granting an ephemeral session key a `spending_limit` and `expiry`. Inference requests are signed by the session key — if compromised, damage is bounded.

**TypeScript** SDK (`npm install arknet-sdk`) exists but has not yet been updated for the p2p architecture. Coming soon.

## As a node operator

Run the binary, pick your role, earn ARK.

```bash
curl -fsSL https://arknet.arkengel.com/install.sh | sh
arknet init
arknet start --role compute   # or: --role validator
```

See the full [Node Operator Guide](https://arknet.arkengel.com/docs/node-operators.html) for step-by-step validator and compute setup.

### Ports

| Port | Default | Public? | Purpose |
|------|---------|---------|---------|
| **P2P** | 26656 | **Yes** — open in firewall | Node discovery, gossip, relay, consensus, inference. Noise-encrypted. |
| **RPC** | 26657 | **No** — localhost only | Operator admin. Load models, check status, submit transactions. |
| **Metrics** | 9090 | **No** — localhost only | Prometheus metrics. |

### Discovery

The SDK discovers compute nodes through the gossip mesh:

1. Load seed validator addresses from `seeds.json` (hardcoded fallback in SDK binary)
2. Bootstrap into the libp2p mesh via a seed validator
3. Subscribe to `arknet/pool/offer/1` — receive `PoolOffer` messages from compute nodes
4. Connect to a compute node offering the requested model (relay, then hole-punch)

No HTTP endpoints. No gateway. Everything is libp2p.

### Wallet

Ed25519 keypair. 64-byte file at `~/.arknet/wallet.key`. Address = `blake3(pubkey)[0..20]`.

```python
from arknet_sdk import Wallet
wallet = Wallet.create()
wallet.save()
print(wallet.address)
```

Or: `arknet wallet create`

The same wallet file works across Python, Rust, and the CLI.

## CLI

```bash
# Wallet
arknet wallet create
arknet wallet balance
arknet wallet send --to 0x... --amount 1000000000

# Staking
arknet wallet stake --role compute --amount 50000000000000
arknet wallet unstake --role compute --amount 50000000000000
arknet wallet complete-unbond --role compute --unbond-id 1
arknet wallet redelegate --role compute --to-node 0x... --amount 50000000000000

# Governance
arknet governance propose --title "Add Llama 4" --body @proposal.md
arknet governance vote --proposal 1 --choice yes

# TEE
arknet tee keygen
arknet tee register --platform intel-tdx --quote-file quote.bin
```

## Why arknet?

| | Centralized (OpenAI, etc.) | arknet |
|---|---|---|
| **Censorship** | Single kill switch | Zero-trust, permissionless |
| **Pricing** | Opaque, changes overnight | On-chain dynamic market |
| **Revenue** | Goes to one company | 80% to compute, rest split across network |
| **Models** | Vendor-locked | Open registry, any GGUF model |
| **Data** | You trust the provider | Encrypted P2P, prompts never touch consensus |
| **Confidentiality** | Trust the provider | TEE enclaves — even host OS can't read prompts |

## Reward split

Every verified inference job mints ARK:

| Recipient | Share |
|-----------|-------|
| **Compute** | 80% |
| **Verifier** | 7% |
| **Treasury** | 5% |
| **Burn** | 3% |
| **Delegators** | 5% |

## Privacy — three tiers

| Tier | What it does | Available |
|------|-------------|-----------|
| **Transport** | Noise-encrypted P2P. Eavesdroppers see nothing. | Genesis |
| **Prompt isolation** | Only the assigned compute node sees your prompt. Validators and verifiers see hashes — never content. | Genesis |
| **Confidential inference (TEE)** | Prompts encrypted to hardware enclave (Intel TDX / AMD SEV-SNP). Even the host OS cannot read them. | Genesis (protocol ready) |

## Fair launch

No premine. No investors. No team allocation. No airdrop. Every ARK is minted against verified inference work after genesis. The maintainer holds zero tokens at launch.

**Bootstrap period** (first 6 months or until 100 unique validators): inference is free, `min_stake = 0`, anyone can join. Compute nodes earn ARK from block emission for serving free requests.

## Genesis models

10 models ship at genesis — from Raspberry Pi to server:

| Tier | Model | Size |
|------|-------|------|
| Edge | Llama 3.2 1B | 1.3 GB |
| Edge | Qwen3.5 4B | 2.7 GB |
| Gamer | Llama 3.1 8B | 4.9 GB |
| Gamer | Qwen3.5 9B | 5.7 GB |
| Prosumer | Gemma 4 26B MoE | 16.9 GB |
| Prosumer | Qwen3.6 27B | 16.8 GB |
| Server | Qwen3.6 35B MoE | 22.1 GB |

Full list with SHA256 digests: [models](https://arknet.arkengel.com/docs/models)

## Status

| Milestone | Status |
|-----------|--------|
| Core protocol (consensus, staking, slashing) | Done |
| Inference pipeline (escrow, compute, receipt, reward) | Done |
| Economic layer (emission, rewards, governance, pricing) | Done |
| Model registry + SDKs (Python, Rust) | Done |
| P2P mesh (gossip discovery, relay, hole-punch) | Done |
| Session keys (DelegationCert, spending limit, expiry) | Done |
| Bootstrap emission (free-tier mints ARK) | Done |
| Wallet CLI + block explorer | Done |
| TEE confidential inference | Done |
| **Genesis** | **Next** |

## License

Apache-2.0
