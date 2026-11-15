#!/usr/bin/env bash
# Wrapper around `scripts/demo/cli.mjs` ensuring macOS PATH (`rustup`, `node`, `swiftc`, `kubectl`, `helm`).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export PLUGIN_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
export OPENIDEA_ROOT="${OPENIDEA_ROOT:-$(cd "$PLUGIN_ROOT/../openidea" && pwd)}"
export KUBE_APP_ROOT="${KUBE_APP_ROOT:-$SCRIPT_DIR/fixtures/kube-app}"
export DEMO_OUT_DIR="${DEMO_OUT_DIR:-$(cd "$PLUGIN_ROOT/.." && pwd)}"
export KUBE_CONTEXT="${KUBE_CONTEXT:-docker-desktop}"
export VOICE="${VOICE:-en-US-AndrewNeural}"
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

if [[ $# -eq 0 ]]; then
  exec node "$SCRIPT_DIR/cli.mjs" build --mode both
elif [[ "$1" == --* ]]; then
  exec node "$SCRIPT_DIR/cli.mjs" build "$@"
else
  exec node "$SCRIPT_DIR/cli.mjs" "$@"
fi
