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

By default, identification builds a small English-title lookup from the metadata
already enrolled in the visual index. It contains only identities that can be
visually checked and does not load rules, footer text or the all-language
catalogue. Supply `--text-index data/mtg/text.index.gz` only when identification
needs aliases from that broader metadata index. The output records which title
source was used and its initialization time.

Add `--cropped` for already extracted cards. Identification crops only the title
band and runs detector-free single-line OCR. `--rotation` accepts `0` or `180`;
the opposite orientation is tried only when the expected orientation fails to
produce an accepted identity. The faster `--title-model small` is the default;
use `--title-model default` for the language registry's larger recognizer.
`--ocr-threads 4` is the default RTen worker count. Measure other counts on the
target device; the engine retains its private inference pool across the batch.

Exact title postings return in microseconds. Noisy titles use a bounded fuzzy
alias search. The resulting semantic identities expand through visual-index
postings, so a scan never walks every catalogue descriptor. Compact descriptors
rank at most `--max-candidates 256` title-derived faces, then local geometry checks
a group-preserving `--max-geometry 8`. At most `--title-candidates 5` semantic
identities are proposed. These limits are work budgets, not elapsed-time promises.

Output retains each attempted orientation, literal OCR, candidate rankings,
geometry and the selected attempt. A `verified_shortlist` identity requires the
visual winner to pass geometry, have sufficient scoped separation and occur in
the title alternatives. Every proposed identity and relevant artwork group must
be represented. Identities that OCR never proposed remain untested, so this path
does not accept an exact printing. Strong disjoint orientation proposals are
reported as a conflict rather than hidden.

The existing `scan` command remains the exhaustive visual path. Use it when an
exact comparison against every enrolled reference is required; its geometric
fallback can be expensive for a large catalogue.

## Camera frames

A decoded camera frame can be passed as an `image::DynamicImage`; a native
integration does not need to encode it as JPEG or write it to disk. Keep the
extractor, visual index, title index and OCR engine alive across frames. The
browser demo currently captures individual stills from its camera preview.

For continuous capture, run identification on a worker with at most one pending
frame, replacing that pending frame when newer input arrives. Detection can run
at a different cadence from identification. With `magicextract` 0.3 or newer, a
tracker can pass its current source-pixel corners to `CardExtractor::rectify`
without rerunning outline detection. Submit the resulting stable crops for
OCR/matching and associate results with frame IDs or track IDs so a delayed
result does not label a different card. This scheduling belongs to the camera
application; `identify` is synchronous and does not provide capture or tracking.

Measure an optimized `--release` build. Initialization, full-frame extraction,
title OCR, descriptor ranking and geometry are separate costs; crop latency is
not camera frame rate. Reuse recognition for a stable track rather than reading
the same card on every preview frame.

## Library and browser

Native callers can reuse
`mtg::ocr::MagicOcr::open_title_small_with_threads` and `read_title` across
images. Build `mtg::evidence::TitleIndex` from the loaded visual index, then call
`mtg::identify::identify_title_with` to combine indexed title retrieval with
`matching::ShardedIndex::recognize_candidates_with`. The older
`identify` API remains the explicit exhaustive visual/text path. Text observations
can come from another recognizer, and fusion does not require the `ocr` feature.
The `ocr` Cargo feature is independent of the optional CLIP matching backend.
The browser explorer uses its bundled local Tesseract.js engine; it does not run
the native ocracle adapter. See [browser OCR and its limits](matching.md#explore-the-pipeline)
and [OCR component licences](ocr/NOTICE.md).

[Documentation](README.md)
