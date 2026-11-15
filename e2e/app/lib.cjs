// Shared plumbing of the Splat Live application checks: build, static
// server, pages, the window.__splatLive hook, screenshots.
//
// The check recorder, the IWER (WebXR emulator) injection and the image
// statistics are the ones of e2e/render.
const { spawn, spawnSync } = require('child_process');
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
// `ranges: true` honours `Range: bytes=a-b` (206, Content-Range), as most
// static hosts do; `cors: true` allows any origin to read (another site
// that publishes recordings). The returned `ranged` array lists the Range
// headers received.
function serve({ dir = DIST, prefix = '/', port = PORT, ranges = true, cors = false } = {}) {
  if (!fs.existsSync(path.join(dir, 'index.html'))) throw new Error(`${dir} has no index.html: build the application first`);
  const requests = [];
  const ranged = [];
  const srv = http.createServer((req, res) => {
    const url = decodeURIComponent(req.url.split('?')[0]);
    requests.push(url);
    const extra = cors ? { 'Access-Control-Allow-Origin': '*', 'Access-Control-Expose-Headers': 'Content-Range, Content-Length', 'Access-Control-Allow-Headers': 'Range' } : {};
    if (req.method === 'OPTIONS') {
      res.writeHead(cors ? 204 : 405, extra).end();
      return;
    }
    if (!url.startsWith(prefix)) {
      res.writeHead(404, extra).end('not found');
      return;
    }
    const rel = url.slice(prefix.length);
    const file = path.join(dir, rel === '' ? 'index.html' : rel);
    if (!file.startsWith(dir) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
      res.writeHead(404, extra).end('not found');
      return;
    }
    const size = fs.statSync(file).size;
    const headers = { 'Content-Type': MIME[path.extname(file)] || 'application/octet-stream', 'Cache-Control': 'no-cache', ...extra };
    const range = ranges && /^bytes=(\d+)-(\d*)$/.exec(req.headers.range || '');
    if (range) {
      ranged.push(`${rel} ${req.headers.range}`);
      const start = Number(range[1]);
      const end = Math.min(range[2] === '' ? size - 1 : Number(range[2]), size - 1);
      if (start >= size || end < start) {
        res.writeHead(416, { ...headers, 'Content-Range': `bytes */${size}` }).end();
        return;
      }
      res.writeHead(206, { ...headers, 'Accept-Ranges': 'bytes', 'Content-Range': `bytes ${start}-${end}/${size}`, 'Content-Length': end - start + 1 });
      fs.createReadStream(file, { start, end }).pipe(res);
      return;
    }
    res.writeHead(200, { ...headers, 'Content-Length': size, ...(ranges ? { 'Accept-Ranges': 'bytes' } : {}) });
    fs.createReadStream(file).pipe(res);
  });
  return new Promise((resolve, reject) => {
    let tries = 0;
    srv.on('error', (e) => {
      if (e.code === 'EADDRINUSE' && ++tries < 60) srv.listen(port + tries, '127.0.0.1');
      else reject(e);
    });
    srv.on('listening', () => resolve({ srv, requests, ranged, url: `http://127.0.0.1:${srv.address().port}${prefix}` }));
    srv.listen(port, '127.0.0.1');
  });
}

// A copy of the build with more files next to it (`files`: served path ->
// source file, or a Buffer / string to write), as a static host that also
// publishes recordings would have. Returns the directory.
function site(name, files = {}) {
  const dir = path.join(OUT, `site-${name}`);
  fs.rmSync(dir, { recursive: true, force: true });
  fs.cpSync(DIST, dir, { recursive: true });
  for (const [rel, source] of Object.entries(files)) {
    const target = path.join(dir, rel);
    fs.mkdirSync(path.dirname(target), { recursive: true });
    if (typeof source === 'string' && fs.existsSync(source)) {
      // A hard link where possible: some recordings are large.
      try {
        fs.linkSync(source, target);
      } catch {
        fs.copyFileSync(source, target);
      }
    } else fs.writeFileSync(target, source);
  }
  return dir;
}

