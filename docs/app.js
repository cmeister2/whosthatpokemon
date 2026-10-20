const $ = (id) => document.getElementById(id);
const SVG_NS = 'http://www.w3.org/2000/svg';
const titleRegions = { modern: [.07, .04, .80, .10], classic: [.06, .05, .81, .12] };
const stages = ['image', 'extraction', 'ocr', 'lookup', 'verification', 'result'];
const timingIds = { image: 'image', extraction: 'extraction', ocr: 'ocr', lookup: 'lookup', verification: 'verification', result: 'decision' };
const state = {
  samples: [], references: [], report: undefined, bytes: undefined, sourceUrl: '',
  cardIndex: 0, generation: 0, busy: false, setup: undefined,
  sourceTiming: 0, extractionTiming: 0, pipelineStarted: 0, attempts: [], activeStage: 'image',
};
let engineWorker;
let workerSequence = 0;
const pending = new Map();
let ocrPromise;
let readyPromise;
let cameraStream;

function node(tag, className, text) {
  const item = document.createElement(tag);
  if (className) item.className = className;
  if (text !== undefined) item.textContent = text;
  return item;
}

function ms(value) {
  if (!Number.isFinite(value)) return '—';
  if (value < .1) return '<0.1 ms';
  return `${value.toLocaleString(undefined, { maximumFractionDigits: 1, minimumFractionDigits: 1 })} ms`;
}

function score(value) { return Number.isFinite(value) ? value.toFixed(3) : '—'; }
function current(token) { return token === state.generation; }
function accepted(result) { return ['matched', 'verified_shortlist'].includes(result?.identity?.status); }
function referenceFor(card) { return state.references.find((item) => item.game === card?.game && item.id === card?.id && (item.face_index ?? 0) === (card?.face_index ?? 0)); }
function printing(card) { return [card?.set?.toUpperCase(), card?.collector_number ? `#${card.collector_number}` : null].filter(Boolean).join(' · '); }

function status(message, error = false) {
  $('status-message').textContent = message;
  $('status-message').classList.toggle('error', error);
}

function busy(value) {
  state.busy = value;
  for (const id of ['rerun-button', 'sample-select', 'file-input', 'camera-button', 'cropped-input', 'layout-select']) {
    $(id).disabled = value || (id === 'rerun-button' && !state.sourceUrl) || (id === 'sample-select' && !state.samples.length);
  }
  for (const button of $('card-tabs').querySelectorAll('button')) button.disabled = value;
}

function stage(name, phase, elapsed) {
  $(`stage-${name}`).dataset.state = phase;
  $(`stage-${name}`).setAttribute('aria-busy', String(phase === 'running'));
  $(`timing-${timingIds[name]}`).textContent = Number.isFinite(elapsed) ? ms(elapsed) : ({ running: 'Running…', waiting: 'Waiting', skipped: 'Skipped', error: 'Failed' }[phase] || '—');
  if (phase === 'running') state.activeStage = name;
}

function resetCard() {
  state.attempts = [];
  for (const name of stages.slice(2)) stage(name, 'waiting');
  $('ocr-text').textContent = 'Waiting for the title pixels.';
  $('ocr-confidence').textContent = '';
  $('title-placeholder').hidden = false;
  $('title-canvas').getContext('2d').clearRect(0, 0, $('title-canvas').width, $('title-canvas').height);
  $('shortlist-list').replaceChildren();
  $('shortlist-count').textContent = '—';
  $('candidate-list').replaceChildren();
  $('visual-measurements').replaceChildren();
  $('visual-method-note').textContent = 'Only the cards proposed by the title will be checked.';
  $('recognition-result').replaceChildren(node('p', 'result-placeholder', 'Waiting for image and text evidence.'));
  $('orientation-note').textContent = '';
  $('attempt-list').replaceChildren();
  $('timing-total').textContent = '—';
  $('timing-summary').textContent = 'Each step is timed as it runs.';
}

