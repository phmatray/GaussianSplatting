// gs-server seen from a real browser: the stand-in page of ./standin is
// served by the headless binary and opened in headless Chromium over plain
// HTTP on loopback, then over HTTPS with the self-signed certificate (what a
// headset does).
//
//   cargo build --release -p gs-server
//   node e2e/server/browser.cjs [port]        (default 7860; the LAN listener takes port + 1)
//
//   GS_SERVER_BIN=path   binary to test (default target/release/gs-server)
//   OUT=dir              where screenshots go (default target/e2e-server)
const { spawn } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { chromium } = require('playwright');

const REPO = path.resolve(__dirname, '../..');
const BIN = process.env.GS_SERVER_BIN || path.join(REPO, 'target/release/gs-server');
const OUT = process.env.OUT || path.join(REPO, 'target/e2e-server');
const PORT = Number(process.argv[2] || 7860);

let failed = 0;
function check(name, ok, detail = '') {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${name}${detail ? `  (${detail})` : ''}`);
  if (!ok) failed += 1;
}
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function startServer(certs) {
  const args = ['--port', String(PORT), '--lan', '--lan-port', String(PORT + 1), '--dist', path.join(__dirname, 'standin'), '--cert-dir', certs, '--light'];
  const child = spawn(BIN, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '';
  child.stdout.on('data', (d) => { output += d; });
  child.stderr.on('data', (d) => { output += d; });
  const exited = new Promise((resolve) => child.on('exit', (code) => resolve(code)));
  // A busy port makes the server take the next one: ask it which it got.
  for (let i = 0; i < 100; i += 1) {
    for (let candidate = PORT; candidate < PORT + 4; candidate += 1) {
      try {
        const info = await (await fetch(`http://127.0.0.1:${candidate}/api/info`)).json();
        if (info.name === 'splat-live') return { child, info, exited, output: () => output };
      } catch (_) { /* not up yet */ }
    }
    await sleep(100);
  }
  child.kill('SIGKILL');
  throw new Error(`gs-server did not start:\n${output}`);
}

async function openPage(browser, url, shot) {
  const context = await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 900, height: 620 } });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  await page.goto(url);
  await page.waitForFunction(() => window.__result && window.__result.frames >= 40, null, { timeout: 30000 }).catch(() => {});
  await sleep(500);
  const result = await page.evaluate(() => window.__result);
  await page.screenshot({ path: path.join(OUT, shot) });
  await context.close();
  return { result, errors };
}

