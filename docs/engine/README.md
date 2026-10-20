# Browser engine

`whosthatpokemon.js` and `whosthatpokemon_bg.wasm` are generated from this
repository's Rust library. They execute `magicextract`, indexed title lookup,
candidate-only visual verification and evidence combination locally in the
browser. No photo upload or recognition server is involved.

Rebuild with the Rust WebAssembly target and the matching binding generator:

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129 --locked
bash scripts/build-browser.sh
```

Alternatively set `WASM_BINDGEN` to a matching official prebuilt executable.
The generated files are checked in so GitHub Pages can serve the static site.

The JavaScript API exports an asynchronous default `init()` function and
`BrowserEngine`. Construct the engine once with the validated reference-index
JSON. The explorer automatically runs these stages on a bundled public example:

1. `extract(imageBytes, alreadyCropped)` finds boundaries and returns actual
   perspective-corrected PNG crops without a visual search.
2. Self-hosted Tesseract reads a crop's title band in JavaScript. The native
   library uses ocracle; the two OCR backends have different performance.
3. `lookup_title(title)` uses the startup-built title index to propose up to five
   card identities. Reprints of one identity do not become independent votes.
4. `verify_candidates(cropBytes, candidatesJson, rotationDegrees)` scores only
   the proposed candidate closure, with limits of 256 descriptors and eight
   geometry checks. It never falls back to a complete-catalogue search. The
   orientation is 0 or 180 degrees; the browser tries the opposite orientation
   if the first attempt does not identify the card.
5. `combine_candidates(title, candidatesJson, visualJson)` applies the shared
   Rust evidence policy to produce separate identity and printing decisions.
   Text proposals alone do not accept a card. A card identity can be verified
   within the shortlist while its exact printing remains ambiguous.

These engine methods return JSON strings. The title lookup, candidate matching
and evidence combination use the same Rust implementations as the native path;
WASM geometry verification runs serially. The page measures each stage as it
runs and shows one-time model/index setup separately from processing. It does
not load prerecorded results or benchmark timings.

The earlier `scan(imageBytes, alreadyCropped)` and
`text_search(observationsJson)` APIs remain available for diagnostic callers and
asset-generation checks. They return independent visual and text rankings and
are not the explorer's default identification route. Scan diagnostics include
the original perspective-corrected PNG, its proposed upright orientation and
actual visual components. A geometric fallback is marked explicitly; its final
score is not the global descriptor sum.

Photo inputs are limited before full decoding to 32 MiB, 8192 pixels per edge,
and 16 megapixels, with an additional 128 MiB decoder allocation budget. Scene
extraction returns at most 16 detections. Already-cropped inputs are reduced to
a 700-pixel long edge for diagnostics; source dimensions and corner coordinates
remain those of the oriented original. Embedded crop images have a combined
16 MiB budget per report.
