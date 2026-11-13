// Adversarial checks: hostile wire messages, degenerate parameters, empty
// layers, a canvas without size, rapid configuration changes, and (through
// the IWER emulator) off-axis stereo against the CPU reference, repeated and
// cancelled session starts and a context loss inside a session.
//   node e2e/render/robust.cjs        (BUILD=1 to rebuild the harness first)
const fs = require('fs');
const path = require('path');
const { OUT, check, newPage, consoleErrors, state, cmd, waitLive, settle, shot, imageStats, hasContent, iwerBundle, main } = require('./lib.cjs');

// ---- wire format (gs_core::codec), little-endian
const KIND = { state: 1, layer: 2, preview: 3, stats: 4 };
function header(kind, magic = 'GSL1') {
  const b = Buffer.alloc(16);
  b.write(magic, 0, 'latin1');
  b.writeUInt8(kind, 4);
  b.writeUInt32LE(7, 8); // seq
  return b;
}
// One 16-byte splat record: position in the AABB (0..65535 per axis), log
// scale codes, RGBA, 3-byte rotation.
function record(pos = [32768, 32768, 32768], scale = 120, rgba = [255, 80, 200, 255], rot = [0, 0, 0]) {
  const b = Buffer.alloc(16);
  pos.forEach((v, i) => b.writeUInt16LE(v, i * 2));
  b.fill(scale, 6, 9);
  rgba.forEach((v, i) => b.writeUInt8(v, 9 + i));
  rot.forEach((v, i) => b.writeUInt8(v, 13 + i));
  return b;
}
function layer({ id = 0, mode = 1, count = null, min = [-1, 0, -1], max = [1, 2, 1], records = [] } = {}) {
  const b = Buffer.alloc(32);
  b.writeUInt8(id, 0);
  b.writeUInt8(mode, 1);
  b.writeUInt32LE(count === null ? records.length : count, 4);
  min.forEach((v, i) => b.writeFloatLE(v, 8 + i * 4));
  max.forEach((v, i) => b.writeFloatLE(v, 20 + i * 4));
  return Buffer.concat([header(KIND.layer), b, ...records]);
}
const apply = (page, bytes) => page.evaluate((a) => window.__harness.apply(new Uint8Array(a)), [...bytes]);

// Small deterministic generator for the fuzz inputs.
function rng(seed) {
  let s = seed >>> 0;
  return () => {
    s = (Math.imul(s, 1664525) + 1013904223) >>> 0;
    return s / 4294967296;
  };
}

