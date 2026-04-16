#!/usr/bin/env bash
# Build the mangle-wasm crate and serve the browser playground locally.
#
# Usage:
#   scripts/playground.sh            # build + serve on http://localhost:8000
#   scripts/playground.sh --port 9000
#   scripts/playground.sh --build-only
#
# Prerequisites:
#   - wasm-pack (https://rustwasm.github.io/wasm-pack/installer/)
#   - python3   (for the static file server; only when serving)

set -euo pipefail

PORT=8000
BUILD_ONLY=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --port)
            PORT="$2"
            shift 2
            ;;
        --build-only)
            BUILD_ONLY=1
            shift
            ;;
        -h|--help)
            sed -n '2,12p' "$0"
            exit 0
            ;;
        *)
            echo "Unknown flag: $1" >&2
            exit 2
            ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CRATE_DIR="$REPO_ROOT/crates/mangle-wasm"
PLAYGROUND_DIR="$CRATE_DIR/playground"

if ! command -v wasm-pack >/dev/null 2>&1; then
    cat >&2 <<EOF
error: wasm-pack not found on PATH.

Install it with one of:
    curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh
    cargo install wasm-pack

Then re-run: scripts/playground.sh
EOF
    exit 1
fi

echo ">> Building mangle-wasm (wasm-pack --target web)"
# Output the generated JS/wasm glue into playground/pkg/ so index.html can load it
# with a relative import path (./pkg/mangle_wasm.js).
wasm-pack build \
    --target web \
    --out-dir "$PLAYGROUND_DIR/pkg" \
    "$CRATE_DIR"

if [[ "$BUILD_ONLY" -eq 1 ]]; then
    echo ">> Build complete. Open $PLAYGROUND_DIR/index.html via a static server."
    exit 0
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo "error: python3 not found; cannot start the local server." >&2
    echo "The built playground is at: $PLAYGROUND_DIR" >&2
    exit 1
fi

echo ">> Serving $PLAYGROUND_DIR on http://localhost:$PORT"
echo ">> Press Ctrl-C to stop."
cd "$PLAYGROUND_DIR"
exec python3 -m http.server "$PORT"
