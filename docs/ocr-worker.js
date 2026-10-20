// Own the OCR runtime so a failed model load can be stopped and retried cleanly.
let runtime;

function getRuntime() {
  if (!runtime) {
    runtime = (async () => {
      const { default: { createWorker } } = await import('./ocr/tesseract.esm.min.js');
      let fail;
      let failed = false;
      const failure = new Promise((_, reject) => { fail = reject; });
      const creating = createWorker('eng', 1, {
        workerPath: new URL('./ocr/worker.min.js', import.meta.url).href,
        corePath: new URL('./ocr/core/', import.meta.url).href,
        langPath: new URL('./ocr/lang', import.meta.url).href,
        workerBlobURL: false,
        // Tesseract can leave its creation promise pending on loadLanguage failure.
        errorHandler: (message) => { failed = true; fail(new Error(String(message))); },
      });
      creating.then((worker) => { if (failed) worker.terminate(); }, () => {});
      const worker = await Promise.race([creating, failure]);
      await worker.setParameters({ tessedit_pageseg_mode: '7', preserve_interword_spaces: '1' });
      return worker;
    })();
  }
  return runtime;
}

self.addEventListener('message', async ({ data }) => {
  try {
    const worker = await getRuntime();
    if (data.action === 'initialize') self.postMessage({ id: data.id, result: true });
    else if (data.action === 'recognize') {
      const { data: reading } = await worker.recognize(new Uint8Array(data.bytes));
      self.postMessage({ id: data.id, result: { text: reading.text, confidence: reading.confidence } });
    } else throw new Error('Unknown OCR operation.');
  } catch (error) {
    self.postMessage({ id: data.id, error: String(error?.message || error) });
  }
});
