const $ = (id) => document.getElementById(id);
const SVG_NS = 'http://www.w3.org/2000/svg';
const stageNames = ['image', 'extraction', 'ocr', 'lookup', 'verification', 'result'];
const timingNames = { result: 'decision' };
const state = { gallery: undefined, references: [], sampleIndex: 0, cardIndex: 0, attemptIndex: 0 };

function node(tag, className, text) {
  const item = document.createElement(tag);
  if (className) item.className = className;
  if (text !== undefined) item.textContent = text;
  return item;
}

function ms(value) {
  if (!Number.isFinite(value)) return '—';
  if (value > 0 && value < .1) return '<0.1 ms';
  return `${value.toLocaleString(undefined, { maximumFractionDigits: 1, minimumFractionDigits: 1 })} ms`;
}

function score(value) { return Number.isFinite(value) ? value.toFixed(3) : '—'; }
function printing(card) { return [card?.set?.toUpperCase(), card?.collector_number ? `#${card.collector_number}` : null].filter(Boolean).join(' · ') || 'Printing metadata unavailable'; }
function referenceFor(card) { return state.references.find((reference) => reference.game === card?.game && reference.id === card?.id && (reference.face_index ?? 0) === (card?.face_index ?? 0)); }
function accepted(result) { return ['matched', 'verified_shortlist'].includes(result?.identity?.status); }
function sum(items, value) { return items.reduce((total, item) => total + (value(item) || 0), 0); }
function sample() { return state.gallery.samples[state.sampleIndex]; }
function card() { return sample().result.cards[state.cardIndex]; }
function attempt() { return card()?.attempts[state.attemptIndex]; }

function setStatus(text, error = false) {
  $('status-message').textContent = text;
  $('status-message').classList.toggle('error', error);
}

function stage(name, phase, elapsed) {
  $(`stage-${name}`).dataset.state = phase;
  $(`timing-${timingNames[name] || name}`).textContent = phase === 'skipped' ? 'Not run' : ms(elapsed);
}

function showImage(id, url, placeholder, alt) {
  const image = $(id);
  image.hidden = !url;
  $(placeholder).hidden = !!url;
  if (url) {
    image.src = url;
    image.alt = alt;
  } else image.removeAttribute('src');
}

function exampleUrl(index, cardIndex = 0, attemptIndex) {
  const url = new URL(location.href);
  url.search = '';
  url.searchParams.set('example', state.gallery.samples[index].id);
  if (cardIndex) url.searchParams.set('card', cardIndex + 1);
  if (attemptIndex !== undefined) url.searchParams.set('attempt', attemptIndex + 1);
  return `${url.pathname}${url.search}`;
}

function navigate(sampleIndex, cardIndex = 0, attemptIndex, push = true) {
  state.sampleIndex = sampleIndex;
  const cards = sample().result.cards;
  state.cardIndex = Math.max(0, Math.min(cardIndex, cards.length - 1));
  const selected = cards[state.cardIndex];
  const preferred = attemptIndex ?? selected?.selected_attempt ?? 0;
  state.attemptIndex = Math.max(0, Math.min(preferred, (selected?.attempts.length || 1) - 1));
  if (push) history.pushState(null, '', exampleUrl(sampleIndex, state.cardIndex, attemptIndex));
  render();
}

function readLocation() {
  const params = new URLSearchParams(location.search);
  const index = state.gallery.samples.findIndex((item) => item.id === params.get('example'));
  const cardIndex = Math.max(0, Number.parseInt(params.get('card') || '1', 10) - 1) || 0;
  const attemptIndex = params.has('attempt') ? Math.max(0, Number.parseInt(params.get('attempt'), 10) - 1) || 0 : undefined;
  navigate(index < 0 ? 0 : index, cardIndex, attemptIndex, false);
}

