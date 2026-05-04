#!/usr/bin/env sh
# arknet installer — downloads the latest release binary.
#
# Usage:
#   curl -fsSL https://docs.arknet.arkengel.com/install.sh | sh
#
# Detects OS/arch, downloads the matching binary from GitHub Releases,
# places it in /usr/local/bin (or ~/.local/bin if no sudo).

set -eu

REPO="st-hannibal/arknet"
INSTALL_DIR="/usr/local/bin"

# ── Detect platform ───────────────────────────────────────────────────

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Linux)  PLATFORM="linux" ;;
    Darwin) PLATFORM="macos" ;;
    *)      echo "Unsupported OS: $OS"; exit 1 ;;
esac

case "$ARCH" in
    x86_64|amd64)   ARCH_TAG="amd64" ;;
    aarch64|arm64)   ARCH_TAG="arm64" ;;
    *)               echo "Unsupported arch: $ARCH"; exit 1 ;;
esac

ARTIFACT="arknet-${PLATFORM}-${ARCH_TAG}"

# ── Find latest release ──────────────────────────────────────────────

echo "arknet installer"
echo "  Platform: $PLATFORM-$ARCH_TAG"

LATEST_URL="https://api.github.com/repos/$REPO/releases/latest"
DOWNLOAD_URL=$(curl -fsSL "$LATEST_URL" \
    | grep "browser_download_url.*$ARTIFACT" \
    | head -1 \
    | cut -d '"' -f 4)

if [ -z "$DOWNLOAD_URL" ]; then
    echo ""
    echo "No release binary found for $ARTIFACT."
    echo "The first release has not been published yet, or your"
    echo "platform is not supported. Build from source instead:"
    echo ""
    echo "  git clone https://github.com/$REPO.git"
    echo "  cd arknet"
    echo "  cargo build --release"
    echo "  sudo cp target/release/arknet /usr/local/bin/"
    echo ""
    exit 1
fi

echo "  Release:  $DOWNLOAD_URL"

# ── Download ──────────────────────────────────────────────────────────

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "  Downloading..."
curl -fsSL -o "$TMP/arknet" "$DOWNLOAD_URL"
chmod +x "$TMP/arknet"

# ── Install ───────────────────────────────────────────────────────────

if [ -w "$INSTALL_DIR" ] 2>/dev/null; then
    mv "$TMP/arknet" "$INSTALL_DIR/arknet"
elif command -v sudo >/dev/null 2>&1; then
    echo "  Installing to $INSTALL_DIR (sudo)..."
    sudo mv "$TMP/arknet" "$INSTALL_DIR/arknet"
else
    INSTALL_DIR="$HOME/.local/bin"
    mkdir -p "$INSTALL_DIR"
    mv "$TMP/arknet" "$INSTALL_DIR/arknet"
    echo ""
    echo "  Installed to $INSTALL_DIR/arknet"
    echo "  Add to PATH: export PATH=\"\$HOME/.local/bin:\$PATH\""
fi

# ── Verify ────────────────────────────────────────────────────────────

echo ""
if command -v arknet >/dev/null 2>&1; then
    echo "Installed: $(arknet --version)"
    echo ""
    echo "Get started:"
    echo "  arknet wallet create     # create your wallet"
    echo "  arknet init              # initialize data directory"
    echo "  arknet start --role compute  # start serving inference"
else
    echo "Binary installed at $INSTALL_DIR/arknet"
    echo "Run: arknet --help"
fi
