const $ = (id) => document.getElementById(id);
const SVG_NS = 'http://www.w3.org/2000/svg';
const regionDescriptions = {
  artwork: ['Artwork', 'Compares the artwork’s structure and colour.', '#b44d57'],
  title: ['Title bar', 'Fuzzy name matching identifies cards but usually leaves reprints unresolved.', '#95620b'],
  rules: ['Rules text', 'Matches distinctive words against printed and Oracle text.', '#326a9f'],
  footer: ['Bottom bar', 'Looks for a set code, collector number, and language.', '#8053a0'],
};
const templates = {
  modern: { title: [.07, .04, .80, .10], artwork: [.09, .16, .91, .57], rules: [.075, .60, .925, .90], footer: [.035, .90, .96, .995] },
  classic: { title: [.06, .05, .81, .12], artwork: [.09, .19, .91, .59], rules: [.08, .63, .92, .90], footer: [.035, .92, .96, .995] },
};
const state = { samples: [], references: [], report: undefined, cardIndex: 0, candidateIndex: 0, region: 'artwork', rotation: 0, sourceUrl: '', sourceBytes: undefined, crop: undefined, busy: false, request: 0 };
let engineWorker;
let nextWorkerRequest = 0;
const pendingRequests = new Map();
let ocrWorker;
let ocrGeneration = 0;
let ocrRunning = false;
let activeOcrGeneration = 0;
let cameraStream;

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function formatScore(score) {
  return Number.isFinite(score) ? score.toFixed(3) : '—';
}

function printingLabel(card) {
  return [card?.set?.toUpperCase(), card?.collector_number ? `#${card.collector_number}` : null, card?.language?.toUpperCase()].filter(Boolean).join(' · ') || 'Printing metadata unavailable';
}

function currentCard() { return state.report?.cards?.[state.cardIndex]; }
function currentCandidate() { return currentCard()?.recognition?.candidates?.[state.candidateIndex]; }
function referenceFor(card) {
  return state.references.find((reference) => reference.id === card?.id && (reference.face_index ?? 0) === (card?.face_index ?? 0));
}

function setStatus(message, error = false) {
  $('status-message').textContent = message;
  $('status-message').classList.toggle('error', error);
}

function setBusy(busy) {
  state.busy = busy;
  for (const id of ['sample-select', 'rerun-button', 'camera-button']) $(id).disabled = busy || (id === 'rerun-button' && !state.sourceBytes);
  $('file-input').disabled = busy;
}

function runEngine(action, payload = {}) {
  if (!engineWorker) {
    engineWorker = new Worker(new URL('./engine-worker.js', import.meta.url), { type: 'module' });
    engineWorker.addEventListener('message', ({ data }) => {
      const pending = pendingRequests.get(data.id);
      if (!pending) return;
      pendingRequests.delete(data.id);
      data.error ? pending.reject(new Error(data.error)) : pending.resolve(data.result);
    });
    engineWorker.addEventListener('error', (error) => {
      for (const request of pendingRequests.values()) request.reject(new Error(error.message || 'The recognition worker stopped.'));
      pendingRequests.clear();
      engineWorker.terminate();
      engineWorker = undefined;
    });
  }
  return new Promise((resolve, reject) => {
    const id = ++nextWorkerRequest;
    pendingRequests.set(id, { resolve, reject });
    engineWorker.postMessage({ id, action, ...payload });
  });
}

async function loadImage(src) {
  const image = new Image();
  image.decoding = 'async';
  image.src = src;
  await image.decode();
  return image;
}

async function showSource(url) {
  const image = $('source-image');
  image.src = url;
  await image.decode();
  image.hidden = false;
  $('source-placeholder').hidden = true;
  $('source-dimensions').textContent = `${image.naturalWidth} × ${image.naturalHeight}`;
  drawSourceOverlay();
}