function runEngine(action, payload = {}) {
  if (!engineWorker) {
    engineWorker = new Worker(new URL('./engine-worker.js', import.meta.url), { type: 'module' });
    engineWorker.addEventListener('message', ({ data }) => {
      const job = pending.get(data.id);
      if (!job) return;
      pending.delete(data.id);
      if (data.error) {
        const error = new Error(data.error);
        error.elapsedMs = data.elapsed_ms;
        job.reject(error);
      } else job.resolve(data);
    });
    engineWorker.addEventListener('error', (event) => {
      for (const job of pending.values()) job.reject(new Error(event.message || 'The image worker stopped.'));
      pending.clear();
      engineWorker.terminate();
      engineWorker = undefined;
      readyPromise = undefined;
    });
  }
  return new Promise((resolve, reject) => {
    const id = ++workerSequence;
    pending.set(id, { resolve, reject });
    engineWorker.postMessage({ id, action, ...payload });
  });
}

function getOcr() {
  if (!ocrPromise) {
    ocrPromise = (async () => {
      const started = performance.now();
      const bridge = new Worker(new URL('./ocr-worker.js', import.meta.url), { type: 'module' });
      const jobs = new Map();
      let sequence = 0;
      const stop = (error) => {
        for (const job of jobs.values()) job.reject(error);
        jobs.clear();
        bridge.terminate();
        ocrPromise = undefined;
        readyPromise = undefined;
      };
      bridge.addEventListener('message', ({ data }) => {
        const job = jobs.get(data.id);
        if (!job) return;
        jobs.delete(data.id);
        if (data.error) {
          const error = new Error(data.error);
          job.reject(error);
          stop(error);
        } else job.resolve(data.result);
      });
      bridge.addEventListener('error', (event) => {
        event.preventDefault();
        stop(new Error(event.message || 'The text reader stopped.'));
      });
      const request = (action, payload = {}, transfer = []) => new Promise((resolve, reject) => {
        const id = ++sequence;
        jobs.set(id, { resolve, reject });
        bridge.postMessage({ id, action, ...payload }, transfer);
      });
      await request('initialize');
      const worker = {
        async recognize(canvas) {
          const blob = await new Promise((resolve) => canvas.toBlob(resolve, 'image/png'));
          if (!blob) throw new Error('The title image could not be prepared.');
          const bytes = await blob.arrayBuffer();
          const data = await request('recognize', { bytes }, [bytes]);
          return { data };
        },
        terminate() { bridge.terminate(); },
      };
      return { worker, setup_ms: performance.now() - started };
    })();
    ocrPromise.catch(() => { ocrPromise = undefined; });
  }
  return ocrPromise;
}

function ready() {
  if (!readyPromise) {
    $('timing-setup').textContent = 'Loading the local OCR model and reference index…';
    readyPromise = (async () => {
      const started = performance.now();
      const [engine, ocr] = await Promise.all([runEngine('initialize'), getOcr()]);
      state.setup = { elapsed: performance.now() - started, engine: engine.result.setup_ms, ocr: ocr.setup_ms };
      $('timing-setup').textContent = `${ms(state.setup.elapsed)} · reused for subsequent runs`;
      $('timing-setup').title = `Reference engine ${ms(state.setup.engine)}; OCR model ${ms(state.setup.ocr)}. Loaded concurrently; excluded from processing totals.`;
      return ocr.worker;
    })();
    readyPromise.catch(() => {
      readyPromise = undefined;
      $('timing-setup').textContent = 'Setup failed. Run again to retry.';
    });
  }
  return readyPromise;
}

async function imageFrom(url) {
  const image = new Image();
  image.src = url;
  await image.decode();
  return image;
}

