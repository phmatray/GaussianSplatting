// Pictures and measurements of the blending modes on the product's own
// clouds (?test=scene): not a pass/fail suite, a report.
//
//   node e2e/render/measure.cjs            (BUILD=1 to rebuild the harness first)
//
// Environment:
//   MEASURE=pictures,perf,popping,flicker,xr   what to produce (default: all)
//   MEASURE_SCENES=stage,house                 which scenes (default: both)
//   MEASURE_FRAMES=12                          frames per timing
//   BEFORE_PATCH_STAGE / BEFORE_PATCH_HOUSE    JSON merged into the pipeline
//       configuration for the "before" pictures (default: the configuration
//       of the product before the quality round)
//   GS_RENDER_OUT                              output directory
//
// Writes pic-<scene>-<view>-<mode>.png (1280x720, the real WebGL2 canvas),
// sheet-<scene>-<view>.png (before | sorted | hard | soft side by side),
// pic-xr-<scene>-<mode>.png (one stereo frame through the IWER emulator)
// and measure.json.
//
// Headless Chromium renders with SwiftShader, a software rasterizer on the
// CPU: its frame times rank the modes by work done, they are not the frame
// times of a GPU. The machine may be shared: every timing carries the load
// average, and the fastest and median frames are reported next to the mean.
const fs = require('fs');
const os = require('os');
const path = require('path');
const { OUT, newPage, consoleErrors, main, check } = require('./lib.cjs');

const WANT = (process.env.MEASURE || 'pictures,perf,popping,flicker,xr').split(',');
const SCENES = (process.env.MEASURE_SCENES || 'stage,house').split(',');
const FRAMES = Number(process.env.MEASURE_FRAMES || 12);
// The pipeline of the product before the quality round: eight stacked
// 160x120 cameras; 3.2 cm scan cells.
const BEFORE_PATCH = {
  stage: process.env.BEFORE_PATCH_STAGE || '{"fusion":{"enabled":false},"rig":{"width":160,"height":120}}',
  house: process.env.BEFORE_PATCH_HOUSE || '{"scan":{"voxel_size":0.032,"kernel":0.6}}',
};

// [label, blend, quality, content]. "before" is drawn on its own cloud, as
// imported content: sorted with the plain dilation, the renderer of the
// product before the blending modes existed.
const MODES = [
  ['sorted', 'sorted', 'soft', 'surfels'],
  ['hard', 'normalized', 'hard', 'surfels'],
  ['soft', 'normalized', 'soft', 'surfels'],
];

const scene = (page, fn, ...args) => page.evaluate(([f, a]) => window.__scene[f](...a), [fn, args]);
const sceneCmd = (page, name, arg) => page.evaluate(([n, a]) => window.__scene.cmd(n, a), [name, arg]);
const load = () => os.loadavg().map((x) => Number(x.toFixed(2)));

async function open(browser, url, sceneName, { patch = null, extra = '', iwer = false, width = 1280, height = 720 } = {}) {
  const { ctx, page, logs } = await newPage(browser, { width, height, iwer });
  const query = `?test=scene&scene=${sceneName}${patch ? `&patch=${encodeURIComponent(patch)}` : ''}${extra}`;
  await page.goto(`${url}/${query}`);
  await page.waitForFunction(() => window.__ready || window.__error, null, { timeout: 900000 });
  const error = await page.evaluate(() => window.__error || null);
  if (error) throw new Error(`scene page failed (${query}): ${error}`);
  await page.addStyleTag({ content: '#hud{display:none}' });
  return { ctx, page, logs, state: await scene(page, 'state') };
}

function views(sceneName, st) {
  if (sceneName === 'stage') {
    return {
      orbit: { view: [3.0, 2.0, 4.5, 0, 1.0, 0] },
      closeup: { view: [0.7, 1.55, 1.5, 0, 1.2, 0] },
      far: { view: [9, 6, 12, 0, 0.9, 0] },
    };
  }
  const b = st.bounds;
  const c = [(b[0] + b[3]) / 2, (b[2] + b[5]) / 2];
  return {
    interior: { view: st.viewerStart },
    // The whole house from above with the ceiling clipped away, as the
    // miniature of the headset shows it.
    miniature: { view: [c[0] + 0.5, 14, c[1] + 5, c[0], 0, c[1]], clipY: st.ceiling != null ? st.ceiling - 0.1 : null },
  };
}