function drawSourceOverlay() {
  const overlay = $('source-overlay');
  const image = $('source-image');
  overlay.replaceChildren();
  if (!state.report || !image.naturalWidth) return;
  const width = state.report.width || image.naturalWidth;
  const height = state.report.height || image.naturalHeight;
  overlay.setAttribute('viewBox', `0 0 ${width} ${height}`);
  overlay.setAttribute('preserveAspectRatio', 'xMidYMid meet');
  state.report.cards.forEach((card, index) => {
    if (!card.corners?.length) return;
    const polygon = document.createElementNS(SVG_NS, 'polygon');
    polygon.setAttribute('points', card.corners.map((point) => Array.isArray(point) ? point.join(',') : `${point.x},${point.y}`).join(' '));
    polygon.classList.toggle('selected', index === state.cardIndex);
    polygon.setAttribute('role', 'button');
    polygon.setAttribute('tabindex', '0');
    polygon.setAttribute('aria-label', `Inspect extracted card ${index + 1}`);
    polygon.addEventListener('click', () => selectCard(index));
    polygon.addEventListener('keydown', (event) => { if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); selectCard(index); } });
    overlay.append(polygon);
  });
}

function resetTextEvidence() {
  ocrGeneration += 1;
  for (const region of ['title', 'footer', 'rules']) {
    $(`${region}-text`).value = '';
    $(`${region}-confidence`).textContent = '';
  }
  $('text-results').replaceChildren();
  $('ocr-status').textContent = 'No OCR run yet.';
  $('text-search-button').disabled = true;
}

async function selectCard(index) {
  state.cardIndex = index;
  state.candidateIndex = 0;
  state.rotation = 0;
  state.crop = undefined;
  $('raw-crop-image').hidden = true;
  $('raw-crop-button').hidden = true;
  $('raw-crop-button').setAttribute('aria-pressed', 'false');
  $('raw-crop-button').textContent = 'Show raw crop';
  resetTextEvidence();
  const card = currentCard();
  const tabs = $('card-tabs');
  tabs.replaceChildren();
  state.report?.cards.forEach((_, cardIndex) => {
    const button = element('button', `card-tab${cardIndex === index ? ' active' : ''}`, `Card ${cardIndex + 1}`);
    button.setAttribute('aria-pressed', String(cardIndex === index));
    button.addEventListener('click', () => selectCard(cardIndex));
    tabs.append(button);
  });
  drawSourceOverlay();
  renderRecognition();
  renderMeasurements();
  $('rotate-button').disabled = !card?.crop_data_url;
  $('ocr-button').disabled = ocrRunning || !card?.crop_data_url;
  if (card?.crop_data_url) {
    try {
      const image = await loadImage(card.crop_data_url);
      if (currentCard() !== card) return;
      state.crop = image;
      if (card.raw_crop_data_url) {
        $('raw-crop-image').src = card.raw_crop_data_url;
        $('raw-crop-button').hidden = false;
      }
      $('ocr-button').disabled = ocrRunning;
      drawCrop();
      $('crop-placeholder').hidden = true;
      const rotation = card.upright_rotation_degrees || 0;
      $('crop-information').textContent = rotation ? `Rectified · ${rotation}° orientation corrected` : 'Perspective corrected';
    } catch (error) {
      $('crop-placeholder').hidden = false;
      $('crop-placeholder').textContent = 'This crop could not be displayed.';
      setStatus(error.message, true);
    }
  } else {
    $('crop-canvas').getContext('2d').clearRect(0, 0, 500, 700);
    $('region-preview').getContext('2d').clearRect(0, 0, 360, 180);
    $('region-overlay').replaceChildren();
    $('crop-placeholder').hidden = false;
    $('crop-placeholder').textContent = state.report?.cards.length ? 'Crop unavailable in this report.' : 'No card-shaped region was detected.';
    $('crop-information').textContent = 'No crop to inspect';
  }
}

function drawCrop() {
  if (!state.crop) return;
  const canvas = $('crop-canvas');
  const context = canvas.getContext('2d');
  context.clearRect(0, 0, canvas.width, canvas.height);
  context.save();
  context.translate(250, 350);
  context.rotate(state.rotation * Math.PI / 180);
  if (state.rotation % 180) context.drawImage(state.crop, -350, -250, 700, 500);
  else context.drawImage(state.crop, -250, -350, 500, 700);
  context.restore();
  drawRegions();
  drawRegionPreview();
}

