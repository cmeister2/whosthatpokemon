# Usage

Run the examples from the repository root. With an installed binary, replace
`cargo run --release --` with `whosthatpokemon`.

The default portable matcher needs no model files or native inference runtime.
For CPU CLIP embeddings, see the [optional backend](clip.md).

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

## Library

`mtg::build_index` imports local references; `RecognitionPipeline::scan` detects
and recognises a scene, while `recognize_crop` matches a supplied card crop.
`CardMetadata` and the `Matcher` trait allow other games and matching backends to
reuse the pipeline. `RecognitionPipeline::new` uses the matcher's default
thresholds; `with_config` accepts explicit options. The extractor uses decoded
images and RGB crops; callers
using transparent scene inputs should composite them before extraction.

[Documentation](README.md)