function drawOutlines() {
  const overlay = $('source-overlay');
  overlay.replaceChildren();
  if (!state.report) return;
  overlay.setAttribute('viewBox', `0 0 ${state.report.width} ${state.report.height}`);
  overlay.setAttribute('preserveAspectRatio', 'xMidYMid meet');
  state.report.cards.forEach((card, index) => {
    const polygon = document.createElementNS(SVG_NS, 'polygon');
    polygon.setAttribute('points', card.corners.map((point) => point.join(',')).join(' '));
    polygon.classList.toggle('selected', index === state.cardIndex);
    polygon.setAttribute('role', 'button');
    polygon.setAttribute('tabindex', state.busy ? '-1' : '0');
    polygon.setAttribute('aria-label', `Follow card ${index + 1}`);
    const choose = () => { if (!state.busy) selectCard(index); };
    polygon.addEventListener('click', choose);
    polygon.addEventListener('keydown', (event) => { if (['Enter', ' '].includes(event.key)) { event.preventDefault(); choose(); } });
    overlay.append(polygon);
  });
}

function drawTabs() {
  $('card-tabs').replaceChildren();
  if (state.report?.cards.length < 2) return;
  state.report?.cards.forEach((_, index) => {
    const button = node('button', `card-tab${index === state.cardIndex ? ' active' : ''}`, `Card ${index + 1}`);
    button.type = 'button';
    button.disabled = state.busy;
    button.setAttribute('aria-pressed', String(index === state.cardIndex));
    button.addEventListener('click', () => selectCard(index));
    $('card-tabs').append(button);
  });
}

function drawCrop(image, rotation) {
  const canvas = $('crop-canvas');
  const context = canvas.getContext('2d');
  context.clearRect(0, 0, canvas.width, canvas.height);
  context.save();
  context.translate(canvas.width / 2, canvas.height / 2);
  context.rotate(rotation * Math.PI / 180);
  context.drawImage(image, -canvas.width / 2, -canvas.height / 2, canvas.width, canvas.height);
  context.restore();
  $('crop-placeholder').hidden = true;
  const [left, top, right, bottom] = titleRegions[$('layout-select').value];
  const title = $('title-canvas');
  title.width = Math.round((right - left) * canvas.width * 2);
  title.height = Math.round((bottom - top) * canvas.height * 2);
  title.getContext('2d').drawImage(canvas, left * canvas.width, top * canvas.height, (right - left) * canvas.width, (bottom - top) * canvas.height, 0, 0, title.width, title.height);
  $('title-placeholder').hidden = true;
  // Draw the highlight after copying, so OCR only sees the card pixels.
  context.strokeStyle = '#0969da';
  context.lineWidth = 3;
  context.strokeRect(left * canvas.width, top * canvas.height, (right - left) * canvas.width, (bottom - top) * canvas.height);
  $('crop-information').textContent = `${image.naturalWidth} × ${image.naturalHeight} crop · title band highlighted`;
}

function showLookup(proposals) {
  $('shortlist-list').replaceChildren();
  const identities = proposals.title_identities || [];
  $('shortlist-count').textContent = String(identities.length);
  for (const candidate of identities) {
    const row = node('li', 'shortlist-item');
    row.append(node('strong', 'shortlist-name', candidate.representative.face_name || candidate.representative.name));
    row.append(node('span', 'shortlist-meta', candidate.exact ? 'Exact title' : `Title similarity ${score(candidate.title_similarity)}`));
    $('shortlist-list').append(row);
  }
  if (!identities.length) $('shortlist-list').append(node('li', 'empty-state', 'No title match. The image search stays empty; it does not fall back to the entire catalogue.'));
  else if (!proposals.hints?.length) $('shortlist-list').append(node('li', 'empty-state', 'These title matches are too weak for image verification.'));
}