function drawRegions() {
  const overlay = $('region-overlay');
  overlay.replaceChildren();
  if (!state.crop) return;
  const profile = templates[$('layout-select').value];
  for (const [name, [left, top, right, bottom]] of Object.entries(profile)) {
    const rectangle = document.createElementNS(SVG_NS, 'rect');
    rectangle.setAttribute('x', left * 500);
    rectangle.setAttribute('y', top * 700);
    rectangle.setAttribute('width', (right - left) * 500);
    rectangle.setAttribute('height', (bottom - top) * 700);
    rectangle.setAttribute('rx', '2');
    rectangle.setAttribute('stroke', regionDescriptions[name][2]);
    rectangle.style.setProperty('--region-fill', `${regionDescriptions[name][2]}22`);
    rectangle.classList.toggle('active', name === state.region);
    rectangle.setAttribute('role', 'button');
    rectangle.setAttribute('tabindex', '0');
    rectangle.setAttribute('aria-label', `Inspect ${regionDescriptions[name][0]}`);
    rectangle.addEventListener('click', () => setRegion(name));
    rectangle.addEventListener('keydown', (event) => { if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); setRegion(name); } });
    overlay.append(rectangle);
  }
}

function setRegion(region) {
  state.region = region;
  document.querySelectorAll('.region-button').forEach((button) => {
    button.classList.toggle('active', button.dataset.region === region);
    button.setAttribute('aria-pressed', String(button.dataset.region === region));
  });
  $('region-name').textContent = regionDescriptions[region][0];
  $('region-name').style.color = regionDescriptions[region][2];
  $('region-description').textContent = regionDescriptions[region][1];
  drawRegions();
  drawRegionPreview();
}

function regionCanvas(region, scale = 1) {
  const source = $('crop-canvas');
  const [left, top, right, bottom] = templates[$('layout-select').value][region];
  const canvas = document.createElement('canvas');
  canvas.width = Math.round((right - left) * source.width * scale);
  canvas.height = Math.round((bottom - top) * source.height * scale);
  canvas.getContext('2d').drawImage(source, left * source.width, top * source.height, (right - left) * source.width, (bottom - top) * source.height, 0, 0, canvas.width, canvas.height);
  return canvas;
}

function drawRegionPreview() {
  const canvas = $('region-preview');
  const context = canvas.getContext('2d');
  context.clearRect(0, 0, canvas.width, canvas.height);
  if (!state.crop) return;
  const region = regionCanvas(state.region);
  const scale = Math.min(canvas.width / region.width, canvas.height / region.height);
  const width = region.width * scale;
  const height = region.height * scale;
  context.drawImage(region, (canvas.width - width) / 2, (canvas.height - height) / 2, width, height);
}

