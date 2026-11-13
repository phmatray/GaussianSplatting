// Shared plumbing of the gs-render end-to-end checks: build, static server,
// browser pages, screenshots and a tiny check recorder.
const { chromium } = require('playwright');
const { spawnSync } = require('child_process');
const fs = require('fs');
const http = require('http');
const path = require('path');

const REPO = path.resolve(__dirname, '../..');
const HARNESS = path.join(REPO, 'crates/gs-render-harness');
const DIST = path.join(HARNESS, 'dist');
// Screenshots and reports. `out/` at the repository root is git-ignored.
const OUT = path.resolve(process.env.GS_RENDER_OUT || path.join(REPO, 'out/render-e2e'));

const results = [];
function check(name, ok, detail = '') {
  results.push({ name, ok: !!ok, detail: String(detail) });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? '  -- ' + detail : ''}`);
  return !!ok;
}
const failures = () => results.filter((r) => !r.ok);

function build() {
  console.log('building the harness (trunk build --release) ...');
  const r = spawnSync('trunk', ['build', '--release'], { cwd: HARNESS, stdio: 'inherit' });
  if (r.status !== 0) throw new Error('trunk build failed');
}

const MIME = { '.html': 'text/html', '.js': 'text/javascript', '.wasm': 'application/wasm', '.css': 'text/css' };
function serve(dir = DIST) {
  if (!fs.existsSync(path.join(dir, 'index.html'))) throw new Error(`${dir} has no index.html: build the harness first`);
  return new Promise((resolve) => {
    const srv = http.createServer((req, res) => {
      const rel = decodeURIComponent(req.url.split('?')[0]);
      const file = path.join(dir, rel === '/' ? 'index.html' : rel);
      if (!file.startsWith(dir) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
        res.writeHead(404).end('not found');
        return;
      }
      res.writeHead(200, { 'Content-Type': MIME[path.extname(file)] || 'application/octet-stream', 'Cache-Control': 'no-store' });
      fs.createReadStream(file).pipe(res);
    });
    srv.listen(0, '127.0.0.1', () => resolve({ srv, url: `http://127.0.0.1:${srv.address().port}` }));
  });
}

// The IWER WebXR emulator bundle (UMD, defines window.IWER). Looked up in
// e2e/render/node_modules (npm install), then in $GS_NPM_TOOLS (a node_modules
// directory), or given directly as $IWER_BUNDLE.
function iwerBundle() {
  if (process.env.IWER_BUNDLE) return process.env.IWER_BUNDLE;
  const paths = [__dirname, path.join(REPO, 'e2e')];
  if (process.env.GS_NPM_TOOLS) paths.push(path.dirname(process.env.GS_NPM_TOOLS));
  try {
    return require.resolve('iwer/build/iwer.min.js', { paths });
  } catch {
    return null;
  }
}

// New browser context + page with console capture. `iwer: true` installs an
// emulated Meta Quest 3 before any page script runs (window.xrdev).
async function newPage(browser, { width = 960, height = 600, iwer = false, hasTouch = false, deviceScaleFactor = 1 } = {}) {
  const ctx = await browser.newContext({ viewport: { width, height }, hasTouch, deviceScaleFactor });
  const page = await ctx.newPage();
  const logs = [];
  page.on('console', (m) => logs.push(`[${m.type()}] ${m.text()}`));
  // With the script the exception came from: the emulator's own exceptions
  // (it does not expect a lost WebGL context) can then be told apart.
  page.on('pageerror', (e) => logs.push(`[pageerror] ${e.message}${/iwer/i.test(e.stack || '') ? ' (thrown inside the IWER emulator)' : ''}`));
  if (iwer) {
    await page.addInitScript({ path: iwerBundle() });
    await page.addInitScript(() => {
      // stereoEnabled defaults to false (one full-width view); forceInstall
      // replaces Chromium's own (useless on a desktop) navigator.xr.
      window.xrdev = new IWER.XRDevice(IWER.metaQuest3, { stereoEnabled: true });
      window.xrdev.installRuntime({ forceInstall: true });
    });
  }
  return { ctx, page, logs };
}

// Console lines that mean something broke. WebGL reports misuse (a deleted
// texture, a wrong enum ...) as console *warnings*: they count. SwiftShader's
// performance notes about readPixels are not errors.
function consoleErrors(logs) {
  return logs.filter(
    (l) =>
      (l.startsWith('[pageerror]') || l.startsWith('[error]') || /panicked|RuntimeError|already borrowed|unreachable|WebGL: INVALID_|GL_INVALID_|GL ERROR/.test(l)) &&
      !/GPU stall due to ReadPixels/.test(l),
  );
}

const state = (page) => page.evaluate(() => window.__harness.state());
const cmd = (page, name, arg) => page.evaluate(([n, a]) => window.__harness.cmd(n, a), [name, arg]);

