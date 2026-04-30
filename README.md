<p align="center">
  <strong>arknet</strong><br>
  <em>Decentralized AI inference. One binary. Fair launch.</em>
</p>

<p align="center">
  <a href="https://arknet.arkengel.com">Website</a> ·
  <a href="https://arknet.arkengel.com/docs">Docs</a> ·
  <a href="https://arknet.arkengel.com/tokenomics">Tokenomics</a> ·
  <a href="https://github.com/st-hannibal/arknet/discussions">Forum</a>
</p>

---

Anyone with a computer earns **ARK** for serving AI models. Any developer queries AI through an OpenAI-compatible API — change one line, get decentralized inference.

```python
from openai import OpenAI

client = OpenAI(base_url="https://your-node:26657/v1", api_key="ark_...")
response = client.chat.completions.create(
    model="meta-llama/Llama-3.1-8B-Instruct",
    messages=[{"role": "user", "content": "Hello from arknet"}],
)
```

## Run a node

```bash
curl -fsSL https://arknet.arkengel.com/install.sh | sh
arknet init --network mainnet
arknet start --role compute
```

One binary, four roles: **Validator** · **Router** · **Compute** · **Verifier**. Pick what fits your hardware.

## Why arknet?

| | Centralized (OpenAI, etc.) | arknet |
|---|---|---|
| **Censorship** | Single kill switch | Zero-trust, permissionless |
| **Pricing** | Opaque, changes overnight | On-chain dynamic market |
| **Revenue** | Goes to one company | 75% to compute, rest split across network |
| **Models** | Vendor-locked | Open registry, any GGUF model |
| **Data** | You trust the provider | Encrypted transport, verified execution |

## Fair launch

No premine. No investors. No team allocation. No airdrop. Every ARK is minted against verified inference work after genesis. The maintainer holds zero tokens at launch.

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
| Multi-node smoke test | Done |
| **Genesis** | **Next** |

## License

Apache-2.0
