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
  if (!proposals.length) $('shortlist-list').append(node('li', 'empty-state', 'No title candidates. The collector bar can still propose a printing.'));
  const footer = result.text.footer || [];
  $('footer-shortlist-count').textContent = String(footer.length);
  $('footer-shortlist-list').replaceChildren(...footer.map((hit) => {
    const row = node('li', 'shortlist-item');
    row.append(node('strong', 'shortlist-name', hit.card.face_name || hit.card.name));
    row.append(node('span', 'shortlist-meta', `${printing(hit.card)} · ${hit.card.language || 'language unknown'}`));
    row.append(node('span', 'shortlist-meta', hit.language_match === false ? 'Observed language disagrees' : (hit.reasons || []).join('; ')));
    return row;
  }));
  if (!footer.length) $('footer-shortlist-list').append(node('li', 'empty-state', 'No collector key matched. Title candidates remain available.'));
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
  const metrics = [['Reference faces in catalogue', visual.total_references], ['Text-derived faces', visual.candidate_count], ['Descriptors scored', visual.descriptors_scored], ['Geometry checks', visual.geometry_attempted], ['Geometry agreements', visual.geometry_verified_count]];
  $('visual-measurements').replaceChildren(...metrics.map(([label, value]) => {
    const row = node('div', 'measurement-label');
    row.append(node('span', '', label), node('span', 'measurement-score', String(value ?? 0)));
    return row;
  }));
  $('visual-method-note').textContent = visual.omitted_descriptor_groups || visual.omitted_geometry_groups ? 'The budget omitted candidate groups; the library keeps the decision unresolved.' : result.visual_strategy === 'exact_footer_faces' ? 'The collector key narrowed verification to these physical faces. Other catalogue faces were not searched.' : 'Only indexed text candidates and their related printings were compared. Shared artwork may leave the printing ambiguous.';
  if (result.footer_verification) {
    const first = result.footer_verification;
    $('visual-method-note').textContent += ` An initial footer pass scored ${first.descriptors_scored} faces and attempted ${first.geometry_attempted} geometry checks before expanding the candidates. Verification time includes both passes.`;
  }
}

