// Builds the harness and runs every gs-render check in headless Chromium.
//
//   node e2e/render/run.cjs
//
// Environment:
//   SKIP_BUILD=1       use the existing crates/gs-render-harness/dist
//   GS_RENDER_OUT=dir  where screenshots and reports go (default: out/render-e2e)
//   ONLY=compare,xr    run a subset (compare, modes, adversarial, nav, live, xr, robust, perf),
//                      or `measure`: pictures and timings of the blending
//                      modes (see measure.cjs), not part of the default run
//   PERF_N=200000,1000000   splat counts of the perf check
//   GS_NPM_TOOLS / IWER_BUNDLE   where to find the IWER emulator if it is not
//                      installed with `npm install` in e2e/render
const { chromium } = require('playwright');
const fs = require('fs');
const path = require('path');
const lib = require('./lib.cjs');

const SUITES = ['compare', 'modes', 'adversarial', 'nav', 'live', 'xr', 'robust', 'perf'];
// On request only (ONLY=measure): pictures and timings, not checks.
const OPTIONAL = ['measure'];

(async () => {
  const only = process.env.ONLY ? process.env.ONLY.split(',') : SUITES;
  if (process.env.SKIP_BUILD !== '1') lib.build();
  fs.mkdirSync(lib.OUT, { recursive: true });
  const { srv, url } = await lib.serve();
  const browser = await chromium.launch();
  console.log(`chromium ${browser.version()}, serving ${url}, output in ${lib.OUT}`);
  for (const name of [...SUITES, ...OPTIONAL].filter((s) => only.includes(s))) {
    const before = lib.results.length;
    const start = Date.now();
    try {
      await require(`./${name}.cjs`).run({ browser, url });
    } catch (e) {
      lib.check(`${name}: no uncaught exception`, false, e.stack || e);
    }
    const mine = lib.results.slice(before);
    console.log(`--- ${name}: ${mine.filter((r) => r.ok).length}/${mine.length} passed in ${((Date.now() - start) / 1000).toFixed(1)} s`);
  }
  await browser.close();
  srv.close();
  fs.writeFileSync(path.join(lib.OUT, 'results.json'), JSON.stringify(lib.results, null, 2));
  const failed = lib.failures();
  console.log(`\n${lib.results.length - failed.length}/${lib.results.length} checks passed`);
  for (const f of failed) console.log(`FAILED: ${f.name}  -- ${f.detail}`);
  process.exit(failed.length ? 1 : 0);
})();
