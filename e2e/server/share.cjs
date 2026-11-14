// Sharing, end to end: a recording made by the real `gs-server` is exported
// as a share archive, unpacked, served by a plain static web server
// (`python3 -m http.server`) and opened in headless Chromium, where the
// real web build must start on the recording by itself. The same recording
// is then opened through the link of the local network
// (`https://<lan address>/?file=recordings/<name>.gsrec&embed=1`).
//
// Two recordings are shared: a capture streamed by `tools/fake_phone.py`
// (the simulated phone of gs-core when its example binary exists, the
// synthetic walk of `tools/gscap.py` otherwise), and a few seconds of the
// live stage recorded with `POST /api/record`.
//
//   cargo build --release -p gs-server
//   (cd crates/gs-app && trunk build --release)
//   node e2e/server/share.cjs [port]      (default 7895; uses port .. port + 3)
//
//   GS_SERVER_BIN=path   binary to test (default target/release/gs-server)
//   GS_DIST=dir          web build (default crates/gs-app/dist)
//   OUT=dir              where screenshots and logs go (default target/e2e-server)
const { spawn, spawnSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { chromium } = require('playwright');

const REPO = path.resolve(__dirname, '../..');
const BIN = process.env.GS_SERVER_BIN || path.join(REPO, 'target/release/gs-server');
const DIST = process.env.GS_DIST || path.join(REPO, 'crates/gs-app/dist');
const SIMULATOR = process.env.GS_GSCAP_WRITE || path.join(REPO, 'target/release/examples/gscap_write');
const OUT = process.env.OUT || path.join(REPO, 'target/e2e-server');
const PORT = Number(process.argv[2] || 7895);
const TOOLS = path.join(REPO, 'tools');

let failed = 0;
function check(name, ok, detail = '') {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${name}${detail ? `  (${detail})` : ''}`);
  if (!ok) failed += 1;
}
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function python(args, options = {}) {
  return spawnSync('python3', args, { encoding: 'utf8', ...options });
}

async function startServer(dist, data) {
  const args = ['--port', String(PORT), '--lan', '--lan-port', String(PORT + 1), '--dist', dist, '--light',
    '--cert-dir', path.join(data, 'cert'), '--recordings-dir', path.join(data, 'recordings')];
  const child = spawn(BIN, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '';
  child.stdout.on('data', (d) => { output += d; });
  child.stderr.on('data', (d) => { output += d; });
  const exited = new Promise((resolve) => child.on('exit', (code) => resolve(code)));
  for (let i = 0; i < 100 && child.exitCode === null; i += 1) {
    try {
      const info = await (await fetch(`http://127.0.0.1:${PORT}/api/info`)).json();
      if (info.name === 'splat-live') return { child, info, exited, output: () => output };
    } catch (_) { /* not up yet */ }
    await sleep(100);
  }
  child.kill('SIGKILL');
  throw new Error(`gs-server did not start on port ${PORT}:\n${output}`);
}

// A static web server for one directory, as anybody would start it.
async function startStatic(dir, port) {
  const child = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1', '--directory', dir], { stdio: 'ignore' });
  for (let i = 0; i < 50; i += 1) {
    try {
      if ((await fetch(`http://127.0.0.1:${port}/share.json`)).ok) return child;
    } catch (_) { /* not up yet */ }
    await sleep(100);
  }
  child.kill('SIGKILL');
  throw new Error('python3 -m http.server did not start');
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

// Opens the viewer and waits until it replays a recording: what it holds,
// what it draws, and whether it moves.
async function watch(browser, url, shot) {
  const context = await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 1280, height: 760 }, deviceScaleFactor: 1 });
  const page = await context.newPage();
  const errors = [];
  const requests = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  page.on('response', (r) => requests.push({ url: r.url(), status: r.status(), range: r.request().headers().range || '' }));
  await page.goto(url);
  const state = () => page.evaluate(() => (window.__splatLive ? window.__splatLive.state() : null));
  const ready = await page.waitForFunction(() => {
    const s = window.__splatLive && window.__splatLive.state();
    // A still or a paused recording is drawn once, not sixty times a second.
    return s && s.replay && s.replay.status && s.replay.status.ready && (s.layers.static + s.layers.dynamic) > 0
      && s.viewer.framesRendered >= 1 && s.viewer.splatsDrawn > 0;
  }, null, { timeout: 90000 }).then(() => true, () => false);
  const first = await state();
  await sleep(2500);
  const second = await state();
  const picture = await pictureStats(page, path.join(OUT, shot));
  await context.close();
  return { ready, first, second, picture, errors, requests };
}