function renderRecognition() {
  const recognition = currentCard()?.recognition;
  const result = $('recognition-result');
  const list = $('candidate-list');
  result.replaceChildren();
  list.replaceChildren();
  if (!recognition?.candidates?.length) {
    result.append(element('div', 'result-placeholder', state.report?.cards?.length ? 'No reference candidates.' : 'No card to recognise.'));
    return;
  }
  const best = recognition.candidates[0];
  const topLine = element('div', 'result-topline');
  const identityStatus = recognition.identity_status || recognition.status;
  topLine.append(element('span', `status-pill ${identityStatus}`, `Identity: ${identityStatus}`));
  topLine.append(element('span', `status-pill ${recognition.status}`, `Printing: ${recognition.status}`));
  result.append(topLine);
  const cardRow = element('div', 'recognition-card');
  const reference = referenceFor(best.card);
  if (reference?.image) {
    const thumbnail = element('img', 'reference-thumb');
    thumbnail.src = reference.image;
    thumbnail.alt = `${best.card.name} reference image`;
    cardRow.append(thumbnail);
  }
  const info = element('div');
  info.append(element('h4', 'result-name', best.card.face_name || best.card.name));
  info.append(element('p', 'result-printing', printingLabel(best.card)));
  const caveat = identityStatus === 'unknown' ? 'Closest reference; below the acceptance threshold.' : identityStatus === 'ambiguous' ? 'Several card identities have similar scores.' : recognition.status === 'ambiguous' ? 'Card identity matched; printing unresolved.' : `Confidence ${formatScore(recognition.confidence)} (uncalibrated)`;
  info.append(element('p', 'result-caveat', caveat));
  cardRow.append(info);
  result.append(cardRow);
  recognition.candidates.slice(0, 5).forEach((candidate, index) => {
    const item = element('li');
    const button = element('button', `candidate-button${index === state.candidateIndex ? ' active' : ''}`);
    button.setAttribute('aria-pressed', String(index === state.candidateIndex));
    button.append(element('span', 'candidate-number', String(index + 1)));
    const detail = element('span', 'candidate-information');
    detail.append(element('span', 'candidate-name', candidate.card.face_name || candidate.card.name));
    detail.append(element('span', 'candidate-set', printingLabel(candidate.card)));
    button.append(detail, element('span', 'candidate-score', formatScore(candidate.similarity)));
    button.addEventListener('click', () => { state.candidateIndex = index; renderRecognition(); renderMeasurements(); });
    item.append(button);
    list.append(item);
  });
}

function renderMeasurements() {
  const node = $('visual-measurements');
  node.replaceChildren();
  const candidate = currentCandidate();
  const evidence = currentCard()?.visual_evidence?.find((entry) => entry.id === candidate?.card.id && (entry.face_index ?? 0) === (candidate?.card.face_index ?? 0));
  if (!evidence) { node.append(element('p', 'empty-state', 'Component measurements are unavailable for this result.')); return; }
  const rows = [
    ['Artwork structure', evidence.artwork_luminance, 'Luminance correlation in the artwork window · weight 45%'],
    ['Artwork colour', evidence.artwork_colour, 'HSV colour histogram similarity · weight 30%'],
    ['Full-card structure', evidence.full_luminance, 'Luminance correlation across the card · weight 20%'],
    ['Artwork fingerprint', evidence.artwork_hash, 'Perceptual hash similarity · weight 5%'],
  ];
  for (const [title, value, description] of rows) {
    const row = element('div', 'measurement');
    const label = element('div', 'measurement-label');
    label.append(element('span', '', title), element('span', 'measurement-score', formatScore(value)));
    const bar = element('div', 'measurement-bar');
    const fill = element('span');
    fill.style.width = `${Math.max(0, Math.min(1, value || 0)) * 100}%`;
    bar.append(fill);
    row.append(label, bar, element('p', 'measurement-description', description));
    node.append(row);
  }
  const method = String(evidence.method || 'global visual comparison').replaceAll('_', ' ');
  const inset = Number.isFinite(evidence.reference_inset) ? ` · reference inset ${(evidence.reference_inset * 100).toFixed(0)}%` : '';
  $('visual-method-note').textContent = `${candidate.card.face_name || candidate.card.name}: ${method}${inset}. Components above explain the global comparison. Geometrically verified local-feature fallback, when used, has a separate score.`;
}

async function displayReport(report) {
  state.report = report;
  $('detection-count').textContent = `${report.cards.length} card${report.cards.length === 1 ? '' : 's'} detected`;
  await selectCard(0);
}