async function setMode(page, [, blend, quality, content]) {
  await sceneCmd(page, 'content', content);
  await sceneCmd(page, 'blend', blend);
  await sceneCmd(page, 'quality', quality);
}

async function setView(page, v) {
  await sceneCmd(page, 'view', v.view);
  await sceneCmd(page, 'clipY', v.clipY ?? null);
}

// Waits until the frame loop has drawn the current settings. The loop only
// draws when something changed: setting the view again asks for one frame.
async function drawn(page) {
  await page.evaluate(async () => {
    const s = window.__scene.state();
    window.__scene.cmd('view', [...s.eye, ...s.target]);
    const deadline = performance.now() + 60000;
    while (window.__scene.state().framesRendered <= s.framesRendered && performance.now() < deadline) {
      await new Promise((r) => requestAnimationFrame(r));
    }
  });
  await page.waitForTimeout(100);
}

// One row of labelled pictures as a single image.
async function sheet(browser, files, labels, out) {
  const ctx = await browser.newContext({ viewport: { width: 640 * files.length + 8 * (files.length + 1), height: 420 }, deviceScaleFactor: 2 });
  const page = await ctx.newPage();
  const cells = files
    .map((f, i) => `<figure><img src="data:image/png;base64,${fs.readFileSync(f).toString('base64')}"><figcaption>${labels[i]}</figcaption></figure>`)
    .join('');
  await page.setContent(
    `<style>body{margin:8px;background:#111;color:#eee;font:15px system-ui,sans-serif;display:flex;gap:8px}figure{margin:0}img{width:640px;height:360px;display:block}figcaption{padding:4px 0}</style>${cells}`,
  );
  await page.screenshot({ path: out, fullPage: true });
  await ctx.close();
}

async function pictures(browser, url, sceneName, report) {
  const shots = {};
  // "before": the old cloud under the old renderer.
  {
    const { ctx, page, state: st } = await open(browser, url, sceneName, { patch: BEFORE_PATCH[sceneName] });
    await setMode(page, ['before', 'sorted', 'soft', 'imported']);
    for (const [name, v] of Object.entries(views(sceneName, st))) {
      await setView(page, v);
      await drawn(page);
      const file = path.join(OUT, `pic-${sceneName}-${name}-before.png`);
      await page.screenshot({ path: file });
      shots[name] = [[file, `before: ${st.splatsTotal} splats, sorted, plain dilation`]];
    }
    report.before = { patch: BEFORE_PATCH[sceneName], splats: st.splatsTotal };
    await ctx.close();
  }
  const { ctx, page, logs, state: st } = await open(browser, url, sceneName);
  report.splats = st.splatsTotal;
  for (const [name, v] of Object.entries(views(sceneName, st))) {
    await setView(page, v);
    for (const mode of MODES) {
      await setMode(page, mode);
      await drawn(page);
      const s = await scene(page, 'state');
      const file = path.join(OUT, `pic-${sceneName}-${name}-${mode[0]}.png`);
      await page.screenshot({ path: file });
      shots[name].push([file, `${s.blend}: ${s.splatsTotal} splats`]);
    }
    await sheet(browser, shots[name].map((x) => x[0]), shots[name].map((x) => x[1]), path.join(OUT, `sheet-${sceneName}-${name}.png`));
    console.log(`      pictures: ${sceneName} ${name}`);
  }
  report.pictureErrors = consoleErrors(logs);
  await ctx.close();
}

// Frame loop timing in the page: a new view every animation frame, so every
// frame redraws (and every sorted frame sorts).
async function loopTiming(page, frames) {
  return page.evaluate(async (frames) => {
    const s0 = window.__scene.state();
    const [ex, ey, ez] = s0.eye;
    const [tx, ty, tz] = s0.target;
    const frame = () => new Promise((r) => requestAnimationFrame(r));
    const turn = (i) => {
      const a = (i * 2 * Math.PI) / 180;
      const dx = ex - tx, dz = ez - tz;
      window.__scene.cmd('view', [tx + dx * Math.cos(a) + dz * Math.sin(a), ey, tz - dx * Math.sin(a) + dz * Math.cos(a), tx, ty, tz]);
    };
    // Warm-up: target allocation, and the smoothed statistics settle.
    for (let i = 1; i <= 4; i++) { turn(i); await frame(); }
    const a = window.__scene.state();
    const t0 = performance.now();
    let cpu = 0, sort = 0;
    for (let i = 5; i < 5 + frames; i++) {
      turn(i);
      await frame();
      const s = window.__scene.state();
      cpu += s.frameMs;
      sort += s.blend === 'sorted' ? s.sortMs : 0;
    }
    const b = window.__scene.state();
    window.__scene.cmd('view', [ex, ey, ez, tx, ty, tz]);
    const rendered = b.framesRendered - a.framesRendered;
    return {
      blend: b.blend,
      frames: rendered,
      intervalMs: (performance.now() - t0) / Math.max(rendered, 1),
      // Smoothed by the viewer (weight 0.1 per frame): an average over the run.
      mainThreadMs: cpu / frames,
      sortMs: sort / frames,
      sortsPerFrame: (b.sorts - a.sorts) / Math.max(rendered, 1),
      splatsDrawn: b.splatsDrawn,
    };
  }, frames);
}

