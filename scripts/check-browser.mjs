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
} finally {
  engine.free();
}
console.log(`Browser checks passed for ${fileURLToPath(site)}`);
