const $ = (id) => document.getElementById(id);
const MAX_EDGE = 1280;
const MIN_FRAME_INTERVAL_MS = 250;
const SVG_NS = 'http://www.w3.org/2000/svg';
const state = { generation: 0, running: false, mode: 'idle', stream: undefined, worker: undefined, ready: undefined, pending: new Map(), sequence: 0, frameSequence: 0, timer: undefined, inFlight: false, lastCompleted: undefined, hasResult: false, manifest: undefined, selectedSample: undefined, abort: undefined };
const canvas = document.createElement('canvas');
const pixels = canvas.getContext('2d', { willReadFrequently: true });

function node(tag, className, text) {
  const item = document.createElement(tag);
  if (className) item.className = className;
  if (text !== undefined) item.textContent = text;
  return item;
}
function ms(value) { return Number.isFinite(value) ? `${value.toLocaleString(undefined, { maximumFractionDigits: 1, minimumFractionDigits: 1 })} ms` : '—'; }
function score(value) { return Number.isFinite(value) ? value.toFixed(3) : '—'; }
function printing(card) { return [card?.set?.toUpperCase(), card?.collector_number ? `#${card.collector_number}` : ''].filter(Boolean).join(' · ') || 'Printing not established'; }
function accepted(result) { return ['matched', 'verified_shortlist'].includes(result?.identity?.status); }
function current(generation) { return state.generation === generation; }
function status(message, error = false) { $('camera-status').textContent = message; $('camera-status').classList.toggle('error', error); }
function controls() { $('start-camera').disabled = state.running; $('stop-camera').disabled = !state.running && !state.worker && !state.stream; }
function aborted() { return new DOMException('Scanning stopped.', 'AbortError'); }

function stop({ silent = false, preservePreview = true } = {}) {
  state.generation += 1;
  state.running = false;
  state.mode = 'idle';
  state.abort?.abort();
  state.abort = undefined;
  clearTimeout(state.timer);
  state.timer = undefined;
  state.inFlight = false;
  state.lastCompleted = undefined;
  for (const pending of state.pending.values()) pending.reject(aborted());
  state.pending.clear();
  state.worker?.terminate();
  state.worker = undefined;
  state.ready = undefined;
  for (const track of state.stream?.getTracks() || []) track.stop();
  state.stream = undefined;
  const video = $('camera-preview');
  video.pause();
  video.srcObject = null;
  video.hidden = true;
  if (!preservePreview || $('sample-preview').hidden) {
    $('sample-preview').hidden = true;
    $('preview-empty').hidden = false;
    $('preview-badge').hidden = true;
    $('detection-overlay').replaceChildren();
    $('frame-size').textContent = '';
  }
  $('scan-rate').textContent = '';
  $('pending-frame').textContent = '';
  if (!state.hasResult) {
    $('result-status').textContent = 'No completed result for this input.';
    $('reference-transfer').textContent = 'No completed frame measurements.';
    $('card-results').replaceChildren(node('p', 'empty-result', 'Scanning stopped before an input was processed. Choose the camera or a public example to start again.'));
  }
  if (!silent) {
    status('Stopped. Camera tracks and the processing worker have been released.');
    $('model-status').textContent = 'Model unloaded; it will initialize on the next scan.';
  }
  controls();
}

function begin(mode) {
  if (state.running || state.pending.size || state.stream) stop({ silent: true, preservePreview: false });
  else state.generation += 1;
  state.running = true;
  state.mode = mode;
  state.hasResult = false;
  state.lastCompleted = undefined;
  state.abort = new AbortController();
  $('card-results').replaceChildren(node('p', 'empty-result', 'Waiting for this input to be processed…'));
  $('result-status').textContent = 'Preparing a new input.';
  $('rust-time').textContent = '—';
  $('response-time').textContent = '—';
  $('scan-rate').textContent = '';
  $('pending-frame').textContent = '';
  $('processed-frame').textContent = 'No capture has completed for this input.';
  $('sample-note').hidden = true;
  $('sample-note').textContent = '';
  for (const name of ['capture', 'extraction', 'ocr', 'matching', 'lookup', 'visual', 'fusion']) $(`time-${name}`).textContent = '—';
  $('reference-transfer').textContent = 'Waiting for this frame’s candidate reference requests.';
  $('detection-overlay').replaceChildren();
  controls();
  return state.generation;
}

function workerCall(action, payload = {}, transfer = []) {
  return new Promise((resolve, reject) => {
    const id = ++state.sequence;
    state.pending.set(id, { resolve, reject, action });
    try { state.worker.postMessage({ id, action, ...payload }, transfer); }
    catch (error) { state.pending.delete(id); reject(error); }
  });
}

