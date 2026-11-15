// Pictures of the product for a person to look at, at twice the pixel
// density and with the application's own rendering defaults (normalised
// blending): the stage and the finished house on a desktop and in the
// emulated headset, the time line during a replay, the share dialog, the
// embedded viewer inside a page, the pairing panel, a capture in progress.
//
//   ONLY=shots node e2e/app/run.cjs      (not part of the default run)
//   GS_APP_SHOTS=dir                      where the pictures go (default: <out>/shots)
//
// With a built gs-server (default preset: the stage of eight 240x180
// cameras, the house at 2.4 cm) the pictures are the desktop product's;
// without one, the worker's light preset is used and the server-only
// pictures are skipped. On the software rasterizer this takes minutes.
const { chromium } = require('playwright');
const { spawnSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { check, REPO, OUT, DIST, PORT, iwerBundle, newPage, serve, state, waitState, open, sleep, startServer, SERVER_BIN: BIN } = require('./lib.cjs');
const { synth } = require('./capture.cjs');
const { fakePhone } = require('./studio.cjs');
const { waitFor } = require('./embed.cjs');

const SHOTS = path.resolve(process.env.GS_APP_SHOTS || path.join(OUT, 'shots'));
const taken = [];

async function snap(page, name, clip) {
  fs.mkdirSync(SHOTS, { recursive: true });
  const file = path.join(SHOTS, name);
  // Two frames, so that what is on screen is what the state describes.
  await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
  // A page that draws this slowly takes its time to hand a frame over.
  await page.screenshot({ path: file, timeout: 240000, ...(clip ? { clip } : {}) });
  taken.push(file);
  return file;
}

// A realistic simulated capture (a furnished bedroom filmed by a simulated
// phone) when the example of the ingest package is built; otherwise the
// box room of the reference tool.
function capture(name) {
  const example = path.join(REPO, 'target/release/examples/gscap_write');
  const file = path.join(OUT, name);
  if (fs.existsSync(example) && spawnSync(example, ['bedroom', file, '--frames', '160', '--color-width', '480'], { stdio: 'ignore' }).status === 0) return file;
  return synth(name, { frames: 120, title: 'Chambre' });
}

const hd = { blend: null, deviceScaleFactor: 2 };

// A click made from inside the page: at twice the density the software
// rasterizer keeps the page busy for seconds at a time, and a click
// through the input pipeline can wait longer than its timeout.
const tap = (page, selector) => page.locator(selector).first().evaluate((el) => el.click());
// The same for scrolling something into view (Playwright's own waits for two
// steady frames, which can take longer than its timeout here).
const reveal = (page, selector) => page.locator(selector).first().evaluate((el) => el.scrollIntoView({ block: 'nearest' }));

// The scene alone, then with the interface.
async function pair(page, name) {
  await page.keyboard.press('h');
  await page.waitForFunction(() => window.__splatLive.state().uiHidden);
  await snap(page, `${name}-view.png`);
  await page.keyboard.press('h');
  await page.waitForFunction(() => !window.__splatLive.state().uiHidden);
  await snap(page, `${name}.png`);
}

async function scenes(browser, url) {
  // ---- the stage
  const { ctx, page } = await newPage(browser, hd);
  let s = await open(page, url);
  await waitState(page, (s) => s.stats.tick > 20 && Object.keys(s.previews.painted).length === s.previews.ids.length, 120000);
  await sleep(1500);
  await pair(page, 'stage-desktop');
  // ---- the finished house: from above, then inside
  await tap(page, '#mode-house');
  await waitState(page, (s) => s.scene === 'house' && s.scan && s.scan.state === 'scanning' && s.layers.static > 0, 120000);
  await tap(page, '#scan-finish');
  s = await waitState(page, (s) => s.scan && s.scan.state === 'done' && s.stats.splats_static === s.layers.static, 1200000);
  await sleep(1500);
  await pair(page, 'house-plan-desktop');
  await tap(page, '#visit-house');
  await waitState(page, (s) => !s.display.dollhouse && s.nav === 'fly');
  await sleep(1500);
  await pair(page, 'house-desktop');
  const report = { splats: s.layers.static, blend: s.viewer.blend, splatScale: s.display.splatScale };
  await ctx.close();
  return report;
}

async function headset(browser, url, houseDone) {
  if (!iwerBundle()) return;
  // 1280x640: each eye is square; twice the density gives 1280x1280 per eye.
  const { ctx, page } = await newPage(browser, { ...hd, width: 1280, height: 640, iwer: true });
  await open(page, url, { scene: houseDone ? 'house' : 'stage', timeout: 180000 }).catch(() => open(page, url));
  await page.addStyleTag({ content: '#xr-card { visibility: hidden; }' });
  const enter = async () => {
    await waitState(page, (s) => s.xr.supported === true);
    await tap(page, '#btn-vr');
    return waitState(page, (s) => s.xr.active && s.xr.session && s.xr.session.frames > 8 && s.xr.session.viewports.length === 2, 120000);
  };
  const leave = async () => {
    await page.evaluate(() => document.getElementById('exit-vr').click());
    await waitState(page, (s) => !s.xr.active, 60000);
  };
  let s = await state(page);
  if (s.scene === 'house') {
    s = await waitState(page, (s) => s.scan && s.scan.state === 'done' && s.stats.splats_static === s.layers.static, 1200000);
    await tap(page, '#visit-house').catch(() => {});
    await sleep(500);
    await enter();
    await sleep(2500);
    await snap(page, 'house-headset.png');
    await leave();
    await tap(page, '#mode-stage');
    await waitState(page, (s) => s.scene === 'stage' && s.layers.dynamic > 0, 120000);
  }
  await enter();
  await sleep(2500);
  await snap(page, 'stage-headset.png');
  await leave();
  await ctx.close();
}

// Recording, replay, share, pairing, capture: with the server.
async function studio(browser, url, tmp) {
  const { ctx, page } = await newPage(browser, hd);
  let s = await open(page, url);
  // ---- record a few seconds, replay them
  await tap(page, '#btn-record');
  await waitState(page, (s) => s.recording && s.recording.durationS >= 4, 120000);
  await reveal(page, '#section-record');
  await snap(page, 'recording.png');
  await tap(page, '#btn-record');
  s = await waitState(page, (s) => !s.recording && s.takes.some((t) => t.scene === 'stage' && t.durationS > 0), 60000);
  const take = s.takes.find((t) => t.scene === 'stage' && t.durationS > 0);
  await tap(page, `#takes .take[data-file="${take.file}"] .take-play`);
  await waitState(page, (s, t) => s.replay && s.replay.status.ready && s.replay.status.time > t, 240000, Math.min(take.durationS * 0.45, 2));
  await page.keyboard.press('k');
  await waitState(page, (s) => !s.replay.status.playing, 120000);
  await sleep(800);
  await snap(page, 'timeline.png');

  // ---- the emulated headset during the replay
  if (iwerBundle()) {
    const vr = await newPage(browser, { ...hd, width: 1280, height: 640, iwer: true });
    await vr.page.goto(`${url}?file=recordings/${encodeURIComponent(take.file)}&loop=1&autoplay=1`);
    await waitFor(vr.page, (s) => s.replay && s.replay.status.ready && s.xr.supported === true);
    await vr.page.addStyleTag({ content: '#xr-card { visibility: hidden; }' });
    await tap(vr.page, '#btn-vr');
    await waitFor(vr.page, (s) => s.xr.active && s.xr.session && s.xr.session.frames > 8 && s.xr.session.viewports.length === 2 && s.xr.session.controllers === 2, 120000);
    await vr.page.evaluate(() => {
      xrdev.controllers.left.position.set(-0.1, 1.42, -0.4);
      xrdev.controllers.left.quaternion.set(0, 0, 0, 1);
      xrdev.controllers.right.position.set(0.25, 1.2, -0.35);
    });
    await waitFor(vr.page, (s) => s.replay.status.playing && s.replay.status.time > 1.2, 120000);
    await sleep(1200);
    await snap(vr.page, 'headset-replay.png');
    await vr.ctx.close();
  }
  await tap(page, '#tl-close');
  await waitState(page, (s) => !s.replay && s.connected && s.layers.dynamic > 0, 120000);
  // The pictures of the dialogs are about the interface: with the
  // pipeline paused the view stands still and the page answers at once.
  await page.keyboard.press('p');
  await waitState(page, (s) => s.config && s.config.paused, 120000);

  // ---- the share dialog
  await tap(page, `#takes .take[data-file="${take.file}"] .take-share`);
  await page.waitForSelector('#share-dialog');
  if (await page.locator('#share-lan-on').count()) await tap(page, '#share-lan-on');
  const lan = await page.waitForSelector('#share-qr', { timeout: 30000 }).catch(() => null);
  await sleep(500);
  await snap(page, 'share-dialog.png');
  const link = lan ? await page.inputValue('#share-link') : null;
  await page.keyboard.press('Escape');

  // ---- the embedded viewer inside a page of another origin
  const hostDir = path.join(tmp, 'host');
  fs.mkdirSync(hostDir, { recursive: true });
  fs.copyFileSync(path.join(__dirname, 'embed.html'), path.join(hostDir, 'embed.html'));
  fs.writeFileSync(path.join(hostDir, 'index.html'), '<!doctype html><title>hôte</title>');
  const host = await serve({ dir: hostDir, port: PORT + 86 });
  const framed = await newPage(browser, { ...hd, width: 1100, height: 800 });
  const src = `${url}?file=recordings/${encodeURIComponent(take.file)}&embed=1&autoplay=1&loop=1`;
  await framed.page.goto(`${host.url}embed.html?src=${encodeURIComponent(src)}`);
  const frame = framed.page.frames().find((f) => f !== framed.page.mainFrame()) || (await framed.page.waitForEvent('frameattached'));
  await waitFor(frame, (s) => s.replay && s.replay.status.ready && s.replay.status.playing && s.replay.status.time > 1.0, 180000);
  await sleep(600);
  await snap(framed.page, 'embed-iframe.png');
  await framed.ctx.close();
  host.srv.close();

  // ---- pairing, then a capture in progress
  if (lan) {
    await reveal(page, '#section-phone');
    await tap(page, '#pair-start');
    await page.waitForSelector('#pair-qr', { timeout: 30000 });
    await reveal(page, '#pair');
    await sleep(400);
    await snap(page, 'pairing.png');
    await page.keyboard.press('p');
    await waitState(page, (s) => s.config && !s.config.paused, 120000);
    const uri = (await (await fetch(`${url}api/pair`)).json()).uri;
    const file = capture('shots-capture.gscap');
    const phone = fakePhone(file, uri, ['--realtime', '--speed', '0.5']);
    s = await waitState(page, (s) => s.capture && s.capture.stats && s.capture.stats.frames_used >= 45 && s.layers.static > 20000, 600000);
    await reveal(page, '#section-capture');
    await page.evaluate(() => document.querySelector('.panel-scroll').scrollTo(0, 0));
    await sleep(800);
    await snap(page, 'capture-live.png');
    await phone.done;
    await waitState(page, (s) => s.capture && s.capture.stats && s.capture.stats.finished, 120000);
    await sleep(1500);
    await pair(page, 'capture-finished');
    fs.rmSync(file, { force: true });
  }
  await ctx.close();
  return link;
}

// The static build alone: a capture file ingested by the worker.
async function local(browser, url) {
  const { ctx, page } = await newPage(browser, hd);
  await open(page, url);
  const file = capture('shots-import.gscap');
  await reveal(page, '#section-phone');
  await page.setInputFiles('#gscap-input', file);
  await waitState(page, (s) => s.import && s.capture && s.capture.stats && s.capture.stats.frames_used >= 40 && s.layers.static > 10000, 900000);
  await sleep(500);
  await snap(page, 'capture-import-worker.png');
  fs.rmSync(file, { force: true });
  await ctx.close();
}

async function run({ browser, url }) {
  const report = {};
  // SHOTS_PARTS=studio,local takes some of the pictures only.
  const parts = (process.env.SHOTS_PARTS || 'scenes,headset,studio,local').split(',');
  const wanted = (part) => parts.includes(part);
  if (fs.existsSync(BIN)) {
    const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'splat-live-shots-'));
    const dist = path.join(tmp, 'dist');
    fs.cpSync(DIST, dist, { recursive: true });
    try {
      if (wanted('scenes') || wanted('headset')) {
        // The desktop product's own preset, not the light one.
        const server = await startServer({ dist, certs: path.join(tmp, 'certs'), port: PORT + 82, args: [] });
        try {
          const serverUrl = `${server.url}/`;
          if (wanted('scenes')) report.scenes = await scenes(browser, serverUrl);
          if (wanted('headset')) await headset(browser, serverUrl, wanted('scenes'));
        } finally {
          await server.stop();
        }
      }
      if (wanted('studio')) {
        // The pictures of the interface need a page that answers: the
        // light preset, which the rasterizer of this browser keeps up with.
        const server = await startServer({ dist, certs: path.join(tmp, 'certs'), port: PORT + 82, args: ['--light'] });
        try {
          report.link = await studio(browser, `${server.url}/`, tmp);
        } finally {
          await server.stop();
        }
      }
    } finally {
      fs.rmSync(tmp, { recursive: true, force: true });
    }
  } else {
    console.log(`no ${BIN}: pictures of the worker's light preset only`);
    report.scenes = await scenes(browser, url);
    await headset(browser, url, false);
  }
  if (wanted('local')) await local(browser, url);
  check('shots: every picture was written', taken.length >= 1 && taken.every((f) => fs.statSync(f).size > 20000), `${taken.length} pictures in ${SHOTS}`);
  return { ...report, pictures: taken.map((f) => path.basename(f)) };
}

module.exports = { run };
if (require.main === module) require('./standalone.cjs')(run);
