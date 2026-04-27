set shell := ["bash", "-uc"]

# Default: show tasks
default:
    @just --list

# Build the whole workspace (debug)
build:
    cargo build --all-targets

# Build release
release:
    cargo build --release --all-targets

# Run tests
test:
    cargo test --workspace --all-features

# Run tests with nextest (faster)
nextest:
    cargo nextest run --workspace --all-features

# Format code
fmt:
    cargo fmt --all

# Check formatting
fmt-check:
    cargo fmt --all -- --check

# Run clippy
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Security & license audits
audit:
    cargo audit
    cargo deny check

# Coverage (requires cargo-llvm-cov)
cov:
    cargo llvm-cov --workspace --html --output-dir target/coverage

# All-in-one: what CI runs
check: fmt-check lint test audit

# Clean build artifacts
clean:
    cargo clean

# Build the docs
doc:
    cargo doc --workspace --no-deps --open

# Install dev dependencies
install-dev-tools:
    cargo install cargo-nextest cargo-llvm-cov cargo-audit cargo-deny cargo-watch

# Run the node locally with default config
run *args:
    cargo run --bin arknet -- {{args}}

# ── inference (llama.cpp) ──────────────────────────────────────────────
# GPU backend is selected via the ARKNET_INFERENCE_GPU env var (not a
# cargo feature), so `cargo build --all-features` doesn't try to compile
# CUDA on a Mac laptop.
#
# Values: cpu (default), cuda, metal, rocm, vulkan.

# CPU-only build. Default for dev + CI + verification path.
inference:
    cargo build -p arknet-inference

# Apple-Silicon dev mode with Metal GPU. Compute-node only — the
# verifier path still uses the CPU build.
inference-metal:
    ARKNET_INFERENCE_GPU=metal cargo build -p arknet-inference

# NVIDIA CUDA dev mode. Compute-node only.
inference-cuda:
    ARKNET_INFERENCE_GPU=cuda cargo build -p arknet-inference

# Refresh the vendored llama.cpp submodule to the pinned SHA. Run
# after a fresh clone or after pulling a commit that bumps the pin.
submodules:
    git submodule update --init --recursive
