// The headset's side of gs-server: the LAN HTTPS listener (self-signed
// certificate, WebSocket over TLS) with an emulated Quest 3 (IWER). Enter
// VR, both eyes, locomotion, miniature, exit; then the house. Also the
// desktop that has a headset of its own: it is offered "Entrer en VR" and
// the LAN sharing switch. Skipped when the binary is not built.
//
// The LAN listener binds every interface, so it is reached at 127.0.0.1
// here: no proxy in the way, and the certificate is not trusted either way
// (`ignoreHTTPSErrors` stands for the warning the user accepts once).
const fs = require('fs');
const os = require('os');
const path = require('path');
const { check, DIST, PORT, iwerBundle, consoleErrors, glWarnings, state, waitState, open, shot, imageStats, hasContent, sleep, startServer, SERVER_BIN } = require('./lib.cjs');

const xrFrames = async (page, n) => {
  const start = (await state(page)).xr.session.frames;
  return waitState(page, (s, target) => s.xr.session && s.xr.session.frames >= target, 30000, start + n);
};
const stick = (page, hand, x, y) => page.evaluate(([h, x, y]) => xrdev.controllers[h].updateAxes('thumbstick', x, y), [hand, x, y]);
async function press(page, hand, button) {
  await page.evaluate(([h, b]) => xrdev.controllers[h].updateButtonValue(b, 1), [hand, button]);
  await xrFrames(page, 3);
  await page.evaluate(([h, b]) => xrdev.controllers[h].updateButtonValue(b, 0), [hand, button]);
  return xrFrames(page, 3);
}
const entered = (page) => waitState(page, (s) => s.xr.active && s.xr.session && s.xr.session.frames > 5 && s.xr.session.viewports.length === 2, 30000);
const distance = (a, b) => Math.hypot(...a.map((v, i) => v - b[i]));

// A page with an emulated Quest 3, accepting the self-signed certificate.
async function headsetPage(browser) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 640 }, ignoreHTTPSErrors: true });
  const page = await ctx.newPage();
  const logs = [];
  page.on('console', (m) => logs.push(`[${m.type()}] ${m.text()}`));
  page.on('pageerror', (e) => logs.push(`[pageerror] ${e.message}`));
  await page.addInitScript({ path: iwerBundle() });
  await page.addInitScript(() => {
    window.xrdev = new IWER.XRDevice(IWER.metaQuest3, { stereoEnabled: true });
    window.xrdev.installRuntime({ forceInstall: true });
  });
  return { ctx, page, logs };
}

