// Several clients of one gs-server: a slow page must not fall behind the
// pipeline, a late joiner arrives in the middle of a house scan, controls
// follow the echoed State whoever changed it, the server is replaced under
// a running scan, and the WebSocket falls back to the main thread when the
// relay worker cannot start. Skipped when the binary is not built.
const { chromium } = require('playwright');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { check, DIST, PORT, newPage, consoleErrors, glWarnings, state, waitState, open, openPanel, shot, setSlider, sleep, startServer, SERVER_BIN } = require('./lib.cjs');

// What the "Capture" controls show, read from the document.
const controls = (page) =>
  page.evaluate(() => {
    const q = (s) => document.querySelector(s);
    const checked = (s) => q(s) && q(s).getAttribute('aria-checked') === 'true';
    return {
      cameras: q('#cam-count') ? Number(q('#cam-count').value) : null,
      camerasText: q('output[for=cam-count]') ? q('output[for=cam-count]').textContent : null,
      resolution: q('#res-text') ? q('#res-text').textContent : null,
      resolutionIndex: [0, 1, 2, 3].findIndex((i) => checked(`#res-${i}`)),
      backgroundSubtraction: checked('#toggle-bg'),
      noise: checked('#toggle-noise'),
      shadows: checked('#toggle-shadows'),
      paused: q('#btn-pause').getAttribute('aria-pressed') === 'true',
      tiles: document.querySelectorAll('#tiles canvas').length,
    };
  });

// True when the controls of `page` show exactly the echoed configuration.
async function controlsMatch(page) {
  const [c, s] = [await controls(page), await state(page)];
  const k = s.config;
  const ok = c.cameras === k.cameras && c.camerasText === String(k.cameras) && c.resolution.startsWith(`${k.width} × ${k.height}`) && c.backgroundSubtraction === k.backgroundSubtraction && c.noise === k.noise && c.shadows === k.shadows && c.paused === k.paused && c.tiles === k.cameras;
  return { ok, detail: `controls ${JSON.stringify(c)} / config ${JSON.stringify(k)}` };
}

const freeze = (page, ms) =>
  page.evaluate((ms) => {
    const end = performance.now() + ms;
    while (performance.now() < end);
  }, ms);

