let engine;

async function getEngine() {
  if (!engine) {
    engine = (async () => {
      const started = performance.now();
      const { default: init, BrowserEngine } = await import('./engine/whosthatpokemon.js');
      const [, response] = await Promise.all([init(), fetch('./assets/catalog.index.json')]);
      if (!response.ok) throw new Error(`The reference catalogue could not load (${response.status}).`);
      const matcher = new BrowserEngine(await response.text());
      return { matcher, setup_ms: performance.now() - started };
    })();
    engine.catch(() => { engine = undefined; });
  }
  return engine;
}

self.addEventListener('message', async ({ data }) => {
  const { id, action, bytes, cropped, title, proposals, visual, rotation = 0 } = data;
  let started;
  try {
    const loaded = await getEngine();
    const matcher = loaded.matcher;
    started = performance.now();
    let result;
    if (action === 'initialize') result = { setup_ms: loaded.setup_ms };
    else if (action === 'extract') result = JSON.parse(matcher.extract(new Uint8Array(bytes), !!cropped));
    else if (action === 'lookup') result = JSON.parse(matcher.lookup_title(title));
    else if (action === 'verify') result = JSON.parse(matcher.verify_candidates(new Uint8Array(bytes), JSON.stringify(proposals), rotation));
    else if (action === 'combine') result = JSON.parse(matcher.combine_candidates(title, JSON.stringify(proposals), JSON.stringify(visual)));
    else if (action === 'scan') result = JSON.parse(matcher.scan(new Uint8Array(bytes), !!cropped));
    else if (action === 'text') result = JSON.parse(matcher.text_search(JSON.stringify(data.observations)));
    else throw new Error('Unknown pipeline step.');
    self.postMessage({ id, result, elapsed_ms: performance.now() - started });
  } catch (error) {
    self.postMessage({ id, error: String(error?.message || error), elapsed_ms: started === undefined ? null : performance.now() - started });
  }
});
