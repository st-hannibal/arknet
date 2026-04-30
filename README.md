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

Anyone with a computer earns **ARK** for serving AI models. Any developer queries AI through an OpenAI-compatible API — change one line, get decentralized inference.

**Via the decentralized network** (any public gateway):
```python
from openai import OpenAI

# The network routes your request to the best available compute node
client = OpenAI(base_url="https://api.arknet.arkengel.com/v1", api_key="unused")
response = client.chat.completions.create(
    model="meta-llama/Llama-3.1-8B-Instruct",
    messages=[{"role": "user", "content": "Hello from arknet"}],
)
```

**Via your own local node** (free, no network, runs on your hardware):
```python
from openai import OpenAI

# Point at your own node — inference runs locally, no tokens spent
client = OpenAI(base_url="http://localhost:26657/v1", api_key="unused")
response = client.chat.completions.create(
    model="meta-llama/Llama-3.1-8B-Instruct",
    messages=[{"role": "user", "content": "Hello from arknet"}],
)
```

## How it works

```
┌──────────┐         ┌──────────┐         ┌──────────┐
│  User /  │ ──────► │  Router  │ ──────► │ Compute  │
│  dApp    │  HTTP   │  (picks  │  libp2p │  (runs   │
│          │ ◄────── │   best   │ ◄────── │  model)  │
│          │  tokens │   node)  │  tokens │          │
└──────────┘         └──────────┘         └──────────┘
                           │                    │
                     ┌─────┴─────┐        ┌─────┴─────┐
                     │ Validator │        │ Verifier  │
                     │ (commits  │        │ (checks   │
                     │  blocks)  │        │  output)  │
                     └───────────┘        └───────────┘
```

1. **User** sends an inference request to any **gateway** (a router node with public RPC).
2. **Router** selects the best compute node (by model, latency, stake) and dispatches.
3. **Compute node** runs the model, streams tokens back to the user.
4. **Verifier** re-executes 5% of jobs deterministically — cheaters get slashed.
5. **Validator** finalizes the receipt on L1, emission mints ARK.

Users never interact with the blockchain directly — the OpenAI API is the interface. ARK flows behind the scenes through escrow and settlement.

## Two ways to use arknet

### As a user/developer (no node required)

Point any OpenAI client at a public gateway. The network handles routing, payment, and verification. During the bootstrap period (first 6 months), inference is **free** — the network subsidizes early usage through block emission.

```python
# Any OpenAI-compatible client works — Python, TypeScript, curl, etc.
client = OpenAI(base_url="https://api.arknet.arkengel.com/v1", api_key="unused")
```

### As a node operator (earn ARK)

Run the binary, expose your P2P port, earn tokens for every verified inference job.

```bash
curl -fsSL https://arknet.arkengel.com/install.sh | sh
arknet wallet create          # generate your identity
arknet init --network mainnet
arknet start --role compute   # start earning ARK
```

## Networking for operators

Every node exposes **one public port** (P2P) and keeps everything else private:

| Port | Default | Public? | Purpose |
|------|---------|---------|---------|
| **P2P** | 26656 | **Yes** — open in firewall | Node discovery, block gossip, inference routing. All traffic is Noise-encrypted. |
| **RPC** | 26657 | **No** — localhost only | Your personal dashboard. Query balances, submit transactions, run the explorer. |
| **Metrics** | 9090 | **No** — localhost only | Prometheus metrics for your monitoring stack. |

**If you want to run a public gateway** (let others send inference through you): change RPC bind to `0.0.0.0:26657` in `node.toml`. You earn the 5% router cut on every job you dispatch. This is how `api.arknet.arkengel.com` works — it's just a node with a public RPC.

```toml
# ~/.arknet/node.toml — public gateway config
[roles]
router  = true
compute = true

[network]
p2p_listen = "0.0.0.0:26656"       # always public
rpc_listen = "0.0.0.0:26657"       # expose to serve users
```

This is the same model as every blockchain: Bitcoin exposes port 8333, Ethereum 30303, Cosmos 26656. The P2P port is the backbone; the RPC port is operator-optional.

## Why arknet?

| | Centralized (OpenAI, etc.) | arknet |
|---|---|---|
| **Censorship** | Single kill switch | Zero-trust, permissionless |
| **Pricing** | Opaque, changes overnight | On-chain dynamic market |
| **Revenue** | Goes to one company | 75% to compute, rest split across network |
| **Models** | Vendor-locked | Open registry, any GGUF model |
| **Data** | You trust the provider | Encrypted P2P, prompts never touch consensus |

## Fair launch

No premine. No investors. No team allocation. No airdrop. Every ARK is minted against verified inference work after genesis. The maintainer holds zero tokens at launch.

**Bootstrap period** (first 6 months or until 25 validators): inference is free, `min_stake = 0`, anyone can join. Compute nodes earn ARK from block emission for serving free requests. This bootstraps the token supply from zero — same model as early Bitcoin mining.

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

**552 tests. 0 blockers. Validator producing blocks.**

| Milestone | Status |
|-----------|--------|
| Core protocol (consensus, staking, slashing) | Done |
| Inference pipeline (escrow → compute → receipt → reward) | Done |
| Economic layer (emission, rewards, governance, pricing) | Done |
| Model registry + OpenAI API + SDKs | Done |
| Bootstrap emission (free-tier mints ARK) | Done |
| Wallet CLI + block explorer | Done |
| Multi-node smoke test (17/17 pass) | Done |
| **Genesis** | **Next** |

## License

Apache-2.0
