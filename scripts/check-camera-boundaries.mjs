import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

// Exercise the actual renderer without starting a camera, worker or network request.
class Element {
  constructor(tag) {
    this.tag = tag;
    this.className = '';
    this.dataset = {};
    this.children = [];
    this.attributes = {};
    this.text = '';
  }
  append(...children) { this.children.push(...children); }
  setAttribute(name, value) { this.attributes[name] = String(value); }
  getContext() { assert.equal(this.tag, 'canvas'); return {}; }
  set textContent(value) { this.text = String(value); this.children = []; }
  get textContent() { return this.text + this.children.map((child) => typeof child === 'string' ? child : child.textContent).join(''); }
}

const source = readFileSync(new URL('../docs/camera.js', import.meta.url), 'utf8');
const startup = '\nloadManifest().catch(';
const boundary = source.indexOf(startup);
assert.ok(boundary > 0, 'The camera startup boundary must remain identifiable.');
assert.equal(source.lastIndexOf(startup), boundary, 'The startup boundary must be unique.');
const forbidden = () => { throw new Error('Rendering must not start browser I/O.'); };
const context = vm.createContext({
  document: { createElement: (tag) => new Element(tag), getElementById: forbidden },
  fetch: forbidden,
  Worker: forbidden,
});
vm.runInContext(source.slice(0, boundary).replaceAll('import.meta.url', JSON.stringify('https://camera.example.test/camera.js')), context, { filename: 'docs/camera.js' });
const renderCard = vm.runInContext('renderCard', context);

function descendants(element) {
  return [element, ...element.children.flatMap((child) => typeof child === 'string' ? [] : descendants(child))];
}
function byClass(element, name) {
  const found = descendants(element).find((child) => child.className.split(' ').includes(name));
  assert.ok(found, `Expected .${name}`);
  return found;
}
function byTag(element, tag) {
  const found = descendants(element).find((child) => child.tag === tag);
  assert.ok(found, `Expected <${tag}>`);
  return found;
}
function fixture(status = 'matched') {
  return {
    selected_attempt: 0,
    orientation_conflict: false,
    attempts: [{
      ocr: { rotation_degrees: 0, reading: { text: 'Literal observed title' } },
      footer_ocr: { observations: { footer: 'Literal observed collector' } },
      ocr_ms: 1,
      matching_ms: 2,
      identification: {
        identity: { status, card: { name: 'Synthetic accepted card' }, reason: 'Identity evidence.' },
        printing: { card: { set: 'tst', collector_number: '7' }, reason: 'Printing evidence.' },
        visual: {
          recognition: { candidates: [], confidence: 0.9 },
          geometry_verified: [],
          descriptors_scored: 1,
          geometry_attempted: 1,
          search_complete: true,
        },
        conflicts: [],
      },
    }],
  };
}
const rejection = { reason: 'The full physical card extends outside the captured frame.' };

for (const status of ['matched', 'verified_shortlist']) {
  test(`capture rejection overrides stale ${status} identity, printing and orientation conflict`, () => {
    const card = fixture(status);
    card.capture_rejection = rejection;
    card.orientation_conflict = true;
    const article = renderCard(card, 0);
    const heading = byClass(article, 'card-result-heading');
    assert.equal(byClass(heading, 'card-status').textContent, 'Incomplete card');
    assert.equal(byClass(heading, 'card-status').className, 'card-status');
    assert.equal(byTag(heading, 'h3').textContent, 'Incomplete card—move the whole card into view');
    assert.equal(byClass(heading, 'printing-decision').textContent, 'Printing unresolved');
    assert.doesNotMatch(heading.textContent, /Synthetic accepted card|TST|#7/);
    assert.match(byClass(article, 'card-details').textContent, /full physical card extends outside/);
    assert.match(byClass(article, 'card-details').textContent, /No identity or printing is accepted/);
  });

  test(`complete ${status} result still displays the supported identity and printing`, () => {
    const article = renderCard(fixture(status), 1);
    const heading = byClass(article, 'card-result-heading');
    assert.equal(byClass(heading, 'card-status').textContent, 'Identity supported');
    assert.equal(byClass(heading, 'card-status').className, 'card-status accepted');
    assert.equal(byTag(heading, 'h3').textContent, 'Synthetic accepted card');
    assert.equal(byClass(heading, 'printing-decision').textContent, 'Printing: TST · #7');
    assert.equal(article.dataset.cardIndex, '1');
  });
}

for (const attempts of [undefined, []]) {
  test(`rejected capture without ${attempts ? 'a selected' : 'any'} attempt is still incomplete`, () => {
    const article = renderCard({ selected_attempt: 0, attempts, capture_rejection: rejection }, 0);
    assert.equal(byClass(article, 'empty-result').textContent, 'Incomplete card—move the whole card into view');
    assert.doesNotMatch(article.textContent, /No identification attempt|Synthetic accepted card|Printing:/);
  });
}

for (const conflict of ['orientation', 'identity']) {
  test(`${conflict} conflict cannot display a stale supported identity or printing`, () => {
    const card = fixture(conflict === 'identity' ? 'conflict' : 'matched');
    card.orientation_conflict = conflict === 'orientation';
    const heading = byClass(renderCard(card, 0), 'card-result-heading');
    assert.equal(byClass(heading, 'card-status').className, 'card-status conflict');
    assert.equal(byClass(heading, 'card-status').textContent, 'Conflicting evidence');
    assert.equal(byTag(heading, 'h3').textContent, 'More evidence needed');
    assert.equal(byClass(heading, 'printing-decision').textContent, 'Printing unresolved');
    assert.doesNotMatch(heading.textContent, /Synthetic accepted card|TST|#7/);
  });
}

test('rejected capture retains candidate evidence explicitly as diagnostic, not accepted identity', () => {
  const card = fixture();
  card.capture_rejection = rejection;
  const candidate = { game: 'synthetic', id: 'diagnostic-only', name: 'Diagnostic candidate', set: 'ref', collector_number: '9', face_index: 0 };
  const visual = card.attempts[0].identification.visual;
  visual.recognition.candidates = [{ card: candidate, similarity: 0.99 }];
  visual.geometry_verified = [{ card: candidate }];
  const article = renderCard(card, 0);
  assert.equal(byTag(article, 'h4').textContent, 'Diagnostic candidates—not accepted identities');
  assert.match(byClass(article, 'card-match-list').textContent, /Diagnostic candidate.*Feature geometry aligns/);
  assert.doesNotMatch(byClass(article, 'card-result-heading').textContent, /Diagnostic candidate|REF|#9/);
  assert.match(byClass(article, 'card-details').textContent, /Candidate similarities remain diagnostic model evidence/);
});
