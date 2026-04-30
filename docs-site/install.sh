#!/usr/bin/env sh
# arknet installer — placeholder for Phase 0.
#
# Until Phase 1 ships the first signed binary release, this script tells
# operators to build from source. The final version will:
#   - detect OS / arch
#   - download the matching signed artifact from GitHub Releases
#   - verify the minisign signature
#   - place the `arknet` binary in /usr/local/bin
#
# Serving this file from https://arknet.arkengel.com/install.sh keeps the
# `curl ... | sh` one-liner stable across releases.

set -eu

echo "arknet installer (Phase 0 stub)"
echo
echo "No signed release is published yet. To build from source:"
echo
echo "  git clone --recursive https://github.com/st-hannibal/arknet.git"
echo "  cd arknet"
echo "  cargo build --release"
echo "  ./target/release/arknet --help"
echo
echo "Track Phase 1 progress: https://github.com/st-hannibal/arknet"
exit 0