async function hostileMessages(page) {
  await cmd(page, 'pause', true);
  await page.waitForTimeout(200);
  const before = await state(page);
  await shot(page, 'robust-0-before.png');
  const valid = layer({ id: 0, mode: 1, records: [record(), record([0, 0, 0]), record([65535, 65535, 65535])] });

  const rejected = {
    'empty message': Buffer.alloc(0),
    'three bytes': Buffer.from('GSL'),
    'bad magic': Buffer.concat([header(KIND.layer, 'XXXX'), valid.subarray(16)]),
    'unknown kind': header(99),
    'kind 0': header(0),
    'layer without body': header(KIND.layer),
    'layer header cut short': valid.subarray(0, 30),
    'record cut short': valid.subarray(0, valid.length - 5),
    'trailing bytes': Buffer.concat([valid, Buffer.from([1, 2, 3])]),
    'count far beyond the payload': layer({ count: 0xffffffff, records: [record()] }),
    'count smaller than the payload': layer({ count: 1, records: [record(), record()] }),
    'unknown layer id': layer({ id: 7, records: [record()] }),
    'unknown layer mode': layer({ mode: 9, records: [record()] }),
    'NaN bounding box': layer({ min: [NaN, 0, 0], records: [record()] }),
    'infinite bounding box': layer({ max: [Infinity, 2, 1], records: [record()] }),
    'state with invalid UTF-8': Buffer.concat([header(KIND.state), Buffer.from([0xff, 0xfe, 0x80])]),
    'preview cut short': Buffer.concat([header(KIND.preview), Buffer.from([0, 0, 64, 0, 64, 0, 0, 0, 1, 2, 3])]),
  };
  const wrong = [];
  for (const [name, bytes] of Object.entries(rejected)) {
    const result = await apply(page, bytes);
    if (!result.startsWith('error: ')) wrong.push(`${name} -> ${result}`);
  }
  check(`robust: ${Object.keys(rejected).length} malformed messages are rejected with an error`, wrong.length === 0, wrong.join(' | '));

  // Fuzz: valid magic and kind, everything else random, random length.
  const random = rng(12345);
  const fuzz = [];
  for (let i = 0; i < 1000; i++) {
    const body = Buffer.from(Array.from({ length: Math.floor(random() * 200) }, () => Math.floor(random() * 256)));
    fuzz.push([...Buffer.concat([header(1 + Math.floor(random() * 4)), body])]);
  }
  let outcomes = await page.evaluate((all) => all.map((a) => window.__harness.apply(new Uint8Array(a))), fuzz);
  let s = await state(page);
  // Random bytes can be a valid State/Stats string; a Layer practically never.
  const layers = outcomes.filter((r) => r.startsWith('layer ')).length;
  check('robust: 1000 random messages leave the layers untouched', layers === 0 && s.staticSplats === before.staticSplats && s.dynamicSplats === before.dynamicSplats, `static ${s.staticSplats}, dynamic ${s.dynamicSplats}, ${layers} random layers accepted, ${outcomes.filter((r) => r.startsWith('error: ')).length} rejected`);

  // Payloads the viewer must pass through untouched, whatever they contain.
  const weird = '{"scene":"\\u0000<script>","config":{"n":1e999},"cameras":[[[[[[]]]]]]';
  check('robust: State and Stats payloads come back verbatim, not interpreted',
    (await apply(page, Buffer.concat([header(KIND.state), Buffer.from(weird)]))) === `state: ${weird}` && (await apply(page, Buffer.concat([header(KIND.stats), Buffer.from('not json')]))) === 'stats: not json');

  // ---- well-formed but extreme content
  let result = await apply(page, layer({ id: 1, mode: 0, records: [] }));
  s = await state(page);
  check('robust: an empty Replace empties the dynamic layer', result === 'layer 1: 0 splats, 0 total' && s.dynamicSplats === 0 && s.staticSplats === before.staticSplats, result);
  // Splats spread over a box 10^30 m wide, appended to the static layer:
  // they are far outside the view and must not disturb the picture (depth
  // sort resolution, NaN in the shader, ...).
  await cmd(page, 'showDynamic', false);
  await page.waitForTimeout(300);
  const staticBefore = await shot(page, 'robust-1-static.png');
  const far = [record([0, 0, 0], 255), record([65535, 65535, 65535], 255), record([0, 65535, 0], 0), record([65535, 0, 65535], 255, [255, 255, 255, 255], [255, 255, 255])];
  result = await apply(page, layer({ id: 0, mode: 1, min: [-1e30, -1e30, -1e30], max: [1e30, 1e30, 1e30], records: far }));
  s = await state(page);
  check('robust: splats at astronomical distances are accepted', result === `layer 0: 4 splats, ${before.staticSplats + 4} total` && s.staticSplats === before.staticSplats + 4, result);
  await page.waitForTimeout(300);
  const staticAfter = await shot(page, 'robust-1-static-with-far-outliers.png');
  await cmd(page, 'showDynamic', true);
  const img = await imageStats(page, staticAfter, staticBefore);
  check('robust: far outliers do not disturb the picture of the scene', hasContent(img.left) && hasContent(img.right) && img.diffChangedFrac < 0.001 && img.diffMeanAbs < 0.2, `${(img.diffChangedFrac * 100).toFixed(3)} % of the pixels changed, mean abs diff ${img.diffMeanAbs.toFixed(3)}`);

  // A valid Replace of the dynamic layer with a few header bytes overwritten
  // at random (never the layer id: the static layer is still needed below),
  // sometimes cut short. Some of these are still valid messages.
  const dynamic = layer({ id: 1, mode: 0, records: [record(), record([0, 0, 0]), record([65535, 65535, 65535])] });
  const mutated = [];
  for (let i = 0; i < 1000; i++) {
    let bytes = Buffer.from(dynamic);
    for (let k = 0; k < 1 + Math.floor(random() * 3); k++) {
      const at = Math.floor(random() * 48);
      if (at !== 16) bytes[at] = Math.floor(random() * 256);
    }
    if (random() < 0.3) bytes = bytes.subarray(0, Math.floor(random() * bytes.length));
    mutated.push([...bytes]);
  }
  outcomes = await page.evaluate((all) => all.map((a) => window.__harness.apply(new Uint8Array(a))), mutated);
  s = await state(page);
  const ok = outcomes.filter((r) => r.startsWith('layer ')).length;
  check('robust: 1000 mutated layer messages are each applied or rejected, never half-applied',
    outcomes.every((r) => /^(layer 1: 3 splats, (3|\d+) total|error: |state: |stats: |preview: )/.test(r)) && ok > 10 && ok < 1000 && s.staticSplats === before.staticSplats + 4 && s.dynamicSplats % 3 === 0,
    `${ok} applied, ${outcomes.filter((r) => r.startsWith('error: ')).length} rejected; layers now ${s.staticSplats} + ${s.dynamicSplats}`);
  // Refill the dynamic layer for the checks that follow.
  await cmd(page, 'pause', false);
  await page.waitForFunction(() => window.__harness.state().dynamicSplats > 100);
  await cmd(page, 'pause', true);
}

