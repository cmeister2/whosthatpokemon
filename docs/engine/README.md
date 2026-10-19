# Browser engine

`whosthatpokemon.js` and `whosthatpokemon_bg.wasm` are generated from this
repository's Rust library. They execute `magicextract`, the portable matcher,
and the separate text-evidence search locally in the browser. No photo upload
or recognition server is involved.

Rebuild with the Rust WebAssembly target and the matching binding generator:

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129 --locked
bash scripts/build-browser.sh
```

Alternatively set `WASM_BINDGEN` to a matching official prebuilt executable.
The generated files are checked in so GitHub Pages can serve the static site.

The JavaScript API exports an asynchronous default `init()` function and
`BrowserEngine`. Construct the engine with the validated reference-index JSON,
then call `scan(imageBytes, alreadyCropped)` or `text_search(observationsJson)`.
Both methods return JSON strings. Scan diagnostics include the original
perspective-corrected PNG, its proposed upright orientation, and the actual
visual components for every returned candidate. A geometric fallback is marked
explicitly; its final score is not the global descriptor sum.

Photo inputs are limited before full decoding to 32 MiB, 8192 pixels per edge,
and 16 megapixels, with an additional 128 MiB decoder allocation budget. Scene
extraction returns at most 16 detections. Already-cropped inputs are reduced to
a 700-pixel long edge for diagnostics; source dimensions and corner coordinates
remain those of the oriented original. Embedded crop images have a combined
16 MiB budget per report.
