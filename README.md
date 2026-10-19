# whosthatpokemon

Recognise trading cards in images, starting with **Magic: The Gathering**. The
Rust library and CLI use [`magicextract`](https://github.com/cmeister2/magicextract)
to rectify card outlines, then return ranked reference matches with separate
printing and card-identity confidence.

Requires **Rust 1.90.0+**, edition **2024**. The default portable matcher needs no
model files or native inference runtime. An optional CPU CLIP backend accepts a
local ONNX vision model and ONNX Runtime shared library.

## Prepare references

Indexing and scanning work offline. Supply a Scryfall catalogue and local PNG,
JPEG, or WebP reference images:

```sh
cargo run --release -- mtg index \
  --catalog data/cards.jsonl.gz --images data/references --output data/mtg.index.json
```

Supported metadata formats are JSON arrays, JSON Lines, gzip-compressed JSON
Lines, complete Scryfall search/collection responses, and local manifests with
a `cards` array. Current [Scryfall bulk exports](https://scryfall.com/docs/api/bulk-data)
use `.jsonl.gz`; choose a catalogue covering the artwork and printings you need.
Paginated responses with `has_more: true` are rejected rather than silently
indexing one page.

Images are located as `<id>-<face_index>.png` or `.webp`, `.jpg`, `.jpeg` under
`--images`. Face zero also accepts `<id>.<extension>`. Alternatively, write a
local manifest such as `data/cards.json`:

```json
{
  "cards": [
    {
      "id": "local-lightning-bolt",
      "name": "Lightning Bolt",
      "file": "references/lightning-bolt.png"
    }
  ]
}
```

Explicit `file` paths are relative to the manifest. Optional metadata includes
`oracle_id`, `illustration_id`, `set`, `collector_number`, `lang`, and `source`.
Oracle IDs group reprints for the card-identity result. Double-sided Scryfall
cards enroll each physical face; split/adventure cards with a shared image enroll
once. Missing images, invalid metadata, and duplicate printing/face keys fail the
build. Enroll tight, upright, full-card reference images.

For an explicit, bounded online download:

```sh
cargo run --release -- mtg fetch \
  --query 'set:lea game:paper' --limit 100 --output data/lea
cargo run --release -- mtg index \
  --catalog data/lea/cards.json --images data/lea/images --output data/lea.index.json
```

`fetch` saves the selected metadata and PNG faces, records subset selection, and
reuses completed identical requests for 24 hours. Search pages are spaced at
least 550 ms apart; HTTP failures stop the download, including rate-limit
responses. Use bulk exports for large catalogues. Reference images remain
subject to their source attribution requirements. Local data, models, and
runtimes can live under gitignored `data/`.

## Scan images

Scan a photograph containing one or more cards:

```sh
cargo run --release -- scan --index data/mtg.index.json data/table.jpg
```

For an already cropped card, bypass outline detection. Multiple inputs produce
one JSON array in input order:

```sh
cargo run --release -- scan --index data/mtg.index.json --cropped \
  --top-k 5 --min-similarity 0.80 --min-margin 0.03 \
  --output data/results.json data/card-a.png data/card-b.jpg
```

JSON goes to standard output unless `--output` is supplied. Each entry has an
`input` path and `result`. Scene results include image dimensions and a `cards`
array containing source-image `corners`, `area`, and `recognition`; crop results
contain recognition directly. The CLI applies EXIF orientation before scanning,
so coordinates refer to the oriented image.

Recognition contains:

- `status` and `confidence` for the exact printing/face.
- `identity_status` and `identity_confidence`, grouping printings by Oracle ID
  when available.
- Ranked `candidates` with card metadata, absolute `similarity`, heuristic
  `confidence`, and the query's `rotation_degrees`.

Statuses are `matched`, `ambiguous`, or `unknown`. Similarity and confidence lie
in `[0, 1]` and **are not calibrated probabilities**. Identical artwork can give
a strong card identity while leaving the printing ambiguous. `unknown` indicates
insufficient visual evidence; a scene with no detected outline instead has an
empty `cards` array. Default portable thresholds are `0.80` absolute similarity
and `0.03` competing-match margin; adjust them for your catalogue and captures.

The portable matcher combines full-card luminance, artwork structure and colour,
perceptual hashing, four right-angle orientations, and several reference insets.
When no candidate reaches the absolute threshold, a local-feature fallback matches
oriented FAST/BRIEF descriptors and verifies a projective alignment with
deterministic RANSAC. Artwork coverage and aligned appearance checks help reject
matches supported only by a shared frame or text. Both stages run in Rust.
Portable index format 2 stores compact descriptors and local features; rebuild
older indexes when their format is rejected.

Recognition is limited to the supplied catalogue. Glare, sleeves, blur,
occlusion, extraction errors, and unusual layouts can reduce accuracy. A single
reference scan cannot reliably establish foil finish, condition, or differences
that are absent from the image. Review ambiguous results before using an exact
printing identity. The extractor identifies geometry rather than the game or
artwork; its geometric output does not itself establish recognition confidence.
Search is exhaustive: larger catalogues increase memory use and latency,
especially when local geometric verification runs.

## Optional CPU CLIP backend

Build with `--features clip` and supply a compatible vision ONNX model plus an
[ONNX Runtime](https://github.com/microsoft/onnxruntime/releases) shared library
supporting API 24 (ONNX Runtime 1.24 or newer):

The following Linux x86-64 setup uses a compatible
[quantized CLIP vision export](https://huggingface.co/Xenova/clip-vit-base-patch32/blob/6ef1ebc8b0766a7a8d11b146462c99cdf74dd22d/onnx/vision_model_quantized.onnx)
and the [official CPU runtime](https://github.com/microsoft/onnxruntime/releases/tag/v1.31.0).
These are explicit downloads; each checksum verifies the selected artifact.

```sh
mkdir -p data/models data/runtime
curl -fL \
  'https://huggingface.co/Xenova/clip-vit-base-patch32/resolve/6ef1ebc8b0766a7a8d11b146462c99cdf74dd22d/onnx/vision_model_quantized.onnx' \
  -o data/models/vision_model.onnx
printf '%s  %s\n' \
  '583fd1110a514667812fee7d684952aaf82a99b959760c8d7dca7e0ab9839299' \
  'data/models/vision_model.onnx' | sha256sum --check

curl -fL \
  'https://github.com/microsoft/onnxruntime/releases/download/v1.31.0/onnxruntime-linux-x64-1.31.0.tgz' \
  -o data/runtime/onnxruntime-linux-x64-1.31.0.tgz
printf '%s  %s\n' \
  'cc5c72baf5ae5c8238a6841f0897227be2d02826b9cf98eaf02fdefaeeb03a57' \
  'data/runtime/onnxruntime-linux-x64-1.31.0.tgz' | sha256sum --check
tar -xzf data/runtime/onnxruntime-linux-x64-1.31.0.tgz -C data/runtime
```

Build an index and scan with those files:

```sh
cargo run --release --features clip -- mtg index \
  --backend clip --model data/models/vision_model.onnx \
  --runtime data/runtime/onnxruntime-linux-x64-1.31.0/lib/libonnxruntime.so \
  --catalog data/cards.json --images data/references --output data/mtg.clip.json

cargo run --release --features clip -- scan \
  --backend clip --model data/models/vision_model.onnx \
  --runtime data/runtime/onnxruntime-linux-x64-1.31.0/lib/libonnxruntime.so \
  --index data/mtg.clip.json --cropped data/card-a.png
```

For other platforms, select a CPU package from the official runtime releases or
[installation instructions](https://onnxruntime.ai/docs/install/) and pass its
shared-library path. The CLI
expects `pixel_values` float input `[1, 3, 224, 224]` and pooled `image_embeds`
output `[1, dimensions]`, with CLIP RGB normalization and bicubic center cropping.
Supply a vision export matching that contract; a text encoder or unpooled
transformer output is incompatible. The library also exposes explicit
preprocessing settings. Model files and native runtimes are never downloaded
automatically.

CLIP indexes record the actual model SHA-256, preprocessing, and embedding
dimension; scans require the same model and transform. CLIP's exploratory CLI
defaults are `--min-similarity 0.90 --min-margin 0.04`; they can be overridden and
do not establish calibrated accuracy. CLIP embeddings can also confuse similar
printings. The optional `ort` dependency is pinned to the tested release-candidate
API-24 stack; newer releases requiring Rust 1.92 would change this project's
compatibility contract.

Score scales and defaults differ between backends. Probability estimates require
independent calibration and evaluation on representative captures.

## Library

`mtg::build_index` imports local references; `RecognitionPipeline::scan` detects
and recognises a scene, while `recognize_crop` matches a supplied card crop.
`CardMetadata` and the `Matcher` trait allow other games and matching backends to
reuse the pipeline. `RecognitionPipeline::new` uses the matcher's default
thresholds; `with_config` accepts explicit options. The extractor uses decoded
images and RGB crops; callers
using transparent scene inputs should composite them before extraction.

## Development

Run the CI checks on stable and the declared MSRV:

```sh
cargo check --locked --all-targets --all-features
cargo test --locked --all-targets --all-features
cargo test --locked --doc --all-features
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings

cargo +1.90.0 check --locked --all-targets --all-features
cargo +1.90.0 test --locked --all-targets --all-features
cargo +1.90.0 test --locked --doc --all-features
cargo +1.90.0 fmt --all -- --check
cargo +1.90.0 clippy --locked --all-targets --all-features -- -D warnings
```

These compile and test both backends without downloading a model or loading a
native runtime. Public and private Rust items require documentation, including
actual failure/panic conditions where applicable. CI separately runs
`cargo msrv verify -- cargo check --locked --all-targets --all-features` and
`cargo llvm-cov --all-features --workspace --locked`. Install those tools using
the stable toolchain; coverage requires `llvm-tools-preview`.

Release tooling requires Node.js 24.15.0+ (or Node 22.22.2+) and is private npm
tooling for this Rust crate:

```sh
npm ci
npm test
```

The regression exercises version preparation and notes rendering through
semantic-release's actual dependency graph. Preserve the pinned Conventional
Commits preset and release-notes-generator override together with the lockfile.
Renovate extends `github>cmeister2/renovate`.

## Releases

Registry publication currently requires a dependency update: `magicextract` is
pinned to public Git revision `2c81797da2dc5760fbc5944818d59b436c0b5fe8` and has
no published crates.io release. Cargo rejects publishing a crate whose Git
dependency has no matching registry version. Publish `magicextract` first, then
update and verify its registry dependency before enabling releases here.

The configured repository is `https://github.com/cmeister2/whosthatpokemon`.
Before publishing, add a **`CARGO_REGISTRY_TOKEN`** repository Actions secret
authorized to publish this crate. The release workflow uses `GITHUB_TOKEN` for
tags and GitHub releases. After all checks pass, only pushes to `main` or `master`
run semantic-release and publish to crates.io. Complete history/tags are fetched;
tags are plain SemVer such as `1.2.3`.

Preserve meaningful Conventional Commits when merging: `fix:`/`perf:` request
patch releases, `feat:` requests minor, and a `BREAKING CHANGE:` footer requests
major. The analyzer uses its default preset; use the footer for breaking changes.
Ordinary `docs:`, `test:`, `ci:`, and `chore:` commits alone do not release.

Keep the checked-in version at **`0.0.0`** in both `Cargo.toml` and this crate's
`Cargo.lock` entry. Preparation injects a release version into the build checkout:

```sh
node scripts/prepare-release.mjs 1.2.3
```

When no release is selected, CI builds with `0.0.<github.run_id>` without
publishing that fallback. Do not commit injected versions.

## License

[MIT](LICENSE).
