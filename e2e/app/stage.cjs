// "Scène en direct" with the pipeline in the Web Worker: the page a static
// host serves. Rendering, animation, thumbnails, pause, configuration,
// display settings, orbit navigation, keyboard shortcuts.
const { check, newPage, consoleErrors, glWarnings, state, eye, waitState, open, shot, viewShot, diff, content, setSlider, sleep } = require('./lib.cjs');

// Pixels of a thumbnail canvas: size, colour variety and a hash.
const thumb = (page, id) =>
  page.evaluate((id) => {
    const c = document.getElementById(`preview-${id}`);
    if (!c) return null;
    const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
    const colors = new Set();
    let hash = 2166136261;
    for (let i = 0; i < d.length; i += 4) {
      colors.add(((d[i] >> 3) << 10) | ((d[i + 1] >> 3) << 5) | (d[i + 2] >> 3));
      hash = Math.imul(hash ^ d[i], 16777619);
      hash = Math.imul(hash ^ d[i + 1], 16777619);
      hash = Math.imul(hash ^ d[i + 2], 16777619);
    }
    const box = c.getBoundingClientRect();
    return { width: c.width, height: c.height, colors: colors.size, hash, shown: box.width > 20 && box.height > 15 };
  }, id);

const dist = (a, b) => Math.hypot(a[0] - b[0], a[1] - b[1], a[2] - b[2]);

