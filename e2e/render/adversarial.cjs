// Adversarial checks of the blending modes: the cases a renderer gets wrong
// first (?test=adversarial), and the viewer's GL hygiene under stress on the
// product's own stage cloud (?test=scene) with an instrumented WebGL2
// context: object leaks across mode switches, resizes and VR sessions,
// errors and feedback loops per draw call, the canvas against the CPU
// reference after every kind of disturbance, and frame-to-frame stability
// against the reference's own.
//   node e2e/render/adversarial.cjs        (BUILD=1 to rebuild the harness first)
//   ADV=cases,stress,canvas,loss,live,temporal,fallback,xr   run a subset
//   ADV_CYCLES=200                         length of the leak check
const fs = require('fs');
const path = require('path');
const lib = require('./lib.cjs');
const { OUT, check, newPage, consoleErrors, shot, imageStats, iwerBundle, waitLive, main } = lib;
const { stereoAgainstReference } = require('./robust.cjs');

// As in compare.cjs.
const MIN_PSNR = { sorted: 35, 'normalized-hard': 45, 'normalized-soft': 45 };
const BLEND = { sorted: 'sorted', hard: 'normalized-hard', soft: 'normalized-soft' };
const ALL = ['sorted', 'hard', 'soft'];
const NORMALIZED = ['hard', 'soft'];

// ?test=adversarial: view -> renderings, and per-case bounds where the
// general ones do not apply (see the comments).
const CASES = {
  near_plane_crossing: ALL, near_plane_crossing_both_faces: NORMALIZED,
  grazing_floor: ALL, grazing_floor_in_plane: ALL, floor_from_below: NORMALIZED, floor_from_below_both_faces: NORMALIZED,
  'sheets_1cm_from_0.5m': ALL, sheets_1cm_from_3m: ALL, sheets_1cm_oblique: ALL, sheets_from_behind: NORMALIZED, sheets_from_behind_both_faces: NORMALIZED,
  faint_sheets: ALL, giant_surfel: ALL, dynamic_layer_alone: ALL, no_splats: ALL,
  stage_30m: ALL, stage_30m_tele: ALL, stage_300m: ALL, stage_300m_tele: NORMALIZED,
  weights_live_10000: NORMALIZED, weights_static_10000: NORMALIZED, weights_both_10000: NORMALIZED,
  weights_live_zero: NORMALIZED, weights_static_zero: NORMALIZED, weights_both_tiny: NORMALIZED,
  stage_live_layer_emptied: ALL,
};
const BOUNDS = {};

function expectedCases() {
  const cases = new Map();
  for (const [view, renderings] of Object.entries(CASES)) for (const r of renderings) cases.set(`${view}_${r}`, BLEND[r]);
  return cases;
}