function renderExamples() {
  $('example-nav').replaceChildren(...state.gallery.samples.map((item, index) => {
    const link = node('a', `example-link${index === state.sampleIndex ? ' active' : ''}`);
    link.href = exampleUrl(index);
    if (index === state.sampleIndex) link.setAttribute('aria-current', 'page');
    const image = node('img', 'example-thumb');
    image.src = item.image;
    image.alt = '';
    const text = node('span', 'example-copy');
    text.append(node('strong', '', item.title));
    text.append(node('span', '', `${item.result.cards.length} card${item.result.cards.length === 1 ? '' : 's'}${item.cropped ? ' · cropped' : ''} · ${ms(item.result.total_ms)}`));
    link.append(image, text);
    link.addEventListener('click', (event) => { if (event.button || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return; event.preventDefault(); navigate(index); });
    return link;
  }));
}

function renderSource() {
  const item = sample();
  const report = item.result;
  showImage('source-image', item.image, 'source-placeholder', item.title);
  $('source-dimensions').textContent = `${report.width} × ${report.height}`;
  $('detection-count').textContent = item.cropped ? 'Already cropped · outline detection bypassed' : `${report.cards.length} card${report.cards.length === 1 ? '' : 's'} found`;
  $('sample-description').textContent = item.description;
  $('sample-source').href = item.provenance?.source || 'https://github.com/cmeister2/whosthatpokemon';
  $('source-overlay').replaceChildren();
  $('source-overlay').setAttribute('viewBox', `0 0 ${report.width} ${report.height}`);
  $('source-overlay').setAttribute('preserveAspectRatio', 'xMidYMid meet');
  report.cards.forEach((detected, index) => {
    if (!detected.corners?.length) return;
    const polygon = document.createElementNS(SVG_NS, 'polygon');
    polygon.setAttribute('points', detected.corners.map((point) => point.join(',')).join(' '));
    polygon.classList.toggle('selected', index === state.cardIndex);
    polygon.setAttribute('role', 'button');
    polygon.setAttribute('tabindex', '0');
    polygon.setAttribute('aria-label', `Follow detected card ${index + 1}`);
    const select = () => navigate(state.sampleIndex, index);
    polygon.addEventListener('click', select);
    polygon.addEventListener('keydown', (event) => { if (['Enter', ' '].includes(event.key)) { event.preventDefault(); select(); } });
    $('source-overlay').append(polygon);
  });
  $('card-tabs').replaceChildren();
  if (report.cards.length > 1) report.cards.forEach((detected, index) => {
    const selected = detected.attempts[detected.selected_attempt]?.identification;
    const label = !detected.orientation_conflict && accepted(selected) ? selected.identity.card.name : `Card ${index + 1}`;
    const button = node('button', `card-tab${index === state.cardIndex ? ' active' : ''}`, `${index + 1}. ${label}`);
    button.type = 'button';
    button.setAttribute('aria-pressed', String(index === state.cardIndex));
    button.addEventListener('click', () => navigate(state.sampleIndex, index));
    $('card-tabs').append(button);
  });
  stage('image', 'complete', report.decode_ms);
  stage('extraction', 'complete', report.extraction_ms);
  $('timing-total').textContent = ms(report.total_ms);
}

function renderLookup(result) {
  const proposals = result.text.title_identities || [];
  $('shortlist-count').textContent = String(proposals.length);
  $('shortlist-list').replaceChildren(...proposals.map((proposal) => {
    const row = node('li', 'shortlist-item');
    row.append(node('strong', 'shortlist-name', proposal.representative.face_name || proposal.representative.name));
    row.append(node('span', 'shortlist-meta', proposal.exact ? 'Exact title' : `Title similarity ${score(proposal.title_similarity)}`));
    return row;
  }));
  if (!proposals.length) $('shortlist-list').append(node('li', 'empty-state', 'No title candidates. The library does not fall back to searching every reference.'));
}

function renderVisual(result) {
  const visual = result.visual;
  const verified = visual.geometry_verified || [];
  $('candidate-list').replaceChildren(...(visual.recognition?.candidates || []).map((candidate) => {
    const row = node('li', 'candidate-row');
    const ref = referenceFor(candidate.card);
    if (ref?.image) {
      const image = node('img', 'reference-thumb');
      image.src = ref.image;
      image.alt = `${candidate.card.name} reference`;
      row.append(image);
    }
    const information = node('div', 'candidate-information');
    information.append(node('strong', 'candidate-name', candidate.card.face_name || candidate.card.name));
    const agrees = verified.some((entry) => entry.card.game === candidate.card.game && entry.card.id === candidate.card.id && entry.card.face_index === candidate.card.face_index);
    information.append(node('span', 'candidate-set', `${printing(candidate.card)} · ${agrees ? 'Geometry agrees' : 'Geometry not verified'}`));
    row.append(information, node('span', 'candidate-score', score(candidate.similarity)));
    return row;
  }));
  if (!$('candidate-list').children.length) $('candidate-list').append(node('li', 'empty-state', 'No visual candidates were scored.'));
  const metrics = [['Reference faces in catalogue', visual.total_references], ['Title-derived faces', visual.candidate_count], ['Descriptors scored', visual.descriptors_scored], ['Geometry checks', visual.geometry_attempted], ['Geometry agreements', visual.geometry_verified_count]];
  $('visual-measurements').replaceChildren(...metrics.map(([label, value]) => {
    const row = node('div', 'measurement-label');
    row.append(node('span', '', label), node('span', 'measurement-score', String(value ?? 0)));
    return row;
  }));
  $('visual-method-note').textContent = visual.omitted_descriptor_groups || visual.omitted_geometry_groups ? 'The budget omitted candidate groups; the library keeps the decision unresolved.' : 'Only title-derived candidates were compared. Scores are heuristics, not probabilities; shared artwork may leave the printing ambiguous.';
}

function renderResult(detected) {
  const result = detected.attempts[detected.selected_attempt].identification;
  const container = $('recognition-result');
  container.replaceChildren();
  const conflict = detected.orientation_conflict;
  const isAccepted = !conflict && accepted(result);
  const line = node('div', 'result-topline');
  line.append(node('span', `status-pill ${conflict ? 'conflict' : isAccepted ? 'matched' : result.identity.status}`, conflict ? 'Conflicting orientations' : isAccepted ? 'Card identified' : result.identity.status === 'conflict' ? 'Conflicting evidence' : 'Not identified'));
  container.append(line);
  if (isAccepted && result.identity.card) {
    const identity = result.identity.card;
    const best = result.visual.recognition.candidates.find((entry) => entry.card.oracle_id ? entry.card.oracle_id === identity.oracle_id && entry.card.game === identity.game : entry.card.id === identity.printing_id && entry.card.game === identity.game);
    const ref = referenceFor(best?.card);
    const row = node('div', 'recognition-card');
    if (ref?.image) {
      const image = node('img', 'reference-thumb');
      image.src = ref.image;
      image.alt = `${identity.name} reference`;
      row.append(image);
    }
    const text = node('div');
    text.append(node('h3', 'result-name', identity.name));
    text.append(node('p', 'result-caveat', result.printing.card ? `Printing: ${printing(result.printing.card)}` : 'Printing unresolved'));
    if (ref?.artist) text.append(node('p', 'result-artist', `Reference art: ${ref.artist}`));
    row.append(text);
    container.append(row);
  } else container.append(node('p', 'result-name', conflict ? 'The title readings disagree' : result.identity.status === 'conflict' ? 'The evidence disagrees' : 'More evidence needed'));
  container.append(node('p', 'result-caveat', conflict ? 'The native run flagged strong, conflicting title identities across its orientation attempts. Its selected attempt is provisional, so this gallery does not present it as an accepted identity.' : result.identity.reason));
  if (!conflict && result.printing.reason) container.append(node('p', 'result-caveat', result.printing.reason));
  for (const entry of result.conflicts || []) container.append(node('p', 'result-caveat', `${entry.resolved ? 'Resolved' : 'Unresolved'} ${entry.kind} disagreement: ${entry.reason}`));
}

function renderAttempts(detected) {
  $('attempt-tabs').replaceChildren();
  $('attempt-list').replaceChildren();
  detected.attempts.forEach((reading, index) => {
    const rotation = reading.ocr.rotation_degrees;
    const button = node('button', `card-tab${index === state.attemptIndex ? ' active' : ''}`, `${rotation}° attempt`);
    button.type = 'button';
    button.setAttribute('aria-pressed', String(index === state.attemptIndex));
    button.addEventListener('click', () => navigate(state.sampleIndex, state.cardIndex, index));
    $('attempt-tabs').append(button);
    const row = node('li');
    row.append(node('strong', '', `${rotation}°${index === detected.selected_attempt ? ' · selected by library' : ''}`));
    row.append(node('p', '', `Literal OCR: ${JSON.stringify(reading.ocr.reading.text)} · ${ms(reading.ocr_ms)} OCR + ${ms(reading.matching_ms)} matching`));
    row.append(node('p', '', `Identity: ${reading.identification.identity.status.replaceAll('_', ' ')}. ${reading.identification.identity.reason}`));
    $('attempt-list').append(row);
  });
}

function renderDetails(detected) {
  const engine = state.gallery.engine;
  const initialization = sample().initialization_ms || sample().result.initialization_ms || engine.initialization_ms || {};
  const setup = Object.values(initialization).filter(Number.isFinite).reduce((total, value) => total + value, 0);
  $('timing-setup').textContent = Object.keys(initialization).length ? `${ms(setup)} · excluded from scene total` : 'reported separately';
  $('catalog-size').textContent = `${engine.catalogue.faces} public reference faces`;
  const host = engine.generation_host;
  $('engine-details').textContent = [`Native Rust release build`, engine.crate_version ? `whosthatpokemon ${engine.crate_version}` : '', `OCR: ${engine.ocr_backend}`, `${engine.ocr_threads} OCR threads`, engine.catalogue.label, host ? `${host.cpu} · ${host.os} ${host.architecture}` : '', engine.source_revision ? `Revision ${engine.source_revision}${engine.source_dirty ? ' (working tree modified)' : ''}` : '', state.gallery.generated_at ? `Generated ${state.gallery.generated_at}` : ''].filter(Boolean).join(' · ');
  $('scope-description').textContent = `These are recorded outputs from the native Rust library using magicextract and ocracle. The browser only displays the results. ${engine.catalogue.label} ${engine.timing_note || 'Timings were measured on the generation machine, not your device.'}`;
  const model = detected?.attempts[state.attemptIndex]?.ocr.reading.model;
  $('model-details').textContent = model ? [`Model: ${model.name}`, `backend ${model.backend}`, `language ${model.language}`, `SHA-256 ${model.sha256}`].join(' · ') : 'No OCR model ran for this empty scene.';
}

function renderEmpty() {
  for (const name of stageNames.slice(2)) stage(name, 'skipped');
  showImage('crop-image', undefined, 'crop-placeholder', '');
  showImage('title-image', undefined, 'title-placeholder', '');
  $('crop-placeholder').textContent = 'No card-shaped region detected.';
  $('title-placeholder').textContent = 'OCR was not run.';
  $('crop-information').textContent = 'No crop was produced.';
  $('ocr-text').textContent = 'No title crop to read.';
  $('ocr-confidence').textContent = '';
  $('shortlist-count').textContent = '0';
  $('shortlist-list').replaceChildren(node('li', 'empty-state', 'No title lookup was run.'));
  $('candidate-list').replaceChildren(node('li', 'empty-state', 'No visual search was run.'));
  $('visual-measurements').replaceChildren();
  $('visual-method-note').textContent = 'Extraction found no cards, so the pipeline stopped.';
  $('recognition-result').replaceChildren(node('p', 'result-name', 'No card detected'), node('p', 'result-caveat', 'The native run produced no card identity or printing decision.'));
  $('orientation-note').textContent = '';
  $('attempt-tabs').replaceChildren();
  $('attempt-list').replaceChildren(node('li', 'empty-state', 'No orientation attempts were needed.'));
  $('timing-summary').textContent = 'Native scene total includes decoding and extraction. No OCR, lookup, verification or decision stages ran.';
  setStatus('No card was detected. The native pipeline stopped before OCR and lookup.');
  renderDetails();
}

function renderCard() {
  const detected = card();
  const reading = attempt();
  showImage('crop-image', detected.crop_image, 'crop-placeholder', 'Actual perspective-corrected crop from the native extraction');
  showImage('title-image', reading.ocr_image, 'title-placeholder', `Exact ${reading.ocr.rotation_degrees}° title crop read by native ocracle`);
  $('crop-information').textContent = sample().cropped ? 'Input already cropped; outline detection was bypassed.' : 'Actual native magicextract output before OCR orientation.';
  $('ocr-text').textContent = reading.ocr.reading.text || '(No text read)';
  $('ocr-confidence').textContent = Number.isFinite(reading.ocr.reading.confidence) ? `OCR score ${score(reading.ocr.reading.confidence)} · uncalibrated` : '';
  renderLookup(reading.identification);
  renderVisual(reading.identification);
  renderResult(detected);
  renderAttempts(detected);
  renderDetails(detected);
  stage('ocr', 'complete', detected.timings.ocr_ms);
  stage('lookup', 'complete', sum(detected.attempts, (item) => item.identification.timings.text_retrieval));
  stage('verification', 'complete', sum(detected.attempts, (item) => item.identification.timings.visual_matching));
  stage('result', 'complete', sum(detected.attempts, (item) => item.identification.timings.fusion));
  const multiple = detected.attempts.length > 1;
  $('orientation-note').textContent = `${multiple ? `Showing ${reading.ocr.rotation_degrees}° of two native attempts. ` : 'One native orientation attempt. '}The library selected ${detected.attempts[detected.selected_attempt].ocr.rotation_degrees}°. Stage times ${multiple ? 'include both attempts' : 'are from this attempt'}.`;
  $('timing-summary').textContent = `Scene total covers decoding, extraction and ${sample().result.cards.length === 1 ? 'the card' : `all ${sample().result.cards.length} detected cards`}. Stages 3–6 show card ${state.cardIndex + 1}${multiple ? ', including both orientations' : ''}. One-time setup and gallery image export are excluded.`;
  const result = detected.attempts[detected.selected_attempt].identification;
  setStatus(detected.orientation_conflict ? 'The native run retained conflicting OCR orientations. No accepted identity is shown.' : accepted(result) ? 'The native title and verified picture agree within the shortlist. Its exact printing may remain unresolved.' : 'The native run abstained from identifying this card. The unsuccessful evidence remains visible.');
}

function render() {
  renderExamples();
  renderSource();
  sample().result.cards.length ? renderCard() : renderEmpty();
}

async function loadJson(url) {
  const response = await fetch(url);
  if (!response.ok) throw new Error(`Native gallery assets could not load (${response.status}).`);
  return response.json();
}

try {
  const [gallery, manifest] = await Promise.all([loadJson('./assets/native-gallery.json'), loadJson('./assets/samples.json')]);
  if (gallery.schema_version !== 1 || !Array.isArray(gallery.samples) || !gallery.samples.length) throw new Error('The native gallery format is unavailable.');
  for (const item of gallery.samples) if (!Array.isArray(item.result?.cards)) throw new Error('A native example is missing its actual extraction results.');
  state.gallery = gallery;
  state.references = gallery.references || manifest.references;
  readLocation();
  window.addEventListener('popstate', readLocation);
} catch (error) {
  setStatus(`${error.message} The page cannot display native results until its gallery assets are available.`, true);
  $('example-nav').replaceChildren(node('p', 'empty-state', 'Native examples are unavailable.'));
  $('timing-summary').textContent = 'No processing times are shown because native output could not be loaded.';
  for (const name of stageNames) stage(name, 'error');
}
