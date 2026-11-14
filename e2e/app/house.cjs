// "Scan de maison" in worker mode: the scan builds up, can be paused,
// finished and reset; overview ("maquette") and first-person navigation;
// back to the stage.
const { check, newPage, consoleErrors, glWarnings, state, eye, waitState, open, shot, viewShot, diff, content, setSlider, sleep } = require('./lib.cjs');
const { thumb } = require('./stage.cjs');

const dist = (a, b) => Math.hypot(a[0] - b[0], a[1] - b[1], a[2] - b[2]);

async function run({ browser, url }) {
  const { ctx, page, logs } = await newPage(browser);
  await open(page, url);
  const measurements = {};

  // ---- switch to the house: the scan starts by itself
  await page.click('#mode-house');
  let s = await waitState(page, (s) => s.scene === 'house' && s.scan && s.scan.state === 'scanning' && s.layers.static > 0, 60000);
  check('house: the mode switch starts the scan', s.scene === 'house' && s.scan.state === 'scanning' && s.layers.dynamic === 0, JSON.stringify(s.scan));
  check('house: the mode switch shows the active mode', (await page.locator('#mode-house').getAttribute('aria-selected')) === 'true' && (await page.locator('#mode-stage').getAttribute('aria-selected')) === 'false');
  s = await waitState(page, (s) => s.display.dollhouse);
  check('house: a running scan is shown from above, ceiling clipped', s.nav === 'orbit' && s.eye[1] > 5 && Math.abs(s.display.clipY - 2.35) < 1e-3, `eye y ${s.eye[1].toFixed(2)}, clip ${s.display.clipY}`);
  check('house: the house starts with its own splat size', Math.abs(s.display.splatScale - 1.2) < 1e-6 && (await page.locator('output[for=splat-scale]').innerText()) === '× 1,20', await page.locator('output[for=splat-scale]').innerText());

  const n0 = s.layers.static;
  const p0 = s.scan.progress;
  s = await waitState(page, (s, n) => s.layers.static > n + 5000 && s.scan.progress > 0.08, 120000, n0);
  check('house: the model builds up progressively', s.layers.static > n0 && s.scan.progress > p0, `${n0} -> ${s.layers.static} splats, ${(p0 * 100).toFixed(1)} % -> ${(s.scan.progress * 100).toFixed(1)} %`);
  const pill = await page.locator('#scan-pill').innerText();
  const label = await page.locator('#scan-state').innerText();
  check('house: progress is shown in the panel and over the view', /Scan en cours/.test(pill) && /%/.test(pill) && label === 'Scan en cours' && /^\d+ %$/.test(await page.locator('#scan-progress').innerText()), `${pill.replace(/\s+/g, ' ')} / ${label}`);
  const bar = await page.evaluate(() => document.querySelector('.progress-fill').getBoundingClientRect().width / document.querySelector('.progress').getBoundingClientRect().width);
  check('house: the progress bar follows', Math.abs(bar - s.scan.progress) < 0.08, `bar ${(bar * 100).toFixed(0)} %, scan ${(s.scan.progress * 100).toFixed(0)} %`);

  // The handheld camera: one larger tile, live.
  await waitState(page, (s) => s.previews.ids.length === 1 && s.previews.painted['0'] > 2, 20000);
  const tile = await page.evaluate(() => document.querySelector('#tiles .tile').getBoundingClientRect().width);
  const t1 = await thumb(page, 0);
  await waitState(page, (s, n) => s.previews.painted['0'] > n + 3, 20000, (await state(page)).previews.painted['0']);
  const t2 = await thumb(page, 0);
  check('house: one large live thumbnail for the handheld camera', (await page.locator('#tiles canvas').count()) === 1 && tile > 200 && t1.colors > 8 && t1.hash !== t2.hash, `tile ${tile.toFixed(0)} px wide, ${t1.width}x${t1.height}`);
  await shot(page, 'house-mid-scan.png');

  // ---- pause and resume the scan
  await page.click('#scan-pause');
  s = await waitState(page, (s) => s.scan.state === 'paused');
  const paused = s.layers.static;
  await sleep(700);
  s = await state(page);
  check('house: Pause stops the scan and keeps the map', s.scan.state === 'paused' && s.layers.static === paused && paused > 0 && (await page.locator('#scan-start').innerText()).trim() === 'Reprendre', `${paused} splats`);
  await page.click('#scan-start');
  await waitState(page, (s, n) => s.scan.state === 'scanning' && s.layers.static > n, 30000, paused);
  check('house: Reprendre continues from where it stopped', true);

  // ---- finish now
  const start = Date.now();
  const from = (await state(page)).scan.progress;
  await page.click('#scan-finish');
  s = await waitState(page, (s) => s.scan.state === 'done', 600000);
  measurements.finish_now_s = +((Date.now() - start) / 1000).toFixed(1);
  measurements.finish_now_from_progress = +from.toFixed(2);
  measurements.house_splats = s.layers.static;
  check('house: "Terminer maintenant" brings the scan to 100 %', s.scan.progress === 1 && (await page.locator('#scan-progress').innerText()) === '100 %' && s.layers.static > 100000,
    `${s.layers.static} splats, ${measurements.finish_now_s} s from ${(from * 100).toFixed(0)} %`);
  check('house: Stats and viewer agree on the final map', s.stats.splats_static === s.layers.static, `${s.stats.splats_static} / ${s.layers.static}`);
  await sleep(500);
  const plan = await viewShot(page, 'house-dollhouse-view.png');
  const cp = await content(page, plan);
  await shot(page, 'house-dollhouse.png');
  s = await state(page);
  check('house: the finished scan is still seen as a plan from above', s.display.dollhouse && s.eye[1] > 5 && cp.colors > 150 && cp.bright > 0.12, `colors ${cp.colors}, bright ${cp.bright.toFixed(3)}`);

  // ---- visit: first person
  await page.click('#visit-house');
  s = await waitState(page, (s) => !s.display.dollhouse && s.nav === 'fly');
  check('house: "Visiter la maison" lands at the start pose in fly mode', dist(s.eye, [-0.5, 1.6, -0.45]) < 0.05 && s.display.clipY === null, s.eye.map((x) => x.toFixed(2)).join(', '));
  await sleep(400);
  const inside = await viewShot(page, 'house-interior-view.png');
  const ci = await content(page, inside);
  await shot(page, 'house-interior.png');
  check('house: the interior fills the view', ci.colors > 300 && ci.bright > 0.8, `colors ${ci.colors}, bright ${ci.bright.toFixed(3)}`);

  // Hold the key until the camera has covered some ground: how long that
  // takes depends on the frame rate of the software rasterizer.
  const e0 = await eye(page);
  await page.keyboard.down('w');
  await page.waitForFunction((from) => { const e = window.__splatLive.viewerEye(); return Math.hypot(e[0] - from[0], e[1] - from[1], e[2] - from[2]) > 0.6; }, e0, { timeout: 20000 }).catch(() => {});
  await page.keyboard.up('w');
  await sleep(700);
  const e1 = await eye(page);
  check('house: fly navigation moves the eye', dist(e0, e1) > 0.6 && Math.abs(e1[1] - e0[1]) < 0.6, `moved ${dist(e0, e1).toFixed(2)} m`);
  const tgt0 = (await state(page)).target;
  await page.mouse.move(420, 420);
  await page.mouse.down();
  await page.mouse.move(540, 430, { steps: 6 });
  await page.mouse.up();
  await sleep(500);
  s = await state(page);
  check('house: dragging looks around without moving', dist(s.target, tgt0) > 0.1 && dist(s.eye, e1) < 0.3, `target moved ${dist(s.target, tgt0).toFixed(2)} m`);

  // Fly keys must not act while a control of the panel has the focus.
  await page.focus('#splat-scale');
  const e2 = await eye(page);
  await page.keyboard.down('w');
  await sleep(1200);
  await page.keyboard.up('w');
  await sleep(500);
  const e3 = await eye(page);
  check('house: keys typed in a panel input do not fly', dist(e2, e3) < 1e-3, `moved ${dist(e2, e3).toFixed(4)} m`);
  await page.evaluate(() => document.activeElement.blur());

  // ---- "Vue maquette" by hand, and back
  await page.click('#toggle-dollhouse');
  s = await waitState(page, (s) => s.display.dollhouse);
  await sleep(300);
  const plan2 = await viewShot(page, 'house-dollhouse-again.png');
  const d = await diff(page, plan, plan2);
  check('house: "Vue maquette" shows the plan from above', s.nav === 'orbit' && s.eye[1] > 5 && Math.abs(s.display.clipY - 2.35) < 1e-3 && d.changed < 0.02, `eye y ${s.eye[1].toFixed(2)}, differs from the first plan by ${(d.changed * 100).toFixed(2)} %`);
  await page.click('#toggle-dollhouse');
  s = await waitState(page, (s) => !s.display.dollhouse);
  check('house: leaving the plan returns to where one stood', s.nav === 'fly' && dist(s.eye, e3) < 0.05 && s.display.clipY === null);

  // ---- reset, walking speed
  await page.click('#scan-reset');
  s = await waitState(page, (s) => s.scan.state === 'idle' && s.layers.static === 0);
  check('house: "Recommencer" clears the map', s.stats.splats_static === 0 && (await page.locator('#scan-state').innerText()) === 'Prêt à scanner');
  await setSlider(page, '#walk-speed', 2);
  s = await waitState(page, (s) => Math.abs(s.config.scanSpeed - 2) < 1e-6);
  check('house: the walking speed reaches the pipeline', (await page.locator('output[for=walk-speed]').innerText()) === '2,0 m/s', await page.locator('output[for=walk-speed]').innerText());
  await page.click('#scan-start');
  s = await waitState(page, (s) => s.scan.state === 'scanning' && s.layers.static > 0 && s.display.dollhouse, 30000);
  check('house: "Démarrer" scans again, seen from above', true);

  // ---- back to the stage
  await page.click('#mode-stage');
  s = await waitState(page, (s) => s.scene === 'stage' && s.layers.dynamic > 0 && s.layers.static > 0 && s.stats && s.stats.splats_dynamic > 0, 60000);
  await sleep(500);
  const back = await viewShot(page, 'stage-after-house.png');
  const cb = await content(page, back);
  check('house: switching back to the stage works', s.nav === 'orbit' && !s.display.dollhouse && s.display.clipY === null && s.display.splatScale === 1 && s.scan === null && s.previews.ids.length === 6 && cb.colors > 60 && (await page.locator('#toggle-dollhouse').count()) === 0,
    `${JSON.stringify(s.layers)}, colors ${cb.colors}`);

  s = await state(page);
  check('house: no application error', s.errors.length === 0, s.errors.join(' | '));
  check('house: no console error', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
  check('house: no WebGL warning', glWarnings(logs).length === 0, glWarnings(logs).join(' | '));
  await ctx.close();
  return measurements;
}

module.exports = { run };
if (require.main === module) require('./standalone.cjs')(run);