// ---------------------------------------------------------------------------
// Instrumented WebGL2 context: counts live objects per kind, asks for the GL
// error after every draw call, and looks for feedback loops itself (a
// texture attached to the draw framebuffer and bound to a unit one of the
// program's samplers reads), which a browser reports, ignores or renders as
// black depending on the driver.
function instrument() {
  const P = WebGL2RenderingContext.prototype;
  const KINDS = ['Texture', 'Framebuffer', 'Program', 'Shader', 'Buffer', 'VertexArray', 'Renderbuffer'];
  const stats = (window.__gl = { live: {}, created: {}, errors: [], loops: [], draws: 0, generation: 0 });
  const generationOf = new WeakMap();
  for (const kind of KINDS) {
    stats.live[kind] = 0;
    stats.created[kind] = 0;
    const [create, remove] = [P[`create${kind}`], P[`delete${kind}`]];
    P[`create${kind}`] = function (...args) {
      const object = create.apply(this, args);
      if (object) {
        generationOf.set(object, stats.generation);
        stats.live[kind]++;
        stats.created[kind]++;
      }
      return object;
    };
    P[`delete${kind}`] = function (object) {
      if (object && generationOf.get(object) === stats.generation) {
        generationOf.delete(object);
        stats.live[kind]--;
      }
      return remove.call(this, object);
    };
  }
  // A lost context takes its objects with it.
  const getContext = HTMLCanvasElement.prototype.getContext;
  const watched = new WeakSet();
  HTMLCanvasElement.prototype.getContext = function (...args) {
    if (args[0] === 'webgl2' && !watched.has(this)) {
      watched.add(this);
      this.addEventListener('webglcontextlost', () => {
        stats.generation++;
        for (const kind of KINDS) stats.live[kind] = 0;
      });
    }
    return getContext.apply(this, args);
  };

  const SAMPLERS = new Set([0x8b5e, 0x8b62, 0x8dca, 0x8dd2, 0x8b5f, 0x8dc1, 0x8dcf, 0x8dd7]);
  const samplersOf = new WeakMap();
  function samplers(gl, program) {
    let list = samplersOf.get(program);
    if (!list) {
      list = [];
      for (let i = 0; i < gl.getProgramParameter(program, gl.ACTIVE_UNIFORMS); i++) {
        const u = gl.getActiveUniform(program, i);
        if (u && SAMPLERS.has(u.type)) list.push({ name: u.name, location: gl.getUniformLocation(program, u.name) });
      }
      samplersOf.set(program, list);
    }
    return list;
  }
  function inspect(gl, what, draws) {
    stats.draws++;
    if (gl.isContextLost()) return;
    const program = gl.getParameter(gl.CURRENT_PROGRAM);
    const framebuffer = gl.getParameter(gl.DRAW_FRAMEBUFFER_BINDING);
    // A clear or a read-back runs no program: only draws can sample.
    if (draws && program && framebuffer) {
      const attached = [];
      for (const attachment of [gl.DEPTH_ATTACHMENT, gl.COLOR_ATTACHMENT0, gl.COLOR_ATTACHMENT1, gl.COLOR_ATTACHMENT2, gl.COLOR_ATTACHMENT3]) {
        if (gl.getFramebufferAttachmentParameter(gl.DRAW_FRAMEBUFFER, attachment, gl.FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE) === gl.TEXTURE) {
          attached.push(gl.getFramebufferAttachmentParameter(gl.DRAW_FRAMEBUFFER, attachment, gl.FRAMEBUFFER_ATTACHMENT_OBJECT_NAME));
        }
      }
      const active = gl.getParameter(gl.ACTIVE_TEXTURE);
      for (const sampler of samplers(gl, program)) {
        const unit = gl.getUniform(program, sampler.location);
        gl.activeTexture(gl.TEXTURE0 + unit);
        if (attached.includes(gl.getParameter(gl.TEXTURE_BINDING_2D))) {
          stats.loops.push(`${what}: ${sampler.name} (unit ${unit}) samples a texture attached to the draw framebuffer`);
        }
      }
      gl.activeTexture(active);
    }
    const error = gl.getError();
    if (error && error !== gl.CONTEXT_LOST_WEBGL) stats.errors.push(`${what}: 0x${error.toString(16)}`);
  }
  for (const name of ['drawElements', 'drawArrays', 'clear', 'readPixels']) {
    const real = P[name];
    P[name] = function (...args) {
      const result = real.apply(this, args);
      inspect(this, name, name.startsWith('draw'));
      return result;
    };
  }
}

// ---------------------------------------------------------------------------
const PAGE = '?test=scene&scene=stage&light=1';
const VIEW = [3.0, 1.2, 4.5, 0, 0.8, 0];
const state = (page) => page.evaluate(() => window.__scene.state());
const cmd = (page, name, arg) => page.evaluate(([n, a]) => window.__scene.cmd(n, a), [name, arg]);
const glStats = (page) => page.evaluate(() => JSON.parse(JSON.stringify(window.__gl)));

async function open(browser, url, { extra = '', width = 480, height = 300, iwer = false, instrumented = true, init = null, deviceScaleFactor = 1 } = {}) {
  const { ctx, page, logs } = await newPage(browser, { width, height, iwer, deviceScaleFactor });
  if (instrumented) await page.addInitScript(instrument);
  if (init) await page.addInitScript(init);
  await page.goto(`${url}/${PAGE}${extra}`);
  await page.waitForFunction(() => window.__ready || window.__error, null, { timeout: 300000 });
  const error = await page.evaluate(() => window.__error || null);
  if (error) throw new Error(`scene page failed: ${error}`);
  await page.addStyleTag({ content: '#hud{display:none}' });
  await cmd(page, 'view', VIEW);
  await drawn(page);
  return { ctx, page, logs };
}

// Waits until the frame loop has drawn the current settings.
async function drawn(page) {
  await page.evaluate(async () => {
    const s = window.__scene.state();
    window.__scene.cmd('view', [...s.eye, ...s.target]);
    const deadline = performance.now() + 60000;
    while (window.__scene.state().framesRendered <= s.framesRendered && performance.now() < deadline) {
      await new Promise((r) => requestAnimationFrame(r));
    }
  });
}

async function setMode(page, rendering) {
  await cmd(page, 'blend', rendering === 'sorted' ? 'sorted' : 'normalized');
  if (rendering !== 'sorted') await cmd(page, 'quality', rendering);
  await drawn(page);
}

