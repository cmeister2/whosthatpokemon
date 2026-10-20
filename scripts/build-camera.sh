#!/usr/bin/env bash
# Compile the real OCRacle + matching pipeline into an external Pages deployment input.
set -euo pipefail
repository_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$repository_dir"
if [[ "$#" != 1 ]]; then
    printf 'Usage: scripts/build-camera.sh EXTERNAL_ENGINE_OUTPUT\n' >&2
    exit 1
fi
camera_output="$(realpath -m -- "$1")"
case "$camera_output" in
    "$repository_dir"|"$repository_dir"/*) printf 'Use an output outside the repository.\n' >&2; exit 1 ;;
esac
if [[ -e "$camera_output" ]]; then
    printf 'Engine output must not already exist.\n' >&2
    exit 1
fi
bindgen_command="${WASM_BINDGEN:-wasm-bindgen}"
if [[ "$("$bindgen_command" --version)" != 'wasm-bindgen 0.2.129' ]]; then
    printf 'Expected wasm-bindgen 0.2.129.\n' >&2
    exit 1
fi
# Portable SIMD is supported by current browsers; no shared memory or COOP/COEP is required.
RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+simd128" \
    cargo build --locked --release --target wasm32-unknown-unknown --lib --features web,ocr
camera_target="${CARGO_TARGET_DIR:-target}"
mkdir -p -- "$camera_output"
"$bindgen_command" --target web --out-dir "$camera_output" --no-typescript \
    "$camera_target/wasm32-unknown-unknown/release/whosthatpokemon.wasm"
