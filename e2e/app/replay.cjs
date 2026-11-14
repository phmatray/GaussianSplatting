// 4D replay and in-memory recording on a static host (no server): a
// recording opened by link, by file and from memory; the time line, its
// keyboard shortcuts and its seek latency; the link parameters and their
// validation; recording the live stream in the worker, replaying,
// downloading and deleting the take; a finished scan saved as a still.
const { spawnSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const { REPO, OUT, check, iwerBundle, newPage, serve, site, recording, consoleErrors, state, waitState, open, shot, viewShot, diff, content, setSlider, sleep } = require('./lib.cjs');

// The replay on screen has its first picture.
// (A paused replay is drawn once: the loop only redraws what changed.)
const ready = (page, timeout = 90000) => waitState(page, (s) => s.replay && s.replay.status.ready && s.viewer.framesRendered >= 1 && s.layers.static + s.layers.dynamic > 0, timeout);
const status = async (page) => (await state(page)).replay.status;
const near = (a, b, tolerance) => Math.abs(a - b) <= tolerance;
// Waits for the seek counter to pass `n` (the picture of that seek is applied).
const sought = (page, n) => waitState(page, (s, n) => s.replay.seeks.count > n && !s.replay.status.buffering, 30000, n);

// A `.splat` scene of `n` splats on a sphere (32 bytes per splat).
function splatFile(n) {
  const out = Buffer.alloc(n * 32);
  for (let i = 0; i < n; i++) {
    const [a, b] = [(i * 2.399963) % (2 * Math.PI), Math.acos(1 - (2 * (i + 0.5)) / n)];
    const at = i * 32;
    out.writeFloatLE(Math.sin(b) * Math.cos(a), at);
    out.writeFloatLE(1 + Math.cos(b), at + 4);
    out.writeFloatLE(Math.sin(b) * Math.sin(a), at + 8);
    for (const k of [12, 16, 20]) out.writeFloatLE(0.03, at + k);
    out.set([80 + (i % 150), 120, 255 - (i % 200), 255, 255, 128, 128, 128], at + 24);
  }
  return out;
}

// `tools/gsrec.py verify --json` on a file: the report, or null.
function verify(file) {
  const r = spawnSync('python3', [path.join(REPO, 'tools/gsrec.py'), 'verify', '--json', file], { encoding: 'utf8' });
  try {
    return { ...JSON.parse(r.stdout.trim().split('\n').pop()), exit: r.status };
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// A link to a recording: the time line and its controls.
async function timeline(browser, base) {
  const { ctx, page, logs } = await newPage(browser);
  await page.goto(`${base}?file=recordings/stage.gsrec`);
  let s = await ready(page);
  const st = s.replay.status;
  check('replay: a link opens the recording, paused on its first picture', st.ready && !st.playing && st.time === 0 && near(st.duration, 5, 0.01) && st.scene === 'stage' && st.error === null && s.scene === 'stage' && s.layers.static > 10000 && s.layers.dynamic > 1000, JSON.stringify({ time: st.time, duration: st.duration, layers: s.layers }));
  check('replay: no live source is started for a link to a recording', s.source === null && s.counts.batches === 0, `source ${s.source}`);
  const badge = page.locator('#source-badge');
  check('replay: the badge names the source', /Enregistrement/.test(await badge.innerText()) && (await badge.getAttribute('data-source')) === 'replay' && (await badge.getAttribute('data-link')) === 'connected', await badge.innerText());
  const card = await page.locator('#replay-card').innerText();
  check('replay: the panel describes the recording instead of the capture controls', /5 secondes/.test(card) && /Revenir au direct/.test(card) && (await page.locator('#cam-count').count()) === 0 && (await page.locator('#toggle-noise').count()) === 0, card.replace(/\s+/g, ' '));
  check('replay: neither mode is selected', (await page.locator('#mode-stage').getAttribute('aria-selected')) === 'false' && (await page.locator('#mode-house').getAttribute('aria-selected')) === 'false');
  check('replay: recording is not offered during a replay', await page.locator('#btn-record').isDisabled());
  const controls = await page.evaluate(() =>
    ['tl-play', 'tl-scrub', 'tl-speed', 'tl-loop', 'tl-link', 'tl-close'].map((id) => {
      const el = document.getElementById(id);
      return { id, label: el && (el.getAttribute('aria-label') || el.title), focusable: el && el.tabIndex >= 0 && !el.disabled };
    }),
  );
  check('replay: every control of the time line is labelled and reachable by keyboard', controls.every((c) => c.label && c.label.length > 3 && c.focusable), JSON.stringify(controls));
  check('replay: the time line shows time and duration', (await page.locator('#tl-time').innerText()) === '0:00,0 / 0:05,0', await page.locator('#tl-time').innerText());
  check('replay: the cameras of the recording have their tiles', s.previews.ids.length === 8 && (await page.locator('#tiles .tile').count()) === 8);

  // ---- play, pause
  const a = await viewShot(page, 'replay-a.png');
  await page.click('#tl-play');
  s = await waitState(page, (s) => s.replay.status.playing && s.replay.status.time > 1.0, 20000);
  check('replay: play moves the head and the statistics of the recording', s.replay.status.playing && s.stats.tick > 15 && (await page.locator('#tl-play').getAttribute('aria-pressed')) === 'true', `time ${s.replay.status.time.toFixed(2)}, tick ${s.stats.tick}`);
  check('replay: the thumbnails of the recording are painted', Object.keys(s.previews.painted).length >= 4, JSON.stringify(s.previews.painted));
  await shot(page, 'replay-timeline.png');
  await page.keyboard.press('k');
  s = await waitState(page, (s) => !s.replay.status.playing);
  const paused = s.replay.status.time;
  const b = await viewShot(page, 'replay-b.png');
  await sleep(600);
  const b2 = await viewShot(page, 'replay-b2.png');
  check('replay: the picture follows the recording', (await diff(page, a, b)).changed > 0.002, `changed ${((await diff(page, a, b)).changed * 100).toFixed(2)} %`);
  check('replay: K pauses, and a paused recording stands still', (await status(page)).time === paused && (await diff(page, b, b2)).changed === 0, `at ${paused.toFixed(2)} s`);

  // ---- keyboard
  let n = (await state(page)).replay.seeks.count;
  await page.keyboard.press('Home');
  s = await sought(page, n);
  check('replay: Home goes to the start', s.replay.status.time === 0 && s.stats.tick <= 1, `time ${s.replay.status.time}, tick ${s.stats.tick}`);
  const start = await viewShot(page, 'replay-start.png');
  check('replay: seeking back shows the first picture again', (await diff(page, a, start)).changed < 0.001, `differs by ${((await diff(page, a, start)).changed * 100).toFixed(3)} %`);
  n = s.replay.seeks.count;
  await page.keyboard.press('l');
  s = await sought(page, n);
  check('replay: L skips five seconds ahead (to the end here)', near(s.replay.status.time, 5, 0.01));
  n = s.replay.seeks.count;
  await page.keyboard.press('j');
  s = await sought(page, n);
  check('replay: J skips five seconds back', near(s.replay.status.time, 0, 0.01));
  n = s.replay.seeks.count;
  await page.keyboard.press('.');
  await page.keyboard.press('.');
  s = await waitState(page, (s) => Math.abs(s.replay.status.time - 0.1) < 0.011 && !s.replay.status.buffering, 20000);
  await page.keyboard.press(',');
  s = await waitState(page, (s) => Math.abs(s.replay.status.time - 0.05) < 0.011, 20000);
  check('replay: comma and period step by a twentieth of a second', near(s.replay.status.time, 0.05, 0.011) && s.stats.tick <= 1, `time ${s.replay.status.time}, tick ${s.stats.tick}`);
  n = s.replay.seeks.count;
  await page.keyboard.press('End');
  s = await sought(page, n);
  check('replay: End goes to the end', near(s.replay.status.time, 5, 0.01));
  await page.keyboard.press('p');
  s = await waitState(page, (s) => s.replay.status.playing && s.replay.status.time < 2);
  check('replay: P plays too, and play at the end starts over', s.replay.status.playing);
  await page.keyboard.press('p');
  await waitState(page, (s) => !s.replay.status.playing);

  // ---- scrubbing with the slider
  n = (await state(page)).replay.seeks.count;
  await setSlider(page, '#tl-scrub', 3.2);
  s = await sought(page, n);
  check('replay: the scrub bar moves the head', near(s.replay.status.time, 3.2, 0.011) && near(s.stats.scene_time, 3.2, 0.2) && /^0:03,2/.test(await page.locator('#tl-time').innerText()), `${await page.locator('#tl-time').innerText()}, scene time ${s.stats.scene_time.toFixed(2)}`);
  const text = await page.locator('#tl-scrub').getAttribute('aria-valuetext');
  check('replay: the scrub bar speaks its position', /3 secondes sur 5 secondes/.test(text), text);
  // The bar has the keyboard after a click on it: arrows step it.
  await page.focus('#tl-scrub');
  n = (await state(page)).replay.seeks.count;
  await page.keyboard.press('ArrowRight');
  s = await sought(page, n);
  check('replay: arrow keys on the focused scrub bar seek', s.replay.status.time > 3.2 && s.replay.status.time < 3.5, `time ${s.replay.status.time}`);
  await page.evaluate(() => document.activeElement.blur());

  // ---- speed and loop
  await page.selectOption('#tl-speed', { label: '× 4' });
  await waitState(page, (s) => s.replay.status.speed === 4);
  await page.keyboard.press('b');
  s = await waitState(page, (s) => s.replay.status.looping);
  check('replay: B switches the loop on', (await page.locator('#tl-loop').getAttribute('aria-pressed')) === 'true');
  n = s.replay.seeks.count;
  await page.keyboard.press('Home');
  await sought(page, n);
  await page.click('#tl-play');
  const t0 = Date.now();
  // At four times the speed the five seconds pass in 1.25 s; with the loop
  // the head comes around instead of stopping.
  await waitState(page, (s) => s.replay.status.time > 2.5, 20000);
  s = await waitState(page, (s) => s.replay.status.time < 2.0 && s.replay.status.playing, 20000);
  const lap = (Date.now() - t0) / 1000;
  check('replay: at × 4 with the loop on the head comes around', s.replay.status.playing && !s.replay.status.ended && lap < 12, `one lap in ${lap.toFixed(1)} s of wall clock`);
  await page.click('#tl-loop');
  await page.selectOption('#tl-speed', { label: '× 1' });
  s = await waitState(page, (s) => !s.replay.status.looping && s.replay.status.speed === 1);
  s = await waitState(page, (s) => s.replay.status.ended && !s.replay.status.playing, 30000);
  check('replay: without the loop the recording stops at its end', near(s.replay.status.time, 5, 0.01) && (await page.locator('#tl-play').getAttribute('aria-pressed')) === 'false');

  // ---- the link to this moment
  await ctx.grantPermissions(['clipboard-read', 'clipboard-write']);
  n = s.replay.seeks.count;
  await setSlider(page, '#tl-scrub', 2.5);
  await sought(page, n);
  await page.click('#tl-link');
  await page.waitForFunction(() => /copié/.test(window.__splatLive.state().notice?.text || ''));
  const link = await page.evaluate(() => navigator.clipboard.readText());
  check('replay: the link to this moment carries file, time and view', link.startsWith(base) && /\?file=recordings\/stage\.gsrec&t=2\.5&view=(-?[\d.]+,){5}-?[\d.]+$/.test(link), link);

  // ---- H hides the time line with the rest
  await page.keyboard.press('h');
  await page.waitForFunction(() => window.__splatLive.state().uiHidden);
  const hidden = await page.evaluate(() => getComputedStyle(document.getElementById('timeline')).display);
  await page.keyboard.press('h');
  check('replay: H hides the time line too', hidden === 'none');

  // ---- back to the live source (found only now)
  await page.click('#tl-close');
  s = await waitState(page, (s) => !s.replay && s.connected && s.source === 'worker' && s.scene === 'stage' && s.stats && s.stats.tick > 2 && s.layers.dynamic > 0, 90000);
  check('replay: closing the recording starts the live source', (await page.locator('#timeline').count()) === 0 && /Local \(WASM\)/.test(await badge.innerText()) && (await page.locator('#cam-count').count()) === 1 && !(await page.locator('#btn-record').isDisabled()));
  s = await state(page);
  check('replay: no application error', s.errors.length === 0, s.errors.join(' | '));
  check('replay: no console error', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
  await ctx.close();
  // The link of the clipboard opens on that moment, from that place.
  return link;
}

// ---------------------------------------------------------------------------
// Link parameters: autoplay, loop, t, view, nav; what is refused.
async function parameters(browser, base, momentLink) {
  const { ctx, page, logs } = await newPage(browser, { width: 900, height: 600 });
  await page.goto(`${base}?file=recordings/stage.gsrec&autoplay=1&loop=1&t=3&view=0,6,0.5,0,1,0&nav=fly`);
  let s = await ready(page);
  s = await waitState(page, (s) => s.replay.status.playing && s.replay.status.time > 3.1 && s.replay.seeks.count >= 1, 30000);
  check('params: autoplay, loop and t start the recording playing from there', s.replay.status.looping && s.replay.status.time >= 3 && s.replay.status.time < 5.01, `time ${s.replay.status.time.toFixed(2)}`);
  check('params: view and nav place the camera', s.nav === 'fly' && near(s.eye[0], 0, 0.01) && near(s.eye[1], 6, 0.01) && near(s.eye[2], 0.5, 0.01), `${s.nav}, eye ${s.eye.map((v) => v.toFixed(2))}`);

  await page.goto(momentLink);
  s = await ready(page);
  s = await waitState(page, (s) => Math.abs(s.replay.status.time - 2.5) < 0.011 && !s.replay.status.buffering, 30000);
  const parts = momentLink.split('view=')[1].split(',').map(Number);
  check('params: the copied link opens on its moment and its point of view', !s.replay.status.playing && parts.slice(0, 3).every((v, i) => near(s.eye[i], v, 0.02)), `time ${s.replay.status.time}, eye ${s.eye.map((v) => v.toFixed(2))}`);

  // ---- refused values: nothing is fetched, the live source starts
  const hostile = [
    ['javascript:alert(document.domain)//a.gsrec', /http et https/],
    ['data:application/octet-stream;base64,R1NSRUMx#a.gsrec', /http et https/],
    ['blob:http://127.0.0.1/1234.gsrec', /http et https/],
    ['file:///etc/passwd.gsrec', /http et https/],
    ['https://user:secret@example.org/a.gsrec', /identifiants/],
    ['recordings/stage.txt', /\.gsrec/],
  ];
  for (const [value, expected] of hostile) {
    const requests = [];
    const listener = (r) => requests.push(r.url());
    page.on('request', listener);
    await page.goto(`${base}?file=${encodeURIComponent(value)}`);
    s = await waitState(page, (s) => s.notice && s.connected && s.source === 'worker', 60000);
    page.off('request', listener);
    // Anything but the page's own files (the query of the page itself
    // naturally holds the refused value).
    const fetched = requests.filter((u) => !u.startsWith(base) || /passwd|\.gsrec|\.txt/.test(new URL(u).pathname));
    check(`params: file=${value.slice(0, 28)}... is refused and never fetched`, s.notice.error && expected.test(s.notice.text) && !s.replay && !s.fileMode && fetched.length === 0, `${s.notice.text} ${fetched.join(' ')}`);
  }
  let dialogs = 0;
  page.on('dialog', (d) => { dialogs += 1; d.dismiss(); });
  await page.goto(`${base}?file=${encodeURIComponent('java\tscript:alert(1)//a.gsrec')}&embed=maybe&t=-4&view=1,2&nav=jump`);
  s = await waitState(page, (s) => s.notice && s.connected, 60000);
  check('params: a tab hidden in a scheme does not get through, bad values are dropped', dialogs === 0 && s.notice.error && /Lien incomplet/.test(s.notice.text) && !s.embed && s.nav === 'orbit', s.notice.text);

  // ---- a file that is not there, a file that is not a recording
  await page.goto(`${base}?file=recordings/absent.gsrec`);
  s = await waitState(page, (s) => s.replay && s.replay.status.error, 30000);
  const missing = await page.locator('#tl-error').innerText();
  check('params: a missing recording says so in French', s.replay.status.error === 'http_404' && /introuvable/.test(missing) && /404/.test(missing) && (await page.locator('#source-badge').getAttribute('data-link')) === 'failed', missing);
  await page.goto(`${base}?file=recordings/fake.gsrec`);
  s = await waitState(page, (s) => s.replay && s.replay.status.error, 30000);
  check('params: a file that is not a recording says so', s.replay.status.error === 'bad_magic' && /n'est pas un enregistrement/.test(await page.locator('#tl-error').innerText()), await page.locator('#tl-error').innerText());
  await page.click('#tl-close');
  s = await waitState(page, (s) => !s.replay && s.connected && s.source === 'worker' && s.layers.dynamic > 0, 60000);
  check('params: an unreadable recording still lets one return to the live source', true);
  check('params: no console error', consoleErrors(logs).filter((l) => !/absent\.gsrec/.test(l)).length === 0, consoleErrors(logs).join(' | '));
  await ctx.close();
}

// ---------------------------------------------------------------------------
// Where a recording can come from.
async function sources(browser, base, dir) {
  // ---- a splat scene by link
  const { ctx, page, logs } = await newPage(browser, { width: 900, height: 600 });
  await page.goto(`${base}?file=scenes/boule.splat&view=0,1,4,0,1,0`);
  let s = await waitState(page, (s) => s.fileMode && s.layers.static === 3000 && s.viewer.framesRendered >= 1, 60000);
  check('sources: a .splat scene opens by link, as an imported file', s.file.name === 'boule.splat' && s.viewer.blend === 'sorted' && s.source === null && near(s.eye[2], 4, 0.01), JSON.stringify(s.file));
  const ball = await viewShot(page, 'replay-splat-link.png');
  check('sources: the scene of the link is drawn', (await content(page, ball)).colors > 40);
  await page.goto(`${base}?file=scenes/absente.ply`);
  s = await waitState(page, (s) => s.fileError, 30000);
  check('sources: a missing scene says so', /introuvable/.test(s.fileError) && !/enregistrement/i.test(s.fileError), s.fileError);

  // ---- a recording picked as a file, then dropped
  await open(page, base);
  await page.setInputFiles('#import-input', path.join(dir, 'recordings/stage.gsrec'));
  s = await ready(page);
  check('sources: a .gsrec picked in the file section is replayed, playing', s.replay.origin === 'file' && s.replay.title === 'Scène (demo)' && (await page.locator('#tl-link').count()) === 0, JSON.stringify(s.replay.origin));
  s = await waitState(page, (s) => s.replay.status.playing && s.replay.status.time > 0.5, 20000);
  const liveTick = s.counts.batches;
  await sleep(1500);
  s = await state(page);
  check('sources: the live pipeline is on hold during the replay', s.counts.batches === liveTick, `${s.counts.batches - liveTick} live batches in 1.5 s`);
  await page.click('#replay-close');
  s = await waitState(page, (s) => !s.replay && s.stats && s.layers.dynamic > 0 && s.counts.batches > 0, 30000);
  const tick = s.stats.tick;
  s = await waitState(page, (s, tick) => s.stats.tick > tick + 3, 30000, tick);
  check('sources: "Revenir au direct" resumes the pipeline where it was', s.source === 'worker' && s.scene === 'stage' && !s.config.paused);
  const data = fs.readFileSync(path.join(dir, 'recordings/house.gsrec')).toString('base64');
  await page.evaluate(async (data) => {
    const bytes = Uint8Array.from(atob(data), (c) => c.charCodeAt(0));
    const transfer = new DataTransfer();
    transfer.items.add(new File([bytes], 'maison.gsrec'));
    window.dispatchEvent(new DragEvent('drop', { dataTransfer: transfer, bubbles: true, cancelable: true }));
  }, data);
  s = await waitState(page, (s) => s.replay && s.replay.status.ready && s.scene === 'house', 60000);
  check('sources: a dropped .gsrec is replayed', s.replay.origin === 'file' && s.replay.status.scene === 'house' && near(s.replay.status.duration, 69.4, 0.1), `${s.replay.status.duration}`);

  // ---- the house: seek latency while scrubbing
  await page.keyboard.press('k');
  s = await waitState(page, (s) => !s.replay.status.playing);
  const duration = s.replay.status.duration;
  const seeks = [];
  for (const f of [0.5, 0.9, 0.3, 0.6, 0.1, 0.95, 0.5, 0.52, 0.54, 0.2, 0.8, 0.82]) {
    const n = (await state(page)).replay.seeks.count;
    await setSlider(page, '#tl-scrub', (f * duration).toFixed(2));
    s = await sought(page, n);
    seeks.push({ at: +(f * duration).toFixed(1), ms: Math.round(s.replay.seeks.lastMs), splats: s.layers.static });
  }
  const houseSeeks = seeks.map((x) => x.ms).sort((a, b) => a - b);
  const grows = seeks.find((x) => x.at > 60).splats > seeks.find((x) => x.at < 10).splats * 2;
  check('sources: scrubbing a house scan shows the map of each moment', grows && s.scan && s.scan.state !== 'idle', JSON.stringify(seeks));
  check('sources: seeks in a house scan are applied within a quarter of a second (median)', houseSeeks[6] < 250, `median ${houseSeeks[6]} ms, worst ${houseSeeks[11]} ms`);
  await setSlider(page, '#tl-scrub', duration);
  s = await waitState(page, (s) => s.scan && s.scan.state === 'done' && s.layers.static > 400000, 30000);
  check('sources: the end of the recording is the finished scan', s.stats.splats_static === s.layers.static, `${s.layers.static} splats`);
  await page.click('#tl-close');
  await waitState(page, (s) => !s.replay && s.scene === 'stage' && s.layers.dynamic > 0, 30000);
  check('sources: no console error', consoleErrors(logs).filter((l) => !/absente\.ply/.test(l)).length === 0, consoleErrors(logs).join(' | '));
  await ctx.close();
  return { house_seek_ms_median: houseSeeks[6], house_seek_ms_worst: houseSeeks[11] };
}

// ---------------------------------------------------------------------------
// Hosts: one without Range, another origin with and without CORS, an
// unpacked share archive.
async function hosts(browser, dir) {
  const measurements = {};
  // ---- a host that ignores Range: the recording is downloaded front to back
  const plain = await serve({ dir, ranges: false, port: 8140 });
  const { ctx, page, logs } = await newPage(browser, { width: 900, height: 600 });
  await page.goto(`${plain.url}?file=recordings/stage.gsrec&autoplay=1`);
  let s = await ready(page);
  s = await waitState(page, (s) => s.replay.status.time > 1 && s.replay.status.duration > 4.9, 30000);
  check('hosts: a host without Range support plays the recording as it downloads', s.replay.status.playing && s.replay.status.error === null);
  plain.srv.close();

  // ---- another origin: with CORS it plays, without it says why
  const open_ = await serve({ dir, cors: true, port: 8150 });
  const closed = await serve({ dir, cors: false, port: 8160 });
  const home = await serve({ dir, port: 8170 });
  await page.goto(`${home.url}?file=${encodeURIComponent(open_.url + 'recordings/stage.gsrec')}&autoplay=1`);
  s = await ready(page);
  check('hosts: a recording of another origin plays when that origin allows it (CORS)', s.replay.status.error === null && s.replay.origin.url.startsWith('http://127.0.0.1:815'), JSON.stringify(s.replay.origin));
  await page.goto(`${home.url}?file=${encodeURIComponent(closed.url + 'recordings/stage.gsrec')}`);
  s = await waitState(page, (s) => s.replay && s.replay.status.error, 30000);
  const refused = await page.locator('#tl-error').innerText();
  check('hosts: without CORS the browser refuses, and the message names CORS', s.replay.status.error === 'network' && /CORS/.test(refused) && s.layers.static === 0, refused);
  open_.srv.close();
  closed.srv.close();

  // ---- a recording too long for memory: Range requests
  const long = path.join(dir, 'recordings/long.gsrec');
  if (fs.existsSync(long)) {
    home.ranged.length = 0;
    await page.goto(`${home.url}?file=recordings/long.gsrec`);
    s = await ready(page, 120000);
    const size = fs.statSync(long).size;
    const firstPicture = home.ranged.filter((r) => r.startsWith('recordings/long.gsrec')).length;
    check('hosts: a long recording is opened through Range requests, not downloaded', firstPicture >= 3 && firstPicture < 12 && s.replay.status.duration > 30, `${(size / 1e6).toFixed(0)} MB, ${firstPicture} requests to the first picture`);
    const seeks = [];
    for (const f of [0.5, 0.9, 0.25, 0.7, 0.1, 0.6, 0.62, 0.3]) {
      const n = (await state(page)).replay.seeks.count;
      const t0 = Date.now();
      await setSlider(page, '#tl-scrub', (f * s.replay.status.duration).toFixed(2));
      s = await sought(page, n);
      seeks.push({ ms: Math.round(s.replay.seeks.lastMs), wall: Date.now() - t0 });
    }
    const sorted = seeks.map((x) => x.ms).sort((a, b) => a - b);
    measurements.long_stage_seconds = +s.replay.status.duration.toFixed(1);
    measurements.long_stage_megabytes = +(size / 1e6).toFixed(0);
    measurements.long_stage_seek_ms_median = sorted[4];
    measurements.long_stage_seek_ms_worst = sorted[7];
    check('hosts: seeks in a long stage recording are applied within a quarter of a second (median)', sorted[4] < 250 && near(s.replay.status.time, 0.3 * s.replay.status.duration, 0.02), `median ${sorted[4]} ms, worst ${sorted[7]} ms over HTTP ranges`);
    await page.click('#tl-play');
    const from = (await status(page)).time;
    s = await waitState(page, (s, from) => s.replay.status.time > from + 2 && !s.replay.status.buffering, 30000, from);
    check('hosts: a long recording plays on through its ranges', s.replay.status.playing && s.replay.status.error === null && home.ranged.length < 80, `${home.ranged.length} range requests so far`);
  }
  home.srv.close();

  // ---- an unpacked share archive: share.json names the recording
  const shared = site('share', { 'recording.gsrec': recording('stage'), 'share.json': JSON.stringify({ file: 'recording.gsrec', name: 'Danse', autoplay: true, loop: true, scene: 'stage', duration_s: 5 }) });
  const archive = await serve({ dir: shared, port: 8180 });
  await page.goto(archive.url);
  s = await ready(page);
  s = await waitState(page, (s) => s.replay.status.playing && s.replay.status.time > 0.3, 30000);
  check('hosts: an unpacked share archive opens on its recording', s.replay.status.looping && s.source === null && !s.embed && s.replay.origin.url === 'recording.gsrec', JSON.stringify(s.replay.origin));
  await page.goto(`${archive.url}?embed=1&loop=0&t=2`);
  s = await ready(page);
  s = await waitState(page, (s) => s.replay.status.time >= 2 && s.embed, 30000);
  check('hosts: the link overrides share.json, parameter by parameter', s.embed && !s.replay.status.looping && s.replay.status.playing);
  // A hostile share.json gets no more trust than a link.
  fs.writeFileSync(path.join(shared, 'share.json'), JSON.stringify({ file: 'javascript:alert(1)//x.gsrec', autoplay: true }));
  await page.goto(archive.url);
  s = await waitState(page, (s) => s.connected && s.source === 'worker', 60000);
  check('hosts: a share.json naming a forbidden address is ignored', !s.replay && !s.fileMode);
  archive.srv.close();
  check('hosts: no console error', consoleErrors(logs).filter((l) => !/CORS|Failed to fetch|net::ERR_FAILED/.test(l)).length === 0, consoleErrors(logs).join(' | '));
  await ctx.close();
  return measurements;
}

// ---------------------------------------------------------------------------
// Recording in the worker's memory.
async function recordingInMemory(browser, base) {
  const { ctx, page, logs } = await newPage(browser);
  let s = await open(page, base);
  const hint = await page.locator('#record-hint').innerText();
  check('record: without a server the panel says where the recording goes', /mémoire de cette page/.test(hint) && /268 Mo/.test(hint) && (await page.locator('#takes').count()) === 0, hint);
  await page.click('#btn-record');
  s = await waitState(page, (s) => s.recording && s.recording.durationS > 2.0, 30000);
  const shown = await page.locator('#record-time').innerText();
  check('record: the record button shows elapsed time and size', /^0:0\d · [\d,]+ (ko|Mo)$/.test(shown) && s.recording.bytes > 100000 && s.recording.maxBytes === 268435456 && (await page.locator('#btn-record').getAttribute('aria-pressed')) === 'true', shown);
  check('record: the panel shows the recording in progress', /Arrêter/.test(await page.locator('#record-toggle').innerText()) && (await page.locator('#record-status').isVisible()) && (await page.locator('#section-record [role=progressbar]').count()) === 1);
  await shot(page, 'record-running.png');
  await page.keyboard.press('r');
  s = await waitState(page, (s) => !s.recording && s.takes.length === 1, 30000);
  const take = s.takes[0];
  check('record: R stops, and the take is listed', take.place === 'memory' && take.scene === 'stage' && take.durationS > 2 && take.bytes > 100000 && /\.gsrec$/.test(take.file) && /^Scène \d{4}-\d\d-\d\d \d\dh\d\d$/.test(take.title), JSON.stringify(take));
  check('record: a notice says the take is ready and where it lives', /est prêt/.test(s.notice.text) && /mémoire/.test(s.notice.text) && !s.notice.error, s.notice.text);
  const row = page.locator('#takes .take').first();
  const labels = await row.locator('button').evaluateAll((buttons) => buttons.map((b) => b.getAttribute('aria-label')));
  check('record: the take offers replay, download and delete, each labelled', labels.length === 3 && /^Relire/.test(labels[0]) && /^Télécharger/.test(labels[1]) && /^Supprimer/.test(labels[2]), labels.join(' | '));

  // ---- download: a valid .gsrec for the reference checker
  const [download] = await Promise.all([page.waitForEvent('download'), row.locator('.take-download').click()]);
  const file = path.join(OUT, 'take.gsrec');
  await download.saveAs(file);
  const report = verify(file);
  check('record: the downloaded file passes tools/gsrec.py verify', report && report.exit === 0 && report.complete === true && fs.statSync(file).size === take.bytes && download.suggestedFilename() === take.file, JSON.stringify(report && { complete: report.complete, problems: report.problems, keyframes: report.keyframes && report.keyframes.length, duration: report.duration_s }));

  // ---- replay at once
  await row.locator('.take-play').click();
  s = await ready(page);
  s = await waitState(page, (s) => s.replay.status.playing && s.replay.status.time > 0.5, 30000);
  check('record: the take replays at once, from memory', s.replay.origin.take === take.id && Math.abs(s.replay.status.duration - take.durationS) < 0.01 && s.replay.title === take.title && (await row.getAttribute('class')).includes('playing'), JSON.stringify(s.replay.origin));
  // ---- deleting the take that plays returns to the live source
  // The bin asks once more: the first click arms it, the second deletes.
  await row.locator('.take-delete').click();
  await sleep(300);
  const armed = { takes: (await state(page)).takes.length, label: await row.locator('.take-delete').getAttribute('aria-label'), text: await row.locator('.take-delete').innerText() };
  check('record: the first click on the bin only asks for a confirmation', armed.takes === 1 && /Confirmer la suppression/.test(armed.label) && /Supprimer/.test(armed.text), JSON.stringify(armed));
  await row.locator('.take-delete').click();
  s = await waitState(page, (s) => !s.replay && s.takes.length === 0 && s.layers.dynamic > 0 && s.counts.stats > 0, 30000);
  check('record: deleting the take on screen returns to the live source', (await page.locator('#takes').count()) === 0);

  // ---- a finished scan saved in one click
  await page.click('#mode-house');
  await waitState(page, (s) => s.scene === 'house' && s.scan && s.scan.state === 'scanning', 60000);
  check('record: nothing to save while the scan runs', (await page.locator('#save-still').count()) === 0);
  await page.click('#scan-finish');
  s = await waitState(page, (s) => s.scan && s.scan.state === 'done', 600000);
  const splats = s.layers.static;
  await page.click('#save-still');
  s = await waitState(page, (s) => s.takes.length === 1, 30000);
  check('record: a finished scan is saved as a still in one click', s.takes[0].durationS === 0 && s.takes[0].scene === 'house' && /^Maison /.test(s.takes[0].title) && s.takes[0].bytes > 1e6, JSON.stringify(s.takes[0]));
  check('record: the list calls it a still', /image fixe/.test(await page.locator('#takes .take-meta').first().innerText()), await page.locator('#takes .take-meta').first().innerText());
  await page.locator('#takes .take-play').first().click();
  s = await waitState(page, (s, splats) => s.replay && s.replay.status.ready && s.layers.static === splats, 60000, splats);
  check('record: the still replays as the finished scan', s.scan.state === 'done' && s.replay.status.duration === null && /Image fixe/.test(await page.locator('#replay-meta').innerText()), `${s.layers.static} of ${splats} splats`);
  const [still] = await Promise.all([page.waitForEvent('download'), page.locator('#takes .take-download').first().click()]);
  const stillFile = path.join(OUT, 'still.gsrec');
  await still.saveAs(stillFile);
  const stillReport = verify(stillFile);
  check('record: the still is a valid recording with one keyframe', stillReport && stillReport.exit === 0 && stillReport.keyframes.length === 1, JSON.stringify(stillReport && { complete: stillReport.complete, keyframes: stillReport.keyframes }));
  fs.rmSync(stillFile, { force: true });
  s = await state(page);
  check('record: no application error', s.errors.length === 0, s.errors.join(' | '));
  check('record: no console error', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
  await ctx.close();
}

// ---------------------------------------------------------------------------
// A replay inside a headset (IWER emulator): the left controller holds the
// time line.
async function headset(browser, base) {
  if (!iwerBundle()) return;
  const { ctx, page, logs } = await newPage(browser, { width: 1280, height: 640, iwer: true });
  await page.goto(`${base}?file=recordings/stage.gsrec&loop=1`);
  let s = await ready(page);
  await waitState(page, (s) => s.xr.supported === true);
  await page.click('#btn-vr');
  s = await waitState(page, (s) => s.xr.active && s.xr.session && s.xr.session.frames > 5 && s.xr.session.viewports.length === 2 && s.xr.session.controllers === 2, 30000);
  check('vr replay: a recording can be watched in the headset', s.replay.status.ready && s.viewer.splatsDrawn > 5000 && !s.replay.status.playing, `${s.viewer.splatsDrawn} splats per eye`);
  const frames = (n) => waitState(page, (s, target) => s.xr.session && s.xr.session.frames >= target, 30000, s.xr.session.frames + n);
  const press = async (hand, button) => {
    await page.evaluate(([h, b]) => xrdev.controllers[h].updateButtonValue(b, 1), [hand, button]);
    s = await frames(3);
    await page.evaluate(([h, b]) => xrdev.controllers[h].updateButtonValue(b, 0), [hand, button]);
    s = await frames(3);
  };
  await page.addStyleTag({ content: '#xr-card { visibility: hidden; }' });
  // Bring the left hand into view: the time line sits above it.
  await page.evaluate(() => {
    xrdev.controllers.left.position.set(-0.12, 1.45, -0.42);
    xrdev.controllers.left.quaternion.set(0, 0, 0, 1);
  });
  s = await frames(4);
  const paused = await shot(page, 'vr-replay-paused.png');
  await press('left', 'y-button');
  s = await waitState(page, (s) => s.replay.status.playing && s.replay.status.time > 0.4, 30000);
  check('vr replay: the left Y button starts playback', s.replay.status.playing);
  const playing = await shot(page, 'vr-replay-playing.png');
  check('vr replay: the headset shows the time line above the left hand, and it changes with the state', (await diff(page, paused, playing)).changed > 0.0005, `changed ${((await diff(page, paused, playing)).changed * 100).toFixed(2)} %`);
  await press('left', 'y-button');
  s = await waitState(page, (s) => !s.replay.status.playing, 30000);
  check('vr replay: the left Y button pauses it', !s.replay.status.playing);
  // Scrubbing: left grip held, left stick to the right; the user stays put.
  const before = { time: s.replay.status.time, rig: s.xr.session.rig.slice(0, 3) };
  // (Towards the middle of the recording: the pause may have come near
  // either end, where a scrub outwards has nowhere to go.)
  await page.evaluate((direction) => {
    xrdev.controllers.left.updateButtonValue('squeeze', 1);
    xrdev.controllers.left.updateAxes('thumbstick', direction, 0);
  }, before.time > s.replay.status.duration / 2 ? -1 : 1);
  s = await waitState(page, (s, t) => Math.abs(s.replay.status.time - t) > 0.5, 30000, before.time);
  await page.evaluate(() => {
    xrdev.controllers.left.updateAxes('thumbstick', 0, 0);
    xrdev.controllers.left.updateButtonValue('squeeze', 0);
  });
  s = await frames(4);
  const moved = Math.hypot(...s.xr.session.rig.slice(0, 3).map((v, i) => v - before.rig[i]));
  check('vr replay: grip plus left stick scrubs through the recording without moving the user', s.replay.status.time !== before.time && moved < 1e-3 && !s.replay.status.playing, `time ${before.time.toFixed(2)} -> ${s.replay.status.time.toFixed(2)}, moved ${moved.toFixed(4)} m`);
  // Without the grip the stick moves the user, as ever.
  const t = s.replay.status.time;
  await page.evaluate(() => xrdev.controllers.left.updateAxes('thumbstick', 0, -1));
  s = await waitState(page, (s, from) => Math.hypot(s.xr.session.rig[0] - from[0], s.xr.session.rig[2] - from[2]) > 0.3, 30000, s.xr.session.rig.slice(0, 3));
  await page.evaluate(() => xrdev.controllers.left.updateAxes('thumbstick', 0, 0));
  check('vr replay: without the grip the left stick still moves the user', (await status(page)).time === t);
  // (The exit card is hidden for the pictures: click it from the page.)
  await page.evaluate(() => document.getElementById('exit-vr').click());
  await waitState(page, (s) => !s.xr.active, 30000);
  check('vr replay: no console error', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
  await ctx.close();
}

async function run({ browser }) {
  const files = {
    'recordings/stage.gsrec': recording('stage'),
    'recordings/house.gsrec': recording('house'),
    'recordings/fake.gsrec': Buffer.from('this is not a recording, whatever its name says'),
    'scenes/boule.splat': splatFile(3000),
  };
  // A stage recording too large to be held whole (more than 64 MiB),
  // unless the disk is short: LONG_REPLAY=0 skips it.
  if (process.env.LONG_REPLAY !== '0') files['recordings/long.gsrec'] = recording('long');
  const dir = site('replay', files);
  const { srv, url } = await serve({ dir, port: 8130 });
  // REPLAY_PARTS=sources,hosts runs some of the parts only.
  const parts = (process.env.REPLAY_PARTS || 'timeline,parameters,sources,hosts,record,headset').split(',');
  const wanted = (part) => parts.includes(part);
  try {
    const link = wanted('timeline') ? await timeline(browser, url) : `${url}?file=recordings/stage.gsrec&t=2.5&view=3,2,4.5,0,1,0`;
    if (wanted('parameters')) await parameters(browser, url, link);
    const seeks = wanted('sources') ? await sources(browser, url, dir) : {};
    const long = wanted('hosts') ? await hosts(browser, dir) : {};
    if (wanted('record')) await recordingInMemory(browser, url);
    if (wanted('headset')) await headset(browser, url);
    return { ...seeks, ...long };
  } finally {
    srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

module.exports = { run, splatFile, verify, ready };
if (require.main === module) require('./standalone.cjs')(run);