function orbit(degrees) {
  const [ex, ey, ez, tx, ty, tz] = VIEW;
  const a = (degrees * Math.PI) / 180;
  const [dx, dz] = [ex - tx, ez - tz];
  return [tx + dx * Math.cos(a) + dz * Math.sin(a), ey, tz - dx * Math.sin(a) + dz * Math.cos(a), tx, ty, tz];
}

// The canvas as the frame loop draws it against the CPU reference.
async function canvasMatches(page, label, rendering, extraOk = () => true) {
  const c = await page.evaluate(() => window.__scene.compareCanvas());
  const blend = BLEND[rendering];
  return check(`adversarial: ${label} [${blend}]: the canvas matches the CPU reference (PSNR >= ${MIN_PSNR[blend]} dB)`,
    c.blend === blend && c.psnr >= MIN_PSNR[blend] && extraOk(c), `${c.blend}, ${c.width}x${c.height}, PSNR ${c.psnr.toFixed(2)} dB, mean abs error ${c.mae.toFixed(3)}`);
}

function cleanGl(label, gl, logs) {
  check(`adversarial: ${label}: no WebGL error after any draw call`, gl.errors.length === 0, `${gl.draws} draw calls inspected${gl.errors.length ? '; ' + [...new Set(gl.errors)].slice(0, 4).join(' | ') : ''}`);
  check(`adversarial: ${label}: no pass samples a texture it renders to`, gl.loops.length === 0, [...new Set(gl.loops)].slice(0, 4).join(' | '));
  const errs = consoleErrors(logs);
  check(`adversarial: ${label}: no console errors or panics`, errs.length === 0, errs.slice(0, 4).join(' | '));
}

// ---------------------------------------------------------------------------
async function cases(browser, url) {
  console.log('\n=== adversarial: hand-built cases, WebGL2 vs the CPU references ===');
  const { ctx, page, logs } = await newPage(browser, { width: 1500, height: 900 });
  await page.goto(`${url}/?test=adversarial`);
  await page.waitForFunction(() => window.__done === true, null, { timeout: 900000 });
  const error = await page.evaluate(() => window.__error || null);
  check('adversarial: the case page finished without error', !error, error || '');
  const results = await page.evaluate(() => JSON.parse(JSON.stringify(window.__results)));
  const want = expectedCases();
  const got = new Set(results.map((c) => c.name));
  const missing = [...want.keys()].filter((name) => !got.has(name));
  check(`adversarial: all ${want.size} cases ran`, missing.length === 0 && results.length === want.size, `${results.length} cases${missing.length ? `, missing: ${missing.join(', ')}` : ''}`);
  for (const c of results) {
    const blend = want.get(c.name);
    const min = BOUNDS[c.name] ?? MIN_PSNR[blend] ?? 45;
    check(`adversarial: ${c.name} is drawn ${blend || '?'} and matches its reference (PSNR >= ${min} dB)`, c.blend === blend && c.psnr >= min,
      `${c.blend}, PSNR ${c.psnr.toFixed(2)} dB, mean abs error ${c.mae.toFixed(3)}, ${c.splats} splats`);
    await page.locator(`[id="case-${c.name}"]`).screenshot({ path: path.join(OUT, `adversarial-${c.name}.png`) });
  }
  fs.writeFileSync(path.join(OUT, 'adversarial.json'), JSON.stringify(results, null, 2));
  fs.writeFileSync(path.join(OUT, 'console-adversarial.log'), logs.join('\n'));
  const errs = consoleErrors(logs);
  check('adversarial: no console errors or panics in the cases', errs.length === 0, errs.slice(0, 4).join(' | '));
  await ctx.close();
}