// The demo recordings of the record package (light preset), made on first
// use with its example and kept in the output directory: `stage` is 5 s
// of the stage, `house` a complete house scan, `long` a minute of the
// stage (about 120 MB: too large to be held in memory whole).
function recording(kind) {
  const file = path.join(OUT, `demo-${kind}.gsrec`);
  if (!fs.existsSync(file)) {
    fs.mkdirSync(OUT, { recursive: true });
    const scene = kind === 'house' ? ['house'] : ['stage', '--seconds', kind === 'long' ? '60' : '5'];
    const args = ['run', '--release', '-q', '-p', 'gs-core', '--example', 'gsrec_make', '--', scene[0], file, '--light', ...scene.slice(1)];
    const r = spawnSync('cargo', args, { cwd: REPO, stdio: ['ignore', 'inherit', 'inherit'] });
    if (r.status !== 0) throw new Error(`gsrec_make ${kind} failed`);
  }
  return file;
}

// New page with console capture; `iwer: true` installs an emulated Quest 3
// before any page script runs (window.xrdev).
//
// `blend` is the rendering choice the page starts with, through the
// setting the application stores ("auto", "soft", "hard", "sorted"). The
// suites pin it to "sorted": on the software rasterizer of a headless
// browser the normalised modes take 2 to 4.5 times longer per frame, and
// most checks here wait on the wall clock. The `render` suite and the
// screenshots of the product use `blend: null`, the application's own
// default.
async function newPage(browser, { blend = 'sorted', ...options } = {}) {
  const made = await render.newPage(browser, { width: 1280, height: 760, ...options });
  if (blend) await pinBlend(made.ctx, blend);
  return made;
}

// Makes every page of a browser context start with this rendering choice
// (for the contexts the suites create themselves, with their own options).
const pinBlend = (ctx, blend = 'sorted') =>
  ctx.addInitScript((blend) => {
    try {
      localStorage.setItem('splat-live.blend', blend);
    } catch {}
  }, blend);

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

// The native server of the server-mode suites.
const SERVER_BIN = process.env.GS_SERVER_BIN || path.join(REPO, 'target/release/gs-server');

// Starts gs-server on `port` (LAN listener on `port + 1`) serving `dist`,
// which must be a COPY of the build (Trunk wipes the original when it
// rebuilds). Resolves once `/api/info` answers. `args` are extra command
// line options (`--lan`, `--scene house`); the light preset is the default.
// Recordings and captures go to `recordings` and `captures` next to the
// certificate directory, never to the user's own data directory.
async function startServer({ dist, certs, port, args = ['--light'] }) {
  const data = path.dirname(certs);
  const dirs = ['--recordings-dir', path.join(data, 'recordings'), '--captures-dir', path.join(data, 'captures')];
  const child = spawn(SERVER_BIN, ['--port', String(port), '--lan-port', String(port + 1), '--dist', dist, '--cert-dir', certs, ...dirs, ...args], { stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '';
  child.stdout.on('data', (d) => (output += d));
  child.stderr.on('data', (d) => (output += d));
  const exited = new Promise((resolve) => child.on('exit', resolve));
  const stop = async () => {
    child.kill('SIGKILL');
    await exited;
  };
  for (let i = 0; i < 150; i++) {
    // The server takes the next free port when its own is busy.
    for (const candidate of [port, port + 2, port + 3]) {
      try {
        const info = await (await fetch(`http://127.0.0.1:${candidate}/api/info`)).json();
        if (info.name === 'splat-live') return { child, exited, stop, url: info.http_url, info, output: () => output };
      } catch {}
    }
    if (child.exitCode !== null) break;
    await sleep(100);
  }
  await stop();
  throw new Error(`gs-server did not start:\n${output}`);
}

module.exports = {
  REPO, APP, DIST, OUT, PORT, MIME,
  check, results, failures, iwerBundle, imageStats, hasContent,
  SERVER_BIN, startServer, build, serve, site, recording, newPage, pinBlend, consoleErrors, glWarnings, state, eye, waitState, open, shot, viewShot, diff, content, openPanel, setSlider, sleep,
};