function judge(label, seen, moving) {
  const s = seen.second;
  if (!seen.ready || !s) {
    check(`${label}: the viewer started on the recording`, false, JSON.stringify(s && { source: s.source, replay: s.replay, layers: s.layers, viewer: s.viewer, errors: s.errors, fileError: s.fileError, notice: s.notice }) + ' ' + seen.errors.join(' | '));
    return;
  }
  check(`${label}: the viewer started on the recording`, true, JSON.stringify(s.replay.origin));
  check(`${label}: it holds and draws the scene`, s.layers.static + s.layers.dynamic > 1000 && s.viewer.splatsDrawn > 1000,
    `${s.layers.static} static + ${s.layers.dynamic} live splats, ${s.viewer.splatsDrawn} drawn`);
  check(`${label}: the picture is not blank`, seen.picture.colors > 40 && seen.picture.varied > 0.03,
    `${seen.picture.colors} colours, ${(seen.picture.varied * 100).toFixed(1)} % off the dominant one`);
  if (moving) {
    const layers = s.counts.layers - seen.first.counts.layers;
    check(`${label}: the recording plays (the live layer keeps changing)`, layers >= 3, `${layers} layers applied in 2.5 s`);
  }
  const bad = seen.requests.filter((r) => r.status >= 400);
  check(`${label}: no failed request, no page error`, bad.length === 0 && seen.errors.length === 0 && s.errors.length === 0,
    [...bad.map((r) => `${r.status} ${r.url}`), ...seen.errors, ...s.errors].join(' | '));
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
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'gs-share-'));
  const dist = path.join(tmp, 'dist');
  fs.cpSync(DIST, dist, { recursive: true });
  const server = await startServer(dist, tmp);
  const base = `http://127.0.0.1:${PORT}`;
  const lanUrl = server.info.lan.urls[0];
  const lanHosts = server.info.lan.urls.map((u) => new URL(u).hostname);
  // In sandboxes with an egress proxy, the machine's own addresses must not go through it.
  const proxy = process.env.HTTPS_PROXY ? { server: process.env.HTTPS_PROXY, bypass: ['127.0.0.1', 'localhost', ...lanHosts].join(',') } : undefined;
  const browser = await chromium.launch({ proxy });
  const statics = [];
  try {
    // -- two recordings ---------------------------------------------------
    const post = (action) => fetch(`${base}/api/record`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(action) });
    const started = await (await post({ action: 'start', name: 'Danse sur scène' })).json();
    check('POST /api/record starts a recording', !!started.recording, JSON.stringify(started));

    const capture = path.join(tmp, 'capture.gscap');
    if (fs.existsSync(SIMULATOR)) {
      const made = spawnSync(SIMULATOR, ['bedroom', capture, '--color-width', '480'], { encoding: 'utf8' });
      check('the simulated phone films the bedroom', made.status === 0, made.stderr.split('\n').slice(-2).join(' '));
    } else {
      const made = python([path.join(TOOLS, 'gscap.py'), 'synth', capture, '--frames', '60', '--name', 'Marche']);
      check('tools/gscap.py writes a synthetic walk', made.status === 0, made.stderr);
    }
    // The stage dances on the recording for a few seconds before the
    // capture takes the viewers over.
    await sleep(4000);
    const pair = await (await fetch(`${base}/api/pair`)).json();
    if (!pair.uri) {
      check('GET /api/pair gives a pairing URI', false, JSON.stringify(pair));
      return;
    }
    const stopped = await (await post({ action: 'stop' })).json();
    check('POST /api/record stops it', !!stopped.saved && stopped.saved.duration_s > 3, JSON.stringify(stopped.saved));
    const streamed = python([path.join(TOOLS, 'fake_phone.py'), 'stream', capture, pair.uri]);
    check('tools/fake_phone.py streams the capture', streamed.status === 0, streamed.stderr.trim().split('\n').pop());

    let list = [];
    for (let i = 0; i < 100; i += 1) {
      list = (await (await fetch(`${base}/api/recordings`)).json()).recordings;
      if (list.length >= 2 && list.every((r) => !r.recording)) break;
      await sleep(100);
    }
    const stage = list.find((r) => r.scene === 'stage');
    const scan = list.find((r) => r.scene === 'capture');
    check('both are listed', !!stage && !!scan, JSON.stringify(list.map((r) => [r.file, r.scene, r.duration_s])));
    if (!stage || !scan) return;

    // -- the share archive, unpacked onto a static host -------------------
    let port = PORT + 2;
    for (const [label, entry, moving] of [['capture', scan, false], ['stage', stage, true]]) {
      const answer = await fetch(`${base}/api/share/${encodeURIComponent(entry.name)}.zip`);
      const zip = Buffer.from(await answer.arrayBuffer());
      const zipPath = path.join(tmp, `${label}.zip`);
      fs.writeFileSync(zipPath, zip);
      check(`${label}: GET /api/share/<name>.zip`, answer.status === 200 && answer.headers.get('content-type') === 'application/zip'
        && Number(answer.headers.get('content-length')) === zip.length, `HTTP ${answer.status}, ${(zip.length / 1e6).toFixed(1)} MB`);
      const site = path.join(tmp, `site-${label}`);
      const unpacked = python(['-m', 'zipfile', '-e', zipPath, site]);
      const names = unpacked.status === 0 ? fs.readdirSync(site).sort() : [];
      const built = fs.readdirSync(dist).filter((n) => !n.startsWith('.')).sort();
      check(`${label}: the archive unpacks to the web build, the recording and share.json`,
        unpacked.status === 0 && built.every((n) => names.includes(n)) && names.includes('recording.gsrec') && names.includes('share.json')
        && names.length === built.length + 2, names.join(' '));
      if (unpacked.status !== 0) continue;
      const tested = python(['-c', 'import sys, zipfile; sys.exit(1 if zipfile.ZipFile(sys.argv[1]).testzip() else 0)', zipPath]);
      check(`${label}: every checksum of the archive holds`, tested.status === 0);
      const manifest = JSON.parse(fs.readFileSync(path.join(site, 'share.json'), 'utf8'));
      check(`${label}: share.json names the recording`, manifest.file === 'recording.gsrec' && manifest.name === entry.title, JSON.stringify(manifest));
      const same = fs.readFileSync(path.join(site, 'recording.gsrec')).equals(fs.readFileSync(path.join(tmp, 'recordings', entry.file)));
      check(`${label}: the recording inside is the recording`, same);
      const verified = python([path.join(TOOLS, 'gsrec.py'), 'verify', path.join(site, 'recording.gsrec')]);
      check(`${label}: tools/gsrec.py verifies it`, verified.status === 0, verified.stdout.trim().split('\n').pop());

      statics.push(await startStatic(site, port));
      const seen = await watch(browser, `http://127.0.0.1:${port}/`, `share-${label}.png`);
      judge(`${label}, unpacked archive on a static host`, seen, moving);
      port += 1;
    }

    // -- the link of the local network -----------------------------------
    if (lanUrl) {
      for (const [label, entry, moving] of [['capture', scan, false], ['stage', stage, false], ['stage-playing', stage, true]]) {
        // The link of the specification opens on the first picture; with
        // `autoplay` it plays at once.
        const link = `${lanUrl}/?file=${entry.url}&embed=1${moving ? '&autoplay=1&loop=1' : ''}`;
        const seen = await watch(browser, link, `share-lan-${label}.png`);
        judge(`${label}, LAN link`, seen, moving);
        const ranged = seen.requests.filter((r) => r.url.includes('/recordings/') && r.status === 206).length;
        const whole = seen.requests.filter((r) => r.url.includes('/recordings/') && r.status === 200).length;
        console.log(`     ${label}: the viewer made ${ranged} range request(s) and ${whole} whole-file request(s) for the recording`);
      }
    } else {
      console.log('     this machine has no network address: the LAN link is not tested');
    }
  } finally {
    await browser.close();
    for (const child of statics) child.kill('SIGKILL');
    server.child.kill('SIGTERM');
    const code = await Promise.race([server.exited, sleep(15000).then(() => 'timeout')]);
    check('the server stops cleanly', code === 0, `exit code ${code}`);
    fs.writeFileSync(path.join(OUT, 'share-server.log'), server.output());
    fs.rmSync(tmp, { recursive: true, force: true });
  }
  console.log(failed ? `${failed} check(s) failed` : 'all checks passed');
  process.exit(failed ? 1 : 0);
})().catch((error) => {
  console.error(error);
  process.exit(1);
});