// Mode switches, resizes and VR sessions, again and again: the number of
// live GL objects must be periodic, and no draw call may raise an error.
async function stress(browser, url) {
  console.log('\n=== adversarial: GL objects, errors and feedback loops under stress ===');
  const cycles = Number(process.env.ADV_CYCLES || 200);
  const xr = !!iwerBundle();
  // With the gizmo lines: their pass samples the visibility depth of the
  // normalised mode right after it was rendered.
  const { ctx, page, logs } = await open(browser, url, { iwer: xr, extra: '&gizmos=1' });
  const SIZES = [[480, 300], [400, 260], [520, 280], [360, 300]];
  const MODES = ['soft', 'hard', 'sorted'];
  const PERIOD = 12;
  // One VR session every 25 cycles, in each mode in turn.
  const session = async () => {
    await cmd(page, 'enterXr');
    await page.waitForFunction(() => { const s = window.__scene.state(); return s.xrActive && s.xrFrames > 2; }, null, { timeout: 60000 });
    await cmd(page, 'exitXr');
    await page.waitForFunction(() => !window.__scene.state().xrActive, null, { timeout: 20000 });
    await drawn(page);
  };
  const samples = [];
  let sessions = 0;
  for (let i = 0; i < cycles; i++) {
    const [width, height] = SIZES[i % SIZES.length];
    await page.setViewportSize({ width, height });
    await page.waitForFunction(([w, h]) => { const s = window.__scene.state(); return s.canvasWidth === w && s.canvasHeight === h; }, [width, height], { timeout: 20000 }).catch(() => {});
    await setMode(page, MODES[i % MODES.length]);
    if (xr && i % 25 === 24) {
      await session();
      sessions++;
    }
    const gl = await glStats(page);
    samples.push({ i, live: gl.live });
  }
  const gl = await glStats(page);
  const key = (live) => JSON.stringify(live);
  // After the first period every state has been visited once: from then on
  // the counts must repeat with the period.
  const drift = samples.filter((s) => s.i >= 2 * PERIOD && key(s.live) !== key(samples[PERIOD + (s.i % PERIOD)].live));
  const last = samples[samples.length - 1];
  check(`adversarial: ${cycles} mode switches and resizes${xr ? ` and ${sessions} VR sessions` : ''} leak no GL object`, drift.length === 0 && samples.length === cycles,
    drift.length ? `cycle ${drift[0].i}: ${key(drift[0].live)} against ${key(samples[PERIOD + (drift[0].i % PERIOD)].live)}`
      : `live after cycle ${last.i}: ${key(last.live)}; created in total: ${key(gl.created)}`);
  const s = await state(page);
  check('adversarial: the viewer still reports the mode it was asked for after the stress', s.blend === BLEND[MODES[(cycles - 1) % MODES.length]] && !s.blendFallback, `${s.blend} (${s.blendReason})`);
  cleanGl('stress', gl, logs);

  // The sorted picture and the gizmo lines must not depend on what was drawn
  // before: the passes of the normalised mode leave the GL state usable.
  await page.setViewportSize({ width: 480, height: 300 });
  await setMode(page, 'sorted');
  const fAfter = await shot(page, 'adversarial-gizmos-sorted-after-stress.png');
  await setMode(page, 'soft');
  const fSoftAfter = await shot(page, 'adversarial-gizmos-soft-after-stress.png');
  fs.writeFileSync(path.join(OUT, 'console-adversarial-stress.log'), logs.join('\n'));
  await ctx.close();
  for (const [rendering, file] of [['sorted', fAfter], ['soft', fSoftAfter]]) {
    const fresh = await open(browser, url, { extra: `&gizmos=1&blend=${rendering === 'sorted' ? 'sorted' : 'normalized'}`, instrumented: false });
    const fFresh = await shot(fresh.page, `adversarial-gizmos-${rendering}-fresh.png`);
    const img = await imageStats(fresh.page, file, fFresh);
    check(`adversarial: after the stress, splats and gizmo lines [${BLEND[rendering]}] are drawn exactly as by a fresh viewer`, img.diffMeanAbs === 0, `mean abs diff ${img.diffMeanAbs}`);
    await fresh.ctx.close();
  }
}

