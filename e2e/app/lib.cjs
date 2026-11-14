// Shared plumbing of the Splat Live application checks: build, static
// server, pages, the window.__splatLive hook, screenshots.
//
// The check recorder, the IWER (WebXR emulator) injection and the image
// statistics are the ones of e2e/render.
const { spawnSync } = require('child_process');
const fs = require('fs');
const http = require('http');
const path = require('path');
const render = require('../render/lib.cjs');

const REPO = path.resolve(__dirname, '../..');
const APP = path.join(REPO, 'crates/gs-app');
const DIST = path.join(APP, 'dist');
// Screenshots and reports. `out/` at the repository root is git-ignored.
const OUT = path.resolve(process.env.GS_APP_OUT || path.join(REPO, 'out/app-e2e'));
// First port tried by the static server (the range 8100-8199 is ours).
const PORT = Number(process.env.GS_APP_PORT || 8110);

const { check, results, failures, iwerBundle, imageStats, hasContent } = render;

function build() {
  console.log('building the application (trunk build --release in crates/gs-app) ...');
  const r = spawnSync('trunk', ['build', '--release'], { cwd: APP, stdio: 'inherit' });
  if (r.status !== 0) throw new Error('trunk build failed');
}

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript',
  '.wasm': 'application/wasm',
  '.css': 'text/css',
  '.svg': 'image/svg+xml',
};

// Serves `dir` under `prefix` ("/" or a sub-path such as "/demo/splat/") the
// way a static host does: no API, 404 for anything that is not a file.
function serve({ dir = DIST, prefix = '/', port = PORT } = {}) {
  if (!fs.existsSync(path.join(dir, 'index.html'))) throw new Error(`${dir} has no index.html: build the application first`);
  const requests = [];
  const srv = http.createServer((req, res) => {
    const url = decodeURIComponent(req.url.split('?')[0]);
    requests.push(url);
    if (!url.startsWith(prefix)) {
      res.writeHead(404).end('not found');
      return;
    }
    const rel = url.slice(prefix.length);
    const file = path.join(dir, rel === '' ? 'index.html' : rel);
    if (!file.startsWith(dir) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
      res.writeHead(404).end('not found');
      return;
    }
    res.writeHead(200, { 'Content-Type': MIME[path.extname(file)] || 'application/octet-stream', 'Cache-Control': 'no-cache' });
    fs.createReadStream(file).pipe(res);
  });
  return new Promise((resolve, reject) => {
    let tries = 0;
    srv.on('error', (e) => {
      if (e.code === 'EADDRINUSE' && ++tries < 60) srv.listen(port + tries, '127.0.0.1');
      else reject(e);
    });
    srv.on('listening', () => resolve({ srv, requests, url: `http://127.0.0.1:${srv.address().port}${prefix}` }));
    srv.listen(port, '127.0.0.1');
  });
}

// New page with console capture; `iwer: true` installs an emulated Quest 3
// before any page script runs (window.xrdev).
const newPage = (browser, options = {}) => render.newPage(browser, { width: 1280, height: 760, ...options });

// Console lines that mean something broke (errors, panics, WebGL misuse).
// The 404 of the /api/info probe on a static host is the documented way the
// page finds out that there is no server.
function consoleErrors(logs) {
  return render.consoleErrors(logs).filter((l) => !/Failed to load resource.*404/.test(l));
}
// WebGL warnings that are only performance notes of the software rasterizer.
const glWarnings = (logs) => logs.filter((l) => /WebGL|GL_|GL Driver/i.test(l) && !/GPU stall due to ReadPixels|Automatic fallback to software WebGL/i.test(l));

const state = (page) => page.evaluate(() => window.__splatLive.state());
const eye = (page) => page.evaluate(() => window.__splatLive.viewerEye());

// Waits until `predicate(state)` is true; returns the state. The predicate
// runs in the page, so it must not use anything from this file.
async function waitState(page, predicate, timeout = 60000, arg = null) {
  await page.waitForFunction(
    ([source, arg]) => {
      const hook = window.__splatLive;
      if (!hook) return false;
      const s = hook.state();
      return s && new Function('s', 'arg', `return (${source})(s, arg)`)(s, arg);
    },
    [predicate.toString(), arg],
    { timeout, polling: 100 },
  );
  return state(page);
}

// Loads the application and waits for the first complete picture.
async function open(page, url, { scene = 'stage', timeout = 90000 } = {}) {
  await page.goto(url);
  return waitState(
    page,
    (s, scene) => s.connected && s.scene === scene && s.layers.static + s.layers.dynamic > 0 && s.viewer.framesRendered > 2 && s.counts.stats > 1,
    timeout,
    scene,
  );
}

async function shot(page, name) {
  fs.mkdirSync(OUT, { recursive: true });
  const file = path.join(OUT, name);
  await page.screenshot({ path: file });
  return file;
}

// Screenshot of the 3D view alone: the interface is hidden with the H key
// so that only splats (and gizmos) are compared.
async function viewShot(page, name) {
  await page.keyboard.press('h');
  await page.waitForFunction(() => window.__splatLive.state().uiHidden);
  // Two frames, so that what is on screen is what the state describes.
  await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
  const file = await shot(page, name);
  await page.keyboard.press('h');
  await page.waitForFunction(() => !window.__splatLive.state().uiHidden);
  return file;
}

// Mean absolute difference and changed fraction between two screenshots.
const diff = async (page, a, b) => {
  const s = await imageStats(page, a, b);
  return { mean: s.diffMeanAbs, changed: s.diffChangedFrac };
};

// Whole-image content: colour variety and fraction of non-background pixels.
const content = async (page, file) => {
  const s = await imageStats(page, file);
  return { colors: s.left.colors + s.right.colors, bright: (s.left.brightFrac + s.right.brightFrac) / 2, left: s.left, right: s.right, raw: s };
};

// Opens the side panel if it is closed (narrow windows start with it closed).
async function openPanel(page) {
  if (!(await state(page)).panelOpen) await page.click('#btn-panel');
  await page.waitForFunction(() => window.__splatLive.state().panelOpen);
}

// Sets a range input the way a user drag does: value, then an input event.
const setSlider = (page, selector, value) =>
  page.$eval(
    selector,
    (el, value) => {
      el.value = String(value);
      el.dispatchEvent(new Event('input', { bubbles: true }));
    },
    value,
  );

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

module.exports = {
  REPO, APP, DIST, OUT, PORT, MIME,
  check, results, failures, iwerBundle, imageStats, hasContent,
  build, serve, newPage, consoleErrors, glWarnings, state, eye, waitState, open, shot, viewShot, diff, content, openPanel, setSlider, sleep,
};
