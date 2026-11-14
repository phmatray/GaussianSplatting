// Server mode: the same build served by a native gs-server. Source
// detection, WebSocket stream, shared configuration between two clients,
// LAN sharing panel, reconnection. Skipped when the binary is not built.
//
// The server gets a COPY of the dist (Trunk wipes the original when it
// rebuilds) and ports of our range.
const { chromium } = require('playwright');
const { spawn } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { check, REPO, DIST, PORT, MIME, newPage, consoleErrors, glWarnings, state, waitState, open, shot, viewShot, diff, content, setSlider, sleep } = require('./lib.cjs');

const BIN = process.env.GS_SERVER_BIN || path.join(REPO, 'target/release/gs-server');

// Starts the server and resolves with its loopback URL once it answers.
async function startServer(dist, certs, port) {
  const child = spawn(BIN, ['--port', String(port), '--lan-port', String(port + 1), '--dist', dist, '--cert-dir', certs, '--light'], { stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '';
  child.stdout.on('data', (d) => (output += d));
  child.stderr.on('data', (d) => (output += d));
  const exited = new Promise((resolve) => child.on('exit', resolve));
  for (let i = 0; i < 150; i++) {
    for (const candidate of [port, port + 2, port + 3]) {
      try {
        const info = await (await fetch(`http://127.0.0.1:${candidate}/api/info`)).json();
        if (info.name === 'splat-live') return { child, exited, url: info.http_url, info, output: () => output };
      } catch {}
    }
    if (child.exitCode !== null) break;
    await sleep(100);
  }
  child.kill('SIGKILL');
  throw new Error(`gs-server did not start:\n${output}`);
}

async function run({ browser }) {
  if (!fs.existsSync(BIN)) {
    console.log(`SKIPPED: ${BIN} does not exist (cargo build -p gs-server --release)`);
    return { skipped: true };
  }
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'splat-live-server-'));
  const dist = path.join(tmp, 'dist');
  fs.cpSync(DIST, dist, { recursive: true });
  const port = PORT + 50;
  let server = await startServer(dist, path.join(tmp, 'certs'), port);
  const measurements = {};
  try {
    const url = `${server.url}/`;
    const { ctx, page, logs } = await newPage(browser);
    let s = await open(page, url);
    const badge = await page.locator('#source-badge').innerText();
    check('server: the page served by gs-server uses the WebSocket', s.source === 'server' && s.connected && /Serveur natif/.test(badge) && s.server && s.server.ws_path === '/ws', `${badge}, ${JSON.stringify(s.server)}`);
    check('server: State and both layers arrive', s.scene === 'stage' && s.layers.static > 1000 && s.layers.dynamic > 100, JSON.stringify(s.layers));
    const a = await viewShot(page, 'server-stage-a.png');
    await sleep(700);
    const b = await viewShot(page, 'server-stage-b.png');
    let d = await diff(page, a, b);
    const ca = await content(page, a);
    check('server: the picture is not blank and animates', ca.colors > 60 && d.changed > 0.002, `colors ${ca.colors}, changed ${(d.changed * 100).toFixed(2)} %`);
    await shot(page, 'server-stage.png');
    s = await waitState(page, (s) => s.previews.ids.length > 0 && Object.keys(s.previews.painted).length === s.previews.ids.length && s.transferBytesPerSecond > 1000, 30000);
    check('server: thumbnails and network rate', (await page.locator('#stat-rate').innerText()).length > 0 && (await page.evaluate(() => document.querySelector('#section-stats .stat:last-child dt').textContent)) === 'Réseau', await page.locator('#stat-rate').innerText());
    const t0 = s.stats.tick;
    const w0 = Date.now();
    await sleep(3000);
    s = await state(page);
    measurements.stage = { tick_hz: +s.stats.tick_hz.toFixed(1), ticks_per_s: +((s.stats.tick - t0) / ((Date.now() - w0) / 1000)).toFixed(1), animation_fps: +s.viewer.fps.toFixed(1), capture_ms: +s.stats.capture_ms.toFixed(1), reconstruct_ms: +s.stats.reconstruct_ms.toFixed(1), transfer_kb_s: +(s.transferBytesPerSecond / 1000).toFixed(0) };

    // ---- pause, with the keyboard and the button
    await page.keyboard.press('p');
    await waitState(page, (s) => s.config.paused);
    const p1 = await viewShot(page, 'server-paused-a.png');
    await sleep(600);
    const p2 = await viewShot(page, 'server-paused-b.png');
    d = await diff(page, p1, p2);
    check('server: pause stops the animation', d.changed === 0, `changed ${(d.changed * 100).toFixed(3)} %`);
    await page.click('#btn-pause');
    await waitState(page, (s) => !s.config.paused);

    // ---- a second client: the configuration is shared
    const second = await newPage(browser, { width: 900, height: 600 });
    await open(second.page, url);
    const cameras = s.config.cameras === 5 ? 4 : 5;
    await setSlider(second.page, '#cam-count', cameras);
    s = await waitState(page, (s, n) => s.config.cameras === n && s.previews.ids.length === n, 30000, cameras);
    await page.waitForFunction((n) => document.querySelector('#cam-count').value === String(n) && document.querySelector('output[for=cam-count]').textContent === String(n), cameras);
    check('server: a change made by another client shows in this one', s.config.cameras === cameras && (await page.locator('#tiles canvas').count()) === cameras, `cameras ${s.config.cameras}`);
    await second.page.click('#toggle-shadows');
    await page.waitForFunction(() => document.querySelector('#toggle-shadows').getAttribute('aria-checked') === 'false');
    check('server: switches follow the shared configuration too', !(await state(page)).config.shadows);
    await second.page.click('#toggle-shadows');
    s = await waitState(page, (s) => s.server && s.server.clients >= 0);
    await second.ctx.close();

    // ---- LAN sharing panel
    await page.locator('#section-vr').scrollIntoViewIfNeeded();
    s = await waitState(page, (s) => s.xr.supported === false);
    check('server: without WebXR the VR panel offers LAN sharing', (await page.locator('#lan-toggle').isVisible()) && (await page.locator('#enter-vr').count()) === 0 && (await page.locator('#lan-toggle').getAttribute('aria-checked')) === 'false');
    await page.click('#lan-toggle');
    await page.waitForFunction(() => { const s = window.__splatLive.state(); return (s.server && s.server.lan.enabled) || document.querySelector('#lan-error'); }, null, { timeout: 20000 });
    s = await state(page);
    if (s.server.lan.enabled && s.server.lan.urls.length > 0) {
      await page.waitForFunction(() => { const img = document.querySelector('#lan-qr'); return img && img.complete && img.naturalWidth > 0; }, null, { timeout: 15000 });
      const lan = await page.evaluate(() => ({ urls: [...document.querySelectorAll('#lan-urls code')].map((c) => c.textContent), fingerprint: document.querySelector('#lan-fingerprint').textContent, qr: document.querySelector('#lan-qr').naturalWidth, text: document.querySelector('.lan').innerText }));
      check('server: LAN sharing shows the https address, the QR code and the fingerprint', lan.urls.every((u) => u.startsWith('https://')) && lan.qr > 0 && /^([0-9A-F]{2}:){31}[0-9A-F]{2}$/i.test(lan.fingerprint) && /avertissement de certificat/.test(lan.text) && /une seule fois/.test(lan.text),
        `${lan.urls.join(', ')} / ${lan.fingerprint.slice(0, 23)}…`);
      await page.locator('#lan-fingerprint').scrollIntoViewIfNeeded();
      await shot(page, 'vr-panel-lan.png');

      // The headset side: the https address, WebSocket over TLS. A browser
      // of its own: where an egress proxy is imposed (sandboxes), the
      // machine's own LAN address has to bypass it explicitly.
      const lanHost = new URL(lan.urls[0]).hostname;
      const proxy = process.env.HTTPS_PROXY ? { proxy: { server: process.env.HTTPS_PROXY, bypass: `127.0.0.1,localhost,${lanHost}` } } : {};
      const lanBrowser = await chromium.launch(proxy);
      const headset = await lanBrowser.newContext({ viewport: { width: 900, height: 600 }, ignoreHTTPSErrors: true });
      const hp = await headset.newPage();
      try {
        const hs = await open(hp, `${lan.urls[0]}/`);
        check('server: the LAN https address serves the same application over wss', hs.source === 'server' && hs.connected && hs.layers.static > 0, lan.urls[0]);
        await hp.locator('#section-vr').scrollIntoViewIfNeeded();
        await hp.click('#lan-toggle');
        await hp.waitForFunction(() => document.querySelector('#lan-error'), null, { timeout: 15000 });
        const refused = await hp.locator('#lan-error').innerText();
        check('server: LAN sharing cannot be changed from the LAN side', /ordinateur/.test(refused) && (await state(page)).server.lan.enabled, refused);
      } catch (e) {
        check('server: the LAN https address serves the same application over wss', false, String(e).split('\n')[0]);
      }
      await lanBrowser.close();

      await page.click('#lan-toggle');
      await page.waitForFunction(() => !window.__splatLive.state().server.lan.enabled, null, { timeout: 20000 });
      check('server: LAN sharing switches off again', (await page.locator('#lan-qr').count()) === 0 && (await page.locator('#lan-urls').count()) === 0);
    } else {
      const reason = (await page.locator('#lan-error').count()) ? await page.locator('#lan-error').innerText() : JSON.stringify(s.server.lan);
      check('server: LAN sharing shows the https address, the QR code and the fingerprint', false, `LAN could not be enabled here: ${reason}`);
    }

    // ---- as inside the desktop shell: the page comes from a Tauri origin
    // (http://tauri.localhost on Windows) and learns the server address from
    // the `server_info` command; everything else is cross-origin. Chromium
    // takes the simulated origin for a public site and would keep it away
    // from loopback addresses (Local Network Access), which no webview of
    // the real shell does: that check is switched off for this page.
    const shellBrowser = await chromium.launch({ args: ['--disable-features=LocalNetworkAccessChecks'] });
    const shell = await newPage(shellBrowser, { width: 900, height: 600 });
    await shell.page.route('http://tauri.localhost/**', (route) => {
      const rel = decodeURIComponent(new URL(route.request().url()).pathname).slice(1);
      const file = path.join(dist, rel);
      // Like the shell's asset resolver: the index page for unknown paths.
      const found = rel !== '' && file.startsWith(dist) && fs.existsSync(file) ? file : path.join(dist, 'index.html');
      route.fulfill({ status: 200, contentType: MIME[path.extname(found)] || 'application/octet-stream', path: found });
    });
    await shell.page.addInitScript((info) => {
      window.__invoked = [];
      window.__TAURI__ = { core: { invoke: async (command) => { window.__invoked.push(command); if (command !== 'server_info') throw new Error(`unknown command ${command}`); return info; } } };
    }, server.info);
    s = await open(shell.page, 'http://tauri.localhost/');
    const invoked = await shell.page.evaluate(() => window.__invoked);
    check('server: in the desktop shell the source comes from the server_info command', invoked.includes('server_info') && s.source === 'server' && s.connected && s.layers.dynamic > 0, `invoked ${invoked.join(',')}`);
    await shell.page.locator('#section-vr').scrollIntoViewIfNeeded();
    await shell.page.click('#lan-toggle');
    await shell.page.waitForFunction(() => { const s = window.__splatLive.state(); return (s.server && s.server.lan.enabled && document.querySelector('#lan-qr')) || document.querySelector('#lan-error'); }, null, { timeout: 20000 });
    const shellLan = await shell.page.evaluate(() => ({ enabled: window.__splatLive.state().server.lan.enabled, qr: !!document.querySelector('#lan-qr') && document.querySelector('#lan-qr').src.slice(0, 19), error: document.querySelector('#lan-error') && document.querySelector('#lan-error').textContent }));
    check('server: LAN sharing and the QR code work across origins from the shell', shellLan.enabled && shellLan.qr === 'data:image/svg+xml;' && !shellLan.error, JSON.stringify(shellLan));
    await shell.page.click('#lan-toggle');
    await shell.page.waitForFunction(() => !window.__splatLive.state().server.lan.enabled, null, { timeout: 20000 });
    const shellErrors = consoleErrors(shell.logs);
    check('server: no console error in the shell-like page', shellErrors.length === 0, shellErrors.join(' | '));
    await shellBrowser.close();

    // ---- the house through the server
    await page.click('#mode-house');
    s = await waitState(page, (s) => s.scene === 'house' && s.scan && s.layers.static > 0 && s.display.dollhouse, 60000);
    const start = Date.now();
    await page.click('#scan-finish');
    s = await waitState(page, (s) => s.scan.state === 'done', 600000);
    measurements.house_finish_now_s = +((Date.now() - start) / 1000).toFixed(1);
    measurements.house_splats = s.layers.static;
    check('server: the house scan completes', s.scan.progress === 1 && s.layers.static > 100000 && s.stats.splats_static === s.layers.static, `${s.layers.static} splats in ${measurements.house_finish_now_s} s`);
    await sleep(500);
    await shot(page, 'server-house.png');

    // ---- file import pauses the shared pipeline; returning resynchronises over a new socket
    const splats = s.layers.static;
    const [download] = await Promise.all([page.waitForEvent('download'), page.click('#export-splat')]);
    const file = path.join(tmp, download.suggestedFilename());
    await download.saveAs(file);
    await page.click('#mode-stage');
    await waitState(page, (s) => s.scene === 'stage' && s.layers.dynamic > 0, 60000);
    await page.setInputFiles('#import-input', file);
    s = await waitState(page, (s) => s.fileMode && s.config.paused);
    check('server: an exported house re-imports with the same splat count', s.file.splats === splats && s.layers.static === splats, `${s.file.splats} splats`);
    await page.click('#back-live');
    s = await waitState(page, (s, splats) => !s.fileMode && s.connected && !s.config.paused && s.layers.dynamic > 0 && s.layers.static > 0 && s.layers.static < splats, 30000, splats);
    check('server: "Revenir au direct" resynchronises from a fresh snapshot', s.scene === 'stage' && s.counts.states >= 2);

    // ---- the server goes away and comes back
    const states = s.counts.states;
    server.child.kill('SIGKILL');
    await server.exited;
    s = await waitState(page, (s) => s.link === 'reconnecting', 20000);
    const lost = await page.locator('#source-badge').getAttribute('data-link');
    check('server: a lost connection is shown', lost === 'reconnecting' && !s.connected, lost);
    await shot(page, 'server-reconnecting.png');
    server = await startServer(dist, path.join(tmp, 'certs'), port);
    s = await waitState(page, (s, n) => s.connected && s.counts.states > n && s.layers.dynamic > 0 && s.layers.static > 0, 60000, states);
    const tick = s.stats.tick;
    s = await waitState(page, (s, tick) => s.stats.tick > tick + 3, 20000, tick);
    check('server: the page reconnects by itself and gets a fresh snapshot', s.connected && s.scene === 'stage' && (await page.locator('#source-badge').getAttribute('data-link')) === 'connected', `states ${states} -> ${s.counts.states}`);

    s = await state(page);
    check('server: no application error', s.errors.length === 0, s.errors.join(' | '));
    // While the server was down the browser logged the refused connections.
    const errors = consoleErrors(logs).filter((l) => !/WebSocket connection to|ERR_CONNECTION_REFUSED|net::ERR_/.test(l));
    check('server: no console error', errors.length === 0, errors.join(' | '));
    check('server: no WebGL warning', glWarnings(logs).length === 0, glWarnings(logs).join(' | '));
    await ctx.close();
  } finally {
    server.child.kill('SIGKILL');
    await server.exited;
    fs.rmSync(tmp, { recursive: true, force: true });
  }
  return measurements;
}

module.exports = { run };
if (require.main === module) require('./standalone.cjs')(run);
