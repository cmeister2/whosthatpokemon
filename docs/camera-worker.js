// Runs the real ocracle + shared Rust pipeline away from the camera preview.
let initialization;
let engine;
let activeFrame = false;
const pageRoot = new URL('./', self.location.href);
const MAX_IMAGE_BYTES = 20 * 1024 * 1024;

function progress(id, message) {
  self.postMessage({ id, event: 'progress', message });
}

async function pinnedAsset(specification, label, id) {
  const { path, bytes, sha256 } = specification;
  if (typeof path !== 'string' || !Number.isSafeInteger(bytes) || bytes <= 0 || bytes > 128 * 1024 * 1024 || !/^[0-9a-f]{64}$/.test(sha256 || '')) {
    throw new Error(`${label} has invalid integrity metadata.`);
  }
  const url = new URL(path, pageRoot);
  if (url.origin !== pageRoot.origin) throw new Error(`${label} must be served by this site.`);
  const response = await fetch(url);
  if (!response.ok) throw new Error(`${label} could not load (${response.status}).`);
  let content;
  if (response.body?.getReader) {
    const reader = response.body.getReader();
    const buffer = new Uint8Array(bytes);
    let received = 0;
    let previousUpdate = 0;
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      if (received + value.byteLength > bytes) { await reader.cancel(); throw new Error(`${label} exceeded its expected byte count.`); }
      buffer.set(value, received);
      received += value.byteLength;
      if (id !== undefined && (performance.now() - previousUpdate >= 250 || received === bytes)) {
        progress(id, `${label}: ${(received / 1_000_000).toFixed(1)} / ${(bytes / 1_000_000).toFixed(1)} MB downloaded.`);
        previousUpdate = performance.now();
      }
    }
    if (received !== bytes) throw new Error(`${label} has an unexpected byte count.`);
    content = buffer.buffer;
  } else content = await response.arrayBuffer();
  if (content.byteLength !== bytes) throw new Error(`${label} has an unexpected byte count.`);
  const digest = await crypto.subtle.digest('SHA-256', content);
  const actual = Array.from(new Uint8Array(digest), (value) => value.toString(16).padStart(2, '0')).join('');
  if (actual !== sha256) throw new Error(`${label} failed its SHA-256 check.`);
  return content;
}

async function initialize(id) {
  if (!initialization) {
    initialization = (async () => {
      const started = performance.now();
      progress(id, 'Loading the browser engine description…');
      const response = await fetch(new URL('./assets/camera/manifest.json', pageRoot));
      if (!response.ok) throw new Error(`Camera assets are unavailable (${response.status}).`);
      const manifest = await response.json();
      if (manifest.schema_version !== 1 || !manifest.catalogue?.metadata || !manifest.model) throw new Error('The camera asset manifest is unsupported.');
      const model = manifest.model;
      progress(id, 'Loading and verifying the ocracle model and English card metadata…');
      const [{ default: init, RemoteBrowserEngine }, metadataBytes, modelBytes, configuration] = await Promise.all([
        import('./camera-engine/whosthatpokemon.js'),
        pinnedAsset(manifest.catalogue.metadata, 'English card metadata', id),
        pinnedAsset(model, 'OCR model', id),
        pinnedAsset({ path: model.config_path, bytes: model.config_bytes, sha256: model.config_sha256 }, 'OCR configuration', id),
      ]);
      progress(id, 'Preparing the full English lookup catalogue…');
      const metadata = manifest.catalogue.metadata;
      let decodedMetadata;
      if (metadata.compression === 'gzip') {
        if (typeof DecompressionStream !== 'function') throw new Error('This browser does not support gzip catalogue decompression.');
        const stream = new Blob([metadataBytes]).stream().pipeThrough(new DecompressionStream('gzip'));
        decodedMetadata = await new Response(stream).arrayBuffer();
      } else if (metadata.compression === undefined) decodedMetadata = metadataBytes;
      else throw new Error('The catalogue compression is unsupported.');
      if (decodedMetadata.byteLength > 512 * 1024 * 1024) throw new Error('The decompressed catalogue exceeds the supported size.');
      progress(id, 'Initializing ocracle and the Rust matcher…');
      await init();
      const decoder = new TextDecoder('utf-8', { fatal: true });
      const configJson = decoder.decode(configuration);
      engine = new RemoteBrowserEngine(decoder.decode(decodedMetadata), new Uint8Array(modelBytes), configJson);
      return {
        setup_ms: performance.now() - started,
        catalogue: manifest.catalogue,
        model: { name: JSON.parse(configJson).model_name, sha256: model.sha256, bytes: model.bytes },
      };
    })();
    initialization.catch(() => { initialization = undefined; });
  }
  return initialization;
}

class ImageTransferError extends Error {
  constructor(message, retryable = false) { super(message); this.retryable = retryable; }
}

