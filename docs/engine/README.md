# Browser engine

`whosthatpokemon.js` and `whosthatpokemon_bg.wasm` are generated from this
repository's Rust library. They execute `magicextract`, the portable visual
matcher and independent metadata text search locally in the browser. No photo
upload or recognition server is involved.

Rebuild with the Rust WebAssembly target and the matching binding generator:

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129 --locked
bash scripts/build-browser.sh
```

Alternatively set `WASM_BINDGEN` to a matching official prebuilt executable.
The generated bindings remain checked in for browser integrations.

The published gallery does not invoke this WebAssembly engine. It displays
recorded runs of the production native Rust CLI with ocracle, generated during
the site build; see [the public asset guide](../assets/README.md). This avoids
presenting another OCR backend or browser timings as the native library's
behaviour. The bindings remain available for browser integrations and tests.

The JavaScript API exports an asynchronous default `init()` function and
`BrowserEngine`. Construct the engine with validated reference-index JSON, then
call `scan(imageBytes, alreadyCropped)` or
`text_search(observationsJson)`. Both methods return JSON strings. The latter
accepts supplied text observations; it does not run OCR.

The staged `extract`, `lookup_title`, `verify_candidates` and
`combine_candidates` APIs also remain available. They separate extraction,
indexed title retrieval, bounded visual verification and evidence fusion for
browser integrations supplying their own OCR. The static gallery invokes the
native CLI instead.

These APIs return independent visual and text rankings. They remain available
for browser integrations, diagnostic callers and asset-generation checks; they
do not implement the native gallery's OCR-first identification route. Scan
diagnostics include the original perspective-corrected PNG, proposed upright
orientation and actual visual components. A geometric fallback is marked
explicitly; its final score is not the global descriptor sum.

Photo inputs are limited before full decoding to 32 MiB, 8192 pixels per edge,
and 16 megapixels, with an additional 128 MiB decoder allocation budget. Scene
extraction returns at most 16 detections. Already-cropped inputs are reduced to
a 700-pixel long edge for diagnostics; source dimensions and corner coordinates
remain those of the oriented original. Embedded crop images have a combined
16 MiB budget per report.
