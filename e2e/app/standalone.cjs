// Runs one suite on its own: `node e2e/app/<suite>.cjs` (BUILD=1 rebuilds first).
const { chromium } = require('playwright');
const lib = require('./lib.cjs');

module.exports = async function standalone(run) {
  if (process.env.BUILD === '1') lib.build();
  const { srv, url } = await lib.serve();
  const browser = await chromium.launch();
  try {
    const extra = await run({ browser, url });
    if (extra) console.log(JSON.stringify(extra, null, 2));
  } catch (e) {
    lib.check('no uncaught exception', false, e.stack || e);
  } finally {
    await browser.close();
    srv.close();
  }
  const failed = lib.failures();
  console.log(`\n${lib.results.length - failed.length}/${lib.results.length} checks passed`);
  process.exit(failed.length ? 1 : 0);
};