async function run({ browser }) {
  if (!fs.existsSync(SERVER_BIN)) {
    console.log(`SKIPPED: ${SERVER_BIN} does not exist (cargo build -p gs-server --release)`);
    return { skipped: true };
  }
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'splat-live-shared-'));
  const dist = path.join(tmp, 'dist');
  fs.cpSync(DIST, dist, { recursive: true });
  const port = PORT + 70;
  let server = await startServer({ dist, certs: path.join(tmp, 'certs'), port });
  // The reference client runs in a browser of its own, with a tiny view:
  // whatever slows the page under test does not slow this one.
  const referenceBrowser = await chromium.launch();
  const measurements = {};
  try {
    const url = `${server.url}/`;
    const { ctx, page, logs } = await newPage(browser);
    let s = await open(page, url);
    check('shared: the WebSocket of a server page lives in the relay worker', s.source === 'server' && s.channel === 'relay' && page.workers().length === 1, `channel ${s.channel}, ${page.workers().length} worker(s)`);
    const reference = await newPage(referenceBrowser, { width: 320, height: 240 });
    await open(reference.page, url);
    await openPanel(reference.page);

    // ---- latency: how many ticks the page is behind the reference client
    const lag = async () => {
      const [a, b] = await Promise.all([state(page), state(reference.page)]);
      return b.stats.tick - a.stats.tick;
    };
    const lagOver = async (ms) => {
      const samples = [];
      for (const end = Date.now() + ms; Date.now() < end; await sleep(150)) samples.push(await lag());
      return samples;
    };
    await sleep(1500);
    const steady = await lagOver(2000);
    measurements.steady_lag_ticks = Math.max(...steady);
    // 20 ticks per second: 10 ticks are half a second.
    check('shared: a page that renders slowly still shows the present', Math.max(...steady) <= 10, `behind the reference by ${steady.join(' ')} ticks`);
    await freeze(page, 3000);
    const rightAfter = await lag();
    await sleep(700);
    const recovered = await lagOver(1500);
    measurements.lag_after_freeze_ticks = { right_after: rightAfter, later: Math.max(...recovered) };
    check('shared: after a 3 s freeze the page catches up at once instead of replaying the backlog', Math.max(...recovered) <= 10, `${rightAfter} ticks behind right after, then ${recovered.join(' ')}`);
    const cdp = await ctx.newCDPSession(page);
    await cdp.send('Emulation.setCPUThrottlingRate', { rate: 8 });
    await sleep(4000);
    const throttled = await lagOver(3000);
    await cdp.send('Emulation.setCPUThrottlingRate', { rate: 1 });
    measurements.throttled_lag_ticks = Math.max(...throttled);
    check('shared: a main thread throttled 8 times does not fall behind either', Math.max(...throttled) <= 14, `behind by ${throttled.join(' ')} ticks`);
    // The rate shown is counted from the ticks: both clients agree on it,
    // however differently they are loaded.
    await sleep(2500);
    const [mine, theirs] = [await state(page), await state(reference.page)];
    const hz = await page.locator('#stat-hz').innerText();
    check('shared: the pipeline rate shown is the measured tick rate', mine.pipelineHz > 5 && Math.abs(mine.pipelineHz - theirs.pipelineHz) < 3 && /^\d+(,\d)? Hz$/.test(hz), `${hz}: ${mine.pipelineHz.toFixed(1)} here, ${theirs.pipelineHz.toFixed(1)} for the reference client, ${mine.stats.tick_hz.toFixed(1)} says the pipeline`);

    // ---- controls follow the echoed State, whoever changes them
    for (const [who, action] of [
      [reference.page, (p) => setSlider(p, '#cam-count', 9)],
      [page, (p) => p.click('#res-0')],
      [reference.page, (p) => p.click('#toggle-noise')],
      [page, (p) => p.click('#toggle-shadows')],
      [reference.page, (p) => p.click('#toggle-bg')],
      [page, (p) => setSlider(p, '#cam-count', 4)],
      [reference.page, (p) => p.keyboard.press('p')],
    ]) {
      await action(who);
      await sleep(120);
    }
    // A burst of slider events, as a drag sends them, from both sides at once.
    for (let n = 5; n <= 12; n++) {
      await setSlider(page, '#cam-count', n);
      await setSlider(reference.page, '#cam-count', 17 - n);
    }
    // Longer than the time an optimistic value may be shown.
    await sleep(6500);
    const [a, b] = [await controlsMatch(page), await controlsMatch(reference.page)];
    const [sa, sb] = [await state(page), await state(reference.page)];
    check('shared: after concurrent changes both clients show the same echoed configuration', a.ok && b.ok && JSON.stringify(sa.config) === JSON.stringify(sb.config) && sa.config.paused && sa.config.noise && !sa.config.shadows && !sa.config.backgroundSubtraction && sa.config.width === 96, `${a.detail} || ${b.detail}`);
    await page.keyboard.press('p');
    await page.click('#toggle-bg');
    await page.click('#toggle-shadows');
    await page.click('#toggle-noise');
    await waitState(reference.page, (s) => !s.config.paused && s.config.backgroundSubtraction && s.config.shadows && !s.config.noise, 20000);

    // ---- a late joiner in the middle of a house scan
    await page.click('#mode-house');
    s = await waitState(page, (s) => s.scene === 'house' && s.scan && s.scan.state === 'scanning' && s.scan.progress > 0.12, 120000);
    await waitState(reference.page, (s) => s.scene === 'house' && s.layers.static > 0, 30000);
    const late = await newPage(referenceBrowser, { width: 900, height: 600 });
    const joined = Date.now();
    let l = await open(late.page, url, { scene: 'house' });
    measurements.late_join_ms = Date.now() - joined;
    s = await state(page);
    check('shared: a late joiner gets the house scanned so far and the scan in progress', l.scan && l.scan.state === 'scanning' && l.scan.progress > 0.12 && l.layers.static > 0.8 * s.layers.static && l.display.dollhouse && l.nav === 'orbit',
      `joined at ${(l.scan.progress * 100).toFixed(0)} % with ${l.layers.static} splats (first client: ${s.layers.static}) in ${measurements.late_join_ms} ms`);
    await sleep(600);
    await shot(late.page, 'shared-late-joiner.png');
    check('shared: the late joiner sees the scan controls in the same state', (await late.page.locator('#scan-state').innerText()) === 'Scan en cours' && (await late.page.locator('#scan-pause').isEnabled()) && (await late.page.locator('#scan-pill').isVisible()));
    // The newcomer pauses the scan: everyone sees it, with the same map.
    await late.page.click('#scan-pause');
    await waitState(page, (s) => s.scan.state === 'paused', 20000);
    await sleep(1200);
    [s, l] = [await state(page), await state(late.page)];
    check('shared: a scan paused by one client is paused for all, with identical maps', l.scan.state === 'paused' && s.scan.progress === l.scan.progress && s.layers.static === l.layers.static && s.layers.static === s.stats.splats_static, `${s.layers.static} / ${l.layers.static} splats at ${(s.scan.progress * 100).toFixed(1)} %`);
    await page.click('#scan-start');
    await waitState(late.page, (s) => s.scan.state === 'scanning', 20000);
    await late.page.click('#scan-finish');
    s = await waitState(page, (s) => s.scan.state === 'done', 600000);
    l = await waitState(late.page, (s) => s.scan.state === 'done', 60000);
    await sleep(800);
    [s, l] = [await state(page), await state(late.page)];
    check('shared: early and late clients end with the same finished house', s.layers.static === l.layers.static && s.layers.static === s.stats.splats_static && s.layers.static > 100000, `${s.layers.static} / ${l.layers.static} splats`);
    await late.ctx.close();

    // ---- the server is replaced while a scan runs
    await page.click('#scan-start');
    s = await waitState(page, (s) => s.scan.state === 'scanning' && s.scan.progress > 0.05, 60000);
    const statesBefore = s.counts.states;
    await server.stop();
    await waitState(page, (s) => s.link === 'reconnecting', 20000);
    check('shared: without the server the pause button is disabled and the badge says so', (await page.locator('#btn-pause').isDisabled()) && /Reconnexion|Serveur natif/.test(await page.locator('#source-badge').getAttribute('title')));
    // Controls used while the link is down are sent once it is back.
    await page.click('#mode-house');
    server = await startServer({ dist, certs: path.join(tmp, 'certs'), port, args: ['--light', '--scene', 'stage'] });
    // Two States: the new server's own (the stage), then the echo of the queued mode switch.
    s = await waitState(page, (s, n) => s.connected && s.counts.states >= n + 2 && s.scene === 'house' && s.scan && s.layers.static > 0 && s.stats.tick > 2, 60000, statesBefore);
    const pill = await page.locator('#scan-pill').innerText();
    check('shared: after a server restart the page shows the new pipeline, not the old one', s.channel === 'relay' && s.scan.progress < 0.5 && s.layers.static === s.stats.splats_static && /Scan en cours/.test(pill) && (await page.locator('#mode-house').getAttribute('aria-selected')) === 'true',
      `progress ${(s.scan.progress * 100).toFixed(0)} %, ${s.layers.static} splats, pill "${pill.replace(/\s+/g, ' ')}"`);
    await page.click('#mode-stage');
    await waitState(page, (s) => s.scene === 'stage' && s.layers.dynamic > 0, 60000);

    s = await state(page);
    check('shared: no application error', s.errors.length === 0, s.errors.join(' | '));
    const errors = consoleErrors(logs).filter((l) => !/WebSocket connection to|ERR_CONNECTION_REFUSED|net::ERR_/.test(l));
    check('shared: no console error', errors.length === 0, errors.join(' | '));
    check('shared: no WebGL warning', glWarnings(logs).length === 0, glWarnings(logs).join(' | '));
    await ctx.close();

    // ---- the relay worker cannot start: the socket moves to the main thread
    for (const [what, block] of [
      ['its script is missing', (route) => route.fulfill({ status: 404, body: 'not found' })],
      ['its WebAssembly module is broken', (route) => (/_bg\.wasm/.test(route.request().url()) ? route.fulfill({ status: 200, contentType: 'application/wasm', body: 'not wasm' }) : route.continue())],
      // A host that lets the worker run but not open sockets.
      ['it may not open a WebSocket', async (route) => {
        if (!/splat-worker\.js/.test(route.request().url())) return route.continue();
        const response = await route.fetch();
        return route.fulfill({ response, body: `self.WebSocket = class { constructor() { throw new DOMException('blocked', 'SecurityError'); } };\n${await response.text()}` });
      }],
    ]) {
      const fallback = await newPage(browser, { width: 900, height: 600 });
      await fallback.page.route(/worker-boot\.js|splat-worker/, block);
      s = await open(fallback.page, url);
      const tick = s.stats.tick;
      s = await waitState(fallback.page, (s, tick) => s.stats.tick > tick + 5, 20000, tick);
      await fallback.page.keyboard.press('p');
      s = await waitState(fallback.page, (s) => s.config.paused, 20000);
      await fallback.page.keyboard.press('p');
      await waitState(fallback.page, (s) => !s.config.paused, 20000);
      check(`shared: when the relay worker cannot start (${what}) the page connects by itself`, s.source === 'server' && s.channel === 'direct' && s.layers.dynamic > 0 && s.errors.length === 0, `channel ${s.channel}`);
      await fallback.ctx.close();
    }
  } finally {
    await referenceBrowser.close();
    await server.stop();
    fs.rmSync(tmp, { recursive: true, force: true });
  }
  return measurements;
}

module.exports = { run };
if (require.main === module) require('./standalone.cjs')(run);