async function degenerateState(page) {
  // ---- nonsense parameters are sanitized, not propagated
  for (const [name, arg] of [['splatScale', 1e30], ['splatScale', NaN], ['splatScale', -5], ['fov', NaN], ['fov', 0], ['fov', 1e9], ['maxRadius', NaN], ['maxRadius', -1], ['renderScale', NaN], ['renderScale', 0], ['clipY', Infinity], ['clipY', NaN], ['autoRotate', NaN], ['view', [NaN, 0, 0, 0, 0, 0]], ['view', [1, 1, 1, 1, 1, 1]], ['view', [0, 1e30, 0, 0, 0, 0]], ['view', [0, 5, 0, 0, 0, 0]]]) {
    await cmd(page, name, arg);
    await page.waitForTimeout(40);
  }
  let s = await settle(page);
  check('robust: nonsense parameters leave a finite camera and a valid canvas', s.view.every(Number.isFinite) && s.eye.every(Number.isFinite) && s.canvasWidth >= 1 && s.canvasHeight >= 1 && s.canvasWidth <= 8192, `canvas ${s.canvasWidth}x${s.canvasHeight}, eye ${JSON.stringify(s.eye)}`);
  // Back to sane values.
  await cmd(page, 'splatScale', 1);
  await cmd(page, 'fov', (50 * Math.PI) / 180);
  await cmd(page, 'maxRadius', 1024);
  await cmd(page, 'renderScale', 1);
  await cmd(page, 'clipY', null);
  await cmd(page, 'view', [0.6, 1.5, 4.0, 0, 1, 0]);
  await page.waitForTimeout(300);
  s = await state(page);
  const sane = await shot(page, 'robust-2-sane-again.png');
  let img = await imageStats(page, sane);
  check('robust: the viewer recovers from nonsense parameters', hasContent(img.left) && hasContent(img.right) && s.canvasWidth === 960 && s.canvasHeight === 600, `canvas ${s.canvasWidth}x${s.canvasHeight}`);

  // ---- rapid configuration switching, no waiting in between
  const frames0 = s.framesRendered;
  await page.evaluate(() => {
    const h = window.__harness;
    for (let i = 0; i < 300; i++) {
      h.cmd('points', i % 2 === 0);
      h.cmd('showStatic', i % 3 !== 0);
      h.cmd('showDynamic', i % 5 !== 0);
      h.cmd('clipY', i % 7 === 0 ? 0.5 + (i % 4) : null);
      h.cmd('nav', i % 2 ? 'fly' : 'orbit');
      h.cmd('gizmos', i % 4 < 2);
      h.cmd('splatScale', 0.25 + (i % 8) / 4);
      h.cmd('renderScale', i % 9 === 0 ? 0.5 : 1);
      if (i % 11 === 0) h.cmd('emptyDynamic');
      if (i % 50 === 49) h.cmd('clear');
      h.state();
    }
    for (const [n, a] of [['points', false], ['showStatic', true], ['showDynamic', true], ['clipY', null], ['nav', 'orbit'], ['gizmos', true], ['splatScale', 1], ['renderScale', 1]]) h.cmd(n, a);
  });
  // Everything above happened between two frames: one redraw shows the result.
  await page.waitForFunction((n) => window.__harness.state().framesRendered > n, frames0);
  s = await state(page);
  check('robust: 300 rapid configuration changes (including clear) keep the viewer alive', s.splatsTotal === 0 && s.framesRendered > frames0, `total ${s.splatsTotal}`);

  // ---- no splats at all: just the background, no stale order drawn
  await cmd(page, 'gizmos', false);
  await page.evaluate(() => { document.getElementById('hud').style.display = 'none'; });
  await page.waitForTimeout(250);
  const empty = await shot(page, 'robust-3-empty.png');
  img = await imageStats(page, empty);
  s = await state(page);
  check('robust: empty layers draw only the background', s.splatsDrawn === 0 && img.right.colors === 1 && img.left.colors === 1, `drawn ${s.splatsDrawn}, colors L ${img.left.colors} R ${img.right.colors}`);
  await page.evaluate(() => { document.getElementById('hud').style.display = ''; });
  await cmd(page, 'gizmos', true);

  // ---- the data comes back through the normal path
  await cmd(page, 'pause', false);
  await page.waitForFunction(() => window.__harness.state().dynamicSplats > 100);
  s = await state(page);
  check('robust: the dynamic layer refills after a clear', s.dynamicSplats > 100 && s.staticSplats === 0);

  // ---- a canvas without size
  const sizes = [];
  for (const css of ['display:none', 'width:0px', 'height:0px', 'width:1px;height:1px']) {
    await page.evaluate((css) => document.getElementById('gl').setAttribute('style', css), css);
    await page.waitForTimeout(250);
    const t = await state(page);
    sizes.push(`${css} -> ${await page.evaluate(() => { const c = document.getElementById('gl'); return c.width + 'x' + c.height; })}`);
    if (!(t.frames > s.frames)) sizes.push('LOOP STOPPED');
    s = t;
  }
  const hiddenRendered = s.framesRendered;
  await page.evaluate(() => document.getElementById('gl').setAttribute('style', 'display:none'));
  await page.waitForTimeout(400);
  const hidden = await state(page);
  await page.evaluate(() => document.getElementById('gl').removeAttribute('style'));
  await page.waitForFunction(() => { const s = window.__harness.state(); return s.canvasWidth === 960 && s.canvasHeight === 600; }, null, { timeout: 10000 });
  await page.waitForTimeout(300);
  s = await state(page);
  const back = await shot(page, 'robust-4-after-zero-size.png');
  img = await imageStats(page, back);
  check('robust: a canvas without size is not drawn to and comes back intact', hidden.framesRendered - hiddenRendered <= 1 && !sizes.includes('LOOP STOPPED') && s.canvasWidth === 960 && s.canvasHeight === 600 && img.left.colors > 20 && img.right.colors > 20,
    `${sizes.join(', ')}; frames rendered while hidden ${hidden.framesRendered - hiddenRendered}`);
}

