// The blending modes on the product's own stage cloud (?test=scene): what
// the viewer reports, the fallback without float render targets (missing
// extensions, or targets that cannot be allocated), gizmo lines against the
// splat surface, order popping, resize and context loss with the off-screen
// targets.
//   node e2e/render/modes.cjs        (BUILD=1 to rebuild the harness first)
const fs = require('fs');
const path = require('path');
const { OUT, check, newPage, consoleErrors, shot, imageStats, hasContent, main } = require('./lib.cjs');

// The light pipeline preset: this suite is about the viewer, and the cloud
// is built in WASM in every page.
const PAGE = '?test=scene&scene=stage&light=1';
const VIEW = [3.0, 1.2, 4.5, 0, 0.8, 0];

const state = (page) => page.evaluate(() => window.__scene.state());
const cmd = (page, name, arg) => page.evaluate(([n, a]) => window.__scene.cmd(n, a), [name, arg]);

async function open(browser, url, extra = '') {
  const { ctx, page, logs } = await newPage(browser, { width: 960, height: 540 });
  await page.goto(`${url}/${PAGE}${extra}`);
  await page.waitForFunction(() => window.__ready || window.__error, null, { timeout: 300000 });
  const error = await page.evaluate(() => window.__error || null);
  if (error) throw new Error(`scene page failed: ${error}`);
  await page.addStyleTag({ content: '#hud{display:none}' });
  await cmd(page, 'view', VIEW);
  await drawn(page);
  return { ctx, page, logs };
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

// The view turned `degrees` around the orbit target.
function orbit(degrees) {
  const [ex, ey, ez, tx, ty, tz] = VIEW;
  const a = (degrees * Math.PI) / 180;
  const [dx, dz] = [ex - tx, ez - tz];
  return [tx + dx * Math.cos(a) + dz * Math.sin(a), ey, tz - dx * Math.sin(a) + dz * Math.cos(a), tx, ty, tz];
}

async function setBlend(page, blend, quality = 'soft') {
  await cmd(page, 'blend', blend);
  await cmd(page, 'quality', quality);
  await drawn(page);
}

// Pixels of a screenshot that have the color of the +Y axis gizmo (a bright
// green nothing in the stage scene has).
async function axisPixels(page, file) {
  const b64 = fs.readFileSync(file).toString('base64');
  return page.evaluate(async (b64) => {
    const img = new Image();
    img.src = 'data:image/png;base64,' + b64;
    await img.decode();
    const c = document.createElement('canvas');
    c.width = img.width;
    c.height = img.height;
    const ctx = c.getContext('2d', { willReadFrequently: true });
    ctx.drawImage(img, 0, 0);
    const d = ctx.getImageData(0, 0, img.width, img.height).data;
    let n = 0;
    for (let i = 0; i < d.length; i += 4) if (d[i + 1] > 200 && d[i] < 150 && d[i + 2] < 150 && d[i + 1] - d[i] > 80) n++;
    return n;
  }, b64);
}

async function fallback(browser, url) {
  // A GPU without EXT_color_buffer_half_float / EXT_color_buffer_float.
  const { ctx, page, logs } = await open(browser, url, '&mask_float=1');
  let s = await state(page);
  const fMasked = await shot(page, 'modes-0-no-float-targets.png');
  const img = await imageStats(page, fMasked);
  check('modes: without the float-target extensions the viewer falls back to sorted and says why', s.blend === 'sorted' && s.blendReason === 'no-float-targets' && s.blendFallback === true && s.offscreen[0] === 0 && s.sorts > 0,
    `${s.blend} (${s.blendReason}), fallback ${s.blendFallback}, ${s.sorts} sorts`);
  check('modes: the fallback draws the scene', hasContent(img.left) && hasContent(img.right) && s.splatsDrawn === s.splatsTotal, `${s.splatsDrawn} of ${s.splatsTotal} splats drawn`);
  check('modes: the fallback is announced once in the console', logs.filter((l) => /falling back to sorted blending/.test(l)).length === 1, logs.filter((l) => /falling back/.test(l)).join(' | '));
  await setBlend(page, 'normalized', 'hard');
  s = await state(page);
  check('modes: forcing the normalised mode there still reports the fallback', s.blend === 'sorted' && s.blendReason === 'no-float-targets' && s.blendFallback === true, `${s.blend} (${s.blendReason})`);
  await setBlend(page, 'sorted');
  s = await state(page);
  check('modes: sorted on request is not a fallback', s.blend === 'sorted' && s.blendReason === 'requested' && s.blendFallback === false, `${s.blend} (${s.blendReason})`);
  const errs = consoleErrors(logs);
  check('modes: no console errors on a GPU without float targets', errs.length === 0, errs.join(' | '));
  await ctx.close();
  return fMasked;
}

async function run({ browser, url }) {
  console.log('\n=== modes: blending modes on the stage cloud ===');
  fs.mkdirSync(OUT, { recursive: true });
  const fMasked = await fallback(browser, url);

  const { ctx, page, logs } = await open(browser, url);
  let s = await state(page);
  check('modes: the stage is drawn normalised (soft) by default', s.blend === 'normalized-soft' && s.blendReason === 'surfel-content' && !s.blendFallback && s.offscreen[0] === 960 && s.offscreen[1] === 540 && s.sorts === 0,
    `${s.blend} (${s.blendReason}), off-screen targets ${s.offscreen.join('x')}, ${s.sorts} sorts, ${s.splatsTotal} splats`);
  const fSoft = await shot(page, 'modes-1-soft.png');

  // ---- the three renderings; the forced sorted one equals the fallback
  await setBlend(page, 'normalized', 'hard');
  const hard = await state(page);
  const fHard = await shot(page, 'modes-2-hard.png');
  let img = await imageStats(page, fHard, fSoft);
  check('modes: hard and soft silhouettes differ only along the rims', hard.blend === 'normalized-hard' && img.diffMeanAbs > 0 && img.diffChangedFrac < 0.05, `${(img.diffChangedFrac * 100).toFixed(2)} % of the pixels differ`);
  await setBlend(page, 'sorted');
  const sorted = await state(page);
  const fSorted = await shot(page, 'modes-3-sorted.png');
  img = await imageStats(page, fSorted, fMasked);
  check('modes: the fallback picture is the sorted picture', sorted.blend === 'sorted' && sorted.offscreen[0] === 0 && img.diffMeanAbs === 0, `mean abs diff ${img.diffMeanAbs}`);

  // ---- no order, no popping
  const sortsBefore = sorted.sorts;
  const popSorted = await page.evaluate(() => window.__scene.popping(24, 0.25));
  const sortsAfter = (await state(page)).sorts;
  // A sorted picture depends on when the order was last computed: the
  // viewer keeps an order while the view direction stays within a quarter of
  // a degree (and, for large scenes, for a frame after a slow sort). Reach
  // the same pose twice, once with the order of a view 0.2 degrees away:
  // any difference is a pop a viewer sees when the order catches up. The
  // normalised mode has no order, so its two pictures must be identical.
  const samePoseTwice = async (name) => {
    await cmd(page, 'view', orbit(-0.2));
    await drawn(page);
    await cmd(page, 'view', orbit(0));
    await drawn(page);
    const a = await shot(page, `modes-pop-${name}-a.png`);
    await cmd(page, 'view', orbit(0.4));
    await drawn(page);
    await cmd(page, 'view', orbit(0.2));
    await drawn(page);
    await cmd(page, 'view', orbit(0));
    await drawn(page);
    const b = await shot(page, `modes-pop-${name}-b.png`);
    return imageStats(page, b, a);
  };
  const historySorted = await samePoseTwice('sorted');
  const sortsSorted = (await state(page)).sorts;
  await setBlend(page, 'auto');
  const popSoft = await page.evaluate(() => window.__scene.popping(24, 0.25));
  const historySoft = await samePoseTwice('normalized');
  s = await state(page);
  check('modes: a turning camera re-sorts in the sorted mode only', sortsAfter >= sortsBefore + 20 && s.sorts === sortsSorted, `sorted: +${sortsAfter - sortsBefore} sorts for 24 views; normalised: +${s.sorts - sortsSorted}`);
  check('modes: a normalised picture depends on the pose alone (no order to catch up, no popping)', historySoft.diffMeanAbs === 0,
    `the same pose reached from two sides: normalised mean abs diff ${historySoft.diffMeanAbs}; sorted, drawn once with the order of a view 0.2 degrees away: ${historySorted.diffMeanAbs.toFixed(5)}`);
  // For the record: how much two views a quarter of a degree apart differ.
  // Not a check: a crisper picture changes more per pixel of motion.
  for (const p of [popSorted, popSoft]) {
    console.log(`      ${p.blend}: turning 0.25 degrees changes a channel by ${p.meanAbsDiff.toFixed(3)} levels on average, ${(100 * p.jumpFraction8).toFixed(2)} % of the pixels by more than 8, ${(100 * p.jumpFraction32).toFixed(3)} % by more than 32`);
  }
  await cmd(page, 'view', VIEW);

  // ---- gizmo lines: on top of sorted splats, hidden behind normalised ones
  // From below the stage, both faces drawn: the floor is in front of the +Y
  // axis (all but its foot, which is within the depth tolerance of the
  // floor), while the grid and the other two axes lie on it.
  await cmd(page, 'cullBackfaces', false);
  await cmd(page, 'view', [2.0, -2.0, 3.0, 0, 0.2, 0]);
  await drawn(page);
  const fBelow = await shot(page, 'modes-4a-below-no-gizmos.png');
  await cmd(page, 'gizmos', true);
  await drawn(page);
  const fGizmoSoft = await shot(page, 'modes-4-gizmos-normalized.png');
  await setBlend(page, 'sorted');
  const fGizmoSorted = await shot(page, 'modes-5-gizmos-sorted.png');
  const [axisSoft, axisSorted, axisNone] = [await axisPixels(page, fGizmoSoft), await axisPixels(page, fGizmoSorted), await axisPixels(page, fBelow)];
  img = await imageStats(page, fGizmoSoft, fBelow);
  check('modes: gizmo lines are drawn over sorted splats and hidden behind the normalised surface', axisNone === 0 && axisSorted > 40 && axisSoft <= 0.2 * axisSorted && img.diffChangedFrac > 0.002,
    `pixels of the +Y axis seen through the floor: ${axisSorted} sorted, ${axisSoft} normalised, ${axisNone} without gizmos; the grid lying on the floor still changes ${(img.diffChangedFrac * 100).toFixed(2)} % of the normalised picture`);
  await cmd(page, 'gizmos', false);
  await cmd(page, 'cullBackfaces', true);
  await cmd(page, 'view', VIEW);
  await setBlend(page, 'auto');

  // ---- resize: the off-screen targets follow the canvas
  await page.setViewportSize({ width: 800, height: 500 });
  await page.waitForFunction(() => { const t = window.__scene.state(); return t.canvasWidth === 800 && t.offscreen[0] === 800 && t.offscreen[1] === 500; }, null, { timeout: 20000 }).catch(() => {});
  s = await state(page);
  check('modes: the off-screen targets follow a resize', s.canvasWidth === 800 && s.canvasHeight === 500 && s.offscreen[0] === 800 && s.offscreen[1] === 500 && s.blend === 'normalized-soft', `canvas ${s.canvasWidth}x${s.canvasHeight}, targets ${s.offscreen.join('x')}`);
  await page.setViewportSize({ width: 960, height: 540 });
  await page.waitForFunction(() => window.__scene.state().offscreen[0] === 960, null, { timeout: 20000 }).catch(() => {});
  await drawn(page);
  const fResized = await shot(page, 'modes-6-after-resize.png');
  img = await imageStats(page, fResized, fSoft);
  check('modes: the picture is the same after resizing back', img.diffMeanAbs === 0, `mean abs diff ${img.diffMeanAbs}`);

  // ---- context loss: programs and targets of the normalised mode come back
  await page.evaluate(() => {
    window.__lose = document.getElementById('gl').getContext('webgl2').getExtension('WEBGL_lose_context');
    window.__lose.loseContext();
  });
  await page.waitForTimeout(300);
  await page.evaluate(() => window.__lose.restoreContext());
  await page.waitForFunction(() => !document.getElementById('gl').getContext('webgl2').isContextLost(), null, { timeout: 10000 });
  await drawn(page);
  s = await state(page);
  const fRestored = await shot(page, 'modes-7-after-context-restore.png');
  img = await imageStats(page, fRestored, fSoft);
  check('modes: after a context loss the normalised picture comes back', s.blend === 'normalized-soft' && !s.blendFallback && s.offscreen[0] === 960 && img.diffMeanAbs === 0, `${s.blend}, targets ${s.offscreen.join('x')}, mean abs diff to before ${img.diffMeanAbs}`);

  // ---- render targets that cannot be created: fall back at run time
  await page.evaluate(() => {
    // From now on every new framebuffer is "incomplete", as when the GPU
    // runs out of memory for the float targets.
    WebGL2RenderingContext.prototype.checkFramebufferStatus = () => 0x8cd6;
  });
  await page.setViewportSize({ width: 900, height: 520 });
  await page.waitForFunction(() => window.__scene.state().canvasWidth === 900, null, { timeout: 20000 }).catch(() => {});
  await drawn(page);
  s = await state(page);
  const fFailed = await shot(page, 'modes-8-targets-failed.png');
  img = await imageStats(page, fFailed);
  check('modes: when the targets cannot be allocated the viewer falls back to sorted and keeps drawing', s.blend === 'sorted' && s.blendReason === 'targets-failed' && s.blendFallback === true && s.sorts > 0 && hasContent(img.left) && hasContent(img.right),
    `${s.blend} (${s.blendReason}), ${s.sorts} sorts`);

  fs.writeFileSync(path.join(OUT, 'console-modes.log'), logs.join('\n'));
  const errs = consoleErrors(logs);
  check('modes: no console errors or panics', errs.length === 0, errs.join(' | '));
  await ctx.close();
}

module.exports = { run };
if (require.main === module) main(run);
