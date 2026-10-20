# Matching

## Results and scores

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
Single-file portable JSON indexes use format 2; native shard directories use
separate binary global and local evidence files. Rebuild older indexes when their
format is rejected.

The global score uses fixed coefficients, rather than trained weights:

| Component | Weight |
| --- | ---: |
| Full-card luminance | 20% |
| Artwork luminance | 45% |
| Artwork perceptual hash | 5% |
| Artwork colour distribution | 30% |

Each query is tried at 0°, 90°, 180° and 270°, against reference insets of 0%, 3%,
6%, 10% and 15%. The best comparison for each reference enters the full-catalogue
ranking. The geometric fallback runs only when no global candidate reaches the
absolute threshold; ambiguity alone does not trigger it. Confidence uses the
heuristic `similarity × clamp(margin / 0.06, 0, 1)`; unknown results receive zero.
When geometric fallback finds matching artwork shared by other enrolled
printings, the printing result remains ambiguous with zero confidence. Identity
also remains ambiguous when that illustration spans different Oracle identities.
Scans and corrected OCR text do not update reference descriptors or train a model.


Recognition is limited to the supplied catalogue. Glare, sleeves, blur,
occlusion, extraction errors, and unusual layouts can reduce accuracy. A single
reference scan cannot reliably establish foil finish, condition, or differences
that are absent from the image. Review ambiguous results before using an exact
printing identity. The extractor identifies geometry rather than the game or
artwork; its geometric output does not itself establish recognition confidence.
Search is exhaustive: larger catalogues increase memory use and latency,
especially when local geometric verification runs.

Native callers can use `ShardedIndex::recognize_candidates_with`, or the
[`mtg identify` command](ocr.md#combine-ocr-and-visual-matching), to score only
the indexed closure of OCR title identities. Compact descriptors rank that
closure before a smaller group-preserving geometric shortlist; neither stage
walks unrelated gallery records. Results record descriptor and geometry scope,
omitted groups and exhausted budgets. A verified candidate identity still cannot
claim separation from identities that OCR did not propose. The older
`recognize_shortlisted` API keeps its complete global scan for explicit exhaustive
recovery while bounding only its geometric work.

Native `scan` rectifies scene cards to the extractor's default **250 × 350**
pixels. The separate native OCR command requests **500 × 700** scene crops for
text reading. `--cropped` bypasses scene extraction and uses the supplied crop.

## Search OCR evidence

Use observed or corrected OCR text with a portable index:

```sh
cargo run --release -- mtg search --index data/mtg.index.json \
  --title 'Lightnlng Bolt' --footer '00127/269 C M15 EN' \
  --rules 'Lightning Bolt deals 3 damage to any target.'
```

The independent top-five text ranking returns title edit similarity, fuzzy rules
similarity, rules coverage, exact footer evidence and reasons. Rules compare
distinctive words against `printed_text` and `oracle_text`, weighted across card
identities so repeated printings do not dominate. Short phrases such as “draw a
card” are ignored. Longer boilerplate, negation, OCR symbol loss, errata and
translation can still produce misleading candidates. Rules and titles commonly
identify a card while leaving its printing unresolved. Footer parsing preserves
collector suffixes, excludes fraction denominators and copyright-year evidence,
and treats an unqualified four-digit year as unresolved. Shared-image
split/adventure references currently need top-level text in the supplied
catalogue for text retrieval; individual face text is preserved for physical
double-faced references.

Text ranking has no matched/ambiguous/unknown acceptance state. An exact
set-and-collector footer can outrank a contradictory title because footer
agreement receives a fixed high score. It does not test whether the title and
footer describe the same card. Inspect the returned evidence rather than treating
the first result or its score as an accepted identification.

The combined identifier searches each text field separately and verifies their
candidate union visually. It retains contradictions rather than letting a single
footer lookup replace title or rules candidates. See the [combined matching
guide](ocr.md#combine-ocr-and-visual-matching) for its scoped decisions and limits.

Metadata can also be searched with `--catalog` or a reusable `--text-index`
without enrolling images; see [metadata text search](usage.md#metadata-text-search).
The [native OCR guide](ocr.md) explains how to obtain raw readings with ocracle.

## Explore the pipeline

The [pipeline gallery](https://cmeister2.github.io/whosthatpokemon/) opens a
complete public example without requiring an upload. It follows actual runs of
the native Rust library: source picture, detected boundaries and magicextract
rectifications, literal ocracle title and collector-bar readings, indexed
candidate lookup, shortlisted visual comparisons and the returned ranked matches,
alongside combined identity/printing decisions. Other examples include a
four-card scene with Counterspell, an empty scene, three Secret Lair variants and the
Christopher Rush Alpha Lightning Bolt scan. Detected cards and recorded
orientation attempts can be inspected individually.

These are production `mtg identify` results generated in a release build, not an
interactive approximation using WebAssembly or another OCR backend. The page
shows the real card/title/footer PNG traces, exact OCR model provenance, retrieval
candidates, geometric evidence and final native decisions. Failed readings,
abstentions and conflicting orientations remain visible. Titles propose identities
and collector keys propose physical faces before visual verification. Reprints
do not add independent votes. The visual stage never falls back to checking the
entire catalogue.

Stage durations were measured on the generation machine. Its CPU, source
revision, model hash and catalogue scope appear in the run details. The scene
total covers all detected cards; OCR, lookup, verification and decision timings
cover the selected card, including both orientations when attempted. Model and
index loading is separate, and gallery PNG export is excluded. The browser
only displays the recorded native output: these are not viewer-device timings
or full-catalogue accuracy measurements.

The site build generates the gallery JSON and native image traces outside the
repository; they are deployment artifacts rather than committed evidence. See
[the public asset guide](assets/README.md) for reproduction. The published page
has no upload, OCR-model download or card-adjustment flow. Conventional portrait
title regions can fail on unusual frames, and a verified card identity can still
leave the printing ambiguous. Similarity and OCR scores remain heuristics, not
calibrated identity probabilities. The final panel displays each returned
printing’s visual similarity on its original 0–1 scale, with geometric agreement
and the native identity/printing decision. The bars preserve those scores; they
are not normalized into probabilities. Native full-catalogue confidence remains
unestablished when only a shortlist was checked; if the native search was
complete, the decision details show its uncalibrated printing confidence.

[Documentation](README.md)
