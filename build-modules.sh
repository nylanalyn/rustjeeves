#!/usr/bin/env bash
# Build every module under modules-src/ to WebAssembly and install it into modules/
# (where the running bot auto-loads it). Run with no arguments to build all modules,
# or pass specific names: ./build-modules.sh admin weather
set -euo pipefail

cd "$(dirname "$0")"

TARGET=wasm32-unknown-unknown
DEST=modules
mkdir -p "$DEST"

# Make sure the wasm target is installed (one-time).
if command -v rustup >/dev/null 2>&1; then
    if ! rustup target list --installed 2>/dev/null | grep -q "^$TARGET\$"; then
        echo "Installing Rust target $TARGET ..."
        rustup target add "$TARGET"
    fi
fi

# Decide which modules to build: the named ones, or every crate under modules-src/.
mods=()
if [ "$#" -gt 0 ]; then
    mods=("$@")
else
    for d in modules-src/*/; do
        [ -f "${d}Cargo.toml" ] && mods+=("$(basename "$d")")
    done
fi

if [ "${#mods[@]}" -eq 0 ]; then
    echo "No modules found under modules-src/."
    exit 0
fi

for m in "${mods[@]}"; do
    src="modules-src/$m"
    if [ ! -f "$src/Cargo.toml" ]; then
        echo "skip: no crate at $src"
        continue
    fi
    echo "==> building $m"
    ( cd "$src" && cargo build --release --target "$TARGET" )
    # Each module crate is its own workspace, so its wasm lives in its own target/ dir.
    wasm=$(ls "$src/target/$TARGET/release/"*.wasm 2>/dev/null | head -n1 || true)
    if [ -n "$wasm" ]; then
        cp "$wasm" "$DEST/$(basename "$wasm")"
        echo "    installed $DEST/$(basename "$wasm")"
    else
        echo "    WARNING: no .wasm produced for $m"
    fi
done

echo "done. modules/: $(ls "$DEST"/*.wasm 2>/dev/null | xargs -n1 basename 2>/dev/null | tr '\n' ' ')"
