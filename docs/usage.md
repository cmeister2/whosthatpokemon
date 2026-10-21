# Usage

Run the examples from the repository root. With an installed binary, replace
the Cargo command prefix with `whosthatpokemon`.

The default portable matcher needs no model files or native inference runtime.
For CPU CLIP embeddings, see the [optional backend](clip.md). Native text reading
is covered in the [OCR guide](ocr.md).

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
Optional `oracle_text`, `printed_text`, `printed_name`, `type_line` and `layout`
fields are retained for text searches. Oracle IDs group reprints for the
card-identity result. Double-sided Scryfall
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

## Prepare a complete catalogue

Use official bulk metadata rather than paging a search across the entire game.
The following workflow retains **all-language metadata** and selects **English
paper printings** for the first visual gallery, including separate physical faces:

```sh
cargo run --release --locked -- mtg bulk \
  --kind all-cards --output data/mtg/bulk
cargo run --release --locked -- mtg prepare \
  --catalog data/mtg/bulk/all_cards.jsonl.gz \
  --output data/mtg/prepared --image-language en
cargo run --release --locked -- mtg fetch-images \
  --plan data/mtg/prepared/images.jsonl --images data/mtg/images --all --workers 4
```

`bulk` records snapshot provenance, supports resuming an interrupted transfer,
and validates the entire download before publishing it. `prepare` streams the
source into `metadata.jsonl`, a physical-face `images.jsonl` plan and
`coverage.json`. Coverage distinguishes metadata records, selected image faces,
languages, missing/placeholder images and identities without a selected reference.
Foreign-only and digital-only identities can remain searchable as metadata
without being visually enrolled under this paper/English selection policy.

`--all` acquires every remaining planned face. Alternatively, use `--limit 1000`
for bounded batches; the default limit is 100. Existing images are decoded before
reuse and completed downloads survive failures. `image-progress.json` records
live whole-plan counts and worker activity. After complete plan/path validation,
its `auditing` state tracks bounded parallel pixel checks through `audited_faces`
and `active_audits`. Cached counts increase as images validate; remaining counts
include faces still being checked. Network acquisition starts after the whole
audit completes. A failed request stops new work and records its printing, face
and URL. `--limit 0` performs the same pixel audit entirely offline.
Visual coverage is complete only when the report says `complete: true`.

### Native shard enrollment

After all planned images are available, build a native sharded index in a new
or empty output directory:

```sh
cargo run --release --locked -- mtg index \
  --catalog data/mtg/prepared/images.jsonl --images data/mtg/images \
  --shard-size 256 --workers 4 --output data/mtg/portable
cargo run --release --locked -- scan --index data/mtg/portable data/table.jpg
```

New native builds store lossless binary global fingerprints separately from
local geometric features. Opening validates all shards, including local-feature
files, and caches metadata. Subsequent global comparisons read only the smaller
global files; local features are read again when the complete-gallery global
threshold fails. Every reference participates in the printing and identity
margins. Legacy JSON shard directories still load.

Parallel enrollment preserves source order and descriptor values. Completed
shards have a checkpoint; repeat the index command with `--resume` after an
interruption to skip those faces. The final manifest is published only after
enrollment succeeds. Resume with the same catalogue and unchanged reference
images; pending faces since the last completed shard are recomputed.

Enrollment can also overlap a separate running `mtg fetch-images` process:

```sh
cargo run --release --locked -- mtg index \
  --catalog data/mtg/prepared/images.jsonl --images data/mtg/images \
  --shard-size 256 --workers 4 --wait-for-images 900 --output data/mtg/portable
```

`--wait-for-images` requires `--shard-size` and accepts 0–86,400 seconds **per
missing face**. The default, zero, fails immediately for a missing file. Waiting
only checks local paths; the indexer makes no network requests. A timeout retains
completed shard checkpoints, so rerun with `--resume` after acquisition continues.
An existing corrupt image fails decoding immediately rather than waiting for repair.
Use a new output directory for a fresh build, or add `--resume` for an existing
checkpoint.

Search remains exact and linear; sharding bounds descriptor memory rather than
making retrieval sublinear. Opening validates all shards, including local-feature
files; repeated global comparisons read the smaller global evidence. See
[matching and limits](matching.md) for the scoring and geometry behavior.

## Metadata text search

Text search can use metadata immediately, without any downloaded image gallery:

```sh
cargo run --release --locked -- mtg search \
  --catalog data/mtg/prepared/metadata.jsonl --title 'Lightnlng Bolt'
```

For repeated text or OCR queries, prepare a compact reusable metadata index:

```sh
cargo run --release --locked -- mtg text-index \
  --catalog data/mtg/prepared/metadata.jsonl --output data/mtg/text.index.gz
cargo run --release --locked -- mtg search \
  --text-index data/mtg/text.index.gz --title 'Lightnlng Bolt'
```

This pools metadata strings and caches normalized titles, rules sequences and
document-frequency weights. Printings sharing an alias reuse its text comparison;
the original scores, reasons and five-printing tie order are preserved. Images
and visual descriptors are not required. A raw `--catalog` search remains available.

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

See [Matching](matching.md) for result fields, confidence scores, and limitations.

Scene detection requires the complete physical card inside the image (or configured
detection window), with three pixels of clearance. Clipped outlines and
source-evidenced incomplete panels are withheld from recognition. Full mode checks
bounded coarse/color proposals against the original RGB boundaries; rectified
crops always use the original pixels. These safeguards do not prove that every
remaining outline is correct, especially with glare, occlusion or overlapping cards.

The browser camera also checks verified reference geometry against the source
frame. A conclusive outside-frame projection produces `capture_rejection` and no
accepted identity or printing; former decisions remain diagnostic proposals.
Missing or contradictory evidence prevents this rejection, so its absence is not
a completeness certificate. Explicitly cropped-input recognition is unchanged.

## Library

`mtg::build_index` imports local references; `RecognitionPipeline::scan` detects
and recognises a scene, while `recognize_crop` matches a supplied card crop.
`CardMetadata` and the `Matcher` trait allow other games and matching backends to
reuse the pipeline. `RecognitionPipeline::new` uses the matcher's default
thresholds; `with_config` accepts explicit options. The extractor uses decoded
images and RGB crops; callers
using transparent scene inputs should composite them before extraction.

Native callers can stream catalogue faces with `mtg::for_each_reference`, manage
explicit downloads through `mtg::bulk`, and use `matching::ShardedIndex` for
bounded descriptor storage. `mtg::evidence::TextIndex` retains reusable metadata
and normalized text evidence without loading reference images.

[Documentation](README.md)