function showVisual(visual) {
  $('candidate-list').replaceChildren();
  const matches = visual.recognition?.candidates || [];
  const verified = visual.geometry_verified || [];
  for (const candidate of matches) {
    const row = node('li', 'candidate-row');
    const ref = referenceFor(candidate.card);
    if (ref?.image) {
      const thumb = node('img', 'reference-thumb');
      thumb.src = ref.image;
      thumb.alt = `${candidate.card.name} reference`;
      row.append(thumb);
    }
    const info = node('div', 'candidate-information');
    info.append(node('strong', 'candidate-name', candidate.card.face_name || candidate.card.name));
    const geometry = verified.some((item) => item.card.game === candidate.card.game && item.card.id === candidate.card.id && item.card.face_index === candidate.card.face_index);
    info.append(node('span', 'candidate-set', `${printing(candidate.card)} · ${geometry ? 'Geometry agrees' : 'Geometry not verified'}`));
    row.append(info, node('span', 'candidate-score', score(candidate.similarity)));
    $('candidate-list').append(row);
  }
  if (!matches.length) $('candidate-list').append(node('li', 'empty-state', 'No image candidates to verify.'));
  $('visual-measurements').replaceChildren();
  const counts = [['Reference faces in catalogue', visual.total_references], ['Title-derived faces', visual.candidate_count], ['Descriptors scored', visual.descriptors_scored], ['Geometry checks', visual.geometry_attempted], ['Geometry agreements', visual.geometry_verified_count]];
  for (const [label, count] of counts) {
    const row = node('div', 'measurement-label');
    row.append(node('span', '', label), node('span', 'measurement-score', String(count ?? 0)));
    $('visual-measurements').append(row);
  }
  $('visual-method-note').textContent = visual.omitted_descriptor_groups || visual.omitted_geometry_groups ? 'The budget omitted candidate groups, so the decision must stay unresolved.' : 'Compare the image only with the title candidates. Matching artwork does not distinguish every reprint.';
}

function showResult(result) {
  const container = $('recognition-result');
  container.replaceChildren();
  const identity = result.identity;
  const line = node('div', 'result-topline');
  line.append(node('span', `status-pill ${accepted(result) ? 'matched' : identity.status}`, accepted(result) ? 'Card identified' : identity.status === 'conflict' ? 'Conflicting evidence' : 'Not identified'));
  container.append(line);
  if (identity.card) {
    const row = node('div', 'recognition-card');
    const best = result.visual.recognition.candidates.find((candidate) => candidate.card.oracle_id ? candidate.card.oracle_id === identity.card.oracle_id && candidate.card.game === identity.card.game : candidate.card.id === identity.card.printing_id && candidate.card.game === identity.card.game);
    const ref = referenceFor(best?.card);
    if (ref?.image) {
      const thumb = node('img', 'reference-thumb');
      thumb.src = ref.image;
      thumb.alt = `${identity.card.name} reference image`;
      row.append(thumb);
    }
    const text = node('div');
    text.append(node('h3', 'result-name', identity.card.name));
    text.append(node('p', 'result-caveat', result.printing.card ? `Printing: ${printing(result.printing.card)}` : 'Printing unresolved'));
    row.append(text);
    container.append(row);
    container.append(node('p', 'result-caveat', 'Title and verified image agree within the shortlisted candidates. Other card identities were not checked.'));
  } else {
    container.append(node('p', 'result-name', identity.status === 'conflict' ? 'The evidence disagrees' : 'More evidence needed'));
    container.append(node('p', 'result-caveat', identity.reason));
  }
  if (result.conflicts?.length) container.append(node('p', 'result-caveat', `${result.conflicts.length} disagreement${result.conflicts.length === 1 ? '' : 's'} retained with the result.`));
}

function totals(attempts) {
  return attempts.reduce((sum, item) => ({ ocr: sum.ocr + item.times.ocr, lookup: sum.lookup + item.times.lookup, verification: sum.verification + item.times.verification, result: sum.result + item.times.result }), { ocr: 0, lookup: 0, verification: 0, result: 0 });
}