// High-DPI display, idle auto-rotation, and a canvas that has no CSS size.
async function layout(browser, url) {
  console.log('\n=== robust: device pixel ratio 3, auto-rotate, unsized canvas ===');
  const { ctx, page, logs } = await newPage(browser, { width: 300, height: 200, deviceScaleFactor: 3 });
  await page.goto(`${url}/?test=live&static_cams=3&capture_scale=0.35&autorotate=1`);
  await waitLive(page);
  let s = await state(page);
  check('robust: the device pixel ratio is capped at 2', s.canvasWidth === 600 && s.canvasHeight === 400, `${s.canvasWidth}x${s.canvasHeight} for 300x200 CSS pixels at DPR 3`);

  // Orbit auto-rotation starts after a few idle seconds and keeps the target.
  await page.waitForTimeout(4500);
  const a = await state(page);
  await page.waitForTimeout(700);
  const b = await state(page);
  const radius = (t) => Math.hypot(t.eye[0] - t.target[0], t.eye[2] - t.target[2]);
  const angle = Math.atan2(b.eye[0] - b.target[0], b.eye[2] - b.target[2]) - Math.atan2(a.eye[0] - a.target[0], a.eye[2] - a.target[2]);
  check('robust: idle auto-rotation turns the camera around a fixed target', Math.abs(angle) > 0.05 && Math.abs(angle) < 0.6 && Math.abs(radius(a) - radius(b)) < 1e-3 && Math.abs(a.eye[1] - b.eye[1]) < 1e-4 && a.target.every((v, i) => Math.abs(v - b.target[i]) < 1e-5),
    `${angle.toFixed(3)} rad in 0.7 s at radius ${radius(b).toFixed(3)}`);
  await cmd(page, 'autoRotate', null);

  // Without a CSS size the element is as large as its drawing buffer, and a
  // buffer of 2x the element size would grow without bound.
  await page.evaluate(() => document.getElementById('gl').setAttribute('style', 'position:static;width:auto;height:auto'));
  const measure = () => page.evaluate(() => { const c = document.getElementById('gl'); return [c.width, c.height, c.clientWidth, c.clientHeight]; });
  await page.waitForTimeout(500);
  const first = await measure();
  await page.waitForTimeout(700);
  const later = await measure();
  check('robust: a canvas without CSS size does not grow without bound', JSON.stringify(first) === JSON.stringify(later) && later[0] === 2 * later[2] && later[1] === 2 * later[3] && later[0] <= 1200,
    `buffer ${later[0]}x${later[1]}, element ${later[2]}x${later[3]} CSS px; warned: ${logs.some((l) => /no CSS size/.test(l))}`);
  const errs = consoleErrors(logs);
  check('robust: no console errors in the layout checks', errs.length === 0, errs.slice(0, 3).join(' | '));
  await ctx.close();
}

