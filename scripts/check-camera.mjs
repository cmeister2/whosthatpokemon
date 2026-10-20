/** Run the actual deployed WASM OCR model and compare public decoded frames with native Rust. */
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { createHash } from 'node:crypto';
import { gunzipSync } from 'node:zlib';

const [sitePath, fixturesPath] = process.argv.slice(2);
assert.ok(sitePath && fixturesPath, 'usage: node scripts/check-camera.mjs DEPLOYMENT TEMP_FIXTURES');
const site = path.resolve(sitePath);
const load = relative => readFile(path.join(site, relative));
const manifest = JSON.parse(await load('assets/camera/manifest.json'));
const asset = async descriptor => {
  const content = await load(descriptor.path);
  assert.equal(content.length, descriptor.bytes);
  assert.equal(createHash('sha256').update(content).digest('hex'), descriptor.sha256);
  return content;
};
const [compressedMetadata, model, config] = await Promise.all([
  asset(manifest.catalogue.metadata), asset(manifest.model),
  asset({ path: manifest.model.config_path, bytes: manifest.model.config_bytes,
    sha256: manifest.model.config_sha256 }),
]);
const metadata = manifest.catalogue.metadata.compression === 'gzip' ? gunzipSync(compressedMetadata) : compressedMetadata;
assert.ok(manifest.catalogue.faces > 100_000, 'production camera catalogue must cover all English printings, not the small gallery');
assert.ok(!JSON.parse(metadata).cards.some(card => card.id === 'bd8fa327-dd41-4737-8f19-2cf5eb1f7cdd'),
  'the digital-only Vintage Masters Lotus fixture is outside the declared paper catalogue');