function showAttempt(attempt, allAttempts, wallMs) {
  drawCrop(attempt.image, attempt.rotation);
  $('ocr-text').textContent = attempt.title || '(No title text read)';
  $('ocr-confidence').textContent = Number.isFinite(attempt.confidence) ? `OCR character confidence ${Math.round(attempt.confidence)}/100 · uncalibrated` : '';
  showLookup(attempt.proposals);
  showVisual(attempt.visual);
  showResult(attempt.result);
  const times = totals(allAttempts);
  for (const name of stages.slice(2)) stage(name, 'complete', times[name]);
  const total = state.sourceTiming + state.extractionTiming + Object.values(times).reduce((sum, value) => sum + value, 0);
  $('timing-total').textContent = ms(wallMs);
  $('timing-summary').textContent = `One card through the complete process${allAttempts.length > 1 ? ', including both orientation attempts' : ''}. Stage work: ${ms(total)}. End to end also includes crop display and worker messaging; asset loading and one-time setup are excluded.`;
  $('orientation-note').textContent = allAttempts.length > 1 ? `The first orientation did not identify the card. Tried a 180° turn; showing the ${attempt.rotation}° attempt. Times include both attempts.` : 'Read the expected upright title once.';
  $('attempt-list').replaceChildren(...allAttempts.map((item) => {
    const row = node('li');
    row.textContent = `${item.rotation}° · ${item.title || 'no text'} · ${item.result.identity.status.replaceAll('_', ' ')} · ${ms(Object.values(item.times).reduce((sum, value) => sum + value, 0))}`;
    return row;
  }));
}

async function identifyCard(token, index) {
  const card = state.report.cards[index];
  const image = await imageFrom(card.raw_crop_data_url || card.crop_data_url);
  const bytes = new Uint8Array(await (await fetch(card.raw_crop_data_url || card.crop_data_url)).arrayBuffer());
  const worker = await ready();
  if (!current(token)) return;
  const attempts = [];
  for (const rotation of [0, 180]) {
    const times = {};
    stage('ocr', 'running');
    status(rotation ? 'The first read did not identify a card. Trying its other short edge…' : 'Reading the title from the cropped image…');
    const started = performance.now();
    drawCrop(image, rotation);
    const { data } = await worker.recognize($('title-canvas'));
    times.ocr = performance.now() - started;
    if (!current(token)) return;
    const title = data.text.trim();
    $('ocr-text').textContent = title || '(No title text read)';
    $('ocr-confidence').textContent = `OCR character confidence ${Math.round(data.confidence)}/100 · uncalibrated`;
    stage('ocr', 'complete', (attempts[0]?.times.ocr || 0) + times.ocr);
    stage('lookup', 'running');
    status('Using the observed title to look up possible cards…');
    const lookup = await runEngine('lookup', { title });
    if (!current(token)) return;
    times.lookup = lookup.elapsed_ms;
    const proposals = lookup.result;
    showLookup(proposals);
    stage('lookup', 'complete', (attempts[0]?.times.lookup || 0) + times.lookup);
    stage('verification', 'running');
    status('Comparing the image with only the proposed cards…');
    const verification = await runEngine('verify', { bytes, proposals, rotation });
    if (!current(token)) return;
    times.verification = verification.elapsed_ms;
    const visual = verification.result;
    showVisual(visual);
    stage('verification', 'complete', (attempts[0]?.times.verification || 0) + times.verification);
    stage('result', 'running');
    const combined = await runEngine('combine', { title, proposals, visual });
    if (!current(token)) return;
    times.result = combined.elapsed_ms;
    const attempt = { rotation, title, confidence: data.confidence, image, proposals, visual, result: combined.result, times };
    attempts.push(attempt);
    showResult(attempt.result);
    stage('result', 'complete', totals(attempts).result);
    if (accepted(attempt.result)) break;
  }
  if (!current(token)) return;
  // Preserve the strongest observed attempt; a failed upside-down read must not erase a useful first read.
  const selected = attempts.find((item) => accepted(item.result)) || attempts.find((item) => item.result.identity.status === 'conflict') || attempts.find((item) => item.visual.recognition?.candidates?.length) || attempts.find((item) => item.title) || attempts[0];
  state.attempts = attempts;
  showAttempt(selected, attempts, performance.now() - state.pipelineStarted);
  status(accepted(selected.result) ? 'The card’s title and image agree. Its exact printing may remain unresolved.' : 'The pipeline finished without identifying this card. The observed text and remaining evidence are shown above.');
}