async function selectSample(id) {
  const sample = state.samples.find((entry) => entry.id === id);
  if (!sample) return;
  const request = ++state.request;
  setBusy(true);
  setStatus('Loading the public test capture and its recorded extraction…');
  $('sample-description').textContent = sample.description || 'Public rendered test capture.';
  $('cropped-input').checked = !!sample.cropped;
  $('layout-select').value = sample.layout === 'classic' ? 'classic' : 'modern';
  try {
    const [imageResponse, reportResponse] = await Promise.all([fetch(sample.image), typeof sample.report === 'string' ? fetch(sample.report) : Promise.resolve(null)]);
    if (!imageResponse.ok) throw new Error('The sample picture could not be loaded.');
    if (reportResponse && !reportResponse.ok) throw new Error('The sample report could not be loaded.');
    const [bytes, report] = await Promise.all([imageResponse.arrayBuffer(), reportResponse ? reportResponse.json() : Promise.resolve(sample.report)]);
    if (request !== state.request) return;
    state.sourceBytes = new Uint8Array(bytes);
    if (state.sourceUrl.startsWith('blob:')) URL.revokeObjectURL(state.sourceUrl);
    state.sourceUrl = sample.image;
    await showSource(sample.image);
    await displayReport(report);
    setStatus('Saved sample output. Run again to repeat extraction and matching.');
  } catch (error) {
    setStatus(error.message, true);
  } finally {
    if (request === state.request) setBusy(false);
  }
}

async function scanCurrentImage() {
  if (!state.sourceBytes) return;
  setBusy(true);
  setStatus('Extracting and matching cards…');
  const started = performance.now();
  try {
    const report = await runEngine('scan', { bytes: state.sourceBytes, cropped: $('cropped-input').checked });
    await displayReport(report);
    const elapsed = ((performance.now() - started) / 1000).toFixed(1);
    setStatus(`Processed in ${elapsed}s. ${report.cards.length} card${report.cards.length === 1 ? '' : 's'} extracted.`);
  } catch (error) {
    setStatus(`Recognition could not complete: ${error.message}`, true);
  } finally { setBusy(false); }
}

async function usePicture(blob, description) {
  setBusy(true);
  setStatus('Preparing the picture locally…');
  try {
    const image = await createImageBitmap(blob, { imageOrientation: 'from-image' });
    const scale = Math.min(1, 2200 / Math.max(image.width, image.height));
    const canvas = document.createElement('canvas');
    canvas.width = Math.round(image.width * scale);
    canvas.height = Math.round(image.height * scale);
    canvas.getContext('2d').drawImage(image, 0, 0, canvas.width, canvas.height);
    image.close();
    const prepared = await new Promise((resolve) => canvas.toBlob(resolve, 'image/jpeg', .95));
    if (!prepared) throw new Error('The browser could not prepare this image.');
    state.sourceBytes = new Uint8Array(await prepared.arrayBuffer());
    if (state.sourceUrl.startsWith('blob:')) URL.revokeObjectURL(state.sourceUrl);
    state.sourceUrl = URL.createObjectURL(prepared);
    state.report = undefined;
    $('sample-select').value = '';
    $('sample-description').textContent = `${description} Processed on this device; this picture is not uploaded.`;
    await showSource(state.sourceUrl);
    await scanCurrentImage();
  } catch (error) { setStatus(`This picture could not be read: ${error.message}`, true); }
  finally { setBusy(false); }
}

async function getOcrWorker() {
  if (!ocrWorker) {
    const { default: { createWorker } } = await import('./ocr/tesseract.esm.min.js');
    ocrWorker = createWorker('eng', 1, {
      workerPath: new URL('./ocr/worker.min.js', location.href).href,
      corePath: new URL('./ocr/core/', location.href).href,
      langPath: new URL('./ocr/lang', location.href).href,
      workerBlobURL: false,
      logger: ({ status, progress }) => {
        if (ocrRunning && activeOcrGeneration === ocrGeneration) $('ocr-status').textContent = `${status}${Number.isFinite(progress) ? ` · ${Math.round(progress * 100)}%` : ''}`;
      },
    });
    ocrWorker.catch(() => { ocrWorker = undefined; });
  }
  return ocrWorker;
}