// PSNR of one eye of a stereo screenshot against the CPU reference image.
async function eyePsnr(page, file, eye) {
  const b64 = fs.readFileSync(file).toString('base64');
  return page.evaluate(
    async ([b64, eye]) => {
      const ref = window.__harness.xrReference(eye);
      if (!ref) return null;
      const img = new Image();
      img.src = 'data:image/png;base64,' + b64;
      await img.decode();
      const c = document.createElement('canvas');
      c.width = img.width;
      c.height = img.height;
      const ctx = c.getContext('2d', { willReadFrequently: true });
      ctx.drawImage(img, 0, 0);
      const [x, y, w, h] = ref.viewport;
      // GL viewports count from the bottom, images from the top.
      const d = ctx.getImageData(x, img.height - y - h, w, h).data;
      let se = 0, bright = 0;
      for (let i = 0, j = 0; i < w * h; i++, j += 4) {
        for (let k = 0; k < 3; k++) {
          const e = d[j + k] - ref.rgb[i * 3 + k];
          se += e * e;
        }
        if (ref.rgb[i * 3] > 40 || ref.rgb[i * 3 + 1] > 40 || ref.rgb[i * 3 + 2] > 50) bright++;
      }
      const mse = se / (w * h * 3);
      return { psnr: mse === 0 ? 99 : 10 * Math.log10((255 * 255) / mse), width: w, height: h, brightFrac: bright / (w * h) };
    },
    [b64, eye],
  );
}

async function stereoAgainstReference(page, label, file) {
  // A still picture: no ticks, no gizmos, hands behind the head, no HUD.
  const f = await shot(page, file);
  const eyes = [await eyePsnr(page, f, 0), await eyePsnr(page, f, 1)];
  const xr = (await state(page)).xr;
  const p = xr.views.map((v) => v.projection);
  check(`robust: ${label}: both eyes match the CPU reference (PSNR >= 35 dB)`, eyes.every((e) => e && e.psnr >= 35 && e.brightFrac > 0.02),
    eyes.map((e, i) => (e ? `eye ${i}: ${e.psnr.toFixed(2)} dB, ${e.width}x${e.height}, principal point offset (${p[i][8].toFixed(2)}, ${p[i][9].toFixed(2)})` : 'no reference')).join('; '));
  return eyes;
}