function failed(error, token) {
  if (!current(token)) return;
  stage(state.activeStage, 'error', error.elapsedMs);
  status(`This run could not finish: ${error.message}. Run again to retry.`, true);
  $('timing-total').textContent = 'Incomplete';
  $('recognition-result').replaceChildren(node('p', 'result-placeholder', 'No identification was produced.'));
}

async function selectCard(index) {
  if (!state.report?.cards[index] || state.busy) return;
  await runImage(index);
}

async function runImage(selectedIndex = 0) {
  if (!state.sourceUrl) return;
  const token = ++state.generation;
  busy(true);
  resetCard();
  for (const name of stages.slice(0, 2)) stage(name, 'waiting');
  state.report = undefined;
  state.cardIndex = Number.isInteger(selectedIndex) ? selectedIndex : 0;
  $('detection-count').textContent = 'Waiting for extraction';
  $('card-tabs').replaceChildren();
  $('source-overlay').replaceChildren();
  $('crop-canvas').getContext('2d').clearRect(0, 0, $('crop-canvas').width, $('crop-canvas').height);
  $('crop-placeholder').hidden = false;
  $('crop-placeholder').textContent = 'Waiting for the card outline.';
  $('crop-information').textContent = 'Perspective correction';
  try {
    stage('image', 'running');
    status('Loading the example and preparing local recognition…');
    const started = performance.now();
    const [response] = await Promise.all([fetch(state.sourceUrl), ready()]);
    if (!response.ok) throw new Error(`The source image could not load (${response.status}).`);
    const bytes = new Uint8Array(await response.arrayBuffer());
    // Loading the models has its own clock; time pixel decode separately from setup.
    const decodeStarted = performance.now();
    state.pipelineStarted = decodeStarted;
    const image = $('source-image');
    image.src = state.sourceUrl;
    await image.decode();
    if (!current(token)) return;
    image.hidden = false;
    $('source-placeholder').hidden = true;
    $('source-dimensions').textContent = `${image.naturalWidth} × ${image.naturalHeight}`;
    state.bytes = bytes;
    state.sourceTiming = performance.now() - decodeStarted;
    stage('image', 'complete', state.sourceTiming);
    $('timing-image').title = `Browser image decode ${ms(state.sourceTiming)}. Asset/model loading ${ms(decodeStarted - started)} is outside the processing total.`;
    stage('extraction', 'running');
    status('Finding card outlines and correcting perspective…');
    const extracted = await runEngine('extract', { bytes, cropped: $('cropped-input').checked });
    if (!current(token)) return;
    state.report = extracted.result;
    if (state.cardIndex >= state.report.cards.length) state.cardIndex = 0;
    state.extractionTiming = extracted.elapsed_ms;
    stage('extraction', 'complete', state.extractionTiming);
    $('detection-count').textContent = `${state.report.cards.length} card${state.report.cards.length === 1 ? '' : 's'} found`;
    drawTabs();
    drawOutlines();
    if (!state.report.cards.length) {
      for (const name of stages.slice(2)) stage(name, 'skipped');
      $('crop-placeholder').textContent = 'No card-shaped region found.';
      $('crop-information').textContent = 'No crop produced';
      $('ocr-text').textContent = 'No crop to read.';
      $('recognition-result').replaceChildren(node('p', 'result-name', 'No card detected'), node('p', 'result-caveat', 'The pipeline stops after extraction. OCR, lookup and verification are skipped.'));
      $('timing-total').textContent = ms(performance.now() - state.pipelineStarted);
      $('timing-summary').textContent = 'Image decoding and scene extraction only; later steps did not run.';
      status('No card was detected, so the remaining steps were skipped.');
      return;
    }
    await identifyCard(token, state.cardIndex);
  } catch (error) { failed(error, token); }
  finally { if (current(token)) { busy(false); drawOutlines(); } }
}

async function selectSample(id) {
  const sample = state.samples.find((item) => item.id === id);
  if (!sample) return;
  if (state.sourceUrl.startsWith('blob:')) URL.revokeObjectURL(state.sourceUrl);
  state.sourceUrl = sample.image;
  $('sample-select').value = sample.id;
  $('sample-description').textContent = sample.description;
  $('cropped-input').checked = !!sample.cropped;
  $('layout-select').value = sample.layout === 'classic' ? 'classic' : 'modern';
  await runImage();
}

