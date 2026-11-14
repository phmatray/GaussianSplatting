// Memory soak: the application runs for a while in every mode and must not
// grow. Sampled after a forced garbage collection: the JavaScript heap, the
// WebAssembly memory of the page and of the pipeline worker, DOM nodes and
// event listeners.
//
//   SOAK_S=180 node e2e/app/soak.cjs      (default: 40 s per run)
//
// Three pages run side by side: the stage in worker mode, the stage through
// gs-server (skipped when the binary is missing), and the house in worker
// mode with the scan restarted every time it completes.
const fs = require('fs');
const os = require('os');
const path = require('path');
const { check, DIST, PORT, newPage, consoleErrors, glWarnings, state, waitState, open, sleep, startServer, SERVER_BIN } = require('./lib.cjs');

const SOAK_S = Number(process.env.SOAK_S || 40);
const MB = 1024 * 1024;

// Trunk announces the instantiated module: keep its memory within reach.
const hookWasm = (page) =>
  page.addInitScript(() => {
    addEventListener('TrunkApplicationStarted', (e) => {
      window.__wasmMemory = e.detail && e.detail.wasm && e.detail.wasm.memory;
    });
  });

// The worker's wasm-bindgen module returns its exports when initialised a
// second time; the import resolves to the instance the worker already runs.
async function workerWasmBytes(page) {
  const worker = page.workers()[0];
  if (!worker) return null;
  return worker.evaluate(async () => {
    const version = new URL(self.location.href).searchParams.get('v') || '';
    const glue = await import(`./splat-worker.js${version ? `?v=${encodeURIComponent(version)}` : ''}`);
    return (await glue.default()).memory.buffer.byteLength;
  });
}

async function sample(page, cdp) {
  await cdp.send('HeapProfiler.collectGarbage');
  const { metrics } = await cdp.send('Performance.getMetrics');
  const metric = (name) => metrics.find((m) => m.name === name).value;
  const s = await state(page);
  return {
    heap: metric('JSHeapUsedSize'),
    nodes: metric('Nodes'),
    listeners: metric('JSEventListeners'),
    wasm: await page.evaluate(() => (window.__wasmMemory ? window.__wasmMemory.buffer.byteLength : null)),
    worker: await workerWasmBytes(page),
    tick: s.stats ? s.stats.tick : 0,
    messages: s.counts.messages,
    splats: s.layers.static + s.layers.dynamic,
    errors: s.errors,
  };
}

async function soak(name, page, logs, { during = async () => {} } = {}) {
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('Performance.enable');
  // Warm-up: caches, the first sorts and the scratch buffers reach their size.
  await sleep(Math.min(15000, SOAK_S * 250));
  const samples = [await sample(page, cdp)];
  const end = Date.now() + SOAK_S * 1000;
  while (Date.now() < end) {
    await sleep(Math.min(10000, SOAK_S * 250));
    await during(page);
    samples.push(await sample(page, cdp));
  }
  const first = samples[0];
  const last = samples[samples.length - 1];
  // Growth is judged on the second half: a leak keeps climbing, a pool that
  // filled up early does not.
  const middle = samples[Math.floor(samples.length / 2)];
  const grown = (key) => last[key] - middle[key];
  const report = {
    seconds: SOAK_S,
    ticks: last.tick - first.tick,
    messages: last.messages - first.messages,
    heap_mb: samples.map((s) => +(s.heap / MB).toFixed(2)),
    wasm_mb: samples.map((s) => (s.wasm === null ? null : +(s.wasm / MB).toFixed(2))),
    worker_wasm_mb: samples.map((s) => (s.worker === null ? null : +(s.worker / MB).toFixed(2))),
    nodes: [first.nodes, last.nodes],
    listeners: [first.listeners, last.listeners],
  };
  check(`soak ${name}: the stream kept flowing`, report.messages > SOAK_S * 5, `${report.messages} messages, ${report.ticks} ticks in ${SOAK_S} s`);
  check(`soak ${name}: the JavaScript heap does not grow`, grown('heap') < 3 * MB, `${report.heap_mb.join(' ')} MB`);
  check(`soak ${name}: the page's WebAssembly memory does not grow`, last.wasm !== null && grown('wasm') === 0, `${report.wasm_mb.join(' ')} MB`);
  if (last.worker !== null) check(`soak ${name}: the worker's WebAssembly memory does not grow`, grown('worker') === 0, `${report.worker_wasm_mb.join(' ')} MB`);
  check(`soak ${name}: no DOM node or listener piles up`, last.nodes - middle.nodes < 50 && last.listeners - middle.listeners < 20, `nodes ${report.nodes.join(' -> ')}, listeners ${report.listeners.join(' -> ')}`);
  check(`soak ${name}: no error`, last.errors.length === 0 && consoleErrors(logs).length === 0 && glWarnings(logs).length === 0, [...last.errors, ...consoleErrors(logs), ...glWarnings(logs)].join(' | '));
  await page.context().close();
  return report;
}

async function run({ browser, url }) {
  const runs = [];
  const measurements = {};

  const stage = await newPage(browser, { width: 900, height: 600 });
  await hookWasm(stage.page);
  await open(stage.page, `${url}?source=worker`);
  runs.push(soak('worker stage', stage.page, stage.logs).then((r) => (measurements.worker_stage = r)));

  // The house, scanned again and again: every scan replaces the previous
  // map, so memory is bounded by one house.
  const house = await newPage(browser, { width: 900, height: 600 });
  await hookWasm(house.page);
  await open(house.page, `${url}?source=worker&scene=house`, { scene: 'house' });
  await house.page.click('#scan-finish');
  await waitState(house.page, (s) => s.scan && s.scan.state === 'done', 300000);
  let scans = 1;
  const rescan = async (page) => {
    const s = await state(page);
    if (s.scan && s.scan.state === 'done') {
      scans += 1;
      await page.click('#scan-start');
      await waitState(page, (s) => s.scan.state === 'scanning', 20000);
      await page.click('#scan-finish');
    }
  };
  runs.push(
    soak('worker house', house.page, house.logs, { during: rescan }).then((r) => {
      measurements.worker_house = { ...r, scans };
    }),
  );

  let server = null;
  let tmp = null;
  if (fs.existsSync(SERVER_BIN)) {
    tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'splat-live-soak-'));
    fs.cpSync(DIST, path.join(tmp, 'dist'), { recursive: true });
    server = await startServer({ dist: path.join(tmp, 'dist'), certs: path.join(tmp, 'certs'), port: PORT + 60 });
    const remote = await newPage(browser, { width: 900, height: 600 });
    await hookWasm(remote.page);
    await open(remote.page, `${server.url}/`);
    runs.push(soak('server stage', remote.page, remote.logs).then((r) => (measurements.server_stage = r)));
  } else {
    console.log(`SKIPPED (server stage): ${SERVER_BIN} does not exist`);
  }

  try {
    await Promise.all(runs);
  } finally {
    if (server) await server.stop();
    if (tmp) fs.rmSync(tmp, { recursive: true, force: true });
  }
  return measurements;
}

module.exports = { run };
if (require.main === module) require('./standalone.cjs')(run);