// Waits until the live page has drawn a few frames with data in both layers.
async function waitLive(page) {
  await page.waitForFunction(
    () => {
      if (window.__error) throw new Error('harness failed: ' + window.__error);
      const s = window.__harness && window.__harness.state();
      return s && s.framesRendered > 3 && s.staticSplats > 0 && s.ticks > 1;
    },
    null,
    { timeout: 60000 },
  );
}

// Waits until the desktop camera has stopped moving (damping finished).
async function settle(page) {
  let last = null;
  for (let i = 0; i < 100; i++) {
    const s = await state(page);
    const now = JSON.stringify([s.view, s.flySpeed]);
    if (now === last) return s;
    last = now;
    await page.waitForTimeout(60);
  }
  throw new Error('the camera never settled');
}

async function shot(page, name) {
  fs.mkdirSync(OUT, { recursive: true });
  const file = path.join(OUT, name);
  await page.screenshot({ path: file });
  return file;
}

// Statistics of a screenshot, computed in the page with a 2D canvas:
// per-half color variety and non-background fraction, similarity of the two
// halves (a stereo pair is very similar but not identical), and optionally the
// mean absolute difference to another screenshot.
async function imageStats(page, file, otherFile = null) {
  const b64 = fs.readFileSync(file).toString('base64');
  const other = otherFile ? fs.readFileSync(otherFile).toString('base64') : null;
  return page.evaluate(
    async ([b64, other]) => {
      const load = async (data) => {
        const img = new Image();
        img.src = 'data:image/png;base64,' + data;
        await img.decode();
        const c = document.createElement('canvas');
        c.width = img.width;
        c.height = img.height;
        const ctx = c.getContext('2d', { willReadFrequently: true });
        ctx.drawImage(img, 0, 0);
        return { ctx, w: img.width, h: img.height };
      };
      const a = await load(b64);
      const half = (x0) => {
        const d = a.ctx.getImageData(x0, 0, a.w / 2, a.h).data;
        const colors = new Set();
        let bright = 0;
        for (let i = 0; i < d.length; i += 4 * 7) {
          colors.add(((d[i] >> 4) << 8) | ((d[i + 1] >> 4) << 4) | (d[i + 2] >> 4));
          if (d[i] > 40 || d[i + 1] > 40 || d[i + 2] > 50) bright++;
        }
        return { colors: colors.size, brightFrac: bright / (d.length / 28) };
      };
      const L = a.ctx.getImageData(0, 0, a.w / 2, a.h).data;
      const R = a.ctx.getImageData(a.w / 2, 0, a.w / 2, a.h).data;
      let lr = 0, identical = 0, n = 0;
      for (let i = 0; i < L.length; i += 4 * 5) {
        const dd = Math.abs(L[i] - R[i]) + Math.abs(L[i + 1] - R[i + 1]) + Math.abs(L[i + 2] - R[i + 2]);
        lr += dd;
        if (dd === 0) identical++;
        n++;
      }
      const out = { width: a.w, height: a.h, left: half(0), right: half(a.w / 2), lrMeanAbsDiff: lr / n / 3, lrIdenticalFrac: identical / n };
      if (other) {
        const b = await load(other);
        if (b.w === a.w && b.h === a.h) {
          const A = a.ctx.getImageData(0, 0, a.w, a.h).data;
          const B = b.ctx.getImageData(0, 0, a.w, a.h).data;
          let sum = 0, changed = 0;
          for (let i = 0; i < A.length; i += 4) {
            const dd = Math.abs(A[i] - B[i]) + Math.abs(A[i + 1] - B[i + 1]) + Math.abs(A[i + 2] - B[i + 2]);
            sum += dd;
            if (dd > 24) changed++;
          }
          out.diffMeanAbs = sum / (A.length / 4) / 3;
          out.diffChangedFrac = changed / (A.length / 4);
        } else {
          out.diffMeanAbs = Infinity;
          out.diffChangedFrac = 1;
        }
      }
      return out;
    },
    [b64, other],
  );
}

const hasContent = (half) => half.colors > 20 && half.brightFrac > 0.1;

// Runs `run({ browser, url })` standalone: serves the existing dist (set
// BUILD=1 to rebuild first), prints the summary and sets the exit code.
async function main(run) {
  if (process.env.BUILD === '1') build();
  const { srv, url } = await serve();
  const browser = await chromium.launch();
  try {
    await run({ browser, url });
  } catch (e) {
    check('no uncaught exception', false, e.stack || e);
  } finally {
    await browser.close();
    srv.close();
  }
  console.log(`\n${results.length - failures().length}/${results.length} checks passed`);
  process.exit(failures().length ? 1 : 0);
}

module.exports = { REPO, OUT, results, failures, check, build, serve, iwerBundle, newPage, consoleErrors, state, cmd, waitLive, settle, shot, imageStats, hasContent, main };
