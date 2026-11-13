// GPU renderer vs CPU reference rasterizer (?test=compare).
//   node e2e/render/compare.cjs        (BUILD=1 to rebuild the harness first)
const fs = require('fs');
const path = require('path');
const { OUT, check, newPage, consoleErrors, main } = require('./lib.cjs');

// The two renderers implement the same math; what is left is 8-bit
// framebuffer rounding on the GPU (see the summary printed below).
const MIN_PSNR = 35;
const EXPECTED_CASES = 15;

async function run({ browser, url }) {
  console.log('\n=== compare: WebGL2 vs gs_core::raster::rasterize ===');
  const { ctx, page, logs } = await newPage(browser, { width: 1500, height: 900 });
  await page.goto(`${url}/?test=compare`);
  await page.waitForFunction(() => window.__done === true, null, { timeout: 300000 });
  const error = await page.evaluate(() => window.__error || null);
  check('compare: harness finished without error', !error, error || '');
  const cases = await page.evaluate(() => JSON.parse(JSON.stringify(window.__results)));
  check(`compare: all ${EXPECTED_CASES} cases ran`, cases.length === EXPECTED_CASES, `${cases.length} cases`);
  fs.mkdirSync(OUT, { recursive: true });
  for (const c of cases) {
    check(`compare: ${c.name} PSNR >= ${MIN_PSNR} dB`, c.psnr >= MIN_PSNR, `PSNR ${c.psnr.toFixed(2)} dB, mean abs error ${c.mae.toFixed(3)}, ${c.splats} splats, GPU ${c.gpuMs.toFixed(1)} ms / CPU ${c.cpuMs.toFixed(0)} ms`);
    await page.locator(`#case-${c.name}`).screenshot({ path: path.join(OUT, `compare-${c.name}.png`) });
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
