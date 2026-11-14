// The same build in WebKitGTK, the engine of the desktop shell on Linux
// (and close to Safari): MiniBrowser under Xvfb, with gs-server (WebSocket
// through the relay worker) and on a static host (pipeline in the worker).
//
// WebKitGTK has no remote control here, so a copy of the build gets a
// small reporter script that posts `window.__splatLive.state()` to this
// process once per second. Skipped when MiniBrowser or xvfb-run is missing.
const { spawn, spawnSync } = require('child_process');
const fs = require('fs');
const http = require('http');
const os = require('os');
const path = require('path');
const { check, DIST, OUT, PORT, serve, startServer, SERVER_BIN } = require('./lib.cjs');

const MINI_BROWSER = process.env.WEBKIT_MINIBROWSER || '/usr/lib/x86_64-linux-gnu/webkit2gtk-4.1/MiniBrowser';
const SECONDS = Number(process.env.WEBKIT_S || 25);

// Opens `url` in MiniBrowser for a while; resolves with the reports received.
async function session(tag, url, collectorPort) {
  const reports = [];
  const collector = http.createServer((req, res) => {
    let body = '';
    req.on('data', (d) => (body += d));
    req.on('end', () => {
      try {
        reports.push(JSON.parse(body));
      } catch {
        reports.push({ unreadable: body.slice(0, 200) });
      }
      res.writeHead(204, { 'Access-Control-Allow-Origin': '*' }).end();
    });
  });
  await new Promise((resolve) => collector.listen(collectorPort, '127.0.0.1', resolve));
  fs.mkdirSync(OUT, { recursive: true });
  const shot = path.join(OUT, `webkit-${tag}.png`);
  const script = `"${MINI_BROWSER}" "${url}" >/dev/null 2>&1 & PID=$!; sleep ${SECONDS}; import -window root "${shot}" 2>/dev/null; kill $PID; wait $PID 2>/dev/null; true`;
  await new Promise((resolve) => spawn('xvfb-run', ['-a', '-s', '-screen 0 1280x800x24', 'bash', '-c', script], { stdio: 'ignore' }).on('exit', resolve));
  collector.close();
  return reports;
}

function judge(tag, reports, expected) {
  const states = reports.filter((r) => r.source !== undefined && r.stats);
  const [first, last] = [states[0], states[states.length - 1]];
  if (!last) {
    check(`webkit ${tag}: the application starts`, false, `${reports.length} reports, last: ${JSON.stringify(reports[reports.length - 1] || null).slice(0, 300)}`);
    return {};
  }
  const pageErrors = last.pageErrors || [];
  check(`webkit ${tag}: source and channel`, last.source === expected.source && last.channel === expected.channel && last.link === 'connected', `${last.source} / ${last.channel} / ${last.link}`);
  check(`webkit ${tag}: the stream flows and the view renders`, last.scene === expected.scene && last.stats.tick > first.stats.tick + SECONDS * 2 && last.layers.static > 1000 && last.viewer.framesRendered > 10 && last.viewer.canvasWidth > 300,
    `ticks ${first.stats.tick} -> ${last.stats.tick}, ${last.layers.static} + ${last.layers.dynamic} splats, ${last.viewer.framesRendered} frames at ${last.viewer.fps.toFixed(1)} i/s`);
  const painted = Object.values(last.previews.painted);
  check(`webkit ${tag}: every thumbnail is painted`, painted.length === expected.cameras && painted.every((n) => n > 0), JSON.stringify(last.previews.painted));
  check(`webkit ${tag}: no error`, last.errors.length === 0 && pageErrors.length === 0, [...last.errors, ...pageErrors].join(' | '));
  return { ticks_per_s: +((last.stats.tick - first.stats.tick) / Math.max(1, states.length - 1)).toFixed(1), fps: +last.viewer.fps.toFixed(1) };
}

async function run() {
  const missing = [MINI_BROWSER, '/usr/bin/xvfb-run'].filter((file) => !fs.existsSync(file));
  if (missing.length || spawnSync('which', ['import']).status !== 0) {
    console.log(`SKIPPED: needs WebKitGTK's MiniBrowser, xvfb-run and ImageMagick's import (${missing.join(', ') || 'import'} not found)`);
    return { skipped: true };
  }
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'splat-live-webkit-'));
  const dist = path.join(tmp, 'dist');
  fs.cpSync(DIST, dist, { recursive: true });
  const collectorPort = PORT + 89;
  const reporter = `<script>window.__pageErrors=[];addEventListener('error',e=>__pageErrors.push(String(e.message)));addEventListener('unhandledrejection',e=>__pageErrors.push('rejection: '+String(e.reason)));setInterval(()=>{let s;try{s=window.__splatLive?window.__splatLive.state():null}catch(e){s={threw:String(e)}}s=s||{booting:true};s.pageErrors=__pageErrors;fetch('http://127.0.0.1:${collectorPort}/report',{method:'POST',mode:'no-cors',body:JSON.stringify(s)}).catch(()=>{})},1000)</script>`;
  const index = path.join(dist, 'index.html');
  fs.writeFileSync(index, fs.readFileSync(index, 'utf8').replace('</head>', `${reporter}</head>`));
  const measurements = {};
  try {
    if (fs.existsSync(SERVER_BIN)) {
      const server = await startServer({ dist, certs: path.join(tmp, 'certs'), port: PORT + 84 });
      try {
        measurements.server = judge('server', await session('server', `${server.url}/`, collectorPort), { source: 'server', channel: 'relay', scene: 'stage', cameras: 6 });
      } finally {
        await server.stop();
      }
    } else {
      console.log(`SKIPPED (server mode): ${SERVER_BIN} does not exist`);
    }
    const { srv, url } = await serve({ dir: dist, port: PORT + 87 });
    try {
      measurements.worker = judge('worker', await session('worker', url, collectorPort), { source: 'worker', channel: 'worker', scene: 'stage', cameras: 6 });
      measurements.worker_house = judge('worker house', await session('worker-house', `${url}?scene=house`, collectorPort), { source: 'worker', channel: 'worker', scene: 'house', cameras: 5 });
    } finally {
      srv.close();
    }
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
  return measurements;
}

module.exports = { run };
if (require.main === module) require('./standalone.cjs')(run);
