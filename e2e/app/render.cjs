// Rendering and capture options with the application's own defaults (the
// other suites pin the blending to "sorted", see lib.cjs): the blending
// choice and what the viewer answers, the splat size that follows scene and
// mode, the fallback notice, the stored choice, the headset default, the
// scan cell presets, and what a browser without server gets.
const { check, iwerBundle, newPage, consoleErrors, state, waitState, open, shot, viewShot, diff, content, setSlider } = require('./lib.cjs');

const scaleText = (page) => page.locator('output[for=splat-scale]').innerText();
const blendIs = (page, active) => waitState(page, (s, active) => s.viewer.blend === active, 20000, active);

async function defaults(browser, url) {
  const { ctx, page, logs } = await newPage(browser, { blend: null });
  let s = await open(page, url);
  // What a laptop, a phone or a standalone headset gets without a server.
  check('render: the worker starts on the light preset', s.source === 'worker' && s.config.cameras === 8 && s.config.width === 160 && s.config.height === 120 && s.config.targetHz === 20 && s.config.fusion && Math.abs(s.config.voxelSize - 0.032) < 1e-6, JSON.stringify(s.config));
  check('render: surfels are drawn normalised with soft silhouettes by default', s.display.blend === 'auto' && s.viewer.blend === 'normalized-soft' && s.viewer.blendReason === 'surfel-content' && !s.viewer.blendFallback, `${s.viewer.blend} (${s.viewer.blendReason})`);
  const status = await page.locator('#blend-status').innerText();
  check('render: the panel names the active blending and shows no fallback', /normalisé, contours doux/.test(status) && (await page.locator('#blend-fallback').count()) === 0 && (await page.locator('#blend-0').getAttribute('aria-checked')) === 'true', status);
  check('render: the normalised mode never sorts', s.viewer.sorts === 0 && s.viewer.sortMs === 0, `sorts ${s.viewer.sorts}`);
  check('render: stage splat size is 1 in the normalised mode', s.display.splatScale === 1 && (await scaleText(page)) === '× 1,00', await scaleText(page));

  // The picture differs between the levels and the sorted fallback.
  await page.click('#btn-pause');
  await waitState(page, (s) => s.config.paused);
  const soft = await viewShot(page, 'render-stage-soft.png');
  await page.click('#blend-2');
  s = await blendIs(page, 'normalized-hard');
  const hard = await viewShot(page, 'render-stage-hard.png');
  check('render: "Net" draws hard silhouettes', s.display.blend === 'hard' && s.viewer.blendReason === 'requested' && /contours nets/.test(await page.locator('#blend-status').innerText()) && (await diff(page, soft, hard)).changed > 0.001, `${s.viewer.blend}`);
  await page.click('#blend-3');
  s = await blendIs(page, 'sorted');
  const sorted = await viewShot(page, 'render-stage-sorted.png');
  const d = await diff(page, soft, sorted);
  check('render: "Trié" is the sorted fallback, asked for', s.viewer.blendReason === 'requested' && !s.viewer.blendFallback && (await page.locator('#blend-fallback').count()) === 0 && d.changed > 0.01, `changed ${(d.changed * 100).toFixed(1)} %`);
  s = await waitState(page, (s) => s.viewer.sorts > 0);
  check('render: the sorted mode sorts', s.viewer.sorts > 0, `sorts ${s.viewer.sorts}`);
  const c = await content(page, sorted);
  check('render: every mode draws the scene', c.colors > 60 && c.bright > 0.15, `colors ${c.colors}`);
  await page.click('#btn-pause');
  await waitState(page, (s) => !s.config.paused);

  // ---- the house: 0.8 with soft silhouettes, 1 with hard ones and sorted
  await page.click('#blend-0');
  await blendIs(page, 'normalized-soft');
  await page.click('#mode-house');
  s = await waitState(page, (s) => s.scene === 'house' && s.layers.static > 0 && s.display.splatScale < 1, 60000);
  check('render: house splat size is 0.8 with soft silhouettes', Math.abs(s.display.splatScale - 0.8) < 1e-6 && (await scaleText(page)) === '× 0,80', await scaleText(page));
  await page.click('#blend-2');
  s = await waitState(page, (s) => s.viewer.blend === 'normalized-hard' && s.display.splatScale === 1);
  check('render: house splat size is 1 with hard silhouettes', (await scaleText(page)) === '× 1,00', await scaleText(page));
  await page.click('#blend-3');
  s = await waitState(page, (s) => s.viewer.blend === 'sorted' && s.display.splatScale === 1);
  check('render: sorted keeps the slider at 1 (the viewer widens surfels itself)', s.display.splatScale === 1);
  await page.click('#blend-1');
  s = await waitState(page, (s) => s.viewer.blend === 'normalized-soft' && Math.abs(s.display.splatScale - 0.8) < 1e-6);
  check('render: "Doux" forces soft silhouettes and brings its size back', s.display.blend === 'soft' && s.viewer.blendReason === 'requested');
  // The user's own size wins over the recommendation until the scene changes.
  await setSlider(page, '#splat-scale', 1.5);
  await waitState(page, (s) => Math.abs(s.display.splatScale - 1.5) < 1e-6);
  await page.click('#blend-2');
  s = await blendIs(page, 'normalized-hard');
  check('render: a size chosen by hand survives a change of mode', Math.abs(s.display.splatScale - 1.5) < 1e-6 && (await scaleText(page)) === '× 1,50', await scaleText(page));

  // ---- scan cell presets
  const cells = await page.locator('#section-capture [id^=cell-]:not(#cell-label)').allInnerTexts();
  check('render: the scan offers its cell sizes, the light preset selected', cells.join('|') === '4 cm|3,2 cm|2,4 cm' && (await page.locator('#cell-1').getAttribute('aria-checked')) === 'true', cells.join('|'));
  await page.click('#cell-0');
  s = await waitState(page, (s) => Math.abs(s.config.voxelSize - 0.04) < 1e-6 && s.scan && s.scan.progress < 0.05, 30000);
  check('render: a cell size reaches the pipeline and restarts the scan', (await page.locator('#cell-0').getAttribute('aria-checked')) === 'true' && s.scan.state === 'scanning', JSON.stringify(s.scan));

  // ---- the choice is remembered; a new scene gets the size of its mode
  await page.reload();
  s = await waitState(page, (s) => s.connected && s.scene === 'stage' && s.viewer.framesRendered > 2 && s.viewer.blend === 'normalized-hard', 90000);
  check('render: the rendering choice is stored and found again', s.display.blend === 'hard' && (await page.locator('#blend-2').getAttribute('aria-checked')) === 'true' && s.display.splatScale === 1);

  s = await state(page);
  check('render: no application error', s.errors.length === 0, s.errors.join(' | '));
  check('render: no console error', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
  await ctx.close();
}

// A GPU without float render targets: the viewer draws sorted and the
// interface says so.
async function fallback(browser, url) {
  const { ctx, page, logs } = await newPage(browser, { blend: null, width: 900, height: 700 });
  await page.addInitScript(() => {
    const original = WebGL2RenderingContext.prototype.getExtension;
    WebGL2RenderingContext.prototype.getExtension = function (name) {
      return /color_buffer_(half_)?float/.test(name) ? null : original.call(this, name);
    };
  });
  let s = await open(page, url);
  s = await waitState(page, (s) => s.viewer.blend === 'sorted' && s.viewer.blendFallback);
  await page.locator('#blend-fallback').scrollIntoViewIfNeeded();
  const notice = await page.locator('#blend-fallback').innerText();
  check('render: without float targets the viewer falls back to sorted and says so in French', s.display.blend === 'auto' && s.viewer.blendReason === 'no-float-targets' && /Rendu de secours/.test(notice) && /virgule flottante/.test(notice) && !notice.includes('—'), notice);
  check('render: the fallback keeps the slider at 1', s.display.splatScale === 1 && /trié/.test(await page.locator('#blend-status').innerText()));
  await shot(page, 'render-fallback.png');
  const view = await viewShot(page, 'render-fallback-view.png');
  const c = await content(page, view);
  check('render: the fallback draws the scene', c.colors > 60 && c.bright > 0.15, `colors ${c.colors}`);
  check('render: no console error in the fallback', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
  await ctx.close();
}

// Inside a headset the automatic choice is the hard level.
async function headset(browser, url) {
  if (!iwerBundle()) return;
  const { ctx, page } = await newPage(browser, { blend: null, width: 1280, height: 640, iwer: true });
  let s = await open(page, url);
  await waitState(page, (s) => s.xr.supported === true);
  check('render: on the desktop page of a headset browser the default is still soft', s.viewer.blend === 'normalized-soft');
  await page.click('#btn-vr');
  s = await waitState(page, (s) => s.xr.active && s.xr.session && s.xr.session.frames > 3 && s.viewer.blend === 'normalized-hard', 30000);
  check('render: a VR session draws hard silhouettes by default', s.display.blend === 'auto' && s.viewer.blend === 'normalized-hard');
  await page.click('#exit-vr');
  s = await waitState(page, (s) => !s.xr.active && s.viewer.blend === 'normalized-soft', 30000);
  check('render: leaving VR returns to soft silhouettes', s.viewer.blend === 'normalized-soft');
  // Whoever picks "Doux" gets it in the headset as well.
  await page.click('#blend-1');
  await blendIs(page, 'normalized-soft');
  await page.click('#btn-vr');
  s = await waitState(page, (s) => s.xr.active && s.xr.session && s.xr.session.frames > 3, 30000);
  check('render: an explicit choice is kept inside the headset', s.viewer.blend === 'normalized-soft', s.viewer.blend);
  await page.click('#exit-vr');
  await waitState(page, (s) => !s.xr.active, 30000);
  await ctx.close();
}

async function run({ browser, url }) {
  await defaults(browser, url);
  await fallback(browser, url);
  await headset(browser, url);
}

module.exports = { run };
if (require.main === module) require('./standalone.cjs')(run);
