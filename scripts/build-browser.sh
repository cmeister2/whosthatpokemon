#!/usr/bin/env bash
# Rebuild the checked-in browser engine from the same Rust sources as the CLI.
set -euo pipefail

repository_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$repository_dir"
bindgen_command="${WASM_BINDGEN:-wasm-bindgen}"
bindgen_version="$("$bindgen_command" --version)"
if [[ "$bindgen_version" != "wasm-bindgen 0.2.129" ]]; then
    printf 'Expected wasm-bindgen 0.2.129, found %s\n' "$bindgen_version" >&2
    exit 1
fi

cargo build --locked --release --target wasm32-unknown-unknown --lib --features web
mkdir -p docs/engine
"$bindgen_command" --target web --out-dir docs/engine --no-typescript \
    target/wasm32-unknown-unknown/release/whosthatpokemon.wasm