async function xrStress(browser, url) {
  if (!iwerBundle()) {
    check('robust: IWER emulator bundle found', false, 'run `npm install` in e2e/render, or set GS_NPM_TOOLS / IWER_BUNDLE');
    return;
  }
  console.log('\n=== robust: WebXR (IWER, off-axis per-eye projections) ===');
  const { ctx, page, logs } = await newPage(browser, { width: 1280, height: 640, iwer: true });
  // Real headsets have off-axis frusta (the nose side is narrower) and
  // different focal lengths per axis; the emulator's are symmetric. Patch the
  // matrices the page receives.
  await page.addInitScript(() => {
    const original = Object.getOwnPropertyDescriptor(XRView.prototype, 'projectionMatrix').get;
    Object.defineProperty(XRView.prototype, 'projectionMatrix', {
      get() {
        const m = new Float32Array(original.call(this));
        if (window.__offAxis !== false) {
          m[0] *= 1.12;
          m[5] *= 0.94;
          m[8] = this.eye === 'left' ? 0.21 : -0.17;
          m[9] = 0.08;
        }
        return m;
      },
    });
  });
  await page.goto(`${url}/?test=live&static_cams=4&capture_scale=0.5`);
  await waitLive(page);
  await page.waitForFunction(() => window.__harness.state().xrSupported === true);
  const active = () => page.waitForFunction(() => { const s = window.__harness.state(); return s.xrActive && s.xr && s.xr.frames > 3 && s.xr.views.length === 2; }, null, { timeout: 30000 });
  const inactive = () => page.waitForFunction(() => !window.__harness.state().xrActive, null, { timeout: 10000 });
  const frames = async (n) => {
    const start = (await state(page)).xr.frames;
    await page.waitForFunction((t) => { const s = window.__harness.state(); return s.xr && s.xr.frames >= t; }, start + n, { timeout: 30000 });
  };

  // ---- stereo rendering against the CPU rasterizer
  await cmd(page, 'pause', true);
  await cmd(page, 'gizmos', false);
  await page.evaluate(() => {
    document.getElementById('hud').style.display = 'none';
    xrdev.position.set(0.25, 1.45, 0.3);
    const yaw = -0.15, pitch = -0.2;
    xrdev.quaternion.set(Math.sin(pitch / 2) * Math.cos(yaw / 2), Math.cos(pitch / 2) * Math.sin(yaw / 2), -Math.sin(pitch / 2) * Math.sin(yaw / 2), Math.cos(pitch / 2) * Math.cos(yaw / 2));
    // Behind the head: the controller gizmos are not part of the reference.
    xrdev.controllers.left.position.set(-0.2, 1.2, 3);
    xrdev.controllers.right.position.set(0.2, 1.2, 3);
  });
  await cmd(page, 'enterXr');
  await active();
  await frames(4);
  let s = await state(page);
  const p = s.xr.views.map((v) => v.projection);
  check('robust: the session delivers off-axis projections that differ per eye', Math.abs(p[0][8] - 0.21) < 1e-6 && Math.abs(p[1][8] + 0.17) < 1e-6 && Math.abs(p[0][9] - 0.08) < 1e-6, `P[2][0] = ${p[0][8].toFixed(3)} / ${p[1][8].toFixed(3)}`);
  await stereoAgainstReference(page, 'life-size stereo, off-axis frusta', 'robust-5-xr-off-axis.png');
  await cmd(page, 'miniature', true);
  await frames(4);
  await stereoAgainstReference(page, 'miniature (world scale 15) stereo, off-axis frusta', 'robust-6-xr-off-axis-miniature.png');
  await cmd(page, 'miniature', false);

  // ---- hostile and valid messages while the session runs
  await frames(2);
  const r1 = await apply(page, Buffer.from('GSL1 garbage that is not a message'));
  const r2 = await apply(page, layer({ id: 1, mode: 0, records: [record(), record([10, 20, 30])] }));
  await frames(3);
  s = await state(page);
  check('robust: messages are applied (and bad ones rejected) during a VR session', r1.startsWith('error: ') && r2 === 'layer 1: 2 splats, 2 total' && s.dynamicSplats === 2 && s.xrActive, `${r1} / ${r2}`);

  // ---- context loss inside the session
  await page.evaluate(() => {
    window.__lose = document.getElementById('gl').getContext('webgl2').getExtension('WEBGL_lose_context');
    window.__lose.loseContext();
  });
  await page.waitForTimeout(400);
  const lostFrames = (await state(page)).xr;
  await page.evaluate(() => window.__lose.restoreContext());
  await page.waitForFunction(() => !document.getElementById('gl').getContext('webgl2').isContextLost(), null, { timeout: 10000 });
  await page.waitForTimeout(300);
  if ((await state(page)).xrActive) {
    await frames(4);
    await stereoAgainstReference(page, 'after a context loss inside the session', 'robust-7-xr-after-context-loss.png');
  } else {
    check('robust: the session survives a WebGL context loss or ends cleanly', (await state(page)).xr === null, `frames before the loss ${lostFrames && lostFrames.frames}`);
  }
  await cmd(page, 'exitXr');
  await inactive();
  await page.evaluate(() => { document.getElementById('hud').style.display = ''; });

  // ---- sessions entered and left repeatedly
  let changes = (await state(page)).xrChanges.length;
  for (let i = 0; i < 5; i++) {
    await cmd(page, 'enterXr');
    // A second request while the first is pending must be ignored.
    await cmd(page, 'enterXr');
    await active();
    await cmd(page, 'exitXr');
    await cmd(page, 'exitXr');
    await inactive();
  }
  s = await state(page);
  let added = s.xrChanges.slice(changes);
  check('robust: five sessions in a row, each notified once', JSON.stringify(added) === JSON.stringify([true, false, true, false, true, false, true, false, true, false]) && s.xrError === null, JSON.stringify(added));

  // ---- a start cancelled before the session exists
  changes = s.xrChanges.length;
  await page.evaluate(() => { window.__harness.cmd('enterXr'); window.__harness.cmd('exitXr'); });
  await page.waitForTimeout(800);
  s = await state(page);
  const leftover = await page.evaluate(() => !!xrdev.activeSession);
  check('robust: exit_xr during the start cancels the session silently', !s.xrActive && !leftover && s.xrChanges.length === changes && s.xrError === null, `active ${s.xrActive}, runtime session ${leftover}, changes ${JSON.stringify(s.xrChanges.slice(changes))}, error ${s.xrError}`);
  // The viewer is not stuck: the next request works.
  await cmd(page, 'enterXr');
  await active();
  await cmd(page, 'exitXr');
  await inactive();
  const rendered = (await state(page)).framesRendered;
  await cmd(page, 'pause', false);
  await cmd(page, 'gizmos', true);
  await page.waitForFunction((n) => window.__harness.state().framesRendered > n + 2, rendered);
  await page.waitForTimeout(300);
  const f = await shot(page, 'robust-8-desktop-after-sessions.png');
  const img = await imageStats(page, f);
  const canvas = await page.evaluate(() => { const c = document.getElementById('gl'); return { w: c.width, h: c.height }; });
  check('robust: the desktop view is back after all sessions', canvas.w === 1280 && canvas.h === 640 && hasContent(img.left) && hasContent(img.right), JSON.stringify(canvas));

  // ---- fly mode: the space bar moves up, but not when it activates a button
  await cmd(page, 'nav', 'fly');
  await cmd(page, 'pause', true);
  let eye0 = (await settle(page)).eye;
  await page.focus('#enter-vr');
  await page.keyboard.press('Space');
  await active();
  await cmd(page, 'exitXr');
  await inactive();
  s = await settle(page);
  const stayed = Math.abs(s.eye[1] - eye0[1]) < 1e-6;
  await page.evaluate(() => document.activeElement.blur());
  await page.keyboard.down('Space');
  await page.waitForTimeout(400);
  await page.keyboard.up('Space');
  s = await settle(page);
  check('robust: Space on a focused button presses the button; elsewhere it flies up', stayed && s.eye[1] > eye0[1] + 0.1, `eye y ${eye0[1].toFixed(3)} -> ${s.eye[1].toFixed(3)}, unchanged by the button press: ${stayed}`);
  await cmd(page, 'nav', 'orbit');
  await cmd(page, 'pause', false);

  // ---- destroyed while a session is starting
  changes = (await state(page)).xrChanges.length;
  await page.evaluate(() => { window.__harness.cmd('enterXr'); window.__harness.cmd('destroy'); });
  await page.waitForTimeout(800);
  s = await state(page);
  const orphan = await page.evaluate(() => !!xrdev.activeSession);
  check('robust: destroying the viewer while a session starts leaves no session and notifies nobody', !s.xrActive && !orphan && s.xrChanges.length === changes, `runtime session ${orphan}, changes ${JSON.stringify(s.xrChanges.slice(changes))}`);

  fs.writeFileSync(path.join(OUT, 'console-robust-xr.log'), logs.join('\n'));
  // While the context is lost the emulator's own frame loop throws (it reads
  // gl.getParameter(COLOR_CLEAR_VALUE)[0], which is null then); that is its
  // exception, not the viewer's, and a real runtime has no such code.
  const errs = consoleErrors(logs).filter((l) => !/thrown inside the IWER emulator/.test(l));
  check('robust: exceptions during the context loss come from the emulator only', logs.filter((l) => l.startsWith('[pageerror]')).every((l) => /thrown inside the IWER emulator/.test(l)), logs.filter((l) => l.startsWith('[pageerror]')).slice(0, 2).join(' | '));
  check('robust: no console errors or panics in the VR stress run', errs.length === 0, errs.join(' | '));
  await ctx.close();
}

