#!/usr/bin/env bash
# Manual experiment: WASM lifecycle hook plugin mutates prompt in a real agent turn.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

echo "==> Building hook-test WASM fixture (if needed)"
if [[ ! -f plugins/hook-test/hook_test.wasm ]]; then
  rustup target add wasm32-wasip1 >/dev/null 2>&1 || true
  (cd plugins/hook-test
   cargo build --target wasm32-wasip1 --release
   cp target/wasm32-wasip1/release/hook_test.wasm .)
fi

echo "==> Running integration smoke test (mock LLM + full agent::run path)"
cargo test -p zeroclaw-runtime --features plugins-wasm wasm_hook_plugin_mutates_system_prompt_in_agent_run -- --nocapture

echo ""
echo "OK — WASM hook plugin modified the system prompt before the LLM call."
echo "Next: try the live CLI path with hooks + plugins enabled in config.toml."
