# whosthatpokemon

Recognise trading cards in photos, starting with **Magic: The Gathering**.
A Rust library and CLI using [magicextract](https://github.com/cmeister2/magicextract),
with ranked matches and confidence scores. Requires **Rust 1.90.0+**.

[Try the pipeline explorer](https://cmeister2.github.io/whosthatpokemon/)
to inspect extraction, matching, and OCR in your browser.

## Quick start

Fetch a small reference set, build its index, then scan a photo:

```sh
cargo install whosthatpokemon --locked
whosthatpokemon mtg fetch --query 'set:lea game:paper' --limit 100 --output data/lea
whosthatpokemon mtg index --catalog data/lea/cards.json --images data/lea/images \\
  --output data/lea.index.json
whosthatpokemon scan --index data/lea.index.json photo.jpg
```

Recognition searches the supplied catalogue; add the cards you want to recognise.

## Documentation

- [Usage](https://github.com/cmeister2/whosthatpokemon/blob/main/docs/usage.md): catalogues, scanning, and library integration.
- [Matching](https://github.com/cmeister2/whosthatpokemon/blob/main/docs/matching.md): results, confidence, OCR, and limitations.
- [CLIP backend](https://github.com/cmeister2/whosthatpokemon/blob/main/docs/clip.md): optional model and runtime setup.
- [Development](https://github.com/cmeister2/whosthatpokemon/blob/main/docs/development.md): checks, explorer builds, and releases.

[MIT](LICENSE).
