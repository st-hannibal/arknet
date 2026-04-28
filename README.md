# arknet

**Decentralized AI Inference Protocol**

> Permissionless, censorship-resistant, globally distributed AI inference.
> One binary. Any combination of roles. OpenAI-compatible API.

[![Status](https://img.shields.io/badge/status-pre--alpha-orange)](docs/ROLLOUT_PLAN.md)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.86%2B-red)](rust-toolchain.toml)

---

## What is arknet?

arknet is a blockchain-based network for AI inference. Anyone with a GPU can run a compute node and earn **ARK** tokens for serving verified AI output. Anyone can submit inference requests through an OpenAI-compatible API — and get a complete response or a full refund.

Unlike centralized providers (OpenAI, Anthropic, Google), arknet has:

- **No single point of failure** — thousands of geographically distributed nodes.
- **No single censor** — zero-trust architecture, cryptographic enforcement.
- **No opaque pricing** — market-driven, per-model-pool dynamic pricing.
- **No closed ecosystem** — open-source node software, open model registry, OpenAI-compat API.

---

## The one-binary model

Every participant runs the same `arknet` binary. The admin configures which roles to enable:

```toml
[roles]
validator  = false    # L1 consensus
router     = true     # L2 request orchestration
compute    = true     # L2 AI inference
verifier   = false    # L2 output verification
```

The binary self-manages hardware budgets (GPU/CPU/RAM/bandwidth) per role.

---

## Quick start

**Use arknet as a user (OpenAI drop-in):**

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://localhost:26657/v1",         # your local node, or any community gateway
    api_key="ark_..."                              # wallet-derived session token
)

response = client.chat.completions.create(
    model="meta-llama/Llama-3-70B-Instruct",
    messages=[{"role": "user", "content": "Hello"}],
    stream=True,
)
```

**Run a node:**

```bash
curl -fsSL https://arknet.arkengel.com/install.sh | sh
arknet init --network testnet
arknet start
```

See the [Node Operator Guide](docs/NODE_OPERATOR_GUIDE.md) for full setup.

---

## Architecture

Three layers:

- **L1 — Settlement chain.** Tendermint BFT + DPoS. Instant finality. Balances, staking, governance, model registry.
- **L2 — Inference execution.** Off-chain. Routers dispatch, compute nodes infer, verifiers validate. Receipts batched to L1.
- **L3 — Data availability (Phase 3+).** Celestia / EigenDA. Encrypted I/O storage with 72h retention.

Four active roles, one binary: **Validator** · **Router** · **Compute** · **Verifier**.

Three-tier verification: **Optimistic** (spot-check 5%) · **Deterministic** (re-execute) · **TEE** (hardware attestation).

Full details in [ARCHITECTURE.md](docs/ARCHITECTURE.md).

---

## Tokenomics (ARK)

- **1B hard cap.** 450M initial distribution + 550M minted over ~16 years against verified inference.
- **No free pre-mine.** Team + foundation + investors have multi-year cliffs and vests.
- **Minting-on-inference.** Block rewards mint only against verified output. Fake work → no tokens + slash.
- **Fee market.** EIP-1559 base fees burned, tips to validators.
- **Staking.** Role-based + model-tier-based. Delegation supported.

Full design in [TOKENOMICS.md](docs/TOKENOMICS.md).

---

## Status

arknet is in **pre-alpha**, developed under a **fair-launch** model: no premine, no investor round, no team allocation, no airdrop. The repository stays private through Phases 0-3 and flips public at Phase 4 (genesis mainnet), at which point the code, genesis config, and signed binaries all go live together.

| Phase   | Target               | Status                                                                   |
|---------|----------------------|--------------------------------------------------------------------------|
| Phase 0 | Local single node    | ✅ **Complete** (v0.1.0, 2026-04-28) — 171 tests, determinism proven     |
| Phase 1 | Devnet (4-5 nodes)   | next                                                                     |
| Phase 2 | Solo testnet         | pending                                                                  |
| Phase 3 | Closed beta          | pending                                                                  |
| Phase 4 | Genesis + public     | pending                                                                  |

See the [Rollout Plan](docs/ROLLOUT_PLAN.md), [Checklist](docs/CHECKLIST.md), and [Progress log](docs/PROGRESS.md).

---

## Documentation

- 📐 [Architecture](docs/ARCHITECTURE.md)
- 📜 [Protocol Spec](docs/PROTOCOL_SPEC.md)
- 🛠️ [Tech Stack](docs/TECH_STACK.md)
- 💰 [Tokenomics](docs/TOKENOMICS.md)
- 🔒 [Security](docs/SECURITY.md)
- 🖥️ [Node Operator Guide](docs/NODE_OPERATOR_GUIDE.md)
- 📏 [Coding Standards](docs/CODING_STANDARDS.md)
- 🗓️ [Rollout Plan](docs/ROLLOUT_PLAN.md)
- ☑️ [Implementation Checklist](docs/CHECKLIST.md)

---

## Development

```bash
# Prerequisites
#   rustup (will install toolchain from rust-toolchain.toml)
#   cmake, pkg-config, libssl-dev
#   optional: CUDA toolkit (for GPU inference), Docker

# Build everything
cargo build --release

# Run tests
cargo test

# Lint + format
cargo clippy -- -D warnings
cargo fmt --check

# All-in-one via justfile
just check
```

See [CODING_STANDARDS.md](docs/CODING_STANDARDS.md) for contributor guidelines.

---

## Contributing

Pre-alpha. External contributions gated until Phase 2 (public testnet). Follow [GitHub Discussions](https://github.com/st-hannibal/arknet/discussions) for announcements.

---

## License

Apache-2.0. See [LICENSE](LICENSE).

---

## Comparison to existing projects

| Project     | Niche                           | How arknet differs                                  |
|-------------|---------------------------------|----------------------------------------------------|
| Bittensor   | Subjective quality scoring      | arknet rewards *verifiable* computation, not opinions |
| Akash       | General compute marketplace     | arknet is AI-inference-specific, optimized for model loading + batching + latency |
| Gensyn      | ML training verification         | arknet focuses on inference (deterministic, tractable) |
| Ritual      | AI as Ethereum coprocessor      | arknet is AI-native L1, not a layer on top         |
| Together/Fireworks | Centralized inference APIs | arknet has the same UX but zero-trust + decentralized|

Built with lessons from all of the above. See [ARCHITECTURE.md](docs/ARCHITECTURE.md#1-design-principles).