async function readCardText() {
  if (!state.crop || ocrRunning) return;
  const generation = ++ocrGeneration;
  activeOcrGeneration = generation;
  ocrRunning = true;
  const card = currentCard();
  const snapshots = Object.fromEntries(['title', 'footer', 'rules'].map((region) => [region, regionCanvas(region, 2)]));
  $('ocr-button').disabled = true;
  $('layout-select').disabled = true;
  $('text-search-button').disabled = true;
  $('ocr-status').textContent = 'Loading local English OCR…';
  try {
    const worker = await getOcrWorker();
    for (const region of ['title', 'footer', 'rules']) {
      if (generation !== ocrGeneration || currentCard() !== card) return;
      $('ocr-status').textContent = `Reading ${regionDescriptions[region][0].toLowerCase()}…`;
      await worker.setParameters({ tessedit_pageseg_mode: region === 'title' ? '7' : region === 'footer' ? '6' : '6', preserve_interword_spaces: '1' });
      const { data } = await worker.recognize(snapshots[region]);
      if (generation !== ocrGeneration || currentCard() !== card) return;
      $(`${region}-text`).value = data.text.trim();
      $(`${region}-confidence`).textContent = `OCR ${Math.round(data.confidence)} / 100`;
    }
    $('ocr-status').textContent = 'Text extracted. Correct any OCR errors, then search.';
    updateTextSearchButton();
  } catch (error) {
    if (generation === ocrGeneration) {
      $('ocr-status').textContent = `OCR failed: ${error.message}`;
      updateTextSearchButton();
    }
  } finally {
    ocrRunning = false;
    $('ocr-button').disabled = !state.crop;
    $('layout-select').disabled = false;
  }
}

function updateTextSearchButton() {
  $('text-search-button').disabled = !['title', 'footer', 'rules'].some((region) => $(`${region}-text`).value.trim());
}

async function searchText() {
  const generation = ocrGeneration;
  $('text-search-button').disabled = true;
  $('ocr-status').textContent = 'Searching the local catalog…';
  try {
    const observations = Object.fromEntries(['title', 'footer', 'rules'].map((region) => [region, $(`${region}-text`).value]));
    const matches = await runEngine('text', { observations });
    if (generation !== ocrGeneration) return;
    renderTextResults(matches);
    $('ocr-status').textContent = 'Text results are separate from the visual ranking.';
  } catch (error) { if (generation === ocrGeneration) $('ocr-status').textContent = `Text search failed: ${error.message}`; }
  finally { if (generation === ocrGeneration) updateTextSearchButton(); }
}

function renderTextResults(matches) {
  const results = $('text-results');
  results.replaceChildren();
  if (!matches.length) { results.append(element('p', 'empty-state', 'No useful text evidence matched. Try a readable title, set + collector number, or more distinctive rules text.')); return; }
  for (const match of matches.slice(0, 5)) {
    const row = element('article', 'text-result');
    const heading = element('div', 'text-result-heading');
    heading.append(element('strong', '', match.card.face_name || match.card.name), element('span', '', formatScore(match.score)));
    const metrics = element('div', 'text-result-metrics');
    if (match.footer_match) metrics.append(element('span', '', 'Set + collector number match'));
    if (typeof match.language_match === 'boolean') metrics.append(element('span', '', match.language_match ? 'Language match' : 'Language differs'));
    if (Number.isFinite(match.title_similarity)) metrics.append(element('span', '', `Title ${formatScore(match.title_similarity)}`));
    if (Number.isFinite(match.rules_similarity)) metrics.append(element('span', '', `Rules ${formatScore(match.rules_similarity)}`));
    if (Number.isFinite(match.rules_coverage)) metrics.append(element('span', '', `Rules coverage ${formatScore(match.rules_coverage)}`));
    row.append(heading, metrics, element('p', '', `${printingLabel(match.card)}${match.reasons?.length ? ` · ${match.reasons.join('; ')}` : ''}`));
    const reference = referenceFor(match.card) || match.card;
    if (reference.oracle_text || reference.printed_text) {
      const comparison = element('details', 'reference-text-comparison');
      comparison.append(element('summary', '', 'Compare reference text'));
      if (reference.printed_text) comparison.append(element('p', '', `Printed text: ${reference.printed_text}`));
      if (reference.oracle_text) comparison.append(element('p', '', `Oracle text: ${reference.oracle_text}`));
      row.append(comparison);
    }
    results.append(row);
  }
}