async function downloadReference(url, transport) {
  for (let attempt = 0; attempt < 3; attempt += 1) {
    try {
      transport.requests += 1;
      const response = await fetch(url, { credentials: 'omit', referrerPolicy: 'no-referrer', redirect: 'error', signal: AbortSignal.timeout(20000) });
      if (!response.ok) throw new ImageTransferError(`A candidate image could not load (${response.status}).`, response.status === 429 || response.status >= 500);
      const reader = response.body?.getReader();
      if (!reader) throw new ImageTransferError('This browser does not support streaming reference images.');
      const chunks = [];
      let received = 0;
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        received += value.byteLength;
        transport.downloaded_bytes += value.byteLength;
        if (received > MAX_IMAGE_BYTES) { await reader.cancel(); throw new ImageTransferError('A candidate image exceeded its supported byte count.'); }
        chunks.push(value);
      }
      if (!received) throw new ImageTransferError('A candidate image is empty.');
      const bytes = new Uint8Array(received);
      let offset = 0;
      for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
      return bytes.buffer;
    } catch (error) {
      if (attempt === 2 || (error instanceof ImageTransferError && !error.retryable)) throw error;
      await new Promise((resolve) => setTimeout(resolve, 200 * (attempt + 1)));
    }
  }
}

async function referenceImage(request, transport = { requests: 0, downloaded_bytes: 0 }) {
  if (request.game !== 'mtg' || typeof request.id !== 'string' || !Number.isInteger(request.face_index) || request.face_index < 0) throw new Error('The engine requested an invalid card reference.');
  const url = new URL(request.image_url);
  if (url.protocol !== 'https:' || url.hostname !== 'cards.scryfall.io') throw new Error('A candidate image must come from the Scryfall image CDN.');
  if (request.image_sha256 != null && !/^[0-9a-f]{64}$/.test(request.image_sha256)) throw new Error('A candidate image has invalid integrity metadata.');
  const buffer = await downloadReference(url, transport);
  if (request.image_sha256 != null) {
    const digest = await crypto.subtle.digest('SHA-256', buffer);
    const actual = Array.from(new Uint8Array(digest), (value) => value.toString(16).padStart(2, '0')).join('');
    if (actual !== request.image_sha256) throw new Error('A candidate image failed its SHA-256 check.');
  }
  return { request, buffer };
}

async function recognizeFrame(id, width, height, rgba, cropped, classic) {
  const transport = { requests: 0, downloaded_bytes: 0, fetch_ms: 0, enrollment_ms: 0 };
  let next = JSON.parse(engine.begin_rgba(width, height, new Uint8Array(rgba), !!cropped, !!classic));
  let rounds = 0;
  while (next.state === 'needs_references') {
    if (!Array.isArray(next.requests) || !next.requests.length || next.requests.length > 256 || ++rounds > 32) throw new Error('The engine exceeded its bounded candidate-reference request budget.');
    const requests = [...new Map(next.requests.map((request) => [`${request.game}:${request.id}:${request.face_index}`, request])).values()];
    for (let offset = 0; offset < requests.length; offset += 4) {
      const batch = requests.slice(offset, offset + 4);
      progress(id, `Loading OCR-shortlisted card images: ${offset + 1}–${Math.min(offset + 4, requests.length)} of ${requests.length}…`);
      const started = performance.now();
      const references = await Promise.all(batch.map((request) => referenceImage(request, transport)));
      transport.fetch_ms += performance.now() - started;
      for (const { request, buffer } of references) {
        const enrolling = performance.now();
        engine.add_reference(request.game, request.id, request.face_index, new Uint8Array(buffer));
        transport.enrollment_ms += performance.now() - enrolling;
      }
    }
    progress(id, 'Verifying the shortlisted card pictures…');
    next = JSON.parse(engine.continue_frame());
  }
  if (next.state !== 'complete' || !next.report) throw new Error('The engine returned an incomplete frame result.');
  return { result: next.report, transport };
}

self.addEventListener('message', async ({ data }) => {
  const { id, action } = data;
  try {
    if (action === 'initialize') {
      self.postMessage({ id, result: await initialize(id) });
      return;
    }
    if (action !== 'frame') throw new Error('Unknown camera operation.');
    if (activeFrame) throw new Error('A frame is already being processed.');
    const { width, height, rgba, cropped = false, classic = false } = data;
    if (!Number.isInteger(width) || !Number.isInteger(height) || width < 1 || height < 1 || width > 1280 || height > 1280 || !(rgba instanceof ArrayBuffer) || rgba.byteLength !== width * height * 4) {
      throw new Error('The captured frame has invalid dimensions or pixels.');
    }
    activeFrame = true;
    try {
      const loaded = await initialize(id);
      const started = performance.now();
      const { result, transport } = await recognizeFrame(id, width, height, rgba, cropped, classic);
      for (const card of result.cards || []) for (const attempt of card.attempts || []) {
        for (const reading of [attempt.ocr?.reading, attempt.footer_ocr?.reading]) {
          if (reading && reading.model?.sha256 !== loaded.model.sha256) throw new Error('The OCR result used an unexpected model identity.');
        }
      }
      self.postMessage({ id, result, transport, worker_elapsed_ms: performance.now() - started });
    } finally {
      activeFrame = false;
    }
  } catch (error) {
    self.postMessage({ id, error: String(error?.message || error) });
  }
});
