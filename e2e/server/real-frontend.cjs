// The real interface (crates/gs-app/dist) served by the headless binary and
// opened in headless Chromium: over plain HTTP on loopback, then over HTTPS
// on the LAN listener, on 127.0.0.1 and on the address a headset would use.
// Each page must pick the "Serveur natif" source, receive the stream over
// the WebSocket and draw splats.
//
//   cargo build --release -p gs-server
//   (cd crates/gs-app && trunk build --release)
//   node e2e/server/real-frontend.cjs [port]   (default 7870; the LAN listener takes port + 1)
//
//   GS_SERVER_BIN=path   binary to test (default target/release/gs-server)
//   GS_DIST=dir          web build to serve (default crates/gs-app/dist; a copy is served,
//                        because Trunk empties the original while it rebuilds)
//   OUT=dir              where screenshots go (default target/e2e-server)
const { spawn } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { chromium } = require('playwright');

const REPO = path.resolve(__dirname, '../..');
const BIN = process.env.GS_SERVER_BIN || path.join(REPO, 'target/release/gs-server');
const DIST = process.env.GS_DIST || path.join(REPO, 'crates/gs-app/dist');
const OUT = process.env.OUT || path.join(REPO, 'target/e2e-server');
const PORT = Number(process.argv[2] || 7870);