function ensureWorker() {
  if (!state.worker) {
    const worker = new Worker(new URL('./camera-worker.js', import.meta.url), { type: 'module' });
    state.worker = worker;
    worker.addEventListener('message', ({ data }) => {
      if (state.worker !== worker) return;
      const pending = state.pending.get(data.id);
      if (!pending) return;
      if (data.event === 'progress') {
        if (pending.action === 'initialize') $('model-status').textContent = data.message;
        status(data.message);
        return;
      }
      state.pending.delete(data.id);
      data.error ? pending.reject(new Error(data.error)) : pending.resolve(data);
    });
    worker.addEventListener('error', (event) => {
      if (state.worker !== worker) return;
      event.preventDefault();
      const error = new Error(event.message || 'The processing worker stopped.');
      for (const pending of state.pending.values()) pending.reject(error);
      state.pending.clear();
      stop({ silent: true });
      status(error.message, true);
      $('model-status').textContent = 'Engine stopped. Start again to retry.';
    });
    state.ready = workerCall('initialize').then(({ result }) => {
      if (state.worker === worker) {
        $('model-status').textContent = `${result.model.name || 'ocracle model'} ready. Reused for subsequent frames.`;
        $('setup-time').textContent = `Setup ${ms(result.setup_ms)}.`;
        $('model-details').textContent = `ocracle · ${result.model.name || 'recognition model'} · ${result.model.bytes.toLocaleString()} bytes · SHA-256 ${result.model.sha256}`;
      }
      return result;
    });
  }
  return state.ready;
}

function handleFailure(error, generation) {
  if (!current(generation) || error.name === 'AbortError') return;
  stop({ silent: true });
  const messages = {
    NotAllowedError: 'Camera access was not granted. You can start again or try a public example.',
    NotFoundError: 'No camera was found. Try a public example instead.',
    NotReadableError: 'The camera could not start. It may be in use by another application.',
  };
  status(messages[error.name] || error.message || 'This input could not be processed.', true);
  $('result-status').textContent = 'Processing stopped before a new result was completed.';
  $('model-status').textContent = 'Engine stopped. Choose an input to retry.';
}

async function videoReady(video, signal) {
  if (video.readyState >= 2 && video.videoWidth > 0) return;
  await new Promise((resolve, reject) => {
    const finish = (error) => {
      clearTimeout(timeout);
      video.removeEventListener('loadeddata', loaded);
      signal.removeEventListener('abort', cancelled);
      error ? reject(error) : resolve();
    };
    const loaded = () => finish();
    const cancelled = () => finish(aborted());
    const timeout = setTimeout(() => finish(new Error('The camera did not provide a frame.')), 10000);
    video.addEventListener('loadeddata', loaded, { once: true });
    signal.addEventListener('abort', cancelled, { once: true });
    if (signal.aborted) cancelled();
  });
}

async function startCamera() {
  const generation = begin('camera');
  const signal = state.abort.signal;
  $('sample-preview').hidden = true;
  $('preview-empty').hidden = false;
  state.selectedSample = undefined;
  markExamples();
  try {
    await loadManifest();
    if (!current(generation)) return;
    if (!navigator.mediaDevices?.getUserMedia) throw new Error('Camera access requires a supported browser on HTTPS or localhost. Public examples can still be tried.');
    status('Requesting camera access…');
    const stream = await navigator.mediaDevices.getUserMedia({ video: { facingMode: { ideal: 'environment' }, width: { ideal: 1280 }, height: { ideal: 720 } }, audio: false });
    if (!current(generation)) { for (const track of stream.getTracks()) track.stop(); return; }
    state.stream = stream;
    for (const track of stream.getVideoTracks()) track.addEventListener('ended', () => { if (current(generation)) { stop(); status('The camera stopped providing video. Start it again to resume.'); } }, { once: true });
    const video = $('camera-preview');
    video.srcObject = stream;
    video.hidden = false;
    $('preview-empty').hidden = true;
    $('preview-badge').hidden = false;
    $('preview-badge').textContent = 'Live preview · latest processed outlines';
    await video.play();
    await videoReady(video, signal);
    await ensureWorker();
    if (!current(generation)) return;
    status('Scanning the latest camera frame when the recognizer is ready.');
    tick(generation);
  } catch (error) { handleFailure(error, generation); }
}

