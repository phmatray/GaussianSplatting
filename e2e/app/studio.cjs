// Recording, sharing and phone capture with a native gs-server: the record
// button drives the server's recorder; its recordings are listed,
// replayed (over Range requests), shared (LAN link, QR code, <iframe>
// snippet, web archive) and deleted; an iPhone is paired and its capture
// followed while tools/fake_phone.py plays one; a capture file is sent to
// the server; a client on the LAN side records in its own memory.
// Skipped when the binary is not built.
const { chromium } = require('playwright');
const { spawn, spawnSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { check, REPO, OUT, DIST, PORT, newPage, pinBlend, serve, consoleErrors, state, waitState, open, shot, viewShot, content, setSlider, sleep, startServer, SERVER_BIN: BIN } = require('./lib.cjs');
const { verify, ready } = require('./replay.cjs');
const { synth } = require('./capture.cjs');
const { waitFor } = require('./embed.cjs');

const getJson = async (url, init) => {
  const r = await fetch(url, init);
  return { status: r.status, body: await r.json().catch(() => null) };
};

// Plays a capture file against the server like the phone would. Resolves
// with the exit code and the output.
function fakePhone(file, uri, extra = []) {
  const child = spawn('python3', [path.join(REPO, 'tools/fake_phone.py'), 'stream', file, uri, ...extra], { stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '';
  child.stdout.on('data', (d) => (output += d));
  child.stderr.on('data', (d) => (output += d));
  const done = new Promise((resolve) => child.on('exit', (code) => resolve({ code, output })));
  return { child, done };
}

async function run({ browser }) {
  if (!fs.existsSync(BIN)) {
    console.log(`SKIPPED: ${BIN} does not exist (cargo build -p gs-server --release)`);
    return { skipped: true };
  }
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'splat-live-studio-'));
  const dist = path.join(tmp, 'dist');
  fs.cpSync(DIST, dist, { recursive: true });
  const port = PORT + 70;
  const server = await startServer({ dist, certs: path.join(tmp, 'certs'), port });
  const measurements = {};
  try {
    const url = `${server.url}/`;
    const { ctx, page, logs } = await newPage(browser);
    await ctx.grantPermissions(['clipboard-read', 'clipboard-write']);
    let s = await open(page, url);

    // ------------------------------------------------------------ recording
    await page.locator('#section-record').scrollIntoViewIfNeeded();
    const hint = await page.locator('#record-hint').innerText();
    check('studio: with a server the panel says the recording goes to its disk', /disque de cet ordinateur/.test(hint), hint);
    await page.click('#btn-record');
    s = await waitState(page, (s) => s.recording && s.recording.durationS >= 2 && s.recording.bytes > 100000, 30000);
    const polled = await getJson(`${url}api/record`);
    check('studio: the record button starts the server\'s recorder', polled.body.recording !== null && s.recording.maxBytes === null && /^Scène \d{4}/.test(s.recording.name) && (await page.locator('#btn-record').getAttribute('aria-pressed')) === 'true', JSON.stringify(polled.body.recording));
    const shown = await page.locator('#record-time').innerText();
    check('studio: elapsed time and size are shown', /^0:0\d · [\d,]+ (ko|Mo)$/.test(shown) && /Arrêter/.test(await page.locator('#record-toggle').innerText()), shown);
    // A second window sees the recording in progress.
    const other = await newPage(browser, { width: 900, height: 600 });
    await open(other.page, url);
    let o = await waitState(other.page, (s) => s.recording, 30000);
    check('studio: another window of the same machine sees the recording in progress', o.recording.name === s.recording.name);
    await page.click('#record-toggle');
    s = await waitState(page, (s) => !s.recording && s.takes.length === 1, 30000);
    const take = s.takes[0];
    check('studio: stopping lists the recording of the server', take.place === 'server' && take.scene === 'stage' && take.durationS >= 2 && take.bytes > 100000 && /\.gsrec$/.test(take.file) && /est enregistré/.test(s.notice.text), JSON.stringify(take));
    o = await waitState(other.page, (s) => !s.recording, 30000);
    check('studio: the other window sees it end', o.recording === null);
    await other.ctx.close();
    const listed = await getJson(`${url}api/recordings`);
    check('studio: the list is the server\'s', listed.body.recordings.length === 1 && listed.body.recordings[0].file === take.file && listed.body.recordings[0].title === take.title, JSON.stringify(listed.body.recordings[0]));
    const row = page.locator('#takes .take').first();
    const labels = await row.locator('button').evaluateAll((buttons) => buttons.map((b) => b.getAttribute('aria-label')));
    check('studio: a recording of the server can be replayed, downloaded, shared and deleted', labels.length === 4 && /^Partager/.test(labels[2]) && /sur le serveur/.test(await row.locator('.take-meta').innerText()), labels.join(' | '));

    // ---- download
    const [download] = await Promise.all([page.waitForEvent('download'), row.locator('.take-download').click()]);
    const saved = path.join(OUT, 'studio-take.gsrec');
    await download.saveAs(saved);
    const report = verify(saved);
    check('studio: the downloaded recording passes tools/gsrec.py verify', report && report.exit === 0 && report.complete && fs.statSync(saved).size === take.bytes, JSON.stringify(report && { complete: report.complete, problems: report.problems }));
    fs.rmSync(saved, { force: true });

    // ---- replay from the server
    await row.locator('.take-play').click();
    s = await ready(page);
    s = await waitState(page, (s) => s.replay.status.playing && s.replay.status.time > 0.5, 30000);
    // (The connection to the live stream is closed meanwhile: `source` is null.)
    check('studio: the recording replays from the server', s.replay.origin.url === `recordings/${take.file}` && Math.abs(s.replay.status.duration - take.durationS) < 0.05 && s.source === null && (await row.getAttribute('class')).includes('playing'), `${JSON.stringify(s.replay.origin)}, ${s.replay.status.duration} s`);
    await page.keyboard.press('k');
    s = await waitState(page, (s) => !s.replay.status.playing);
    let n = s.replay.seeks.count;
    await setSlider(page, '#tl-scrub', (take.durationS * 0.5).toFixed(2));
    s = await waitState(page, (s, n) => s.replay.seeks.count > n && !s.replay.status.buffering, 30000, n);
    check('studio: scrubbing a server recording works', Math.abs(s.replay.status.time - take.durationS * 0.5) < 0.02, `seek applied in ${Math.round(s.replay.seeks.lastMs)} ms`);
    await page.click('#tl-close');
    s = await waitState(page, (s) => !s.replay && s.connected && s.layers.dynamic > 0 && s.stats && s.stats.tick > 0, 60000);
    check('studio: closing the replay reconnects to the live stream', s.source === 'server' && s.channel === 'relay');

    // ---------------------------------------------------------------- share
    await row.locator('.take-share').click();
    await page.waitForSelector('#share-dialog');
    const dialog = page.locator('#share-dialog');
    check('studio: the share dialog opens, focused, as a modal dialog', (await dialog.locator('[role=dialog]').getAttribute('aria-modal')) === 'true' && (await page.evaluate(() => document.activeElement && document.activeElement.id)) === 'share-close' && new RegExp(take.title.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).test(await page.locator('#share-title').innerText()), await page.locator('#share-title').innerText());
    check('studio: while LAN sharing is off the dialog offers to switch it on', (await page.locator('#share-lan-on').count()) === 1 && (await page.locator('#share-link').count()) === 0);
    await page.click('#share-lan-on');
    await page.waitForFunction(() => window.__splatLive.state().server.lan.enabled || document.querySelector('#share-dialog .error'), null, { timeout: 30000 });
    s = await state(page);
    let lanBrowser = null;
    if (s.server.lan.enabled && s.server.lan.urls.length > 0) {
      await page.waitForSelector('#share-qr');
      const link = await page.inputValue('#share-link');
      const expected = `${s.server.lan.urls[0]}/?file=recordings/${encodeURIComponent(take.file)}&embed=1&autoplay=1&loop=1`;
      check('studio: the dialog gives the link on the local network', link === expected, link);
      const snippet = await page.inputValue('#share-embed');
      check('studio: and the <iframe> snippet for a web page', snippet.startsWith('<iframe src="') && snippet.includes(link.replace(/&/g, '&amp;')) && /allowfullscreen/.test(snippet) && /xr-spatial-tracking/.test(snippet), snippet);
      const qr = await page.evaluate(() => {
        const img = document.getElementById('share-qr');
        return { width: img.naturalWidth, shown: img.getBoundingClientRect().width, alt: img.alt, data: img.src.startsWith('data:image/svg+xml') };
      });
      check('studio: the QR code of the link is drawn', qr.data && qr.width > 100 && qr.shown > 100 && /QR code/.test(qr.alt), JSON.stringify(qr));
      await page.click('#share-link-copy');
      await page.waitForFunction(() => /Copié/.test(document.getElementById('share-link-copy').textContent));
      check('studio: the copy button puts the link on the clipboard', (await page.evaluate(() => navigator.clipboard.readText())) === link);
      await shot(page, 'share-dialog.png');

      // ---- the archive for a web host
      const zipHref = await page.locator('#share-zip').getAttribute('href');
      const zip = await fetch(zipHref);
      const zipBytes = Buffer.from(await zip.arrayBuffer());
      const zipFile = path.join(tmp, 'share.zip');
      fs.writeFileSync(zipFile, zipBytes);
      check('studio: the dialog links to the web archive of the recording', zip.status === 200 && zipBytes.length > take.bytes && zipBytes.subarray(0, 2).toString() === 'PK', `${zipHref}: ${zipBytes.length} bytes`);
      const unpacked = path.join(tmp, 'unpacked');
      const unzip = spawnSync('python3', ['-c', 'import sys, zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])', zipFile, unpacked]);
      if (unzip.status === 0 && fs.existsSync(path.join(unpacked, 'index.html'))) {
        const host = await serve({ dir: unpacked, port: PORT + 78 });
        const visitor = await newPage(browser, { width: 900, height: 600 });
        await visitor.page.goto(host.url);
        let v = await waitFor(visitor.page, (s) => s.replay && s.replay.status.ready && s.layers.static > 0);
        check('studio: the unpacked archive, on any static host, opens on the recording', v.source === null && Math.abs(v.replay.status.duration - take.durationS) < 0.05 && fs.existsSync(path.join(unpacked, 'share.json')), JSON.stringify(v.replay.origin));
        check('studio: no console error in the unpacked archive', consoleErrors(visitor.logs).length === 0, consoleErrors(visitor.logs).join(' | '));
        await visitor.ctx.close();
        host.srv.close();
      } else {
        check('studio: the archive unpacks to a web site', false, String(unzip.stderr));
      }

      // ---- the link, opened by another device of the network
      const lanHost = new URL(s.server.lan.urls[0]).hostname;
      const proxy = process.env.HTTPS_PROXY ? { proxy: { server: process.env.HTTPS_PROXY, bypass: `127.0.0.1,localhost,${lanHost}` } } : {};
      lanBrowser = await chromium.launch(proxy);
      const device = await lanBrowser.newContext({ viewport: { width: 960, height: 540 }, ignoreHTTPSErrors: true });
      await pinBlend(device);
      const dp = await device.newPage();
      const deviceLogs = [];
      dp.on('console', (m) => deviceLogs.push(`[${m.type()}] ${m.text()}`));
      await dp.goto(link);
      let d = await waitFor(dp, (s) => s.replay && s.replay.status.ready && s.replay.status.playing && s.replay.status.time > 0.3);
      check('studio: the shared link opens the embedded viewer on another device, playing in a loop', d.embed && d.replay.status.looping && d.source === null && (await dp.locator('.topbar').count()) === 0 && (await dp.locator('#tl-fullscreen').count()) === 1, JSON.stringify(d.replay.origin));
      await dp.screenshot({ path: path.join(OUT, 'share-link-device.png') });
      check('studio: no console error on the device', consoleErrors(deviceLogs).length === 0, consoleErrors(deviceLogs).join(' | '));
      await dp.close();

      // ---- a LAN client: no recorder commands, recording in its own memory
      const lp = await device.newPage();
      await open(lp, `${s.server.lan.urls[0]}/`);
      await lp.locator('#section-record').scrollIntoViewIfNeeded();
      let l = await waitState(lp, (s) => s.takes.length === 1, 30000);
      check('studio: a LAN client lists the recordings but cannot share or delete them', l.takes[0].place === 'server' && (await lp.locator('#takes .take-share').count()) === 0 && (await lp.locator('#takes .take-delete').count()) === 0 && /mémoire de cette page/.test(await lp.locator('#record-hint').innerText()));
      check('studio: a LAN client is told where pairing happens', /ordinateur qui exécute Splat Live/.test(await lp.locator('#pair-remote').innerText()) && (await lp.locator('#pair-start').count()) === 0);
      await lp.click('#btn-record');
      await waitState(lp, (s) => s.recording && s.recording.durationS > 1.5 && s.recording.maxBytes > 0, 30000);
      await lp.click('#btn-record');
      l = await waitState(lp, (s) => !s.recording && s.takes.some((t) => t.place === 'memory'), 30000);
      const mine = l.takes.find((t) => t.place === 'memory');
      check('studio: a LAN client records the stream in its own memory (the relay worker)', l.channel === 'relay' && mine.durationS > 1.5 && mine.bytes > 100000 && (await getJson(`${url}api/record`)).body.recording === null, JSON.stringify(mine));
      await lp.close();
      await device.close();

      await page.keyboard.press('Escape');
      await page.waitForFunction(() => !document.getElementById('share-dialog'));
      check('studio: Escape closes the share dialog', (await state(page)).share === null);

      // -------------------------------------------------------- phone capture
      await page.locator('#section-phone').scrollIntoViewIfNeeded();
      const steps = await page.locator('#pair-steps').innerText();
      check('studio: the iPhone section gives the steps in French', /Splat Capture/.test(steps) && /LiDAR/.test(steps) && /code d'appairage/.test(steps) && /pièce, objet, personne ou direct/.test(steps), steps.replace(/\s+/g, ' '));
      await page.click('#pair-start');
      await page.waitForSelector('#pair-qr', { timeout: 30000 });
      const pairQr = await page.evaluate(() => {
        const img = document.getElementById('pair-qr');
        return { width: img.naturalWidth, shown: img.getBoundingClientRect().width, alt: img.alt };
      });
      check('studio: the pairing QR code is shown', pairQr.width > 100 && pairQr.shown > 100 && /appairage/.test(pairQr.alt) && (await state(page)).pair.qr, JSON.stringify(pairQr));
      await shot(page, 'pairing.png');
      const pairing = await getJson(`${url}api/pair`);
      check('studio: the server gives the pairing URI the code stands for', /^splatlive:\/\/pair\?host=.+&port=\d+&fp=[0-9a-f]{64}&token=[0-9a-f]{32}$/.test(pairing.body.uri), pairing.body.uri.replace(/token=.*/, 'token=...'));

      // ---- a phone plays a capture while the page watches
      const room = synth('studio-room.gscap', { frames: 90, title: 'Salon', depth: 128, color: 320 });
      const phone = fakePhone(room, pairing.body.uri, ['--realtime', '--speed', '2']);
      s = await waitState(page, (s) => s.capture && s.capture.stats && s.capture.stats.frames_received >= 8 && s.layers.static > 1000 && s.sceneName === 'capture', 120000);
      await page.locator('#section-capture').scrollIntoViewIfNeeded();
      const card = await page.locator('#capture-card').innerText();
      check('studio: the capture of the phone is followed as it happens', s.capture.info.mode === 'room' && s.capture.info.name === 'Salon' && !s.capture.info.imported && s.capture.info.device.length > 0 && s.capture.state === 'running' && /Capture en cours/.test(card) && /Pièce/.test(card) && new RegExp(s.capture.info.device.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).test(card), card.replace(/\s+/g, ' '));
      check('studio: the phone has its preview tile and its frustum follows it', s.previews.ids.length === 1 && s.previews.painted['0'] > 0 && s.stats.scan && s.stats.scan.camera.width > 0 && s.stats.scan.state === 'scanning', JSON.stringify(s.previews));
      const received = s.capture.stats.frames_received;
      await shot(page, 'capture-live.png');
      s = await waitState(page, (s, n) => s.capture && s.capture.stats && s.capture.stats.frames_received > n + 5, 60000, received);
      check('studio: frames received and used keep counting', s.capture.stats.frames_used > 0 && s.capture.stats.frames_used <= s.capture.stats.frames_received, JSON.stringify(s.capture.stats));
      check('studio: the simulation controls give way to the capture', (await page.locator('#scan-start').count()) === 0 && (await page.locator('#cam-count').count()) === 0 && /images/.test(await page.locator('#capture-pill').innerText()));
      const result = await phone.done;
      s = await waitState(page, (s) => s.capture && s.capture.stats && s.capture.stats.finished, 60000);
      check('studio: the phone finishes its capture and the page says so', result.code === 0 && s.capture.state === 'finished' && /Capture terminée/.test(await page.locator('#capture-state').innerText()), `${result.output.trim().split('\n').pop()}`);
      // The server saves a finished capture as a recording by itself.
      s = await waitState(page, (s) => s.takes.some((t) => t.scene === 'capture'), 30000);
      const kept = s.takes.find((t) => t.scene === 'capture');
      check('studio: the finished capture appears among the recordings', kept.place === 'server' && kept.durationS < 0.25 && /Salon/i.test(kept.title + kept.file), JSON.stringify(kept));
      await sleep(300);
      const view = await viewShot(page, 'capture-live-view.png');
      check('studio: the captured room is drawn', (await content(page, view)).colors > 40);

      // ---- leaving the capture
      await page.click('#mode-stage');
      s = await waitState(page, (s) => !s.capture && s.scene === 'stage' && s.layers.dynamic > 0 && s.sceneName === 'stage', 60000);
      check('studio: a mode button leaves the finished capture for the simulation', (await page.locator('#capture-card').count()) === 0);

      // ---- a capture file sent to the server
      await page.locator('#section-phone').scrollIntoViewIfNeeded();
      const object = synth('studio-upload.gscap', { frames: 40, title: 'Bureau' });
      await page.setInputFiles('#gscap-input', object);
      s = await waitState(page, (s) => s.import && s.import.progress.done, 120000);
      check('studio: a capture file is sent to the server, which ingests it', s.import.upload && s.import.progress.error === null && s.import.progress.frames === 40 && /Capture importée/.test(s.notice.text), JSON.stringify(s.import.progress));
      s = await waitState(page, (s) => s.capture && s.capture.info.name === 'Bureau' && s.capture.info.imported && s.layers.static > 0 && s.capture.stats && s.capture.stats.finished, 60000);
      check('studio: the imported capture becomes what every viewer sees', s.capture.info.mode === 'room' && /Capture terminée/.test(await page.locator('#capture-state').innerText()), `${s.layers.static} splats`);
      await page.click('#import-dismiss');
      const bad = path.join(OUT, 'studio-bad.gscap');
      fs.writeFileSync(bad, 'not a capture');
      await page.setInputFiles('#gscap-input', bad);
      s = await waitState(page, (s) => s.import && s.import.progress.done && s.import.file === 'studio-bad.gscap', 60000);
      check('studio: the server refuses a file that is not a capture, in French', s.import.progress.error && /capture/.test(s.import.progress.error) && s.notice.error && !s.notice.text.includes('—'), s.import.progress.error);
      await page.click('#import-dismiss');
      for (const file of [room, object, bad]) fs.rmSync(file, { force: true });
      await page.click('#mode-stage');
      await waitState(page, (s) => !s.capture && s.scene === 'stage' && s.layers.dynamic > 0, 60000);
      await page.click('#pair-hide');
      check('studio: the pairing code can be hidden again', (await page.locator('#pair-qr').count()) === 0 && (await page.locator('#pair-start').count()) === 1);
    } else {
      check('studio: LAN sharing can be switched on from the share dialog', false, `LAN could not be enabled here: ${JSON.stringify(s.server.lan)}`);
    }
    if (lanBrowser) await lanBrowser.close();

    // -------------------------------------------- a finished scan, in one click
    await page.click('#mode-house');
    await waitState(page, (s) => s.scene === 'house' && s.scan && s.scan.state === 'scanning', 60000);
    await page.click('#scan-finish');
    s = await waitState(page, (s) => s.scan && s.scan.state === 'done', 600000);
    const before = s.takes.length;
    await page.locator('#section-record').scrollIntoViewIfNeeded();
    await page.click('#save-still');
    s = await waitState(page, (s, n) => s.takes.length > n && s.takes.some((t) => t.scene === 'house'), 60000, before);
    const still = s.takes.find((t) => t.scene === 'house');
    // (Started and stopped at once: a tick of the server may slip in.)
    check('studio: a finished scan is saved on the server as a still', still.place === 'server' && still.durationS < 0.25 && still.bytes > 1e6 && /^Maison /.test(still.title) && /image fixe/.test(await page.locator(`#takes .take[data-file="${still.file}"] .take-meta`).innerText()), JSON.stringify(still));

    // ---------------------------------------------------------------- delete
    const count = s.takes.length;
    await page.locator(`#takes .take[data-file="${take.file}"] .take-delete`).click();
    s = await waitState(page, (s, n) => s.takes.length === n - 1, 30000, count);
    const after = await getJson(`${url}api/recordings`);
    check('studio: deleting removes the recording from the server', !after.body.recordings.some((r) => r.file === take.file) && (await fetch(`${url}recordings/${encodeURIComponent(take.file)}`)).status === 404);

    s = await state(page);
    check('studio: no application error', s.errors.length === 0, s.errors.join(' | '));
    // The refused capture file is answered with a 400, which the browser logs.
    const errors = consoleErrors(logs).filter((l) => !/status of 400/.test(l));
    check('studio: no console error', errors.length === 0, errors.join(' | '));
    await ctx.close();
  } finally {
    await server.stop();
    fs.rmSync(tmp, { recursive: true, force: true });
  }
  return measurements;
}

module.exports = { run, fakePhone };
if (require.main === module) require('./standalone.cjs')(run);