async function run({ browser, url }) {
  const { ctx, page, logs } = await newPage(browser);
  // No query parameter: the page probes /api/info, gets a 404 from the
  // static host and falls back to the worker by itself.
  let s = await open(page, url);
  check('stage: falls back to the local worker on a static host', s.source === 'worker' && s.connected, `source ${s.source}, link ${s.link}`);
  const badge = await page.locator('#source-badge').innerText();
  check('stage: the badge names the source', /Local \(WASM\)/.test(badge) && (await page.locator('#source-badge').getAttribute('data-link')) === 'connected', badge);
  check('stage: first State describes the light stage', s.scene === 'stage' && s.config.cameras === 6 && s.config.width === 120 && !s.config.paused, JSON.stringify(s.config));
  check('stage: both layers hold splats', s.layers.static > 1000 && s.layers.dynamic > 100, JSON.stringify(s.layers));
  check('stage: orbit navigation by default', s.nav === 'orbit' && !s.display.dollhouse);
  await shot(page, 'stage.png');

  // ---- picture and animation
  const a = await viewShot(page, 'stage-view-a.png');
  const ca = await content(page, a);
  check('stage: the canvas is not blank', ca.colors > 60 && ca.bright > 0.15, `colors ${ca.colors}, bright ${ca.bright.toFixed(3)}`);
  await sleep(700);
  const b = await viewShot(page, 'stage-view-b.png');
  let d = await diff(page, a, b);
  check('stage: the dancer animates (frames differ over time)', d.changed > 0.002, `changed ${(d.changed * 100).toFixed(2)} % of the pixels`);

  // ---- statistics
  const s2 = await waitState(page, (s, tick) => s.stats && s.stats.tick > tick + 5, 20000, s.stats.tick);
  check('stage: Stats keep arriving', s2.stats.tick > s.stats.tick && s2.stats.tick_hz > 1 && s2.stats.capture_ms > 0 && s2.stats.splats_dynamic > 0, `tick ${s.stats.tick} -> ${s2.stats.tick}, ${s2.stats.tick_hz.toFixed(1)} Hz`);
  await page.waitForFunction(() => /Hz/.test(document.getElementById('stat-hz').textContent) && /ms/.test(document.getElementById('stat-capture').textContent));
  // The rate on screen is counted from the ticks that ran. The pipeline's
  // own figure averages instantaneous rates, which the worker's bursts
  // (two ticks, then a wait for the page) can double.
  const rate0 = await waitState(page, (s) => s.pipelineHz !== null, 20000);
  const rateStart = Date.now();
  await sleep(4000);
  const rate1 = await state(page);
  const counted = (rate1.stats.tick - rate0.stats.tick) / ((Date.now() - rateStart) / 1000);
  const rateShown = await page.locator('#stat-hz').innerText();
  check('stage: the pipeline rate shown is the number of ticks per second', Math.abs(rate1.pipelineHz - counted) < 0.25 * counted + 1 && Math.abs(parseFloat(rateShown.replace(',', '.')) - rate1.pipelineHz) < 3, `${rateShown} shown, ${counted.toFixed(1)} ticks/s counted, pipeline says ${rate1.stats.tick_hz.toFixed(1)} Hz`);
  const shown = await page.evaluate(() => Object.fromEntries(['fps', 'hz', 'capture', 'recon', 'encode', 'static', 'dynamic', 'rate'].map((k) => [k, document.getElementById(`stat-${k}`).textContent])));
  check('stage: the statistics panel shows every figure', /i\/s/.test(shown.fps) && /Hz/.test(shown.hz) && /ms/.test(shown.recon) && /ms/.test(shown.encode) && /\d/.test(shown.static) && /\d/.test(shown.dynamic) && /o\/s/.test(shown.rate), JSON.stringify(shown));
  await page.waitForFunction(() => window.__splatLive.state().transferBytesPerSecond > 1000, null, { timeout: 10000 });
  check('stage: the transfer rate is measured', true, shown.rate);

  // ---- thumbnails
  await waitState(page, (s) => s.previews.ids.length === 6 && Object.keys(s.previews.painted).length === 6, 20000);
  const before = await Promise.all([0, 1, 2, 3, 4, 5].map((id) => thumb(page, id)));
  check('stage: one painted thumbnail per camera', before.every((t) => t && t.width >= 32 && t.colors > 8 && t.shown), JSON.stringify(before.map((t) => t && `${t.width}x${t.height}/${t.colors}`)));
  await waitState(page, (s, n) => Object.values(s.previews.painted).every((v) => v >= n), 20000, Math.max(...Object.values((await state(page)).previews.painted)) + 3);
  const after = await Promise.all([0, 1, 2, 3, 4, 5].map((id) => thumb(page, id)));
  const changed = after.filter((t, i) => t.hash !== before[i].hash).length;
  check('stage: the thumbnails are live', changed >= 4, `${changed}/6 changed`);
  const rgb = await thumb(page, 0);
  await page.click('#preview-mode-1');
  await page.waitForFunction(() => window.__splatLive.state().previews.depth);
  const depth = await thumb(page, 0);
  check('stage: RGB / depth toggle recolours the thumbnails', depth.hash !== rgb.hash && depth.colors > 8, `rgb ${rgb.colors} colours, depth ${depth.colors} colours`);
  await page.locator('#section-cameras').scrollIntoViewIfNeeded();
  await shot(page, 'stage-thumbnails-depth.png');
  await page.click('#preview-mode-0');

  // ---- pause and resume
  await page.click('#btn-pause');
  s = await waitState(page, (s) => s.config.paused);
  const p1 = await viewShot(page, 'stage-paused-a.png');
  const t1 = (await state(page)).stats.scene_time;
  await sleep(800);
  const p2 = await viewShot(page, 'stage-paused-b.png');
  const t2 = (await state(page)).stats.scene_time;
  d = await diff(page, p1, p2);
  check('stage: pause stops the animation', t1 === t2 && d.changed === 0, `scene time ${t1} -> ${t2}, changed ${(d.changed * 100).toFixed(3)} %`);
  check('stage: the pause button shows the state', (await page.locator('#btn-pause').getAttribute('aria-pressed')) === 'true');

  // ---- configuration (while paused, so that only the change alters the picture)
  await setSlider(page, '#cam-count', 3);
  s = await waitState(page, (s) => s.config.cameras === 3 && s.previews.ids.length === 3);
  const p3 = await viewShot(page, 'stage-3-cameras.png');
  d = await diff(page, p2, p3);
  check('stage: the camera count changes the State and the picture', s.config.cameras === 3 && d.changed > 0.002, `cameras ${s.config.cameras}, changed ${(d.changed * 100).toFixed(2)} %`);
  check('stage: the slider shows the echoed value', (await page.inputValue('#cam-count')) === '3' && (await page.locator('output[for=cam-count]').innerText()) === '3');
  check('stage: thumbnails follow the camera count', (await page.locator('#tiles canvas').count()) === 3);

  await page.click('#res-0');
  s = await waitState(page, (s) => s.config.width === 96 && s.config.height === 72);
  check('stage: resolution preset', (await page.locator('#res-0').getAttribute('aria-checked')) === 'true' && /96 × 72/.test(await page.locator('#res-text').innerText()), await page.locator('#res-text').innerText());

  const staticBefore = s.layers.static;
  await page.click('#toggle-bg');
  s = await waitState(page, (s) => !s.config.backgroundSubtraction && s.layers.static === 0);
  check('stage: without background subtraction everything is in the live layer', s.layers.static === 0 && s.layers.dynamic > staticBefore / 2, JSON.stringify(s.layers));
  await page.click('#toggle-bg');
  await waitState(page, (s) => s.config.backgroundSubtraction && s.layers.static > 0);
  await page.click('#toggle-noise');
  s = await waitState(page, (s) => s.config.noise);
  await page.click('#toggle-shadows');
  s = await waitState(page, (s) => !s.config.shadows);
  check('stage: noise and shadow switches reach the pipeline', s.config.noise && !s.config.shadows && (await page.locator('#toggle-noise').getAttribute('aria-checked')) === 'true' && (await page.locator('#toggle-shadows').getAttribute('aria-checked')) === 'false');
  await page.click('#toggle-noise');
  await page.click('#toggle-shadows');
  await waitState(page, (s) => !s.config.noise && s.config.shadows);

  // ---- resume, with the keyboard
  await page.keyboard.press('p');
  s = await waitState(page, (s) => !s.config.paused);
  const r1 = await viewShot(page, 'stage-resumed-a.png');
  await sleep(700);
  const r2 = await viewShot(page, 'stage-resumed-b.png');
  d = await diff(page, r1, r2);
  check('stage: P resumes the animation', d.changed > 0.002, `changed ${(d.changed * 100).toFixed(2)} %`);

  // ---- display settings
  await page.click('#btn-pause');
  await waitState(page, (s) => s.config.paused);
  s = await state(page);
  const base = await viewShot(page, 'stage-display-base.png');
  await page.click('#toggle-dynamic');
  let v = await waitState(page, (s, n) => !s.display.showDynamic && s.viewer.splatsDrawn === n, 10000, s.layers.static);
  check('stage: hiding the dynamic layer draws only the static one', v.viewer.splatsDrawn === s.layers.static, `${v.viewer.splatsDrawn} of ${s.layers.static + s.layers.dynamic}`);
  await page.click('#toggle-dynamic');
  await page.click('#toggle-static');
  v = await waitState(page, (s, n) => !s.display.showStatic && s.viewer.splatsDrawn === n, 10000, s.layers.dynamic);
  check('stage: hiding the static layer draws only the dynamic one', v.viewer.splatsDrawn === s.layers.dynamic);
  await page.click('#toggle-static');
  await page.click('#toggle-gizmos');
  await waitState(page, (s) => !s.display.gizmos);
  const noGizmos = await viewShot(page, 'stage-no-gizmos.png');
  d = await diff(page, base, noGizmos);
  check('stage: gizmos can be hidden', d.changed > 0.0005, `changed ${(d.changed * 100).toFixed(2)} %`);
  await page.click('#toggle-points');
  await setSlider(page, '#splat-scale', 0.5);
  v = await waitState(page, (s) => s.display.pointMode && Math.abs(s.display.splatScale - 0.5) < 1e-6);
  const points = await viewShot(page, 'stage-points.png');
  d = await diff(page, noGizmos, points);
  check('stage: point mode and splat scale change the rendering', d.changed > 0.01, `changed ${(d.changed * 100).toFixed(2)} %`);
  await page.click('#toggle-points');
  await setSlider(page, '#splat-scale', 1);
  await page.click('#toggle-gizmos');
  await waitState(page, (s) => !s.display.pointMode && s.display.splatScale === 1 && s.display.gizmos);

  // ---- orbit navigation
  const e0 = await eye(page);
  await page.mouse.move(400, 400);
  await page.mouse.down();
  await page.mouse.move(520, 380, { steps: 6 });
  await page.mouse.up();
  await sleep(600);
  const e1 = await eye(page);
  s = await state(page);
  check('stage: dragging orbits around the target', dist(e0, e1) > 0.3 && Math.abs(dist(e1, s.target) - dist(e0, s.target)) < 0.05, `eye moved ${dist(e0, e1).toFixed(2)} m`);
  await page.mouse.wheel(0, -400);
  await sleep(600);
  const e2 = await eye(page);
  check('stage: the wheel zooms', dist(e2, s.target) < dist(e1, s.target) - 0.2, `${dist(e1, s.target).toFixed(2)} m -> ${dist(e2, s.target).toFixed(2)} m`);
  await page.click('#tiles .tile >> nth=1');
  await sleep(300);
  const e3 = await eye(page);
  check('stage: a thumbnail click looks from that camera', dist(e3, e2) > 0.3 && e3[1] > 1.0, e3.map((x) => x.toFixed(2)).join(', '));

  // ---- flow control: a frozen main thread must not find a backlog
  await page.keyboard.press('p');
  await waitState(page, (s) => !s.config.paused);
  const f0 = await state(page);
  const frozen = await page.evaluate(() => {
    const before = window.__splatLive.state();
    const end = performance.now() + 2500;
    while (performance.now() < end);
    return new Promise((resolve) => {
      // One macrotask later: everything that queued up during the freeze has been delivered.
      setTimeout(() => {
        const after = window.__splatLive.state();
        resolve({ batches: after.counts.batches - before.counts.batches, ticks: after.stats.tick - before.stats.tick, time: after.stats.scene_time - before.stats.scene_time });
      }, 0);
    });
  });
  check('stage: the worker skips ticks while the page is frozen (at most 2 unacknowledged)', frozen.batches <= 3 && frozen.ticks <= 3, `2.5 s freeze at ${f0.config.targetHz} Hz: ${frozen.batches} batches waiting, ${frozen.ticks} ticks`);
  const f1 = await waitState(page, (s, tick) => s.stats.tick > tick + 10, 20000, f0.stats.tick + frozen.ticks);
  check('stage: the stream resumes after the freeze', f1.stats.tick_hz > 5, `${f1.stats.tick_hz.toFixed(1)} Hz`);
  await page.keyboard.press('p');
  await waitState(page, (s) => s.config.paused);

  // ---- H hides the interface
  await page.keyboard.press('h');
  await page.waitForFunction(() => window.__splatLive.state().uiHidden);
  const hidden = await page.evaluate(() => ['.topbar', '#panel', '#hud'].map((q) => getComputedStyle(document.querySelector(q)).display));
  await page.keyboard.press('h');
  await page.waitForFunction(() => !window.__splatLive.state().uiHidden);
  check('stage: H hides and shows the interface', hidden.every((x) => x === 'none'), hidden.join(','));
  await page.keyboard.press('p');
  await waitState(page, (s) => !s.config.paused);

  s = await state(page);
  check('stage: no application error', s.errors.length === 0, s.errors.join(' | '));
  check('stage: no console error', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
  check('stage: no WebGL warning', glWarnings(logs).length === 0, glWarnings(logs).join(' | '));
  await ctx.close();
}

module.exports = { run, thumb };
if (require.main === module) require('./standalone.cjs')(run);