async function tick(generation) {
  if (!current(generation) || !state.running || state.mode !== 'camera') return;
  const started = performance.now();
  try { await processInput($('camera-preview'), generation, { cropped: false, classic: false }); }
  catch (error) { handleFailure(error, generation); return; }
  if (current(generation) && state.running && state.mode === 'camera') state.timer = setTimeout(() => tick(generation), Math.max(0, MIN_FRAME_INTERVAL_MS - (performance.now() - started)));
}

async function tryExample(sample) {
  const generation = begin('sample');
  state.selectedSample = sample.id;
  markExamples();
  if (sample.camera_note) { $('sample-note').textContent = sample.camera_note; $('sample-note').hidden = false; }
  $('camera-preview').hidden = true;
  $('preview-empty').hidden = false;
  try {
    const image = $('sample-preview');
    image.src = sample.image;
    image.alt = sample.title;
    await image.decode();
    if (!current(generation)) return;
    image.hidden = false;
    $('preview-empty').hidden = true;
    $('preview-badge').hidden = false;
    $('preview-badge').textContent = sample.title;
    status('Preparing this public image for the browser pipeline…');
    await ensureWorker();
    if (!current(generation)) return;
    await processInput(image, generation, { cropped: sample.cropped, classic: sample.layout === 'classic' });
    if (!current(generation)) return;
    state.running = false;
    state.mode = 'idle';
    controls();
    status('Public example complete. The loaded engine is ready for another example or the camera.');
  } catch (error) { handleFailure(error, generation); }
}

async function processInput(source, generation, options) {
  if (state.inFlight) throw new Error('A frame is already pending.');
  state.inFlight = true;
  const started = performance.now();
  const frameId = ++state.frameSequence;
  const capturedAt = new Date().toLocaleTimeString();
  const camera = state.mode === 'camera';
  $('pending-frame').textContent = `${camera ? 'Camera frame' : 'Image capture'} #${frameId} captured at ${capturedAt} is being processed.${camera ? ' The live preview continues.' : ''}`;
  try {
    const sourceWidth = source.videoWidth || source.naturalWidth;
    const sourceHeight = source.videoHeight || source.naturalHeight;
    if (!sourceWidth || !sourceHeight) throw new Error('This input has no decoded frame.');
    const scale = Math.min(1, MAX_EDGE / Math.max(sourceWidth, sourceHeight));
    canvas.width = Math.max(1, Math.round(sourceWidth * scale));
    canvas.height = Math.max(1, Math.round(sourceHeight * scale));
    pixels.drawImage(source, 0, 0, canvas.width, canvas.height);
    const image = pixels.getImageData(0, 0, canvas.width, canvas.height);
    const captureMs = performance.now() - started;
    const response = await workerCall('frame', { width: canvas.width, height: canvas.height, rgba: image.data.buffer, ...options }, [image.data.buffer]);
    if (!current(generation)) return;
    const completed = performance.now();
    renderReport(response.result, { capture: captureMs, response: completed - started, worker: response.worker_elapsed_ms, transport: response.transport, cropped: options.cropped, frameId, capturedAt, camera });
    if (state.mode === 'camera' && state.lastCompleted !== undefined) $('scan-rate').textContent = `${(1000 / (completed - state.lastCompleted)).toFixed(1)} scans/sec`;
    state.lastCompleted = completed;
  } finally { if (current(generation)) { state.inFlight = false; $('pending-frame').textContent = ''; } }
}