// The only viewer handle is dropped inside the viewer's own on_xr_change
// callback, when the session starts or when the runtime ends it.
async function dropInCallback(browser, url, when) {
  const { ctx, page, logs } = await newPage(browser, { width: 640, height: 320, iwer: true });
  // Counts every animation-frame request of the page (viewer and emulator).
  await page.addInitScript(() => {
    window.__raf = 0;
    const original = window.requestAnimationFrame.bind(window);
    window.requestAnimationFrame = (callback) => { window.__raf++; return original(callback); };
  });
  await page.goto(`${url}/?test=drop&when=${when}`);
  await page.waitForFunction(() => typeof window.__enter === 'function' && window.__raf > 5);
  await page.evaluate(() => window.__enter());
  await page.waitForFunction(() => window.__events.length >= 1 && window.__events[0] === true, null, { timeout: 30000 });
  if (when === 'end') {
    await page.waitForTimeout(300);
    const running = await page.evaluate(() => !!xrdev.activeSession && !window.__dropped);
    await page.evaluate(() => xrdev.activeSession.end());
    check(`robust: drop on ${when}: the session ran until the runtime ended it`, running);
  }
  await page.waitForFunction(() => window.__dropped === true, null, { timeout: 10000 });
  await page.waitForTimeout(500);
  const raf0 = await page.evaluate(() => window.__raf);
  await page.waitForTimeout(500);
  const end = await page.evaluate(() => ({ raf: window.__raf, session: !!xrdev.activeSession, events: [...window.__events] }));
  // A destroyed viewer has removed its listeners: input must not reach freed closures.
  await page.mouse.move(300, 160);
  await page.mouse.down();
  await page.mouse.move(360, 200, { steps: 4 });
  await page.mouse.up();
  await page.mouse.wheel(0, 200);
  await page.keyboard.press('KeyW');
  await page.waitForTimeout(200);
  const errs = consoleErrors(logs);
  check(`robust: drop on ${when}: the viewer shuts down (no more frames, no session), listeners told [true,false], nothing crashes`, end.raf === raf0 && !end.session && JSON.stringify(end.events) === '[true,false]' && errs.length === 0,
    `frame requests ${raf0} -> ${end.raf}, session ${end.session}, events ${JSON.stringify(end.events)}${errs.length ? ', errors: ' + errs.slice(0, 2).join(' | ') : ''}`);
  await ctx.close();
}

async function run({ browser, url }) {
  console.log('\n=== robust: hostile input and degenerate state ===');
  const { ctx, page, logs } = await newPage(browser, { width: 960, height: 600 });
  await page.goto(`${url}/?test=live&static_cams=4&capture_scale=0.5`);
  await waitLive(page);
  await hostileMessages(page);
  await degenerateState(page);
  fs.writeFileSync(path.join(OUT, 'console-robust.log'), logs.join('\n'));
  const errs = consoleErrors(logs);
  check('robust: no console errors or panics under hostile input', errs.length === 0, errs.slice(0, 5).join(' | '));
  await ctx.close();
  await layout(browser, url);
  await xrStress(browser, url);
  if (iwerBundle()) {
    console.log('\n=== robust: last handle dropped inside a viewer callback ===');
    await dropInCallback(browser, url, 'start');
    await dropInCallback(browser, url, 'end');
  }
}

module.exports = { run };
if (require.main === module) main(run);
