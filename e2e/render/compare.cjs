// GPU renderer vs CPU reference rasterizers (?test=compare), in every
// blending mode: sorted against gs_core::raster::rasterize, normalised hard
// and soft against gs_core::experimental::wblend with its presets.
//   node e2e/render/compare.cjs        (BUILD=1 to rebuild the harness first)
const fs = require('fs');
const path = require('path');
const { OUT, check, newPage, consoleErrors, main } = require('./lib.cjs');

// Sorted mode: the two renderers implement the same math; what is left is
// 8-bit framebuffer rounding and the sub-pixel snapping of the quad corners.
const MIN_PSNR_SORTED = 35;
// Normalised mode: the shader evaluates every splat at the pixel center,
// like the reference, so the two agree far better (measured: 59 to 70 dB).
// The bound is what a single wrong yes/no decision per thousand pixels
// (a disc edge, a depth test) would still pass.
const MIN_PSNR_NORMALIZED = 45;

// Hand-built clouds, drawn as imported content: always sorted.
const IMPORTED = [
  'sphere_floor', 'sphere_close', 'off_axis_projection', 'splat_scale_half', 'needles', 'translucent', 'translucent_two_layers',
  'captured_stage', 'captured_stage_close', 'captured_stage_points', 'captured_stage_miniature', 'captured_stage_clip_y',
  'degenerate_splats', 'far_outlier', 'canvas_desktop_camera',
];
const ALL = ['sorted', 'hard', 'soft'];
const NORMALIZED = ['hard', 'soft'];
// The product's clouds (and the hostile splats) as surfel content: view -> renderings.
const SURFELS = {
  degenerate_surfels: ALL,
  stage_orbit: ALL, stage_close: ALL, stage_far: ALL, stage_xr_eye: ALL, stage_miniature: ALL, stage_points: ALL, 'stage_scale_0.6': ALL,
  stage_clip_y: NORMALIZED, stage_both_faces: NORMALIZED, stage_equal_weights: NORMALIZED, stage_static_only: NORMALIZED,
  stage_max_radius: ['soft'],
  canvas_stage: ALL,
  house_inside: ALL, house_from_above: ALL,
};
const BLEND = { sorted: 'sorted', hard: 'normalized-hard', soft: 'normalized-soft' };

function expected() {
  const cases = new Map(IMPORTED.map((name) => [name, 'sorted']));
  for (const [view, renderings] of Object.entries(SURFELS)) for (const r of renderings) cases.set(`${view}_${r}`, BLEND[r]);
  return cases;
}

async function run({ browser, url }) {
  console.log('\n=== compare: WebGL2 vs the CPU references, sorted and normalised ===');
  const { ctx, page, logs } = await newPage(browser, { width: 1500, height: 900 });
  await page.goto(`${url}/?test=compare`);
  // Includes a complete house scan in WASM.
  await page.waitForFunction(() => window.__done === true, null, { timeout: 900000 });
  const error = await page.evaluate(() => window.__error || null);
  check('compare: harness finished without error', !error, error || '');
  const cases = await page.evaluate(() => JSON.parse(JSON.stringify(window.__results)));
  const want = expected();
  const got = new Set(cases.map((c) => c.name));
  const missing = [...want.keys()].filter((name) => !got.has(name));
  check(`compare: all ${want.size} cases ran`, missing.length === 0 && cases.length === want.size, `${cases.length} cases${missing.length ? `, missing: ${missing.join(', ')}` : ''}`);
  fs.mkdirSync(OUT, { recursive: true });
  for (const c of cases) {
    const blend = want.get(c.name);
    const min = blend === 'sorted' ? MIN_PSNR_SORTED : MIN_PSNR_NORMALIZED;
    check(
      `compare: ${c.name} is drawn ${blend || '?'} and matches its reference (PSNR >= ${min} dB)`,
      c.blend === blend && c.psnr >= min,
      `${c.blend}, PSNR ${c.psnr.toFixed(2)} dB, mean abs error ${c.mae.toFixed(3)}, ${c.splats} splats, GPU ${c.gpuMs.toFixed(1)} ms / CPU ${c.cpuMs.toFixed(0)} ms`,
    );
    await page.locator(`[id="case-${c.name}"]`).screenshot({ path: path.join(OUT, `compare-${c.name}.png`) });
  }
  const sizes = await page.evaluate(() => window.__snapshotSizes);
  check('compare: snapshots of 0, 1 and far too many pixels are handled', sizes === 'ok', sizes);
  fs.writeFileSync(path.join(OUT, 'compare.json'), JSON.stringify(cases, null, 2));
  fs.writeFileSync(path.join(OUT, 'console-compare.log'), logs.join('\n'));
  const errs = consoleErrors(logs);
  check('compare: no console errors or panics', errs.length === 0, errs.join(' | '));
  await ctx.close();
  return cases;
}

module.exports = { run };
if (require.main === module) main(run);