(async () => {
  if (!fs.existsSync(BIN)) {
    console.log(`SKIPPED: ${BIN} does not exist (cargo build --release -p gs-server)`);
    return;
  }
  fs.mkdirSync(OUT, { recursive: true });
  const certs = fs.mkdtempSync(path.join(os.tmpdir(), 'gs-certs-'));
  const server = await startServer(certs);
  const { info } = server;
  const lanUrl = info.lan.urls[0];
  const lanPort = lanUrl ? Number(new URL(lanUrl).port) : PORT + 1;
  const lanHosts = info.lan.urls.map((u) => new URL(u).hostname);
  // In sandboxes with an egress proxy, the machine's own addresses must not go through it.
  const proxy = process.env.HTTPS_PROXY ? { server: process.env.HTTPS_PROXY, bypass: ['127.0.0.1', 'localhost', ...lanHosts].join(',') } : undefined;
  const browser = await chromium.launch({ proxy });
  try {
    check('info: LAN is on with a fingerprint', info.lan.enabled && /^([0-9A-F]{2}:){31}[0-9A-F]{2}$/.test(info.lan.fingerprint), JSON.stringify(info.lan));

    // 1. Loopback, plain HTTP.
    const local = await openPage(browser, info.http_url + '/', 'loopback.png');
    const a = local.result;
    check('loopback: page served, info fetched', a && a.fetch_matches === true, a && JSON.stringify(a.info));
    check('loopback: WebSocket open and frames arrive', a.ws === 'open' && a.frames >= 40, `${a.frames} frames, ${a.bytes} bytes`);
    check('loopback: frames are codec messages of every kind', a.magic === 'GSL1' && a.kinds[1] >= 1 && a.kinds[2] >= 10 && a.kinds[3] >= 5 && a.kinds[4] >= 10, JSON.stringify(a.kinds));
    check('loopback: POST /api/lan accepted from the page itself', a.lan_status === 200 && a.lan && a.lan.enabled === true, String(a.lan_status));
    check('loopback: QR code image loads', /^\d+x\d+$/.test(a.qr || ''), a.qr);
    check('loopback: wasm served as application/wasm (streaming compile)', a.wasm === true, a.wasm_error);
    check('loopback: no page error', local.errors.length === 0, local.errors.join(' | '));

    // 2. The LAN listener over HTTPS (self-signed), first on loopback...
    const secure = await openPage(browser, `https://127.0.0.1:${lanPort}/`, 'lan-loopback.png');
    const b = secure.result;
    check('https: secure context, WebXR API exposed', b && b.secure === true && b.xr === true, b && `secure=${b.secure} xr=${b.xr}`);
    check('https: wss open and frames arrive', b.ws === 'open' && b.frames >= 40 && b.magic === 'GSL1', `${b.frames} frames`);
    check('https: wasm streaming compile', b.wasm === true, b.wasm_error);
    check('https: POST /api/lan refused from the network side (403)', b.lan_status === 403, String(b.lan_status));
    check('https: the listener is still on after the refused POST', (await (await fetch(info.http_url + '/api/info')).json()).lan.enabled === true);
    // ...then on the address a headset would use.
    if (lanUrl) {
      const lan = await openPage(browser, lanUrl + '/', 'lan-address.png');
      const c = lan.result;
      check(`https on ${lanUrl}: secure context and frames`, c && c.secure === true && c.ws === 'open' && c.frames >= 40, c && `${c.frames} frames`);
    } else {
      console.log('note: this machine has no LAN address, the LAN URL was not exercised');
    }

    // 3. A page of another origin may neither read the API nor open the stream.
    const context = await browser.newContext();
    const page = await context.newPage();
    await page.goto('about:blank');
    const foreign = await page.evaluate(async (base) => {
      const out = {};
      try { await (await fetch(`${base}/api/info`)).json(); out.read = 'allowed'; } catch (e) { out.read = 'blocked'; }
      try {
        const r = await fetch(`${base}/api/lan`, { method: 'POST', mode: 'no-cors', headers: { 'content-type': 'text/plain' }, body: '{"enabled":false}' });
        out.post = r.type;
      } catch (e) { out.post = 'blocked'; }
      out.ws = await new Promise((resolve) => {
        const ws = new WebSocket(base.replace(/^http/, 'ws') + '/ws');
        ws.onopen = () => resolve('open');
        ws.onerror = () => resolve('refused');
      });
      return out;
    }, info.http_url);
    await context.close();
    check('foreign origin: cannot read /api/info (no CORS grant)', foreign.read === 'blocked', foreign.read);
    check('foreign origin: WebSocket refused', foreign.ws === 'refused', foreign.ws);
    check('foreign origin: blind POST did not switch LAN off', (await (await fetch(info.http_url + '/api/info')).json()).lan.enabled === true, foreign.post);
  } finally {
    await browser.close();
    // 4. Ctrl-C stops the server cleanly.
    server.child.kill('SIGINT');
    const code = await Promise.race([server.exited, sleep(8000).then(() => 'timeout')]);
    check('SIGINT: clean exit', code === 0 && /Serveur arrêté\./.test(server.output()), `exit ${code}`);
    if (code === 'timeout') server.child.kill('SIGKILL');
    fs.rmSync(certs, { recursive: true, force: true });
  }
  console.log(failed === 0 ? `\nall checks passed (screenshots in ${OUT})` : `\n${failed} check(s) FAILED`);
  process.exit(failed === 0 ? 0 : 1);
})().catch((e) => { console.error(e); process.exit(1); });
