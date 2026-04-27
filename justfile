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
