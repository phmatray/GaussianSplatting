// Measurements in headless Chromium (software WebGL, so the viewer figures
// are a floor, not what a GPU does): build sizes, worker tick rate and
// viewer frame rate for both scenes, duration of a full house scan.
const fs = require('fs');
const path = require('path');
const { check, DIST, newPage, state, waitState, open, sleep } = require('./lib.cjs');

function sizes() {
  const out = {};
  for (const file of fs.readdirSync(DIST)) {
    const bytes = fs.statSync(path.join(DIST, file)).size;
    if (/^gs-app-.*_bg\.wasm$/.test(file)) out.app_wasm_bytes = bytes;
    else if (/^gs-app-.*\.js$/.test(file)) out.app_js_bytes = bytes;
    else if (file === 'splat-worker_bg.wasm') out.worker_wasm_bytes = bytes;
    else if (file === 'splat-worker.js') out.worker_js_bytes = bytes;
    else if (/\.css$/.test(file)) out.css_bytes = bytes;
  }
  out.total_bytes = fs.readdirSync(DIST).reduce((sum, f) => sum + fs.statSync(path.join(DIST, f)).size, 0);
  return out;
}

// Samples the pipeline and the viewer for `seconds`. `redraw_fps` counts the
// frames the viewer really redrew (it skips frames in which nothing changed).
async function sample(page, seconds) {
  const a = await state(page);
  const t0 = Date.now();
  const ticks = [];
  const fps = [];
  // Not every tick captures (the scan takes 5 frames per second): keep the
  // costliest tick seen.
  const busiest = { capture_ms: 0, reconstruct_ms: 0, encode_ms: 0, total_ms: 0 };
  while (Date.now() - t0 < seconds * 1000) {
    await sleep(250);
    const s = await state(page);
    ticks.push(s.stats.tick_hz);
    fps.push(s.viewer.fps);
    if (s.stats.total_ms > busiest.total_ms) for (const key of Object.keys(busiest)) busiest[key] = s.stats[key];
  }
  const b = await state(page);
  const elapsed = (Date.now() - t0) / 1000;
  const mean = (v) => v.reduce((x, y) => x + y, 0) / v.length;
  return {
    tick_hz_reported: +mean(ticks).toFixed(1),
    tick_hz_shown: b.pipelineHz === null ? null : +b.pipelineHz.toFixed(1),
    ticks_per_s: +((b.stats.tick - a.stats.tick) / elapsed).toFixed(1),
    animation_fps: +mean(fps).toFixed(1),
    redraw_fps: +((b.viewer.framesRendered - a.viewer.framesRendered) / elapsed).toFixed(1),
    capture_ms: +busiest.capture_ms.toFixed(1),
    reconstruct_ms: +busiest.reconstruct_ms.toFixed(1),
    encode_ms: +busiest.encode_ms.toFixed(2),
    tick_total_ms: +busiest.total_ms.toFixed(1),
    splats: b.layers.static + b.layers.dynamic,
    transfer_kb_s: +(b.transferBytesPerSecond / 1000).toFixed(0),
    canvas: `${b.viewer.canvasWidth}x${b.viewer.canvasHeight}`,
  };
}

async function run({ browser, url }) {
  const out = { build: sizes(), chromium: browser.version() };
  check('perf: the build is complete', out.build.app_wasm_bytes > 0 && out.build.worker_wasm_bytes > 0, JSON.stringify(out.build));

  // ---- stage
  let { ctx, page } = await newPage(browser);
  const t0 = Date.now();
  await open(page, `${url}?source=worker`);
  out.stage_first_picture_s = +((Date.now() - t0) / 1000).toFixed(1);
  await sleep(2000);
  out.stage = await sample(page, 8);
  check('perf: stage figures measured', out.stage.ticks_per_s > 1 && out.stage.redraw_fps > 0.5, JSON.stringify(out.stage));
  await ctx.close();

  // ---- house: a whole scan in real time
  ({ ctx, page } = await newPage(browser));
  const start = Date.now();
  await open(page, `${url}?source=worker&scene=house`, { scene: 'house' });
  await waitState(page, (s) => s.scan && s.scan.progress > 0.2, 300000);
  out.house_scanning = await sample(page, 8);
  const s = await waitState(page, (s) => s.scan && s.scan.state === 'done', 900000);
  out.house_full_scan_s = +((Date.now() - start) / 1000).toFixed(1);
  out.house_splats = s.layers.static;
  // Walking through the finished house: fly forward and back so that every
  // frame has to be sorted and drawn.
  await page.click('#visit-house');
  await waitState(page, (s) => s.nav === 'fly');
  const a = await state(page);
  const w0 = Date.now();
  for (const key of ['w', 's', 'w', 's']) {
    await page.keyboard.down(key);
    await sleep(1000);
    await page.keyboard.up(key);
  }
  const b = await state(page);
  out.house_walk_redraw_fps = +((b.viewer.framesRendered - a.viewer.framesRendered) / ((Date.now() - w0) / 1000)).toFixed(1);
  out.house_walk_sort_ms = +b.viewer.sortMs.toFixed(1);
  check('perf: house figures measured', out.house_full_scan_s > 5 && out.house_splats > 100000, `${out.house_full_scan_s} s, ${out.house_splats} splats`);
  await ctx.close();
  return out;
}

module.exports = { run };
if (require.main === module) require('./standalone.cjs')(run);
