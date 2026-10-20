# Native Magic text OCR

Enable the native `ocr` feature to use
[`ocracle`](https://github.com/cmeister2/ocracle) directly as a Rust library.
Download its models using that project's instructions, then supply a model root
containing `models/profiles.json` and the referenced model files. Loading and
recognition use local files and never download models or submit images.

Run examples from the repository root. This example assumes the model registry
is under `data/ocracle/models/`:

```sh
cargo run --release --locked --features ocr -- mtg ocr \
  --model-root data/ocracle --cropped --layout classic \
  --output data/ocr.json data/card.jpg
```

Without `--cropped`, the command detects and rectifies scene cards to **500 × 700**
pixels before reading them. Normal visual `scan` uses the extractor's **250 × 350**
default. Already cropped OCR inputs bypass scene extraction. One recognizer is
loaded and reused across every card and input in the invocation.

`--language en` is the default. Another language needs a working profile and
models in the supplied registry; retaining multilingual catalogue metadata does
not make an OCR model support those languages. `--rotation 90`, `180` or `270`
explicitly rotates each crop before reading. Rotation is not inferred from an
OCR confidence score.

## Reading and field assignment

The whole card is read, including text over artwork. Output retains raw text,
detected lines and their crop-relative coordinates, reading scores, method,
actual model hashes and observed title/rules/footer fields. Catalogue text never
replaces an OCR reading.

The `modern` and `classic` portrait templates approximately group observed lines
into fields. Sleeve margins, unusual layouts, reflection and blur can misassign
or lose text. Full raw readings remain available even when a field is empty.
Reading scores describe OCR output; they are not card identity probabilities.

## Search catalogue metadata

Supply one optional metadata source to rank candidates alongside the reading:

```sh
cargo run --release --locked --features ocr -- mtg ocr \
  --model-root data/ocracle --cropped \
  --text-index data/mtg/text.index.gz data/card.jpg
```

Use `--text-index` for repeated retrieval, `--catalog data/cards.jsonl.gz` for
raw metadata, or `--index data/mtg.index.json` for metadata already present in a
portable visual index. A portable shard directory is also accepted by `--index`.
These sources are mutually exclusive. Omit all three for raw OCR alone.
See [metadata preparation](usage.md#metadata-text-search) to build a text index.

Candidates are independent text evidence, with no accepted-match status or
calibrated probability. An exact set/collector footer can outrank contradictory
title text; inspect the individual reasons and the reading. OCR/text scores do
not change the legacy `scan` decision. Neither scanning nor corrected text trains
or updates a model. See [matching policy](matching.md#search-ocr-evidence).

## Combine OCR and visual matching

Use `mtg identify` with a completed portable shard directory:

```sh
cargo run --release --locked --features ocr -- mtg identify \
  --index data/mtg/portable \
  --model-root data/ocracle --output data/identified.json data/photo.jpg
```

Identification builds reusable English-title and collector-footer lookups from
metadata already enrolled in the visual index. They contain only entries that
can be visually checked and omit the larger rules/all-language text index.
Supply `--text-index data/mtg/text.index.gz` when title lookup needs aliases from
that broader index. Footer proposals still use enrolled visual metadata.
Initialization times identify the visual, title, footer and OCR setup costs.

Add `--cropped` for already extracted cards. Every orientation attempt reads
both the title and the bottom collector bar using the same detector-free model.
Horizontal contrast locates the two collector rows within the bottom-left strip,
with fixed bands as a fallback. The rows are arranged side by side for one recognition call;
raw output and source/input pixel mappings remain available. The current footer
locator targets conventional bottom-left portrait collector rows. Unusual
positions, older layouts or missing identifiers can produce no footer match.

`--rotation` accepts `0` or `180`; the opposite orientation is tried when the
first combined result does not accept an identity. The faster
`--title-model small` remains the default recognizer for both fields; use
`--title-model default` for the language registry's larger model.
`--ocr-threads 4` is the default RTen worker count. The engine and its inference
pool are reused across every crop in the invocation.

Exact set/collector postings propose physical faces before visual verification.
Every matching face and language alternative remains visible. Known unprinted
Secret Lair collector suffixes are indexed as explicit aliases; collisions stay
ambiguous. Glued literal number/set fields are separated using registered keys,
without correcting OCR characters or replacing observed text. Copyright years
and SLX counterpart annotations cannot silently become collector keys.

When a usable collector key is present, the visual stage checks those exact
faces first. A failed pass remains in `footer_verification`, then title/footer
identity and artwork postings provide a bounded fallback. Otherwise it starts
with title-derived candidates. Compact descriptors score at most
`--max-candidates 256` faces per pass, and local geometry checks a
`--max-geometry 8` group-preserving shortlist. Title retrieval proposes at most
`--title-candidates 5` semantic identities. No pass scans unrelated catalogue
descriptors. `visual_strategy` records which candidate scope produced the result.

A footer alone cannot identify a card. Footer-only semantic support requires
actual geometric verification and whole-card descriptor agreement, complete
compatible footer-identity coverage, and no strong contradictory title/rules.
This matters for altered cards that retain an older card's printed collector
bar or artwork. Strong contradictions, language conflicts and omitted groups
remain explicit. Accepted identities retain their shortlist scope; the footer
does not establish a globally unique printing or calibrated confidence.

Output retains every attempted orientation, literal title and footer OCR, model
provenance, ranked candidates, geometry and the selected attempt. `ocr_ms`
includes both fields; `title_ocr_ms` and `footer_ocr_ms` show their separate costs.
`--trace-dir NEW_DIRECTORY` also exports actual card/title/footer pixels and the
composed footer model input after the processing clock stops. No reading trains
or updates a model.

The existing `scan` command remains the exhaustive visual path. Use it when an
exact comparison against every enrolled reference is required; its geometric
fallback can be expensive for a large catalogue.

## Camera frames

A decoded camera frame can be passed as an `image::DynamicImage`; a native
integration does not need to encode it as JPEG or write it to disk. Keep the
extractor, visual index, title/footer indexes and OCR engine alive across frames. The
camera application controls capture and frame scheduling.

For continuous capture, run identification on a worker with at most one pending
frame, replacing that pending frame when newer input arrives. Detection can run
at a different cadence from identification. With `magicextract` 0.3 or newer, a
tracker can pass its current source-pixel corners to `CardExtractor::rectify`
without rerunning outline detection. Submit the resulting stable crops for
OCR/matching and associate results with frame IDs or track IDs so a delayed
result does not label a different card. This scheduling belongs to the camera
application; `identify` is synchronous and does not provide capture or tracking.

Measure an optimized `--release` build. Initialization, full-frame extraction,
title/footer OCR, descriptor ranking and geometry are separate costs; crop latency is
not camera frame rate. Reuse recognition for a stable track rather than reading
the same card on every preview frame.

## Library and browser

Native callers can reuse
`mtg::ocr::MagicOcr::open_title_small_with_threads`, `read_title` and `read_footer`
across frames. Build `mtg::evidence::TitleIndex` and `FooterIndex` once from the
visual catalogue, then call `mtg::identify::identify_fields_with`. Its exact-face
stage uses `matching::ShardedIndex::recognize_faces_with`; the bounded fallback
uses `recognize_candidates_with`. The returned `FieldIdentification` contains
the shared decision in `result` and the visual-pass diagnostics alongside it;
JSON flattens these fields into one object. `identify_title_with` remains available for
callers supplying only title observations, and `identify` is the explicit
exhaustive visual/text path. Fusion does not require the `ocr` feature, so callers
can supply observations from another recognizer.

The GitHub Pages gallery displays recorded runs of the native CLI with ocracle,
including both observed fields and measured costs. Its JSON and pixel traces
are generated as deployment artifacts. That gallery viewer displays saved results;
its separate linked camera page runs actual recognition in the browser.
See [the pipeline gallery](matching.md#explore-the-pipeline).

## Browser camera

The linked camera page runs OCRacle's RTen recognizer in a dedicated Web Worker.
It consumes browser-decoded RGBA pixels, then uses the shared native Rust title
and collector-footer crops, opposite-orientation retry, bounded candidate lookup,
geometric verification and conflict-aware decisions. There is no OCR substitution
or server recognition. Start requests camera access; Stop and leaving the page
release its tracks and terminate pending work. Frames are sampled only while the
worker is free, so slow recognition cannot build a stale-frame queue.

The camera page loads the complete English paper printing metadata snapshot,
including separate physical faces, into compact title/footer and identity/artwork
postings. Its displayed counts and source date describe the actual deployed
snapshot. It downloads canonical reference images only after OCR has selected
candidates, then encodes their pixels with the native reference builder. The
visual cache is bounded to 512 faces; the matcher scores at most 256 proposed
faces and verifies at most eight geometric candidates per pass. It never scans
the entire catalogue's images on a frame. Cold scans can spend substantially more
time acquiring and preparing references than repeat scans, with those costs
shown separately. Reference images are fetched from Scryfall's public image CDN;
input camera frames remain in the tab. The four-card rendered example includes
a digital-only Vintage Masters Black Lotus image. The recorded gallery explicitly
enrolls that image in its curated set; the camera's English paper catalogue can
read its name while leaving that different artwork unresolved.

The English metadata is a compressed deployment artifact generated from Scryfall
bulk data. Other-language metadata in native catalogues stays available to native
callers; the browser snapshot and OCR profile are English. The 21 MB recognizer
and full metadata load once per active session. OCR readings are retained while
reference pixels arrive, so asynchronous lookup does not repeat OCR. Completed
cards are also retained while another card from the same frame is being resolved.

Browser timings measure the viewer's machine. They include separate extraction,
OCR, retrieval, matching and fusion measurements; rendering/PNG export and worker
response overhead remain visible separately. They are distinct from the gallery's
recorded native release timings. RTen uses browser WASM SIMD on one worker, so
native four-thread latency is not a browser speed claim. The page requires a
current browser with WebAssembly SIMD and a secure context (HTTPS or localhost)
for camera access. Public example buttons exercise the same actual browser engine
without requesting a camera.

Build the gallery normally, then run `scripts/build-camera.sh /tmp/camera-engine`
and `scripts/build-full-camera-catalogue.py --binary target/release/whosthatpokemon
--cache /tmp/camera-catalogue-cache --output /tmp/catalogue.json.gz`, followed by
`scripts/build-camera-page.py --metadata /tmp/catalogue.json.gz --cache
/tmp/page-resources --engine /tmp/camera-engine --output /tmp/pages`.
WASM, models, generated indexes and manifests stay outside the source tree and are
packaged by Pages CI. See [development](development.md) for the complete build and verification commands.

[Documentation](README.md)