function renderReport(report, measured) {
  state.hasResult = true;
  const cards = report.cards || [];
  $('frame-size').textContent = `${report.width} × ${report.height}`;
  $('rust-time').textContent = ms(report.total_ms);
  $('response-time').textContent = ms(measured.response);
  $('processed-frame').textContent = `${measured.camera ? 'Camera frame' : 'Public image capture'} #${measured.frameId} · captured at ${measured.capturedAt} · response arrived ${ms(measured.response)} later.`;
  if (measured.camera) $('preview-badge').textContent = `Live preview · outlines from frame #${measured.frameId}`;
  $('result-status').textContent = cards.length ? measured.cropped ? 'The supplied card crop was processed directly.' : `${cards.length} card${cards.length === 1 ? '' : 's'} detected in the latest processed input.` : 'No card-shaped region was detected in this input.';
  $('card-results').replaceChildren(...cards.map(renderCard));
  if (!cards.length) $('card-results').append(node('p', 'empty-result', 'OCR and candidate lookup did not run because extraction found no cards.'));
  const attempts = cards.flatMap((card) => card.attempts || []);
  const sum = (value) => attempts.reduce((total, attempt) => total + (value(attempt) || 0), 0);
  const times = { capture: measured.capture, extraction: report.extraction_ms, ocr: cards.reduce((total, card) => total + (card.timings?.ocr_ms || 0), 0), matching: cards.reduce((total, card) => total + (card.timings?.matching_ms || 0), 0), lookup: sum((attempt) => attempt.identification.timings?.text_retrieval), visual: sum((attempt) => attempt.identification.timings?.visual_matching), fusion: sum((attempt) => attempt.identification.timings?.fusion) };
  for (const [name, value] of Object.entries(times)) $(`time-${name}`).textContent = ms(value);
  const transport = measured.transport;
  $('reference-transfer').textContent = transport ? `Candidate references: ${transport.requests} image fetch${transport.requests === 1 ? '' : 'es'} · ${transport.downloaded_bytes.toLocaleString()} reference bytes read · ${ms(transport.fetch_ms)} transfer and validation · ${ms(transport.enrollment_ms)} preparing reference features.${Number.isFinite(transport.cache_hits) ? ` ${transport.cache_hits} cache hits.` : ''}` : 'Reference transfer metrics were not returned by this engine.';
  $('timing-note').textContent = `Matching total includes all orientations and retries while reference images were unavailable. Lookup, verification and fusion show only the completed matching calls; they are a partial breakdown of that total and must not be added to it. Rust time covers active frame processing. Frame response also includes reference downloads, preparation, worker messaging and result serialization. Crop preview export: ${ms(report.diagnostic_export_ms)}. One-time model and catalogue setup is separate.`;
  const overlay = $('detection-overlay');
  overlay.replaceChildren();
  overlay.setAttribute('viewBox', `0 0 ${report.width} ${report.height}`);
  overlay.setAttribute('preserveAspectRatio', 'xMidYMid meet');
  for (const card of cards) if (card.corners?.length) {
    const polygon = document.createElementNS(SVG_NS, 'polygon');
    polygon.setAttribute('points', card.corners.map((point) => point.join(',')).join(' '));
    overlay.append(polygon);
  }
  const firstModel = attempts[0]?.ocr?.reading?.model;
  if (firstModel) $('model-details').textContent = `Actual OCR: ${firstModel.name} · backend ${firstModel.backend} · language ${firstModel.language} · SHA-256 ${firstModel.sha256}`;
}

function renderCard(card, index) {
  const selected = card.attempts?.[card.selected_attempt];
  const result = selected?.identification;
  const article = node('article', 'detected-card');
  article.dataset.cardIndex = String(index);
  if (!result) { article.append(node('p', 'empty-result', 'No identification attempt was returned for this card.')); return article; }
  const conflict = card.orientation_conflict || result.identity.status === 'conflict';
  const isAccepted = !conflict && accepted(result);
  const heading = node('div', 'card-result-heading');
  if (card.crop_data_url) {
    const image = node('img', 'detected-crop');
    image.src = card.crop_data_url;
    image.alt = `Actual card input for card ${index + 1}`;
    heading.append(image);
  }
  const summary = node('div');
  summary.append(node('span', `card-status${conflict ? ' conflict' : isAccepted ? ' accepted' : ''}`, conflict ? 'Conflicting evidence' : isAccepted ? 'Identity supported' : 'Not identified'));
  summary.append(node('h3', '', isAccepted && result.identity.card ? result.identity.card.name : conflict ? 'More evidence needed' : `Card ${index + 1} remains unresolved`));
  summary.append(node('p', 'printing-decision', !conflict && result.printing.card ? `Printing: ${printing(result.printing.card)}` : 'Printing unresolved'));
  heading.append(summary);
  article.append(heading);
  const observations = node('dl', 'ocr-observations');
  for (const [label, text] of [['Literal title OCR', selected.ocr?.reading?.text], ['Literal collector OCR', selected.footer_ocr?.observations?.footer]]) {
    const field = node('div'); field.append(node('dt', '', label), node('dd', '', text || '(No text read)')); observations.append(field);
  }
  article.append(observations);
  const candidates = result.visual.recognition?.candidates || [];
  const ranking = node('ol', 'card-match-list');
  for (const candidate of candidates) {
    const row = node('li', 'card-match');
    row.dataset.similarity = String(candidate.similarity);
    const label = node('div', 'card-match-name', candidate.card.face_name || candidate.card.name);
    const verified = (result.visual.geometry_verified || []).some((entry) => entry.card.game === candidate.card.game && entry.card.id === candidate.card.id && entry.card.face_index === candidate.card.face_index);
    label.append(node('span', 'card-match-printing', `${printing(candidate.card)} · ${verified ? 'Geometry agrees' : 'Unverified'}`));
    const value = node('span', 'card-match-score', score(candidate.similarity)); value.append(node('small', '', 'Visual similarity'));
    const meter = node('meter'); meter.min = 0; meter.max = 1; meter.value = candidate.similarity; meter.setAttribute('aria-label', `${candidate.card.name}, ${printing(candidate.card)}: visual similarity`); meter.setAttribute('aria-valuetext', `${score(candidate.similarity)} on a scale from 0 to 1`);
    row.append(label, value, meter); ranking.append(row);
  }
  article.append(ranking);
  if (!candidates.length) article.append(node('p', 'quiet', 'No visual candidates were returned.'));
  const details = node('details', 'card-details');
  details.append(node('summary', '', `Evidence and ${card.attempts.length} orientation attempt${card.attempts.length === 1 ? '' : 's'}`));
  details.append(node('p', '', card.orientation_conflict ? 'Strong text readings disagree across orientations. The selected attempt remains provisional.' : result.identity.reason));
  details.append(node('p', '', result.printing.reason));
  details.append(node('p', '', `Compared ${result.visual.descriptors_scored} candidate descriptors; ${result.visual.geometry_attempted} geometry checks. Similarities are heuristic scores, not calibrated probabilities.`));
  details.append(node('p', '', result.visual.search_complete ? `Native heuristic printing confidence: ${score(result.visual.recognition.confidence)} on a 0–1 scale. This score is uncalibrated.` : 'Printing confidence is not established by a search limited to these shortlisted references.'));
  for (const attempt of card.attempts) details.append(node('p', '', `${attempt.ocr.rotation_degrees}° · title ${JSON.stringify(attempt.ocr.reading.text)} · collector ${JSON.stringify(attempt.footer_ocr?.observations.footer || '')} · ${ms(attempt.ocr_ms)} OCR + ${ms(attempt.matching_ms)} matching`));
  for (const disagreement of result.conflicts || []) details.append(node('p', '', `${disagreement.resolved ? 'Resolved' : 'Unresolved'} ${disagreement.kind} disagreement: ${disagreement.reason}`));
  article.append(details);
  return article;
}