async function perf(browser, url, sceneName, report) {
  const { ctx, page, logs, state: st } = await open(browser, url, sceneName);
  report.perf = [];
  for (const [name, v] of Object.entries(views(sceneName, st)).slice(0, 2)) {
    await setView(page, v);
    for (const mode of MODES) {
      await setMode(page, mode);
      await drawn(page);
      const before = load();
      const snapshot = await scene(page, 'timing', FRAMES);
      const loop = await loopTiming(page, FRAMES);
      const fragments = await scene(page, 'fragments');
      const row = { scene: sceneName, view: name, mode: mode[0], splats: st.splatsTotal, load: before, snapshot, loop, fragments };
      report.perf.push(row);
      console.log(
        `      ${sceneName} ${name} ${mode[0].padEnd(6)} ${snapshot.width}x${snapshot.height}: snapshot min ${snapshot.minMs.toFixed(0)} / median ${snapshot.medianMs.toFixed(0)} ms; ` +
          `loop ${loop.intervalMs.toFixed(0)} ms/frame, main thread ${loop.mainThreadMs.toFixed(2)} ms (sort ${loop.sortMs.toFixed(2)} ms, ${loop.sortsPerFrame.toFixed(2)} sorts/frame); ` +
          `fragments/px ${fragments.visibilityPerPixel.toFixed(1)} + ${fragments.shadedPerPixel.toFixed(1)}; load ${before[0]}`,
      );
    }
  }
  report.perfErrors = consoleErrors(logs);
  await ctx.close();
}

async function popping(browser, url, sceneName, report) {
  // A smaller canvas: 40 snapshots per mode.
  const { ctx, page, state: st } = await open(browser, url, sceneName, { width: 640, height: 360 });
  const v = Object.values(views(sceneName, st))[0];
  await setView(page, v);
  report.popping = [];
  for (const mode of MODES) {
    await setMode(page, mode);
    await drawn(page);
    // A quarter of a degree per step: slow enough that what changes between
    // two pictures is parallax (smooth) or a change of draw order (a pop).
    const p = await scene(page, 'popping', 40, 0.25);
    report.popping.push({ scene: sceneName, mode: mode[0], ...p });
    console.log(
      `      popping ${sceneName} ${mode[0].padEnd(6)}: mean abs diff ${p.meanAbsDiff.toFixed(3)}, pixels jumping > 8 levels ${(100 * p.jumpFraction8).toFixed(3)} % (worst step ${(100 * p.worstJumpFraction8).toFixed(3)} %), > 32 levels ${(100 * p.jumpFraction32).toFixed(4)} %`,
    );
  }
  await ctx.close();
}

// Sensor noise with the scene frozen: every tick delivers a new live layer
// of the same pose, so whatever changes between two pictures is flicker.
async function flicker(browser, url, report) {
  const patch = '{"time_scale":0,"noise":{"depth_sigma_at_1m":0.0015,"color_sigma":2.0,"dropout":0.01}}';
  const { ctx, page } = await open(browser, url, 'stage', { patch, extra: '&animate=1', width: 640, height: 360 });
  await sceneCmd(page, 'pause', true);
  await setView(page, { view: [1.6, 1.5, 2.6, 0, 1.0, 0] });
  report.flicker = [];
  for (const mode of MODES) {
    await setMode(page, mode);
    await drawn(page);
    const f = await scene(page, 'flicker', 16);
    report.flicker.push({ mode: mode[0], ...f });
    console.log(
      `      flicker stage ${mode[0].padEnd(6)}: mean abs diff ${f.meanAbsDiff.toFixed(3)}, pixels jumping > 8 levels ${(100 * f.jumpFraction8).toFixed(3)} %, > 32 levels ${(100 * f.jumpFraction32).toFixed(4)} % (${f.pairs} tick pairs, live layer ${f.dynamicSplats} splats)`,
    );
  }
  await ctx.close();
}

