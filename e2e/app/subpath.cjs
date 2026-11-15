// Static hosting under a sub-path: every URL of the build is relative, so
// the same dist works at https://example.org/demos/splat-live/ in worker mode.
const { chromium } = require('playwright');
const { check, DIST, PORT, serve, newPage, consoleErrors, waitState, open, shot } = require('./lib.cjs');

async function run({ browser }) {
  const prefix = '/demos/splat-live/';
  const { srv, url, requests } = await serve({ dir: DIST, prefix, port: PORT + 30 });
  try {
    const { ctx, page, logs } = await newPage(browser, { width: 900, height: 600 });
    let s = await open(page, url);
    check('subpath: the application runs under a sub-path, in worker mode', s.source === 'worker' && s.connected && s.layers.dynamic > 0, url);
    const outside = requests.filter((r) => !r.startsWith(prefix));
    check('subpath: only the /api/info probe leaves the sub-path', outside.length === 1 && outside[0] === '/api/info', outside.join(', '));
    const worker = requests.filter((r) => /worker/.test(r));
    check('subpath: the worker files load from the sub-path', ['worker-boot.js', 'splat-worker.js', 'splat-worker_bg.wasm'].every((f) => worker.includes(prefix + f)), worker.join(', '));
    // The forced source skips the probe altogether.
    requests.length = 0;
    s = await open(page, `${url}?source=worker&scene=house`, { scene: 'house' });
    check('subpath: ?source=worker skips the server probe, ?scene=house starts in the house', s.source === 'worker' && s.scene === 'house' && !requests.includes('/api/info'), requests.filter((r) => !r.startsWith(prefix)).join(', '));
    check('subpath: no console error', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));

    // ?source=server on a host without a server: no silent fallback.
    const forced = await newPage(browser, { width: 900, height: 600 });
    await forced.page.goto(`${url}?source=server`);
    s = await waitState(forced.page, (s) => s.link === 'failed' && s.notice, 20000);
    const badge = await forced.page.locator('#source-badge').getAttribute('data-link');
    check('subpath: ?source=server without a server says so instead of falling back', s.source === null && badge === 'failed' && /injoignable/.test(s.notice.text), s.notice.text);
    await forced.ctx.close();
    await ctx.close();

    // A browser without WebGL: a clear message instead of a black page.
    const bare = await chromium.launch({ args: ['--disable-3d-apis'] });
    try {
      const plain = await newPage(bare, { width: 900, height: 600 });
      await plain.page.goto(url);
      await plain.page.waitForSelector('#fatal', { timeout: 30000 });
      const text = await plain.page.locator('#fatal').innerText();
      const panics = plain.logs.filter((l) => /panicked|unreachable|RuntimeError/.test(l));
      check('subpath: without WebGL2 the page says why it cannot start', /WebGL2/.test(text) && /ne peut pas démarrer/.test(text) && panics.length === 0, text.replace(/\s+/g, ' '));
      await shot(plain.page, 'no-webgl.png');
    } finally {
      await bare.close();
    }
  } finally {
    srv.close();
  }
}

module.exports = { run };
if (require.main === module) require('./standalone.cjs')(run);