async function run({ browser }) {
  if (!fs.existsSync(SERVER_BIN)) {
    console.log(`SKIPPED: ${SERVER_BIN} does not exist (cargo build -p gs-server --release)`);
    return { skipped: true };
  }
  if (!iwerBundle()) {
    check('lanvr: IWER emulator bundle found', false, 'run `npm install` in e2e/render, or set GS_NPM_TOOLS / IWER_BUNDLE');
    return {};
  }
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'splat-live-lanvr-'));
  const dist = path.join(tmp, 'dist');
  fs.cpSync(DIST, dist, { recursive: true });
  const port = PORT + 80;
  const server = await startServer({ dist, certs: path.join(tmp, 'certs'), port, args: ['--light', '--lan'] });
  try {
    const lanPort = new URL(server.info.lan.urls[0]).port;
    const lanUrl = `https://127.0.0.1:${lanPort}/`;
    const { ctx, page, logs } = await headsetPage(browser);
    let s = await open(page, lanUrl);
    const secure = await page.evaluate(() => isSecureContext && location.protocol === 'https:');
    check('lanvr: the LAN https listener serves the application, WebSocket over TLS through the relay', secure && s.source === 'server' && s.channel === 'relay' && s.connected && s.layers.dynamic > 0, `${lanUrl}, channel ${s.channel}`);
    s = await waitState(page, (s) => s.xr.supported !== null);
    await page.locator('#section-vr').scrollIntoViewIfNeeded();
    check('lanvr: the headset is offered "Entrer en VR", not the sharing switch', s.xr.supported === true && (await page.locator('#enter-vr').isVisible()) && (await page.locator('#lan-toggle').count()) === 0 && (await page.locator('#vr-lan-help').count()) === 0);
    await shot(page, 'lanvr-panel.png');
    const desktop = s;

    // ---- enter from the top bar button
    await page.click('#btn-vr');
    s = await entered(page);
    const session = s.xr.session;
    check('lanvr: an immersive session starts over https', s.xr.active && session.referenceSpace === 'local-floor' && session.scale === 1 && Math.abs(session.rig[0] - desktop.eye[0]) < 1e-3 && session.rig[1] === 0, `frames ${session.frames}, rig ${session.rig.map((v) => v.toFixed(2))}`);
    await page.addStyleTag({ content: '#xr-card { visibility: hidden; }' });
    await sleep(300);
    let f = await shot(page, 'lanvr-stereo.png');
    let img = await imageStats(page, f);
    check('lanvr: both eyes render, similar but not identical', hasContent(img.left) && hasContent(img.right) && img.lrIdenticalFrac < 0.99 && img.lrMeanAbsDiff < 30, `left ${img.left.colors} colours, right ${img.right.colors} colours, identical ${(img.lrIdenticalFrac * 100).toFixed(1)} %`);
    const tick = s.stats.tick;
    s = await waitState(page, (s, tick) => s.stats.tick > tick + 10, 30000, tick);
    const f2 = await shot(page, 'lanvr-stereo-later.png');
    const moved = await imageStats(page, f, f2);
    check('lanvr: the live stream keeps animating inside the session', s.layers.dynamic > 0 && moved.diffChangedFrac > 0.002, `tick ${tick} -> ${s.stats.tick}, ${(moved.diffChangedFrac * 100).toFixed(2)} % of the picture changed`);

    // ---- locomotion
    const rig0 = s.xr.session.rig;
    await stick(page, 'left', 0, -1);
    await xrFrames(page, 20);
    await stick(page, 'left', 0, 0);
    s = await xrFrames(page, 3);
    const walked = distance(s.xr.session.rig.slice(0, 3), rig0.slice(0, 3));
    check('lanvr: the left stick moves the rig', walked > 0.2 && s.xr.session.rig[1] === rig0[1], `moved ${walked.toFixed(2)} m`);
    const yaw0 = s.xr.session.rig[3];
    await stick(page, 'right', 1, 0);
    s = await xrFrames(page, 10);
    await stick(page, 'right', 0, 0);
    check('lanvr: the right stick snap-turns', Math.abs(s.xr.session.rig[3] - (yaw0 - Math.PI / 6)) < 1e-4, `yaw ${yaw0.toFixed(3)} -> ${s.xr.session.rig[3].toFixed(3)}`);

    // ---- miniature and back
    s = await press(page, 'right', 'a-button');
    await sleep(200);
    f = await shot(page, 'lanvr-miniature.png');
    img = await imageStats(page, f);
    check('lanvr: A switches to the miniature, rendered in both eyes', s.xr.session.miniature && s.xr.session.scale > 5 && img.left.colors > 20 && img.right.colors > 20, `scale ${s.xr.session.scale}, colours ${img.left.colors} / ${img.right.colors}`);
    s = await press(page, 'right', 'a-button');
    check('lanvr: A again returns to life size', !s.xr.session.miniature && s.xr.session.scale === 1);

    // ---- exit with the page button, then the house
    await page.addStyleTag({ content: '#xr-card { visibility: visible !important; }' });
    const rendered = s.viewer.framesRendered;
    await page.click('#exit-vr');
    s = await waitState(page, (s, n) => !s.xr.active && s.xr.session === null && s.viewer.framesRendered > n + 3, 20000, rendered);
    check('lanvr: leaving returns to the desktop view', (await page.locator('.topbar').isVisible()) && (await page.locator('#xr-card').count()) === 0 && distance(s.eye, desktop.eye) < 1e-3);

    await page.click('#mode-house');
    await waitState(page, (s) => s.scene === 'house' && s.display.dollhouse && s.layers.static > 0, 60000);
    await page.click('#scan-finish');
    await waitState(page, (s) => s.scan && s.scan.state === 'done', 300000);
    await page.locator('#section-vr').scrollIntoViewIfNeeded();
    await page.click('#enter-vr');
    s = await entered(page);
    await page.addStyleTag({ content: '#xr-card { visibility: hidden !important; }' });
    await sleep(500);
    f = await shot(page, 'lanvr-house.png');
    img = await imageStats(page, f);
    check('lanvr: the finished house in VR: inside, life-size, both eyes', !s.display.dollhouse && s.xr.session.scale === 1 && s.xr.session.rig[1] === 0 && hasContent(img.left) && hasContent(img.right), `rig ${s.xr.session.rig.map((v) => v.toFixed(2))}, colours ${img.left.colors} / ${img.right.colors}`);
    s = await press(page, 'right', 'a-button');
    await sleep(300);
    f = await shot(page, 'lanvr-house-miniature.png');
    img = await imageStats(page, f);
    check('lanvr: the house as a miniature', s.xr.session.miniature && img.left.colors > 20 && img.right.colors > 20, `colours ${img.left.colors} / ${img.right.colors}`);
    await page.evaluate(() => xrdev.activeSession.end());
    s = await waitState(page, (s) => !s.xr.active, 20000);

    check('lanvr: no application error', s.errors.length === 0, s.errors.join(' | '));
    check('lanvr: no console error', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
    check('lanvr: no WebGL warning', glWarnings(logs).length === 0, glWarnings(logs).join(' | '));
    await ctx.close();

    // ---- the desktop with a headset of its own (PC VR), on loopback
    const local = await headsetPage(browser);
    // The shared pipeline is still in the house the headset left.
    await open(local.page, `${server.url}/`, { scene: 'house' });
    s = await waitState(local.page, (s) => s.xr.supported === true && s.server && s.server.lan.enabled);
    await local.page.locator('#lan-fingerprint').scrollIntoViewIfNeeded();
    await local.page.waitForFunction(() => { const img = document.querySelector('#lan-qr'); return img && img.complete && img.naturalWidth > 0; }, null, { timeout: 15000 });
    check('lanvr: a desktop with its own headset gets "Entrer en VR" and the LAN sharing panel', (await local.page.locator('#enter-vr').isVisible()) && (await local.page.locator('#lan-toggle').getAttribute('aria-checked')) === 'true' && (await local.page.locator('#lan-urls code').first().innerText()).startsWith('https://'));
    await shot(local.page, 'lanvr-desktop-panel.png');
    await local.ctx.close();
  } finally {
    await server.stop();
    fs.rmSync(tmp, { recursive: true, force: true });
  }
  return {};
}

module.exports = { run };
if (require.main === module) require('./standalone.cjs')(run);
