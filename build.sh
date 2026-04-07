#!/usr/bin/env bash
# Build script for development on Linux / macOS (CPU fallback encoder).
# On Windows, use build.ps1 instead.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"

BUILD_FLAG="${1:---dev}"
TARGET_DIR="debug"
if [ "$BUILD_FLAG" = "--release" ]; then
  TARGET_DIR="release"
fi

echo "=== Building WASM client ==="

# Install wasm-pack if missing
if ! command -v wasm-pack &>/dev/null; then
  echo "Installing wasm-pack…"
  cargo install wasm-pack
fi

cd "$ROOT/client"
wasm-pack build --target web $BUILD_FLAG

# Prepare static directory
STATIC="$ROOT/server/static"
rm -rf "$STATIC"
mkdir -p "$STATIC/pkg"

cp -r "$ROOT/client/web/"* "$STATIC/"
cp "$ROOT/client/pkg/"*.js   "$STATIC/pkg/"
cp "$ROOT/client/pkg/"*.wasm "$STATIC/pkg/"
cp "$ROOT/client/pkg/"*.d.ts "$STATIC/pkg/" 2>/dev/null || true

echo "=== Building server ==="
cd "$ROOT/server"
cargo build $BUILD_FLAG

echo ""
echo "=== Build complete ==="
echo "Binary: server/target/$TARGET_DIR/wasm-remote-server"
echo ""
echo "Run with:"
echo "  cd server"
echo "  ./target/$TARGET_DIR/wasm-remote-server --encoder libx264"
echo ""
echo "Then open http://localhost:9090 in Chrome/Edge."