let failed = 0;
function check(name, ok, detail = '') {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${name}${detail ? `  (${detail})` : ''}`);
  if (!ok) failed += 1;
}
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function startServer(dist, certs) {
  const args = ['--port', String(PORT), '--lan', '--lan-port', String(PORT + 1), '--dist', dist, '--cert-dir', certs, '--verbose'];
  const child = spawn(BIN, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '';
  child.stdout.on('data', (d) => { output += d; });
  child.stderr.on('data', (d) => { output += d; });
  const exited = new Promise((resolve) => child.on('exit', (code) => resolve(code)));
  for (let i = 0; i < 100 && child.exitCode === null; i += 1) {
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

// Share of the screenshot that is not the dominant colour, and how many
// distinct colours it holds: a blank or single-colour canvas fails both.
async function pictureStats(page, file) {
  const png = await page.screenshot({ path: file });
  return page.evaluate(async (base64) => {
    const image = new Image();
    image.src = `data:image/png;base64,${base64}`;
    await image.decode();
    const canvas = new OffscreenCanvas(image.width, image.height);
    const ctx = canvas.getContext('2d');
    ctx.drawImage(image, 0, 0);
    const { data } = ctx.getImageData(0, 0, image.width, image.height);
    const counts = new Map();
    for (let i = 0; i < data.length; i += 16) {
      const key = (data[i] >> 3) << 10 | (data[i + 1] >> 3) << 5 | (data[i + 2] >> 3);
      counts.set(key, (counts.get(key) || 0) + 1);
    }
    const total = data.length / 16;
    return { colors: counts.size, varied: 1 - Math.max(...counts.values()) / total };
  }, png.toString('base64'));
}

async function openApp(browser, url, shot) {
  const context = await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 1280, height: 760 } });
  const page = await context.newPage();
  const errors = [];
  const responses = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  page.on('response', (r) => responses.push({ url: r.url(), status: r.status(), type: r.headers()['content-type'] || '', encoding: r.headers()['content-encoding'] || '' }));
  await page.goto(url);
  const ready = await page.waitForFunction(() => {
    const s = window.__splatLive && window.__splatLive.state();
    return s && s.connected && s.layers.static > 0 && s.layers.dynamic > 0 && s.viewer.framesRendered > 5 && s.counts.layers > 20;
  }, null, { timeout: 60000 }).then(() => true, () => false);
  const first = await page.evaluate(() => window.__splatLive && window.__splatLive.state());
  await sleep(2000);
  const second = await page.evaluate(() => window.__splatLive && window.__splatLive.state());
  const badge = await page.locator('#source-badge').innerText().catch(() => '');
  const picture = await pictureStats(page, path.join(OUT, shot));
  await context.close();
  return { ready, first, second, badge, picture, errors, responses };
}

function judge(label, r, expectScheme) {
  const s = r.second;
  if (!r.ready || !r.first || !s) {
    check(`${label}: the application connected and received the stream`, false, JSON.stringify(s && { source: s.source, link: s.link, layers: s.layers, counts: s.counts, errors: s.errors }));
    return;
  }
  check(`${label}: source "Serveur natif" over the WebSocket`, s.source === 'server' && s.connected && /Serveur natif/.test(r.badge) && s.server && s.server.ws_path === '/ws', `${r.badge.replace(/\s+/g, ' ')}`);
  check(`${label}: State and both layers arrived`, s.scene === 'stage' && s.layers.static > 1000 && s.layers.dynamic > 100, JSON.stringify(s.layers));
  // Modest thresholds: headless Chromium draws with a software rasterizer,
  // so the page applies far fewer layers than the 30 per second sent.
  const dMessages = s.counts.messages - r.first.counts.messages;
  const dLayers = s.counts.layers - r.first.counts.layers;
  const dFrames = s.viewer.framesRendered - r.first.viewer.framesRendered;
  check(`${label}: the stream keeps coming and is drawn`, dMessages >= 10 && dLayers >= 3 && dFrames >= 3 && s.viewer.splatsDrawn > 1000, `${dMessages} messages, ${dLayers} layers and ${dFrames} rendered frames in 2 s, ${s.viewer.splatsDrawn} splats drawn`);
  check(`${label}: thumbnails and Stats arrive`, s.counts.previews > 0 && s.counts.stats > 0 && s.stats && s.stats.tick > 0, `${s.counts.previews} previews, ${s.counts.stats} stats`);
  check(`${label}: the picture is not blank`, r.picture.colors > 60 && r.picture.varied > 0.05, `${r.picture.colors} colours, ${(r.picture.varied * 100).toFixed(1)} % off the dominant one`);
  const wasm = r.responses.filter((x) => x.url.endsWith('.wasm'));
  check(`${label}: wasm served as application/wasm, compressed`, wasm.length >= 1 && wasm.every((x) => x.status === 200 && x.type === 'application/wasm' && /br|gzip/.test(x.encoding)), wasm.map((x) => `${path.basename(x.url)} ${x.status} ${x.type} ${x.encoding}`).join(', '));
  const bad = r.responses.filter((x) => x.status >= 400);
  check(`${label}: no failed request, no page error`, bad.length === 0 && r.errors.length === 0 && s.errors.length === 0, [...bad.map((x) => `${x.status} ${x.url}`), ...r.errors, ...s.errors].join(' | '));
  check(`${label}: the page is on ${expectScheme}`, r.responses.length > 0 && r.responses[0].url.startsWith(expectScheme));
}

(async () => {
  if (!fs.existsSync(BIN)) {
    console.log(`SKIPPED: ${BIN} does not exist (cargo build --release -p gs-server)`);
    return;
  }
  if (!fs.existsSync(path.join(DIST, 'index.html'))) {
    console.log(`SKIPPED: ${DIST} has no index.html (cd crates/gs-app && trunk build --release)`);
    return;
  }
  fs.mkdirSync(OUT, { recursive: true });
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'gs-real-'));
  const dist = path.join(tmp, 'dist');
  fs.cpSync(DIST, dist, { recursive: true });
  const server = await startServer(dist, path.join(tmp, 'certs'));
  const { info } = server;
  const lanUrl = info.lan.urls[0];
  const lanPort = lanUrl ? Number(new URL(lanUrl).port) : PORT + 1;
  const lanHosts = info.lan.urls.map((u) => new URL(u).hostname);
  // In sandboxes with an egress proxy, the machine's own addresses must not go through it.
  const proxy = process.env.HTTPS_PROXY ? { server: process.env.HTTPS_PROXY, bypass: ['127.0.0.1', 'localhost', ...lanHosts].join(',') } : undefined;
  const browser = await chromium.launch({ proxy });
  try {
    judge('http loopback', await openApp(browser, `${info.http_url}/`, 'real-http.png'), 'http://127.0.0.1');
    judge('https 127.0.0.1', await openApp(browser, `https://127.0.0.1:${lanPort}/`, 'real-https-loopback.png'), 'https://127.0.0.1');
    if (lanUrl) {
      judge(`https ${new URL(lanUrl).hostname}`, await openApp(browser, `${lanUrl}/`, 'real-https-lan.png'), lanUrl);
    } else {
      console.log('note: this machine has no LAN address, the LAN URL was not exercised');
    }
    const log = server.output();
    check('server log: WebSocket accepted on both listeners', /Loopback GET \/ws .* -> 101/.test(log) && /Lan GET \/ws .* -> 101/.test(log));
    // The browser closes the socket of a closed page a moment later.
    let after;
    const closing = Date.now();
    do {
      await sleep(100);
      after = await (await fetch(`${info.http_url}/api/info`)).json();
    } while (after.clients !== 0 && Date.now() - closing < 5000);
    check('every page gone: no viewer left', after.clients === 0, `clients ${after.clients} after ${Date.now() - closing} ms`);
  } finally {
    await browser.close();
    server.child.kill('SIGINT');
    const code = await Promise.race([server.exited, sleep(8000).then(() => 'timeout')]);
    check('SIGINT: clean exit', code === 0, `exit ${code}`);
    if (code === 'timeout') server.child.kill('SIGKILL');
    fs.rmSync(tmp, { recursive: true, force: true });
  }
  console.log(failed === 0 ? `\nall checks passed (screenshots in ${OUT})` : `\n${failed} check(s) FAILED`);
  process.exit(failed === 0 ? 0 : 1);
})().catch((e) => { console.error(e); process.exit(1); });