// Resize, device pixel ratio and render scale mid-session.
async function canvas(browser, url) {
  console.log('\n=== adversarial: the canvas after resizes, pixel-ratio and render-scale changes ===');
  const { ctx, page, logs } = await open(browser, url, { extra: '&max_dpr=3' });
  const cdp = await ctx.newCDPSession(page);
  const metrics = (width, height, deviceScaleFactor) => cdp.send('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor, mobile: false });
  const sized = (w, h) => page.waitForFunction(([w, h]) => { const s = window.__scene.state(); return s.canvasWidth === w && s.canvasHeight === h; }, [w, h], { timeout: 20000 }).catch(() => {});
  const targets = (w, h) => (c) => c.width === w && c.height === h;
  for (const rendering of ALL) {
    await metrics(480, 300, 1);
    await sized(480, 300);
    await setMode(page, rendering);
    await canvasMatches(page, 'at 480x300', rendering, targets(480, 300));
    await metrics(371, 233, 1);
    await sized(371, 233);
    await canvasMatches(page, 'resized to an odd 371x233', rendering, targets(371, 233));
    await metrics(300, 200, 2);
    await sized(600, 400);
    await canvasMatches(page, 'device pixel ratio 2 (300x200 CSS px)', rendering, targets(600, 400));
    await metrics(300, 200, 1.25);
    await sized(375, 250);
    await canvasMatches(page, 'device pixel ratio 1.25', rendering, targets(375, 250));
    await cmd(page, 'renderScale', 0.5);
    await sized(188, 125);
    await canvasMatches(page, 'render scale 0.5', rendering);
    await cmd(page, 'renderScale', 1.6);
    await sized(600, 400);
    await canvasMatches(page, 'render scale 1.6', rendering, targets(600, 400));
    await cmd(page, 'renderScale', 1);
    const s = await state(page);
    const want = rendering === 'sorted' ? [0, 0] : [s.canvasWidth, s.canvasHeight];
    check(`adversarial: the off-screen targets follow the canvas [${BLEND[rendering]}]`, s.offscreen[0] === want[0] && s.offscreen[1] === want[1], `canvas ${s.canvasWidth}x${s.canvasHeight}, targets ${s.offscreen.join('x')}`);
  }
  cleanGl('resizes', await glStats(page), logs);
  await ctx.close();
}

// A lost and restored context in each mode.
async function contextLoss(browser, url) {
  console.log('\n=== adversarial: context loss and restore in each mode ===');
  const { ctx, page, logs } = await open(browser, url);
  for (const rendering of ALL) {
    await setMode(page, rendering);
    const before = await glStats(page);
    await page.evaluate(() => {
      window.__lose = document.getElementById('gl').getContext('webgl2').getExtension('WEBGL_lose_context');
      window.__lose.loseContext();
    });
    await page.waitForTimeout(300);
    // Parameters changed while the context is gone must be picked up.
    await cmd(page, 'view', orbit(20));
    await page.evaluate(() => window.__lose.restoreContext());
    await page.waitForFunction(() => !document.getElementById('gl').getContext('webgl2').isContextLost(), null, { timeout: 10000 });
    await drawn(page);
    const s = await state(page);
    const after = await glStats(page);
    check(`adversarial: after a context loss the viewer is back in the mode it had [${BLEND[rendering]}]`, s.blend === BLEND[rendering] && !s.blendFallback, `${s.blend} (${s.blendReason})`);
    await canvasMatches(page, 'after a context loss', rendering);
    check(`adversarial: the restored context holds as many GL objects as the lost one [${BLEND[rendering]}]`, JSON.stringify(after.live) === JSON.stringify(before.live),
      `before ${JSON.stringify(before.live)}, after ${JSON.stringify(after.live)}`);
    await cmd(page, 'view', VIEW);
  }
  cleanGl('context loss', await glStats(page), logs);
  await ctx.close();
}

// The live layer replaced on every tick while the camera orbits.
async function liveOrbit(browser, url) {
  console.log('\n=== adversarial: a layer replaced every frame under an orbiting camera ===');
  const { ctx, page, logs } = await open(browser, url, { extra: '&animate=1' });
  for (const rendering of ALL) {
    await cmd(page, 'pause', false);
    await setMode(page, rendering);
    const start = await state(page);
    const live0 = (await glStats(page)).live;
    for (let i = 1; i <= 30; i++) {
      await cmd(page, 'view', orbit(4 * i));
      await drawn(page);
    }
    const end = await state(page);
    const live1 = (await glStats(page)).live;
    check(`adversarial: 30 orbit frames with the live layer replaced in between [${BLEND[rendering]}]`,
      end.ticks >= start.ticks + 15 && end.framesRendered >= start.framesRendered + 30 && end.blend === BLEND[rendering] && JSON.stringify(live0) === JSON.stringify(live1),
      `${end.ticks - start.ticks} ticks, ${end.framesRendered - start.framesRendered} frames, GL objects ${JSON.stringify(live1)}`);
    await cmd(page, 'pause', true);
    await canvasMatches(page, 'after the animated orbit', rendering);
  }
  cleanGl('animated orbit', await glStats(page), logs);
  await ctx.close();
}

// An orbit of 60 frames on the GPU and on the CPU. A renderer that pops or
// flickers by itself changes more between frames than its reference does.
async function temporal(browser, url) {
  console.log('\n=== adversarial: 60 frames of an orbit against the reference, frame by frame ===');
  const { ctx, page, logs } = await open(browser, url, { instrumented: false });
  const rows = {};
  for (const rendering of ALL) {
    await setMode(page, rendering);
    const r = await page.evaluate(() => window.__scene.orbitCheck(60, 0.5));
    rows[rendering] = r;
    const blend = BLEND[rendering];
    const pct = (x) => `${(100 * x).toFixed(3)} %`;
    check(`adversarial: every frame of the orbit matches the reference [${blend}] (worst PSNR >= ${MIN_PSNR[blend]} dB)`, r.blend === blend && r.frames === 60 && r.worstPsnr >= MIN_PSNR[blend],
      `worst ${r.worstPsnr.toFixed(2)} dB, mean ${r.meanPsnr.toFixed(2)} dB over ${r.frames} frames`);
    // Frame-to-frame change of the GPU sequence against the reference's
    // own. Normalised: within 5 % of it plus one pixel in ten thousand.
    // Sorted: the 8-bit framebuffer rounds after every splat, a noise of a
    // quarter of a level that is new in every frame; large jumps must
    // still be the reference's.
    const sorted = rendering === 'sorted';
    const calm = (g, c, slack) => g <= 1.05 * c + slack;
    check(`adversarial: the GPU orbit changes from frame to frame as the reference does, no more [${blend}]`,
      calm(r.gpu.jumpFraction8, r.cpu.jumpFraction8, sorted ? 1e-3 : 1e-4) && calm(r.gpu.jumpFraction32, r.cpu.jumpFraction32, 1e-4) && calm(r.gpu.meanAbsDiff, r.cpu.meanAbsDiff, sorted ? 0.15 : 0.01),
      `pixels changing by more than 8 levels per 0.5 degree: GPU ${pct(r.gpu.jumpFraction8)}, CPU ${pct(r.cpu.jumpFraction8)}; more than 32: GPU ${pct(r.gpu.jumpFraction32)}, CPU ${pct(r.cpu.jumpFraction32)}; mean abs diff GPU ${r.gpu.meanAbsDiff.toFixed(3)}, CPU ${r.cpu.meanAbsDiff.toFixed(3)}`);
    if (r.truth) {
      console.log(`      the scene itself (ray-traced): ${pct(r.truth.jumpFraction8)} of the pixels change by more than 8 levels, ${pct(r.truth.jumpFraction32)} by more than 32, mean abs diff ${r.truth.meanAbsDiff.toFixed(3)}`);
    }
  }
  fs.writeFileSync(path.join(OUT, 'adversarial-temporal.json'), JSON.stringify(rows, null, 2));
  const errs = consoleErrors(logs);
  check('adversarial: no console errors in the orbit', errs.length === 0, errs.slice(0, 4).join(' | '));
  await ctx.close();
}

// Float render targets: either extension alone is enough, a driver that
// cannot render to RG16F gets RGBA16F, and imported content is drawn sorted.
async function fallback(browser, url) {
  console.log('\n=== adversarial: float-target support and content ===');
  const maskOne = (name) => `(() => { const real = WebGL2RenderingContext.prototype.getExtension;
    WebGL2RenderingContext.prototype.getExtension = function (n) { return n === '${name}' ? null : real.call(this, n); }; })()`;
  for (const hidden of ['EXT_color_buffer_half_float', 'EXT_color_buffer_float']) {
    const { ctx, page, logs } = await open(browser, url, { instrumented: false, init: maskOne(hidden) });
    const s = await state(page);
    check(`adversarial: without ${hidden} alone the normalised mode still runs`, s.blend === 'normalized-soft' && !s.blendFallback, `${s.blend} (${s.blendReason})`);
    await canvasMatches(page, `without ${hidden}`, 'soft');
    const errs = consoleErrors(logs);
    check(`adversarial: no console errors without ${hidden}`, errs.length === 0, errs.slice(0, 4).join(' | '));
    await ctx.close();
  }

  // A driver on which a framebuffer with an RG16F attachment is incomplete.
  const noRg16f = () => {
    const P = WebGL2RenderingContext.prototype;
    const formatOf = new WeakMap();
    const attachedTo = new WeakMap();
    let bound = null;
    window.__rg16fRefused = 0;
    const [storage, bind, attach, status] = [P.texStorage2D, P.bindFramebuffer, P.framebufferTexture2D, P.checkFramebufferStatus];
    P.texStorage2D = function (target, levels, format, ...rest) {
      formatOf.set(this.getParameter(this.TEXTURE_BINDING_2D), format);
      return storage.call(this, target, levels, format, ...rest);
    };
    P.bindFramebuffer = function (target, framebuffer) {
      bound = framebuffer;
      return bind.call(this, target, framebuffer);
    };
    P.framebufferTexture2D = function (target, attachment, textarget, texture, level) {
      if (bound) {
        if (!attachedTo.has(bound)) attachedTo.set(bound, new Map());
        attachedTo.get(bound).set(attachment, texture);
      }
      return attach.call(this, target, attachment, textarget, texture, level);
    };
    P.checkFramebufferStatus = function (target) {
      const textures = bound && attachedTo.get(bound);
      if (textures && [...textures.values()].some((t) => t && formatOf.get(t) === 0x822f)) {
        window.__rg16fRefused++;
        return 0x8cd6;
      }
      return status.call(this, target);
    };
  };
  {
    const { ctx, page, logs } = await open(browser, url, { init: noRg16f });
    for (const rendering of NORMALIZED) {
      await setMode(page, rendering);
      const s = await state(page);
      const refused = await page.evaluate(() => window.__rg16fRefused);
      check(`adversarial: where RG16F is not renderable the coverage target is RGBA16F [${BLEND[rendering]}]`, s.blend === BLEND[rendering] && !s.blendFallback && refused > 0, `${s.blend} (${s.blendReason}), RG16F refused ${refused} times`);
      await canvasMatches(page, 'with an RGBA16F coverage target', rendering);
    }
    cleanGl('RGBA16F coverage target', await glStats(page), logs);
    await ctx.close();
  }

  // Targets that cannot be allocated once (memory pressure, a huge canvas):
  // sorted while it lasts, normalised again at the next size.
  {
    const { ctx, page, logs } = await open(browser, url, { instrumented: false });
    const sized = (w, h) => page.waitForFunction(([w, h]) => { const s = window.__scene.state(); return s.canvasWidth === w && s.canvasHeight === h; }, [w, h], { timeout: 20000 }).catch(() => {});
    await page.evaluate(() => {
      window.__realStatus = WebGL2RenderingContext.prototype.checkFramebufferStatus;
      WebGL2RenderingContext.prototype.checkFramebufferStatus = () => 0x8cd6;
    });
    for (const [width, height] of [[500, 320], [460, 280], [440, 300]]) {
      await page.setViewportSize({ width, height });
      await sized(width, height);
      await drawn(page);
    }
    let s = await state(page);
    const warnings = logs.filter((l) => /falling back to sorted blending/.test(l)).length;
    check('adversarial: while the targets cannot be allocated the viewer draws sorted, says why, and warns once', s.blend === 'sorted' && s.blendReason === 'targets-failed' && s.blendFallback && s.offscreen[0] === 0 && warnings === 1,
      `${s.blend} (${s.blendReason}), ${warnings} console warning(s) over three resizes`);
    await canvasMatches(page, 'during the fallback', 'sorted');
    await page.evaluate(() => { WebGL2RenderingContext.prototype.checkFramebufferStatus = window.__realStatus; });
    await drawn(page);
    s = await state(page);
    check('adversarial: the fallback lasts as long as the views keep their size', s.blend === 'sorted' && s.blendReason === 'targets-failed', `${s.blend} (${s.blendReason})`);
    await page.setViewportSize({ width: 480, height: 300 });
    await sized(480, 300);
    await drawn(page);
    s = await state(page);
    check('adversarial: after a transient allocation failure the normalised mode comes back with the next resize', s.blend === 'normalized-soft' && !s.blendFallback && s.offscreen[0] === 480, `${s.blend} (${s.blendReason}), targets ${s.offscreen.join('x')}`);
    await canvasMatches(page, 'after the failure has passed', 'soft');
    const errs = consoleErrors(logs);
    check('adversarial: no console errors around the allocation failure', errs.length === 0, errs.slice(0, 4).join(' | '));
    await ctx.close();
  }

  // Imported content: sorted in auto mode, with or without float targets,
  // and never reported as a fallback.
  for (const extra of ['', '&mask_float=1']) {
    const { ctx, page, logs } = await open(browser, url, { extra, instrumented: false });
    await cmd(page, 'content', 'imported');
    await drawn(page);
    const s = await state(page);
    const label = extra ? 'without float targets' : 'with float targets';
    check(`adversarial: an imported cloud is drawn sorted in auto mode, ${label}, and that is no fallback`, s.blend === 'sorted' && s.blendReason === 'imported-content' && !s.blendFallback && s.offscreen[0] === 0,
      `${s.blend} (${s.blendReason}), off-screen targets ${s.offscreen.join('x')}`);
    await canvasMatches(page, `imported content in auto mode, ${label}`, 'sorted');
    await cmd(page, 'content', 'surfels');
    await drawn(page);
    const back = await state(page);
    const want = extra ? ['sorted', 'no-float-targets', true] : ['normalized-soft', 'surfel-content', false];
    check(`adversarial: relabelled as surfels the cloud is drawn ${want[0]} (${want[1]}), ${label}`, back.blend === want[0] && back.blendReason === want[1] && back.blendFallback === want[2], `${back.blend} (${back.blendReason})`);
    await canvasMatches(page, `surfel content in auto mode, ${label}`, extra ? 'sorted' : 'soft');
    const errs = consoleErrors(logs);
    check(`adversarial: no console errors with imported content, ${label}`, errs.length === 0, errs.slice(0, 4).join(' | '));
    await ctx.close();
  }
}

// A context lost inside a VR session, in the two renderings robust.cjs does
// not cover, with the GL objects counted across repeated sessions.
async function xrLoss(browser, url) {
  if (!iwerBundle()) {
    check('adversarial: IWER emulator bundle found', false, 'run `npm install` in e2e/render, or set GS_NPM_TOOLS / IWER_BUNDLE');
    return;
  }
  console.log('\n=== adversarial: context loss inside a VR session, hard and sorted ===');
  const { ctx, page, logs } = await newPage(browser, { width: 1000, height: 500, iwer: true });
  await page.addInitScript(instrument);
  await page.goto(`${url}/?test=live&static_cams=4&capture_scale=0.5`);
  await waitLive(page);
  await page.waitForFunction(() => window.__harness.state().xrSupported === true);
  const active = () => page.waitForFunction(() => { const s = window.__harness.state(); return s.xrActive && s.xr && s.xr.frames > 3 && s.xr.views.length === 2; }, null, { timeout: 60000 });
  const inactive = () => page.waitForFunction(() => !window.__harness.state().xrActive, null, { timeout: 20000 });
  const frames = async (n) => {
    const start = (await lib.state(page)).xr.frames;
    await page.waitForFunction((t) => { const s = window.__harness.state(); return s.xr && s.xr.frames >= t; }, start + n, { timeout: 60000 });
  };
  await lib.cmd(page, 'pause', true);
  await lib.cmd(page, 'gizmos', false);
  await page.evaluate(() => {
    document.getElementById('hud').style.display = 'none';
    xrdev.position.set(0.2, 1.5, 0.4);
    xrdev.controllers.left.position.set(-0.2, 1.2, 3);
    xrdev.controllers.right.position.set(0.2, 1.2, 3);
  });
  for (const [blend, quality, name] of [['normalized', 'hard', 'normalized-hard'], ['sorted', 'soft', 'sorted']]) {
    await lib.cmd(page, 'blend', blend);
    await lib.cmd(page, 'quality', quality);
    await lib.cmd(page, 'enterXr');
    await active();
    await frames(3);
    await stereoAgainstReference(page, 'adversarial: VR session', `adversarial-xr-${name}.png`, name);
    await page.evaluate(() => {
      window.__lose = document.getElementById('gl').getContext('webgl2').getExtension('WEBGL_lose_context');
      window.__lose.loseContext();
    });
    await page.waitForTimeout(400);
    await page.evaluate(() => window.__lose.restoreContext());
    await page.waitForFunction(() => !document.getElementById('gl').getContext('webgl2').isContextLost(), null, { timeout: 10000 });
    await page.waitForTimeout(300);
    if ((await lib.state(page)).xrActive) {
      await frames(4);
      await stereoAgainstReference(page, 'adversarial: VR session after a context loss', `adversarial-xr-${name}-restored.png`, name);
      await lib.cmd(page, 'exitXr');
      await inactive();
    } else {
      check(`adversarial: a VR session [${name}] survives a context loss or ends cleanly`, (await lib.state(page)).xr === null, 'the session ended');
    }
  }
  // Sessions entered and left in the normalised mode: per-eye targets come
  // and go, nothing accumulates.
  await lib.cmd(page, 'blend', 'auto');
  await lib.cmd(page, 'quality', 'soft');
  const counts = [];
  for (let i = 0; i < 6; i++) {
    await lib.cmd(page, 'enterXr');
    await active();
    await lib.cmd(page, 'exitXr');
    await inactive();
    await page.waitForFunction(() => { const s = window.__harness.state(); return s.offscreen[0] === s.canvasWidth; }, null, { timeout: 20000 }).catch(() => {});
    counts.push(JSON.stringify((await page.evaluate(() => window.__gl.live))));
  }
  check('adversarial: six VR sessions in a row leave the same GL objects behind each time', counts.every((c) => c === counts[0]), counts[0] === counts[5] ? counts[0] : counts.join(' / '));
  const gl = await page.evaluate(() => JSON.parse(JSON.stringify(window.__gl)));
  // The emulator itself is not written for a lost context.
  const own = logs.filter((l) => !/thrown inside the IWER emulator/.test(l));
  cleanGl('VR sessions', gl, own);
  fs.writeFileSync(path.join(OUT, 'console-adversarial-xr.log'), logs.join('\n'));
  await ctx.close();
}

const PARTS = { cases, stress, canvas, loss: contextLoss, live: liveOrbit, temporal, fallback, xr: xrLoss };

async function run({ browser, url }) {
  fs.mkdirSync(OUT, { recursive: true });
  const only = process.env.ADV ? process.env.ADV.split(',') : Object.keys(PARTS);
  for (const [name, part] of Object.entries(PARTS)) {
    if (!only.includes(name)) continue;
    try {
      await part(browser, url);
    } catch (e) {
      check(`adversarial: ${name}: no uncaught exception`, false, e.stack || e);
    }
  }
}

module.exports = { run };
if (require.main === module) main(run);
