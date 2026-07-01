#!/usr/bin/env bash
# Run the root workspace and every standalone WASM module's native test suite.
set -euo pipefail

cd "$(dirname "$0")"

cargo test --workspace
for manifest in modules-src/*/Cargo.toml; do
    module=$(basename "$(dirname "$manifest")")
    echo "==> testing $module"
    cargo test --manifest-path "$manifest"
done
