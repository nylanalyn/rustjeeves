#!/usr/bin/env bash
# Build every module under modules-src/ to WebAssembly and install it into modules/
# (where the running bot auto-loads it). Run with no arguments to build all modules,
# or pass specific names: ./build-modules.sh admin weather
set -euo pipefail

cd "$(dirname "$0")"

TARGET=wasm32-unknown-unknown
DEST=modules
mkdir -p "$DEST"

# Make sure the wasm std library is available to the active Rust toolchain. This works whether
# Rust came from rustup or a distro package (dnf/apt/pacman) — we check the real sysroot rather
# than assuming rustup exists.
ensure_wasm_target() {
    local sysroot
    sysroot=$(rustc --print sysroot 2>/dev/null || echo "")
    if [ -n "$sysroot" ] && [ -d "$sysroot/lib/rustlib/$TARGET" ]; then
        return 0 # already present
    fi
    if command -v rustup >/dev/null 2>&1; then
        echo "Installing Rust target $TARGET via rustup ..."
        rustup target add "$TARGET"
        return 0
    fi
    cat >&2 <<EOF
ERROR: the '$TARGET' standard library isn't installed for your Rust toolchain,
and rustup isn't available to add it. Install the wasm std, then re-run this script:

  Fedora/RHEL : sudo dnf install rust-std-static-wasm32-unknown-unknown
  Debian/Ubuntu: use rustup (https://rustup.rs), then: rustup target add $TARGET
  Arch        : use rustup, then: rustup target add $TARGET

Alternatively, build the modules on a machine that has the wasm target and copy the
resulting modules/*.wasm to this server — the .wasm files are portable and need no
toolchain to run.
EOF
    exit 1
}
ensure_wasm_target
# rebuild the main bot

cargo build --release -p jeeves

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
