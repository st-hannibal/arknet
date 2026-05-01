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

**Use the network** (4 lines):
```bash
pip install arknet-sdk
```
```python
from arknet_sdk import Wallet, Client

wallet = Wallet.create()        # your identity on the network
wallet.save()                   # saves to ~/.arknet/wallet.key

client = Client.connect(wallet=wallet)    # discovers nodes automatically
response = client.chat_completion(
    model="Qwen/Qwen3-0.6B-Q4_K_M",
    messages=[{"role": "user", "content": "Hello from arknet"}],
)
```

Your wallet address is your API key. The SDK discovers compute nodes from the blockchain, connects directly via encrypted p2p, and sends your signed request. No gateway sees your prompts — you talk directly to the compute node.

**Run locally** (free, offline, your hardware):
```python
from arknet_sdk import Client

client = Client("http://localhost:26657")    # your own node
response = client.chat_completion(
    model="Qwen/Qwen3-0.6B-Q4_K_M",
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
┌──────────┐  discover  ┌──────────┐  gossip   ┌──────────┐
│  User /  │ ─────────► │ Gateway  │ ◄──────── │ Compute  │
│  SDK     │            │ (who has │  "I have  │  (runs   │
│          │            │  model?) │  model X" │  model)  │
│          │ ◄────────────────────────────────► │          │
│          │     direct p2p (Noise-encrypted)   │          │
└──────────┘                                    └──────────┘
                     ┌───────────┐        ┌───────────┐
                     │ Validator │        │ Verifier  │
                     │ (commits  │        │ (checks   │
                     │  blocks)  │        │  output)  │
                     └───────────┘        └───────────┘
```

1. **Compute nodes** gossip their loaded models to the network.
2. **User SDK** asks any gateway "who has model X?" and gets compute node addresses.
3. **SDK connects directly** to the compute node over encrypted p2p — the gateway never sees the prompt or response.
4. **Verifier** re-executes 5% of jobs deterministically — cheaters get slashed.
5. **Validator** finalizes the receipt on L1, emission mints ARK.

The gateway is a discovery service, not a relay. Your data flows directly between you and the compute node.

## Two ways to use arknet

### As a user/developer (no node required)

Install the SDK, create a wallet, connect. The network handles node discovery, direct p2p routing, and verification. During the bootstrap period (first 6 months), inference is **free** (10 jobs/hour, 100 jobs/day per wallet).

```bash
pip install arknet-sdk    # or: npm install arknet-sdk / cargo add arknet-sdk
```
```python
from arknet_sdk import Wallet, Client

wallet = Wallet.create()     # generate your wallet (one-time)
wallet.save()                # persists to ~/.arknet/wallet.key

client = Client.connect(wallet=wallet)
response = client.chat_completion(
    model="Qwen/Qwen3-0.6B-Q4_K_M",
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

SDKs find compute nodes in three steps:

1. Fetch `https://arknet.arkengel.com/seeds.json` — a static file listing known gateways
2. Call `/v1/gateways` on a seed to get the full on-chain gateway list
3. Call `/v1/candidates/<model>` on a gateway to get compute node p2p addresses
4. Connect directly to the compute node over libp2p (Noise-encrypted QUIC)

The gateway never relays your inference data — it only tells the SDK where to find compute nodes. All prompts and responses travel directly between your machine and the compute node.

To add your gateway to the seed list, submit a PR editing [`docs-site/seeds.json`](docs-site/seeds.json).

### Wallet

Your wallet address is your identity on arknet. Create one with the SDK:

```python
from arknet_sdk import Wallet
wallet = Wallet.create()
wallet.save()           # ~/.arknet/wallet.key
print(wallet.address)   # 0xabc123...
```

Or with the CLI: `arknet wallet create`

The same wallet file works across Python, TypeScript, Rust, and the CLI. During the bootstrap period, each wallet gets 10 free inference jobs per hour and 100 per day. After bootstrap, inference is paid from your ARK balance.

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
