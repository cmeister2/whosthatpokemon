/** Exercise the actual static browser engine and public sample assets without a server. */
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { BrowserEngine, initSync } from "../docs/engine/whosthatpokemon.js";

const site = new URL("../docs/", import.meta.url);
const load = (path) => readFile(new URL(path, site));
initSync({ module: await load("engine/whosthatpokemon_bg.wasm") });
const manifest = JSON.parse(await load("assets/samples.json"));
assert.equal(manifest.catalog_count, manifest.references.length);
const engine = new BrowserEngine((await load(manifest.index)).toString());
try {
  for (const sample of manifest.samples) {
    // Native-only gallery examples have no legacy visual scan report.
    if (!sample.report) continue;
    const recorded = JSON.parse(await load(sample.report));
    const actual = JSON.parse(engine.scan(await load(sample.image), sample.cropped));
    assert.equal(actual.cards.length, sample.provenance.expected_card_count, sample.id);
    assert.equal(actual.cards.length, recorded.cards.length, sample.id);
    for (const [index, card] of actual.cards.entries()) {
      const saved = recorded.cards[index].recognition;
      assert.equal(card.recognition.candidates[0]?.card.id, saved.candidates[0]?.card.id, sample.id);
      assert.equal(card.recognition.status, saved.status, sample.id);
      assert.equal(card.recognition.identity_status, saved.identity_status, sample.id);
      assert.ok(card.crop_data_url.startsWith("data:image/png;base64,"));
      assert.ok(card.raw_crop_data_url.startsWith("data:image/png;base64,"));
      for (const evidence of card.visual_evidence) {
        if (evidence.method !== "global_descriptors") continue;
        const candidate = card.recognition.candidates.find((item) => item.card.id === evidence.id);
        const reconstructed = 0.20 * evidence.full_luminance + 0.45 * evidence.artwork_luminance
          + 0.30 * evidence.artwork_colour + 0.05 * evidence.artwork_hash;
        assert.ok(Math.abs(candidate.similarity - reconstructed) < 0.00001, "actual visual component weights");
      }
    }
    console.log(`${sample.id}: ${actual.cards.length} cards; live WASM agrees with recorded Rust output`);
  }
  const search = (observations) => JSON.parse(engine.text_search(JSON.stringify(observations)));
  assert.equal(search({ title: "Lightnlng BoIt" })[0].card.name, "Lightning Bolt");
  const rules = search({ rules: "Lightnlng Bolt deals 3 darnage to any target." });
  assert.ok(rules.length > 1 && rules.every((hit) => hit.card.name === "Lightning Bolt"));
  assert.deepEqual(search({ rules: "Draw a card." }), []);
  assert.deepEqual(search({ footer: "161" }), []);
  const footer = search({ footer: "0806 MSC EN" });
  assert.equal(footer.length, 1);
  assert.equal(footer[0].card.collector_number, "806");
  assert.equal(footer[0].language_match, true);
  console.log("Text: noisy title/rules retrieve reprints; exact footer resolves printing; short text stays unresolved");

  // This engine-level check supplies a known title to isolate the stage protocol;
  // the page itself obtains its title from the live browser OCR worker.
  const reference = manifest.references.find((card) => card.name === "Lightning Bolt");
  const extraction = JSON.parse(engine.extract(await load(reference.image), true));
  assert.equal(extraction.cards.length, 1);
  const crop = extraction.cards[0];
  assert.equal(crop.crop_data_url, crop.raw_crop_data_url);
  assert.equal(Object.hasOwn(crop, "recognition"), false, "extraction performs no matching");
  const cropBytes = Buffer.from(crop.raw_crop_data_url.split(",")[1], "base64");
  const titleCandidatesJson = engine.lookup_title(reference.name);
  const titleCandidates = JSON.parse(titleCandidatesJson);
  assert.equal(titleCandidates.title_identities.length, 1, "reprints occupy one title slot");
  assert.equal(titleCandidates.title_identities[0].exact, true);
  const candidateJson = engine.verify_candidates(cropBytes, titleCandidatesJson, 0);
  const candidate = JSON.parse(candidateJson);
  assert.equal(candidate.scope, "supplied_candidates");
  assert.equal(candidate.search_complete, false);
  assert.ok(candidate.descriptors_scored < manifest.catalog_count, "no whole-gallery descriptor pass");
  assert.ok(candidate.geometry_attempted > 0 && candidate.geometry_attempted <= 8);
  assert.ok(candidate.geometry_verified_count > 0, "actual correspondence and artwork verification");
  assert.ok(candidate.timings.total > 0 && candidate.timings.geometry_verification > 0, "host clock measures WASM work");
  const decision = JSON.parse(engine.combine_candidates(reference.name, titleCandidatesJson, candidateJson));
  assert.equal(decision.identity.status, "verified_shortlist");
  assert.equal(decision.identity.card.oracle_id, reference.oracle_id);
  assert.equal(decision.printing.card, null, "candidate-only scope does not accept a physical printing");
  assert.ok(decision.identity.sources.includes("visual_geometry"));
  assert.ok(decision.identity.sources.includes("title"));
  const emptyCandidatesJson = engine.lookup_title("");
  const emptyVisualJson = engine.verify_candidates(cropBytes, emptyCandidatesJson, 0);
  const emptyVisual = JSON.parse(emptyVisualJson);
  assert.equal(emptyVisual.descriptors_scored, 0);
  assert.equal(emptyVisual.geometry_attempted, 0);
  const emptyDecision = JSON.parse(engine.combine_candidates("", emptyCandidatesJson, emptyVisualJson));
  assert.equal(emptyDecision.identity.status, "unknown");
  assert.equal(emptyDecision.identity.card, null);
  assert.throws(() => engine.verify_candidates(cropBytes, titleCandidatesJson, 90), /0 or 180/);
  console.log("Pipeline: extraction -> indexed title lookup -> bounded geometry -> scoped identity; blank OCR abstains");
} finally {
  engine.free();
}
console.log(`Browser checks passed for ${fileURLToPath(site)}`);