async function openCamera() {
  if (!navigator.mediaDevices?.getUserMedia) { setStatus('Camera access requires a browser with camera support and an HTTPS connection. You can upload a picture instead.', true); return; }
  try {
    cameraStream = await navigator.mediaDevices.getUserMedia({ video: { facingMode: { ideal: 'environment' }, width: { ideal: 1920 }, height: { ideal: 1080 } }, audio: false });
    $('camera-video').srcObject = cameraStream;
    $('camera-dialog').showModal();
  } catch (error) { setStatus(`The camera could not be opened: ${error.message}`, true); }
}

function closeCamera() {
  cameraStream?.getTracks().forEach((track) => track.stop());
  cameraStream = undefined;
  $('camera-video').srcObject = null;
  if ($('camera-dialog').open) $('camera-dialog').close();
}

async function takePicture() {
  const video = $('camera-video');
  if (!video.videoWidth) return;
  const canvas = document.createElement('canvas');
  canvas.width = video.videoWidth;
  canvas.height = video.videoHeight;
  canvas.getContext('2d').drawImage(video, 0, 0);
  const blob = await new Promise((resolve) => canvas.toBlob(resolve, 'image/jpeg', .95));
  closeCamera();
  if (blob) await usePicture(blob, 'Camera capture.');
}

$('sample-select').addEventListener('change', (event) => selectSample(event.target.value));
$('rerun-button').addEventListener('click', scanCurrentImage);
$('file-input').addEventListener('change', async (event) => {
  const file = event.target.files?.[0];
  if (file) await usePicture(file, `Your picture: ${file.name}.`);
  event.target.value = '';
});
document.querySelectorAll('.region-button').forEach((button) => button.addEventListener('click', () => setRegion(button.dataset.region)));
$('rotate-button').addEventListener('click', () => { state.rotation = (state.rotation + 90) % 360; resetTextEvidence(); drawCrop(); $('crop-information').textContent = `Manually rotated ${state.rotation}°`; });
$('raw-crop-button').addEventListener('click', () => {
  const showRaw = $('raw-crop-image').hidden;
  $('raw-crop-image').hidden = !showRaw;
  $('raw-crop-button').setAttribute('aria-pressed', String(showRaw));
  $('raw-crop-button').textContent = showRaw ? 'Show upright' : 'Show raw crop';
});
$('layout-select').addEventListener('change', () => { drawRegions(); drawRegionPreview(); });
$('ocr-button').addEventListener('click', readCardText);
$('text-search-button').addEventListener('click', searchText);
for (const region of ['title', 'footer', 'rules']) $(`${region}-text`).addEventListener('input', updateTextSearchButton);
$('camera-button').addEventListener('click', openCamera);
$('close-camera').addEventListener('click', closeCamera);
$('camera-dialog').addEventListener('close', closeCamera);
$('take-picture').addEventListener('click', takePicture);
window.addEventListener('pagehide', () => { closeCamera(); engineWorker?.terminate(); });

try {
  const response = await fetch('./assets/samples.json');
  if (!response.ok) throw new Error(`Sample manifest could not be loaded (${response.status}).`);
  const manifest = await response.json();
  state.samples = manifest.samples;
  state.references = manifest.references;
  $('catalog-size').textContent = `${manifest.catalog_count} reference faces`;
  $('sample-select').replaceChildren(...state.samples.map((sample) => {
    const option = element('option', '', sample.title);
    option.value = sample.id;
    return option;
  }));
  if (!state.samples.length) throw new Error('The public sample catalog is empty.');
  $('sample-select').disabled = false;
  await selectSample(state.samples[0].id);
} catch (error) {
  setStatus(`The demo could not start: ${error.message}`, true);
  $('source-placeholder').textContent = 'Upload a picture to try the local pipeline.';
}