async function xr(browser, url, sceneName, report) {
  const { ctx, page, logs } = await open(browser, url, sceneName, { iwer: true });
  await sceneCmd(page, 'enterXr');
  await page.waitForFunction(() => window.__scene.state().xrActive && window.__scene.state().xrFrames > 2, null, { timeout: 60000 });
  report.xr = [];
  for (const mode of MODES) {
    await setMode(page, mode);
    // Move the emulated head a little every frame, so that sorted frames sort.
    const row = await page.evaluate(async (frames) => {
      const frame = () => new Promise((r) => setTimeout(r, 0));
      const wait = async (n) => { const f0 = window.__scene.state().xrFrames; while (window.__scene.state().xrFrames < f0 + n) await frame(); };
      const yaw = (i) => { const q = window.xrdev.quaternion; const a = 0.02 * i; q.set(0, Math.sin(a / 2), 0, Math.cos(a / 2)); };
      for (let i = 0; i < 4; i++) { yaw(i); await wait(1); }
      const a = window.__scene.state();
      const t0 = performance.now();
      let cpu = 0, sort = 0;
      for (let i = 4; i < 4 + frames; i++) {
        yaw(i);
        await wait(1);
        const s = window.__scene.state();
        cpu += s.frameMs;
        sort += s.blend === 'sorted' ? s.sortMs : 0;
      }
      const b = window.__scene.state();
      return {
        blend: b.blend,
        eye: [b.canvasWidth, b.canvasHeight],
        offscreen: b.offscreen,
        frames: b.xrFrames - a.xrFrames,
        intervalMs: (performance.now() - t0) / Math.max(b.xrFrames - a.xrFrames, 1),
        mainThreadMs: cpu / frames,
        sortMs: sort / frames,
        sortsPerFrame: (b.sorts - a.sorts) / Math.max(b.xrFrames - a.xrFrames, 1),
      };
    }, FRAMES);
    const file = path.join(OUT, `pic-xr-${sceneName}-${mode[0]}.png`);
    await page.screenshot({ path: file });
    report.xr.push({ scene: sceneName, mode: mode[0], load: load(), ...row });
    console.log(
      `      xr ${sceneName} ${mode[0].padEnd(6)} 2 x ${row.eye[0]}x${row.eye[1]}: ${row.intervalMs.toFixed(0)} ms/frame, main thread ${row.mainThreadMs.toFixed(2)} ms (sort ${row.sortMs.toFixed(2)} ms, ${row.sortsPerFrame.toFixed(2)} sorts/frame), off-screen targets ${row.offscreen.join('x')}`,
    );
  }
  await sceneCmd(page, 'exitXr');
  report.xrErrors = consoleErrors(logs).filter((l) => !/thrown inside the IWER emulator/.test(l));
  await ctx.close();
}

async function run({ browser, url }) {
  console.log(`\n=== measure: blending modes on the product's clouds (${WANT.join(', ')}) ===`);
  fs.mkdirSync(OUT, { recursive: true });
  const report = { chromium: browser.version(), cpus: os.cpus().length, renderer: 'SwiftShader (software) in headless Chromium', scenes: {} };
  for (const sceneName of SCENES) {
    const r = (report.scenes[sceneName] = {});
    if (WANT.includes('pictures')) await pictures(browser, url, sceneName, r);
    if (WANT.includes('perf')) await perf(browser, url, sceneName, r);
    if (WANT.includes('popping')) await popping(browser, url, sceneName, r);
    if (WANT.includes('flicker') && sceneName === 'stage') await flicker(browser, url, r);
    if (WANT.includes('xr')) await xr(browser, url, sceneName, r);
    const errors = [...(r.pictureErrors || []), ...(r.perfErrors || []), ...(r.xrErrors || [])];
    check(`measure: ${sceneName}: no console errors`, errors.length === 0, errors.slice(0, 4).join(' | '));
  }
  fs.writeFileSync(path.join(OUT, 'measure.json'), JSON.stringify(report, null, 2));
  return report;
}

module.exports = { run };
if (require.main === module) main(run);