const { default: init, RemoteBrowserEngine } = await import(pathToFileURL(path.join(site, 'camera-engine/whosthatpokemon.js')));
await init({ module_or_path: await load('camera-engine/whosthatpokemon_bg.wasm') });
const engine = new RemoteBrowserEngine(metadata.toString(), model, config.toString());
const fixtures = JSON.parse(await readFile(path.join(fixturesPath, 'fixtures.json'), 'utf8'));
const references = JSON.parse(await readFile(path.join(fixturesPath, 'references.json'), 'utf8'));
const key = request => `${request.game}:${request.id}:${request.face_index}`;
const face = card => card && [card.game, card.id, card.face_index];
const expectedIdentities = new Map([
  ['perspective', ['Lightning Bolt']],
  ['four-cards', ['Counterspell', 'Lightning Bolt', 'Sol Ring', null]],
  ['empty', []],
  ['classic-bolt', ['Lightning Bolt']],
  ['classic-bolt-inverted', ['Lightning Bolt']],
  ['secret-lair-hadoken', ['Lightning Bolt']],
  ['secret-lair-mschf', ['Swords to Plowshares']],
  ['secret-lair-textless', ['Plains']],
  ['inverted-reference-arvinox', ['Arvinox, the Mind Flail']],
  ['inverted-reference-arvinox-inverted', ['Arvinox, the Mind Flail']],
]);
const expectedRotations = new Map([
  ['classic-bolt-inverted', 180],
  ['inverted-reference-arvinox', 180],
  ['inverted-reference-arvinox-inverted', 0],
]);
const identificationDecision = result => ({
  status: result.identity.status,
  identity: result.identity.card,
  identity_scope: result.identity.scope,
  sources: result.identity.sources,
  printing: face(result.printing.card),
  printing_status: result.printing.status,
  strategy: result.visual_strategy || 'expanded_text_candidates',
  footer_candidates: result.text.footer.map(hit => face(hit.card)),
  title_candidates: result.text.title.map(hit => face(hit.card)),
  descriptors: result.visual.descriptors_scored,
  geometry: result.visual.geometry_attempted,
  scope: result.visual.scope,
  total_references: result.visual.total_references,
  search_complete: result.visual.search_complete,
});
const decision = card => {
  const attempt = card.attempts[card.selected_attempt];
  return {
    rotation: attempt.ocr.rotation_degrees,
    orientation_conflict: card.orientation_conflict,
    title: attempt.ocr.reading.text,
    footer: attempt.footer_ocr.reading.text,
    model_sha256: attempt.ocr.reading.model.sha256,
    ...identificationDecision(attempt.identification),
    restricted_reference_alignment: attempt.restricted_reference_alignment
      ? identificationDecision(attempt.restricted_reference_alignment) : null,
  };
};
try {
  for (const fixture of fixtures) {
    const rgba = await readFile(path.join(fixturesPath, fixture.rgba));
    let progress = JSON.parse(engine.begin_rgba(fixture.width, fixture.height, rgba,
      fixture.cropped, fixture.classic));
    const requested = [];
    let rounds = 0;
    while (progress.state === 'needs_references') {
      assert.ok(++rounds <= 32, 'bounded reference continuation');
      assert.ok(progress.requests.length > 0 && progress.requests.length <= 256, 'only bounded candidate images requested');
      for (const request of progress.requests) {
        const identifier = key(request);
        const saved = references[identifier];
        assert.ok(saved, `${fixture.id}: unexpected reference ${identifier}`);
        const image = await readFile(path.join(fixturesPath, saved.file));
        assert.equal(image.length, saved.bytes);
        assert.equal(createHash('sha256').update(image).digest('hex'), saved.sha256);
        assert.equal(engine.add_reference(request.game, request.id, request.face_index, image), saved.sha256);
        requested.push(identifier);
      }
      progress = JSON.parse(engine.continue_frame());
    }
    assert.equal(progress.state, 'complete');
    assert.deepEqual(requested, fixture.reference_requests, `${fixture.id}: same bounded reference acquisition`);
    const actual = progress.report;
    assert.equal(actual.cards.length, fixture.expected.cards.length, fixture.id);
    assert.deepEqual(actual.cards.map(decision), fixture.expected.cards.map(decision), fixture.id);
    const selected = actual.cards.map(card => card.attempts[card.selected_attempt]);
    assert.ok(expectedIdentities.has(fixture.id), `${fixture.id}: missing independent ground truth`);
    assert.deepEqual(selected.map(attempt => attempt.identification.identity.card?.name ?? null),
      expectedIdentities.get(fixture.id), `${fixture.id}: correct identities and honest abstentions`);
    for (const attempt of selected) {
      assert.equal(attempt.identification.identity.status,
        attempt.identification.identity.card ? 'verified_shortlist' : 'unknown', fixture.id);
      assert.equal(attempt.ocr.rotation_degrees, expectedRotations.get(fixture.id) ?? 0, fixture.id);
      if (fixture.id.startsWith('secret-lair')) {
        assert.ok(attempt.identification.identity.sources.includes('footer'), 'collector footer resolves the variant');
      }
    }
    if (fixture.id.startsWith('inverted-reference-arvinox')) {
      assert.equal(actual.cards.length, 1);
      assert.equal(actual.cards[0].orientation_conflict, false);
      const attempt = selected[0];
      const narrow = attempt.restricted_reference_alignment;
      assert.ok(narrow, `${fixture.id}: retain the failed restricted reference alignment`);
      assert.equal(narrow.identity.status, 'unknown', 'the original fast alignment abstained');
      assert.deepEqual(narrow.observations, attempt.identification.observations,
        'broad alignment reuses literal OCR without changing its orientation');
      assert.ok(attempt.identification.visual.geometry_verified_count > 0,
        'the inverted canonical reference is geometrically verified after broad alignment');
      assert.ok(attempt.identification.visual.recognition.candidates.some(candidate =>
        candidate.card.id === '0fbaa2f0-9ad0-4ce2-8ce6-359d01bd524c' && candidate.card.face_index === 0),
        'the physically inverted canonical printing participates in the successful retry');
    }
    if (fixture.id === 'four-cards') {
      assert.equal(selected[3].ocr.reading.text, 'Black Lotus');
      assert.equal(selected[3].identification.visual.geometry_verified_count, 0,
        'a readable title alone cannot accept the out-of-catalogue digital artwork');
    }
    for (const card of actual.cards) {
      assert.ok(card.crop_data_url.startsWith('data:image/png;base64,'));
      for (const attempt of card.attempts) {
        assert.ok(attempt.title_ocr_ms > 0 && attempt.footer_ocr_ms > 0, 'real browser OCR timings');
        assert.equal(attempt.ocr.reading.model.sha256, manifest.model.sha256);
        assert.equal(attempt.footer_ocr.reading.model.sha256, manifest.model.sha256);
        assert.equal(attempt.identification.observations.title, attempt.ocr.reading.text);
        assert.equal(attempt.identification.observations.footer, attempt.footer_ocr.reading.text);
        assert.ok(attempt.identification.visual.descriptors_scored <= 256);
        assert.ok(attempt.identification.visual.geometry_attempted <= 8);
        assert.equal(attempt.identification.visual.total_references, manifest.catalogue.faces);
        assert.equal(attempt.identification.visual.search_complete, false);
        assert.equal(attempt.identification.printing.card, null, 'sparse cache does not imply globally unique printing');
      }
    }
    console.log(`${fixture.id}: actual OCRacle WASM agrees with native raw OCR, orientation, candidates and decisions; ${actual.total_ms.toFixed(1)} ms`);
  }
  assert.throws(() => engine.begin_rgba(1, 1, new Uint8Array(3), false, false), /RGBA/);
  assert.throws(() => engine.begin_rgba(1281, 1, new Uint8Array(1281 * 4), false, false), /RGBA/);
  assert.throws(() => engine.continue_frame(), /pending/);
  assert.ok(fixtures.some(f => f.id === 'classic-bolt-inverted'));
  assert.ok(fixtures.some(f => f.id === 'inverted-reference-arvinox'));
  assert.ok(fixtures.some(f => f.id === 'inverted-reference-arvinox-inverted'));
  assert.equal(fixtures.length, expectedIdentities.size);
  console.log(`Full catalogue: ${manifest.catalogue.faces.toLocaleString()} faces indexed; ${Object.keys(references).length} selected canonical images prepared for parity`);
  assert.equal(fixtures.filter(f => f.id.startsWith('secret-lair')).length, 3);
} finally {
  engine.free();
}
