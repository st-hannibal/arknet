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

Anyone with a computer earns **ARK** for serving AI models. Any developer queries AI through a familiar API — install the SDK, connect, done.

**Use the network** (3 lines — the SDK finds nodes automatically):
```bash
pip install arknet-sdk
```
```python
from arknet_sdk import Client

client = Client.connect()    # discovers nodes from the on-chain registry
response = client.chat_completion(
    model="meta-llama/Llama-3.1-8B-Instruct",
    messages=[{"role": "user", "content": "Hello from arknet"}],
)
```

No URLs to configure, no gateway to find — the SDK reads the blockchain's gateway registry and connects to the best available node. HTTPS preferred, TEE-capable nodes prioritized when you ask for confidential inference.

**Run locally** (free, offline, your hardware):
```python
from arknet_sdk import Client

client = Client("http://localhost:26657")    # your own node
response = client.chat_completion(
    model="meta-llama/Llama-3.1-8B-Instruct",
    messages=[{"role": "user", "content": "Hello from arknet"}],
)
```

<details>
<summary>Advanced: using the raw OpenAI SDK (no arknet SDK needed)</summary>

If you already have an OpenAI integration and don't want to install the arknet SDK, point the OpenAI client at any arknet node directly. You'll need to know a node's URL — find one via `/v1/gateways` on any running node, or run your own.

```python
from openai import OpenAI

client = OpenAI(base_url="http://localhost:26657/v1", api_key="local")
response = client.chat.completions.create(
    model="meta-llama/Llama-3.1-8B-Instruct",
    messages=[{"role": "user", "content": "Hello from arknet"}],
)
```
</details>

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

Install the SDK and connect. The network handles node discovery, routing, payment, and verification — you never need to know any server URLs. During the bootstrap period (first 6 months), inference is **free**.

```bash
pip install arknet-sdk    # or: npm install arknet-sdk / cargo add arknet-sdk
```
```python
from arknet_sdk import Client

client = Client.connect()    # auto-discovers nodes from the blockchain
response = client.chat_completion(
    model="meta-llama/Llama-3.1-8B-Instruct",
    messages=[{"role": "user", "content": "Hello from arknet"}],
)
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

**If you want to run a public gateway** (let others send inference through you): change RPC bind to `0.0.0.0:26657` in `node.toml` and register on-chain. SDKs discover your gateway automatically. HTTPS gateways earn a 1.2x reward multiplier. You earn the 5% router cut on every job you dispatch.

### How SDK discovery works

SDKs find nodes in two steps:

1. Fetch `https://arknet.arkengel.com/seeds.json` — a static file listing known gateways
2. Call `/v1/gateways` on a seed to get the full on-chain gateway list

To add your gateway to the seed list, submit a PR editing [`docs-site/seeds.json`](docs-site/seeds.json):

```json
{
  "version": 1,
  "chain_id": "arknet-1",
  "seeds": [
    {"url": "https://api.arknet.arkengel.com", "operator": "st-hannibal", "https": true},
    {"url": "https://your-gateway.example.com", "operator": "you", "https": true}
  ]
}
```

No SDK release needed — all SDKs fetch this file at connect time. The hardcoded fallback list in the SDK code is only used if `seeds.json` is unreachable.

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

## CLI — every action is a command

```bash
# Wallet
arknet wallet create                    # generate identity
arknet wallet balance                   # check ARK balance
arknet wallet send --to 0x... --amount 1000000000

# Staking
arknet wallet stake --role compute --amount 50000000000000
arknet wallet unstake --role compute --amount 50000000000000
arknet wallet complete-unbond --role compute --unbond-id 1
arknet wallet redelegate --role compute --to-node 0x... --amount 50000000000000

# Gateway
arknet gateway register --url https://rpc.mynode.com --https
arknet gateway unregister

# Governance
arknet governance propose --title "Add Llama 4" --body @proposal.md
arknet governance vote --proposal 1 --choice yes

# TEE
arknet tee keygen
arknet tee register --platform intel-tdx --quote-file quote.bin
```

No raw transaction hex. Every on-chain action has a CLI command. Run `arknet --help` for the full tree.

## Why arknet?

| | Centralized (OpenAI, etc.) | arknet |
|---|---|---|
| **Censorship** | Single kill switch | Zero-trust, permissionless |
| **Pricing** | Opaque, changes overnight | On-chain dynamic market |
| **Revenue** | Goes to one company | 75% to compute, rest split across network |
| **Models** | Vendor-locked | Open registry, any GGUF model |
| **Data** | You trust the provider | Encrypted P2P, prompts never touch consensus |
| **Confidentiality** | Trust the provider | TEE enclaves — even host OS can't read prompts |

## Privacy — three tiers

| Tier | What it does | Available |
|------|-------------|-----------|
| **Transport** | Noise-encrypted P2P. Eavesdroppers see nothing. | Genesis |
| **Prompt isolation** | Only the assigned compute node sees your prompt. Validators, routers, verifiers see hashes — never content. | Genesis |
| **Confidential inference (TEE)** | Prompts encrypted to hardware enclave (Intel TDX / AMD SEV-SNP). Even the host OS cannot read them. | Genesis (protocol ready) |

```python
# Request confidential inference — one extra parameter
response = client.chat.completions.create(
    model="meta-llama/Llama-3.1-8B-Instruct",
    messages=[{"role": "user", "content": "Confidential question"}],
    extra_body={"prefer_tee": True},
)
```

When `prefer_tee` is set, the router only picks TEE-capable nodes. No silent downgrade — if no TEE node is available, you get a clear error. For absolute privacy, run your own local node (free, no network, prompts never leave your machine).

## Fair launch

No premine. No investors. No team allocation. No airdrop. Every ARK is minted against verified inference work after genesis. The maintainer holds zero tokens at launch.

**Bootstrap period** (first 6 months or until 100 unique validators): inference is free, `min_stake = 0`, anyone can join. Compute nodes earn ARK from block emission for serving free requests. This bootstraps the token supply from zero — same model as early Bitcoin mining.

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

**615 tests. 0 blockers. Validator producing blocks.**

| Milestone | Status |
|-----------|--------|
| Core protocol (consensus, staking, slashing) | Done |
| Inference pipeline (escrow → compute → receipt → reward) | Done |
| Economic layer (emission, rewards, governance, pricing) | Done |
| Model registry + OpenAI API + SDKs | Done |
| Bootstrap emission (free-tier mints ARK) | Done |
| Wallet CLI + block explorer | Done |
| Multi-node smoke test (17/17 pass) | Done |
| TEE confidential inference (encrypt, route, decrypt, slash) | Done |
| **Genesis** | **Next** |

## License

Apache-2.0