function renderResult(detected) {
  const selected = detected.attempts[detected.selected_attempt];
  const result = selected.identification;
  const visual = result.visual;
  const matches = visual.recognition?.candidates || [];
  const container = $('recognition-result');
  container.replaceChildren();
  const conflict = detected.orientation_conflict;
  const isAccepted = !conflict && accepted(result);
  const line = node('div', 'result-topline');
  line.append(node('span', `status-pill ${conflict ? 'conflict' : isAccepted ? 'matched' : result.identity.status}`, conflict ? 'Conflicting orientations' : isAccepted ? 'Card identified' : result.identity.status === 'conflict' ? 'Conflicting evidence' : 'Not identified'));
  container.append(line);
  if (conflict) container.append(node('p', 'result-provisional-note', `Ranking from the provisional ${selected.ocr.rotation_degrees}° attempt; no identity accepted across orientations.`));

  const ranking = node('ol', 'result-ranking');
  ranking.id = 'result-ranking';
  matches.forEach((candidate, index) => {
    const name = candidate.card.face_name || candidate.card.name;
    const label = printing(candidate.card);
    const reference = referenceFor(candidate.card);
    const verified = (visual.geometry_verified || []).some((entry) => entry.card.game === candidate.card.game && entry.card.id === candidate.card.id && entry.card.face_index === candidate.card.face_index);
    const row = node('li', `result-match${index === 0 ? ' leading-match' : ''}`);
    row.dataset.rank = String(index + 1);
    row.dataset.similarity = String(candidate.similarity);
    const heading = node('div', 'result-match-heading');
    heading.append(node('span', 'result-match-rank', String(index + 1)));
    if (reference?.image) {
      const image = node('img', 'result-match-thumb');
      image.src = reference.image;
      image.alt = `${name}, ${label}${reference.artist ? `, art by ${reference.artist}` : ''}`;
      heading.append(image);
    }
    const information = node('div', 'result-match-information');
    information.append(node('strong', 'result-match-name', name));
    information.append(node('span', 'result-match-printing', label));
    const geometry = node('span', 'result-geometry-badge', verified ? 'Geometry agrees' : 'Unverified');
    geometry.dataset.verified = String(verified);
    information.append(geometry);
    heading.append(information);
    const scoreLine = node('div', 'result-score-line');
    scoreLine.append(node('span', '', 'Visual similarity'), node('strong', 'result-match-score', score(candidate.similarity)));
    const bar = node('meter', 'result-similarity-meter');
    bar.min = 0;
    bar.max = 1;
    bar.value = candidate.similarity;
    bar.setAttribute('aria-label', `${name}, ${label}: visual similarity`);
    bar.setAttribute('aria-valuetext', `${score(candidate.similarity)} on a scale from 0 to 1`);
    bar.textContent = score(candidate.similarity);
    row.append(heading, scoreLine, bar);
    ranking.append(row);
  });
  container.append(ranking);
  if (!matches.length) ranking.append(node('li', 'empty-state', 'No ranked image matches were returned.'));
  if (matches.length > 1) container.append(node('p', 'result-margin', `Top-to-next similarity gap: ${score(matches[0].similarity - matches[1].similarity)}.`));
  if (matches.length) container.append(node('p', 'result-score-note', 'Scores compare the shortlisted printings on a 0–1 scale; they are not probabilities.'));

  const decision = node('dl', 'result-decision');
  decision.append(node('dt', '', 'Identity'), node('dd', 'result-identity', conflict ? 'Conflicting orientations' : isAccepted && result.identity.card ? result.identity.card.name : 'Not identified'));
  decision.append(node('dt', '', 'Printing'), node('dd', 'result-printing-decision', !conflict && result.printing.card ? printing(result.printing.card) : 'Unresolved'));
  container.append(decision);

  const details = node('details', 'result-decision-details');
  details.append(node('summary', '', 'Native decision details'));
  const confidenceText = visual.search_complete
    ? `Native heuristic printing confidence: ${score(visual.recognition.confidence)} on a 0–1 scale. This score is uncalibrated.`
    : 'Native confidence is not established across the full catalogue for this candidate-only search. The ranking shows the returned visual similarities.';
  const confidence = node('p', 'result-caveat', confidenceText);
  confidence.id = 'result-confidence-note';
  details.append(confidence);
  details.append(node('p', 'result-caveat', conflict ? 'The native run flagged strong, conflicting text identities across its orientation attempts. Its selected attempt is provisional.' : result.identity.reason));
  if (!conflict && result.printing.reason) details.append(node('p', 'result-caveat', result.printing.reason));
  for (const entry of result.conflicts || []) details.append(node('p', 'result-caveat', `${entry.resolved ? 'Resolved' : 'Unresolved'} ${entry.kind} disagreement: ${entry.reason}`));
  container.append(details);
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
    row.append(node('p', '', `Literal title: ${JSON.stringify(reading.ocr.reading.text)}`));
    if (reading.footer_ocr) row.append(node('p', '', `Literal collector bar: ${JSON.stringify(reading.footer_ocr.observations.footer)}`));
    row.append(node('p', '', `${ms(reading.title_ocr_ms ?? reading.ocr_ms)} title + ${ms(reading.footer_ocr_ms ?? 0)} footer + ${ms(reading.matching_ms)} matching`));
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
  showImage('footer-image', undefined, 'footer-placeholder', '');
  $('footer-text').textContent = 'No collector bar to read.';
  $('footer-input-image').hidden = true;
  $('footer-input-image').removeAttribute('src');
  $('ocr-timing-breakdown').textContent = '';
  $('footer-shortlist-count').textContent = '0';
  $('footer-shortlist-list').replaceChildren(node('li', 'empty-state', 'No collector lookup was run.'));
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
  showImage('footer-image', reading.footer_image, 'footer-placeholder', 'Actual collector bar read by native ocracle');
  $('footer-text').textContent = reading.footer_ocr?.observations.footer || '(No footer text read)';
  $('footer-input-image').hidden = !reading.footer_input_image;
  if (reading.footer_input_image) $('footer-input-image').src = reading.footer_input_image;
  else $('footer-input-image').removeAttribute('src');
  $('ocr-timing-breakdown').textContent = `${ms(reading.title_ocr_ms ?? reading.ocr_ms)} title + ${ms(reading.footer_ocr_ms ?? 0)} collector bar`;
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
  setStatus(detected.orientation_conflict ? 'The native run retained conflicting OCR orientations. No accepted identity is shown.' : accepted(result) ? 'Observed text and verified image evidence agree within the shortlist. The ranked printings remain visible.' : 'The native run abstained from identifying this card. The unsuccessful evidence remains visible.');
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
