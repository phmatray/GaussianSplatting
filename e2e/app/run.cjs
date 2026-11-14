// Builds the application and runs every end-to-end check in headless Chromium.
//
//   node e2e/app/run.cjs
//
// Environment:
//   SKIP_BUILD=1        use the existing crates/gs-app/dist
//   GS_APP_OUT=dir      where screenshots and reports go (default: out/app-e2e)
//   GS_APP_PORT=8110    first port tried by the static server (8100-8199)
//   ONLY=stage,vr       run a subset (stage, house, files, vr, narrow, subpath, server, perf)
//   GS_SERVER_BIN=path  gs-server binary for the server suite
//                       (default: target/release/gs-server; skipped when missing)
//   GS_NPM_TOOLS / IWER_BUNDLE   where to find the IWER WebXR emulator if it is
//                       not installed with `npm install` in e2e/render
const { chromium } = require('playwright');
const fs = require('fs');
const path = require('path');
const lib = require('./lib.cjs');

const SUITES = ['stage', 'house', 'files', 'vr', 'narrow', 'subpath', 'server', 'perf'];

(async () => {
  const only = process.env.ONLY ? process.env.ONLY.split(',') : SUITES;
  if (process.env.SKIP_BUILD !== '1') lib.build();
  fs.mkdirSync(lib.OUT, { recursive: true });
  const { srv, url } = await lib.serve();
  const browser = await chromium.launch();
  console.log(`chromium ${browser.version()}, serving ${url}, output in ${lib.OUT}`);
  const report = {};
  for (const name of SUITES.filter((s) => only.includes(s))) {
    const before = lib.results.length;
    const start = Date.now();
    console.log(`\n=== ${name} ===`);
    try {
      const extra = await require(`./${name}.cjs`).run({ browser, url });
      if (extra) report[name] = extra;
    } catch (e) {
      lib.check(`${name}: no uncaught exception`, false, e.stack || e);
    }
    const mine = lib.results.slice(before);
    console.log(`--- ${name}: ${mine.filter((r) => r.ok).length}/${mine.length} passed in ${((Date.now() - start) / 1000).toFixed(1)} s`);
  }
  await browser.close();
  srv.close();
  fs.writeFileSync(path.join(lib.OUT, 'results.json'), JSON.stringify({ checks: lib.results, measurements: report }, null, 2));
  const failed = lib.failures();
  if (Object.keys(report).length) console.log('\nmeasurements:\n' + JSON.stringify(report, null, 2));
  console.log(`\n${lib.results.length - failed.length}/${lib.results.length} checks passed`);
  for (const f of failed) console.log(`FAILED: ${f.name}  -- ${f.detail}`);
  process.exit(failed.length ? 1 : 0);
})();
