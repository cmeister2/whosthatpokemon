let engine;

async function getEngine() {
  if (!engine) {
    engine = (async () => {
      const { default: init, BrowserEngine } = await import('./engine/whosthatpokemon.js');
      const [, response] = await Promise.all([init(), fetch('./assets/catalog.index.json')]);
      if (!response.ok) throw new Error(`Catalog could not be loaded (${response.status}).`);
      return new BrowserEngine(await response.text());
    })();
    engine.catch(() => { engine = undefined; });
  }
  return engine;
}

self.addEventListener('message', async ({ data }) => {
  const { id, action, bytes, cropped, observations } = data;
  try {
    const matcher = await getEngine();
    let result;
    if (action === 'scan') result = JSON.parse(matcher.scan(new Uint8Array(bytes), !!cropped));
    else if (action === 'text') result = JSON.parse(matcher.text_search(JSON.stringify(observations)));
    else throw new Error('Unknown engine action.');
    self.postMessage({ id, result });
  } catch (error) {
    self.postMessage({ id, error: String(error?.message || error) });
  }
});