async function usePicture(blob) {
  const token = ++state.generation;
  busy(true);
  try {
    status('Preparing your picture on this device…');
    const image = await createImageBitmap(blob, { imageOrientation: 'from-image' });
    const scale = Math.min(1, 2200 / Math.max(image.width, image.height));
    const canvas = document.createElement('canvas');
    canvas.width = Math.round(image.width * scale);
    canvas.height = Math.round(image.height * scale);
    canvas.getContext('2d').drawImage(image, 0, 0, canvas.width, canvas.height);
    image.close();
    const prepared = await new Promise((resolve) => canvas.toBlob(resolve, 'image/jpeg', .95));
    if (!prepared) throw new Error('This picture could not be decoded.');
    if (!current(token)) return;
    if (state.sourceUrl.startsWith('blob:')) URL.revokeObjectURL(state.sourceUrl);
    state.sourceUrl = URL.createObjectURL(prepared);
    $('sample-select').value = '';
    $('sample-description').textContent = 'Your picture, processed locally. It stays on this device.';
    await runImage();
  } catch (error) { failed(error, token); }
  finally { if (current(token)) busy(false); }
}

async function openCamera() {
  if (!navigator.mediaDevices?.getUserMedia) { status('Camera access requires HTTPS and a supported camera. You can use the public examples instead.', true); return; }
  try {
    cameraStream = await navigator.mediaDevices.getUserMedia({ video: { facingMode: { ideal: 'environment' } }, audio: false });
    $('camera-video').srcObject = cameraStream;
    $('camera-dialog').showModal();
  } catch (error) { status(`The camera could not open: ${error.message}`, true); }
}

function closeCamera() {
  cameraStream?.getTracks().forEach((track) => track.stop());
  cameraStream = undefined;
  $('camera-video').srcObject = null;
  if ($('camera-dialog').open) $('camera-dialog').close();
}

$('sample-select').addEventListener('change', (event) => selectSample(event.target.value));
$('rerun-button').addEventListener('click', runImage);
$('file-input').addEventListener('change', async (event) => {
  const file = event.target.files?.[0];
  if (file) await usePicture(file);
  event.target.value = '';
});
$('cropped-input').addEventListener('change', runImage);
$('layout-select').addEventListener('change', runImage);
$('camera-button').addEventListener('click', openCamera);
$('close-camera').addEventListener('click', closeCamera);
$('camera-dialog').addEventListener('cancel', closeCamera);
$('camera-dialog').addEventListener('close', closeCamera);
$('take-picture').addEventListener('click', async () => {
  const video = $('camera-video');
  if (!video.videoWidth) return;
  const canvas = document.createElement('canvas');
  canvas.width = video.videoWidth;
  canvas.height = video.videoHeight;
  canvas.getContext('2d').drawImage(video, 0, 0);
  const blob = await new Promise((resolve) => canvas.toBlob(resolve, 'image/jpeg', .95));
  closeCamera();
  if (blob) await usePicture(blob);
});
window.addEventListener('pagehide', () => { closeCamera(); engineWorker?.terminate(); ocrPromise?.then(({ worker }) => worker.terminate(), () => {}); });

try {
  const response = await fetch('./assets/samples.json');
  if (!response.ok) throw new Error(`The example catalogue could not load (${response.status}).`);
  const manifest = await response.json();
  state.samples = manifest.samples;
  state.references = manifest.references;
  if (!state.samples.length) throw new Error('No public examples are available.');
  $('catalog-size').textContent = `${manifest.catalog_count} public reference printings`;
  $('sample-select').replaceChildren(...state.samples.map((sample) => {
    const option = node('option', '', sample.title);
    option.value = sample.id;
    return option;
  }));
  await selectSample(state.samples[0].id);
} catch (error) {
  status(`The example could not start: ${error.message}`, true);
  busy(false);
}