function markExamples() {
  document.querySelectorAll('.example-button').forEach((button) => { const active = button.dataset.sample === state.selectedSample; button.classList.toggle('active', active); button.setAttribute('aria-pressed', String(active)); });
}

let manifestPromise;
function loadManifest() {
  if (state.manifest) return Promise.resolve(state.manifest);
  if (manifestPromise) return manifestPromise;
  manifestPromise = (async () => {
    const response = await fetch('./assets/camera/manifest.json');
    if (!response.ok) throw new Error(`Browser camera assets are unavailable (${response.status}).`);
    const manifest = await response.json();
    if (manifest.schema_version !== 1 || !Number.isSafeInteger(manifest.catalogue?.faces) || manifest.catalogue.faces < 1 || typeof manifest.catalogue.label !== 'string' || !Number.isSafeInteger(manifest.model?.bytes) || manifest.model.bytes < 1 || !Array.isArray(manifest.samples)) throw new Error('The camera asset manifest is unsupported.');
    $('catalog-scope').textContent = `${manifest.catalogue.faces.toLocaleString()} English reference faces. ${manifest.catalogue.label}`;
    $('model-status').textContent = `The ${(manifest.model.bytes / 1_000_000).toFixed(1)} MB OCR model loads when you choose an input.`;
    $('example-buttons').replaceChildren(...manifest.samples.map((sample) => {
      const button = node('button', 'example-button'); button.type = 'button'; button.dataset.sample = sample.id;
      const image = node('img'); image.src = sample.image; image.alt = '';
      button.append(image, node('span', '', sample.title));
      button.addEventListener('click', () => tryExample(sample));
      return button;
    }));
    state.manifest = manifest;
    return manifest;
  })().catch((error) => { manifestPromise = undefined; throw error; });
  return manifestPromise;
}
loadManifest().catch((error) => { $('catalog-scope').textContent = error.message; $('example-buttons').replaceChildren(node('p', 'quiet', 'Public examples could not load. Start the camera to retry loading the catalogue.')); status(error.message, true); });
$('start-camera').addEventListener('click', startCamera);
$('stop-camera').addEventListener('click', () => stop());
window.addEventListener('pagehide', () => stop({ preservePreview: false }));
document.addEventListener('visibilitychange', () => { if (document.hidden && state.mode === 'camera') { stop(); status('Camera stopped while this page was out of view. Start it again to resume.'); } });
