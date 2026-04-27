#!/usr/bin/env bash
# One-time development environment setup for arknet contributors.

set -euo pipefail

echo "→ Checking toolchain…"
if ! command -v cargo >/dev/null 2>&1; then
    echo "   rustup not found. Install: https://rustup.rs"
    exit 1
fi

echo "→ Installing dev tools…"
cargo install --locked \
    cargo-nextest \
    cargo-llvm-cov \
    cargo-audit \
    cargo-deny \
    cargo-watch \
    cargo-outdated

echo "→ Pre-flight check…"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-targets

echo ""
echo "✓ Dev environment ready."
echo ""
echo "Next steps:"
echo "  just check          # full CI-equivalent locally"
echo "  just run --help     # run the arknet binary"
echo "  just doc            # open API docs"
