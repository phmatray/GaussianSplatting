// "Nouvel affinage", end to end, with a native gs-server: a capture
// file goes to the server, becomes a room of the Pièces gallery, is refined
// (the rapide preset cut down to seconds with --refine-iterations), its
// result is shown, downloaded, shared and deleted; a job is cancelled; the
// capture is exported as a dataset and reopened. Also what day one with a
// real phone needs around it: a wrong file is refused before it is
// uploaded, a finished room opens seen from above, the pause button is
// away during a capture, a page without a server says why the feature is
// absent. Skipped when the binary is not built.
const { spawnSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { check, REPO, OUT, DIST, PORT, newPage, pinBlend, consoleErrors, state, waitState, open, shot, viewShot, content, sleep, startServer, SERVER_BIN: BIN } = require('./lib.cjs');

// Iterations of every refinement of this suite: long enough to be watched
// and stopped, short enough to wait for.
const ITERATIONS = 150;

const getJson = async (url, init) => {
  const r = await fetch(url, init);
  return { status: r.status, body: await r.json().catch(() => null) };
};

// A simulated phone capture of the bedroom of the house (gs-core's phone
// simulator: 480x360 photographs, 256x192 depth), made on first use and
// kept in the output directory.
function bedroom() {
  const file = path.join(OUT, 'refine-bedroom.gscap');
  if (!fs.existsSync(file)) {
    fs.mkdirSync(OUT, { recursive: true });
    const args = ['run', '--release', '-q', '-p', 'gs-core', '--features', 'par', '--example', 'gscap_write', '--', 'bedroom', file, '--frames', '80', '--color-width', '480'];
    const r = spawnSync('cargo', args, { cwd: REPO, stdio: ['ignore', 'ignore', 'inherit'] });
    if (r.status !== 0) throw new Error('gscap_write bedroom failed');
  }
  return file;
}

async function run({ browser, url: staticUrl }) {
  if (!fs.existsSync(BIN)) {
    console.log(`SKIPPED: ${BIN} does not exist (cargo build -p gs-server --release)`);
    return { skipped: true };
  }
  const capture = bedroom();
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'splat-live-refine-'));
  const dist = path.join(tmp, 'dist');
  fs.cpSync(DIST, dist, { recursive: true });
  const port = PORT + 80;
  const server = await startServer({ dist, certs: path.join(tmp, 'certs'), port, args: ['--light', '--refine-iterations', String(ITERATIONS), '--refine-threads', '3'] });
  const measurements = {};
  try {
    const url = `${server.url}/`;
    const { ctx, page, logs } = await newPage(browser);
    await ctx.grantPermissions(['clipboard-read', 'clipboard-write']);
    const posts = [];
    page.on('request', (request) => {
      if (request.method() === 'POST') posts.push(new URL(request.url()).pathname);
    });
    let s = await open(page, url);
    // The kept captures live in the Pièces workspace, one card per room.
    const toRooms = async () => {
      if ((await page.locator('#ws-rooms').getAttribute('aria-selected')) !== 'true') await page.click('#ws-rooms');
      if ((await page.locator('#room-view').count()) === 1) await page.click('#ws-rooms');
      await page.waitForSelector('#gallery');
    };
    const toDirect = async () => {
      await page.click('#ws-direct');
      await page.waitForFunction(() => !document.querySelector('#gallery') && !document.querySelector('#room-view'));
    };
    const openRoom = async () => {
      await toRooms();
      await page.locator('.room-card .room-card-button').first().click();
      // The room's container has no box of its own: its parts are fixed.
      await page.waitForSelector('#room-view .room-strip');
    };
    await toRooms();
    check('refine: with a server the gallery lists the kept captures, none yet', (await page.locator('.gallery-empty').count()) === 1 && !/application de bureau/.test(await page.locator('.gallery-empty').innerText()) && s.captures.length === 0 && s.refined.length === 0, (await page.locator('#gallery').innerText()).replace(/\s+/g, ' ').slice(0, 160));
    await toDirect();

    // ---- a file that is no capture is refused before anything is sent
    const wrong = path.join(OUT, 'wrong.gscap');
    fs.writeFileSync(wrong, Buffer.concat([Buffer.from('GSREC1\0\0'), Buffer.alloc(300000, 7)]));
    await page.setInputFiles('#gscap-input', wrong);
    s = await waitState(page, (s) => s.import && s.import.progress.done && s.import.file === 'wrong.gscap', 30000);
    check('refine: a file that is not a capture is refused by the page, without an upload', /n'est pas une capture/.test(await page.locator('#import-status').innerText()) && s.notice.error && !posts.includes('/api/captures') && s.scene === 'stage' && !s.capture, `${await page.locator('#import-status').innerText()} | posts ${posts.join(',')}`);
    await page.click('#import-dismiss');
    fs.rmSync(wrong, { force: true });

    // ---- the capture goes to the server, which keeps it
    await page.setInputFiles('#gscap-input', capture);
    s = await waitState(page, (s) => s.import && s.import.progress.done && s.capture && s.capture.stats && s.capture.stats.finished && s.captures.length === 1, 180000);
    check('refine: the server ingests the capture and keeps its file', s.import.upload && s.import.progress.error === null && s.import.progress.frames === 80 && s.captures[0].mode === 'room' && /\.gscap$/.test(s.captures[0].file) && posts.includes('/api/captures'), JSON.stringify(s.captures[0]));
    const kept = s.captures[0];
    await page.click('#import-dismiss');
    // A finished room is looked at from outside, above it: not from the
    // phone's last position, inside the room and a step from a wall.
    check('refine: a finished room capture opens seen from above', s.nav === 'orbit' && s.eye[1] > 3.0 && s.eye[1] > s.target[1] + 2.0, `eye ${JSON.stringify(s.eye)}, target ${JSON.stringify(s.target)}`);
    check('refine: the pause button is away during a capture (the server ignores it)', !(await page.locator('#btn-pause').isVisible()));
    await sleep(500);
    const fusedView = await viewShot(page, 'refine-capture-view.png');
    const fused = await content(page, fusedView);
    check('refine: the captured room is drawn', fused.colors > 40 && fused.bright > 0.05, `colors ${fused.colors}, bright ${fused.bright.toFixed(3)}`);

    // ---- the gallery, the room and the form
    await toRooms();
    const card = page.locator('.room-card').first();
    await waitState(page, (s) => s.captures.length === 1 && s.captures[0].frames === 80, 30000);
    await sleep(300);
    const cardText = (await card.innerText()).replace(/\s+/g, ' ');
    check('refine: the kept capture is a room card with its mode, frames, size and versions', (await page.locator('.room-card').count()) === 1 && /Simulation bedroom/.test(cardText) && /Pièce/.test(cardText) && /80 images/.test(cardText) && /(ko|Mo)/.test(cardText) && /\d versions?/.test(cardText) && /pas encore affinée/.test(cardText), cardText);
    await openRoom();
    const strip = (await page.locator('.room-strip').innerText()).replace(/\s+/g, ' ');
    check('refine: an opened room shows its raw capture and offers a new refinement', /Capture brute/.test(strip) && /Nouvel affinage/.test(strip) && (await page.locator('.version[data-kind=refined]').count()) === 0, strip);
    // The mouse over the window of an open room moves nothing: the app root
    // once shared a class with the gallery's cards and took their hover
    // lift, which re-anchored every fixed part (the view, the strip) to it.
    const boxes = [];
    for (let i = 0; i < 12; i++) {
      await page.mouse.move(300 + i * 40, 300 + (i % 3) * 20);
      boxes.push(await page.evaluate(() => [Math.round(document.querySelector('.room-strip').getBoundingClientRect().top), document.getElementById('view').clientHeight, getComputedStyle(document.getElementById('app')).transform]));
    }
    check('refine: the mouse over an open room moves neither the strip nor the view', boxes.every((b) => b[0] === boxes[0][0] && b[1] === boxes[0][1] && b[2] === 'none'), JSON.stringify(boxes.slice(0, 3)));
    await page.click('.version.add .version-open');
    await page.waitForSelector('#refine-form');
    const honest = await page.locator('#refine-honest').innerText();
    check('refine: the form says what the refinement gains, on what data, and that it is CPU only', /simulées/.test(honest) && /contours/.test(honest) && /trajet du téléphone/.test(honest) && /moutonnement/.test(honest) && /processeur/.test(honest) && /GPU/.test(honest) && /n'existe pas encore/.test(honest), honest);
    const hints = [];
    for (const index of [0, 1, 2]) {
      await page.click(`#refine-preset-${index}`);
      hints.push(await page.locator('#refine-preset-hint').innerText());
    }
    check('refine: each preset says what it costs, as measured on the development machine', /480 pixels/.test(hints[0]) && /minutes/.test(hints[0]) && /640 pixels/.test(hints[1]) && /960 pixels/.test(hints[2]) && /Jamais mené à son terme/.test(hints[2]) && hints.every((h) => /machine de développement/.test(h)), hints.join(' | '));
    check('refine: the choice for what the sensor did not measure is offered', /Peindre/.test(await page.locator('#refine-beyond-0').innerText()) && /Laisser les trous/.test(await page.locator('#refine-beyond-1').innerText()));
    check('refine: no text of the form uses a dash as punctuation', !/[–—]/.test(await page.locator('#refine-form').innerText()));
    await page.click('#refine-preset-0');
    await shot(page, 'refine-form.png');

    // ---- a job is started, watched and cancelled: nothing is kept
    await page.click('#refine-start');
    s = await waitState(page, (s) => s.job && s.job.stage === 'optimise' && s.job.iteration >= 5, 120000);
    check('refine: the job runs on the server and the page follows it, in the strip', s.job.kind === 'refine' && s.job.capture === kept.file && s.job.iterations === ITERATIONS && (await page.locator('.version.job').count()) === 1 && (await page.locator('.room-refine #refine-job').count()) === 1, JSON.stringify(s.job));
    const info = await getJson(`${url}api/info`);
    check('refine: the server stays responsive while it refines', info.status === 200 && s.connected);
    await page.click('#refine-cancel');
    s = await waitState(page, (s) => !s.job && s.jobEnd, 60000);
    const cancelled = await page.locator('#refine-summary').innerText();
    check('refine: a cancelled job ends within seconds and keeps nothing', s.jobEnd.outcome === 'cancelled' && s.jobEnd.result === null && s.refined.length === 0 && /annulé/.test(cancelled) && /rien n'a été conservé/.test(cancelled) && (await page.locator('#refine-view').count()) === 0, cancelled);
    check('refine: no result file is left by the cancelled job', fs.readdirSync(path.join(tmp, 'refined')).length === 0, fs.readdirSync(path.join(tmp, 'refined')).join(','));
    await page.click('#refine-dismiss');
    await page.waitForFunction(() => !document.querySelector('#refine-last'));

    // ---- the job again, to its end
    if ((await page.locator('#refine-form').count()) === 0) await page.click('.version.add .version-open');
    await page.click('#refine-preset-0');
    const started = Date.now();
    await page.click('#refine-start');
    s = await waitState(page, (s) => s.job && s.job.stage === 'optimise' && s.job.iteration >= 10, 120000);
    const jobCard = (await page.locator('#refine-job').innerText()).replace(/\s+/g, ' ');
    check('refine: the job card shows the stage, the iteration, the time and the way to stop', /Optimisation/.test(jobCard) && /Rapide/.test(jobCard) && /Itération \d+ sur 150/.test(jobCard) && /gaussiennes/.test(jobCard) && /Depuis/.test(jobCard) && /Annuler/.test(jobCard) && /Simulation bedroom/.test(jobCard) && !/dB|PSNR/.test(jobCard), jobCard);
    const bar = await page.locator('#refine-job [role=progressbar]').getAttribute('aria-valuenow');
    check('refine: the progress bar follows the iterations', Number(bar) > 0 && Number(bar) < 100, `aria-valuenow ${bar}`);
    check('refine: while a job runs no other can be started', (await page.locator('#refine-start').count()) === 0 || (await page.locator('#refine-start').isDisabled()));
    await shot(page, 'refine-running.png');
    s = await waitState(page, (s) => !s.job && s.jobEnd && s.jobEnd.outcome !== 'cancelled', 300000);
    measurements.refineSeconds = Number(((Date.now() - started) / 1000).toFixed(1));
    s = await waitState(page, (s) => s.refined.length === 1, 30000);
    const result = s.refined[0];
    const summary = await page.locator('#refine-summary').innerText();
    check('refine: the job ends with a result, summed up in one sentence', s.jobEnd.outcome === 'done' && s.jobEnd.result === result.file && /est affinée/.test(summary) && /gaussiennes/.test(summary) && !/dB|PSNR/.test(summary) && /est affinée/.test(s.notice.text), summary);
    const resultCard = page.locator('.version[data-kind=refined]').first();
    await resultCard.waitFor();
    check('refine: the result joins the room\'s strip with its preset and held-out score', result.preset === 'rapide' && result.capture === kept.file && result.gaussians > 1000 && /-rapide\.ply$/.test(result.file) && typeof result.psnr === 'number' && /Rapide/.test(await resultCard.innerText()) && /dB/.test(await resultCard.innerText()), JSON.stringify(result));
    const onDisk = fs.readdirSync(path.join(tmp, 'refined')).sort();
    check('refine: the server keeps the .ply and its description', onDisk.length === 2 && onDisk[0] === result.file.replace(/\.ply$/, '.json') && onDisk[1] === result.file, onDisk.join(','));

    // ---- the result on screen
    await resultCard.locator('.version-open').click();
    s = await waitState(page, (s, n) => s.file && s.fileMode && s.layers.static === n, 120000, result.gaussians);
    check('refine: the result opens in the viewer as an imported file, drawn sorted', s.file.name === result.file && s.file.splats === result.gaussians && s.viewer.blend === 'sorted' && s.layers.dynamic === 0, `${s.file.splats} splats, blend ${s.viewer.blend}`);
    check('refine: a refined room is looked at from inside, for a walk', s.nav === 'fly' && s.eye[1] > 0.5 && s.eye[1] < 2.0 && Math.abs(s.eye[0]) < 4 && Math.abs(s.eye[2]) < 6, `nav ${s.nav}, eye ${JSON.stringify(s.eye)}`);
    await sleep(600);
    await shot(page, 'refine-result.png');
    const refinedView = await viewShot(page, 'refine-result-view.png');
    const refined = await content(page, refinedView);
    check('refine: the refined room is drawn', refined.colors > 40 && refined.bright > 0.05, `colors ${refined.colors}, bright ${refined.bright.toFixed(3)}`);
    check('refine: the version on screen is marked in the strip', (await resultCard.getAttribute('class')).includes('on'));
    check('refine: Comparer offers the raw capture', !(await page.locator('#room-compare').isDisabled()) && /Capture brute/.test(await page.locator('#room-compare').getAttribute('title')));
    await toDirect();
    s = await waitState(page, (s) => !s.file && s.capture && s.layers.static > 0 && s.sceneName === 'capture', 60000);
    check('refine: Direct returns to the capture as it was fused', s.capture.stats === null || s.capture.stats.finished);

    // ---- download
    const showResult = async () => {
      await openRoom();
      const r = page.locator('.version[data-kind=refined]').first();
      if (!(await r.getAttribute('class')).includes('on')) await r.locator('.version-open').click();
      await page.waitForSelector('.version-actions [aria-label=Télécharger]');
    };
    await showResult();
    const [download] = await Promise.all([page.waitForEvent('download'), page.click('.version-actions [aria-label=Télécharger]')]);
    const saved = path.join(OUT, 'refine-result.ply');
    await download.saveAs(saved);
    const head = fs.readFileSync(saved).subarray(0, 4).toString('latin1');
    check('refine: the result downloads as a 3DGS .ply', download.suggestedFilename() === result.file && head === 'ply\n' && fs.statSync(saved).size === result.size, `${download.suggestedFilename()}, ${fs.statSync(saved).size} bytes`);
    fs.rmSync(saved, { force: true });

    // ---- shared like a recording: LAN link, embedded viewer, archive
    await page.click('.version-actions [aria-label=Partager]');
    await page.waitForSelector('#share-dialog');
    check('refine: the share dialog opens on the refined result', /Simulation bedroom/.test(await page.locator('#share-title').innerText()) && /Capture affinée/.test(await page.locator('#share-dialog').innerText()));
    if ((await page.locator('#share-lan-on').count()) === 1) await page.click('#share-lan-on');
    await page.waitForSelector('#share-link', { timeout: 30000 });
    const link = await page.locator('#share-link').inputValue();
    const zipHref = await page.locator('#share-zip').getAttribute('href');
    check('refine: the link names the result for the embedded viewer', /^https:\/\/[\d.]+:\d+\/\?file=refined\/.+-rapide\.ply&embed=1&view=[-\d.,]+&nav=fly$/.test(link) && /\/api\/share\/refined\/.+-rapide\.zip$/.test(zipHref) && /<iframe /.test(await page.locator('#share-embed').inputValue()), `${link} | ${zipHref}`);
    const zip = await fetch(zipHref);
    const zipBytes = Buffer.from(await zip.arrayBuffer());
    check('refine: the web archive holds the viewer and the result', zip.status === 200 && zip.headers.get('content-type') === 'application/zip' && zipBytes.includes('scene.ply') && zipBytes.includes('share.json') && zipBytes.includes('index.html') && zipBytes.length > result.size, `${zipBytes.length} bytes`);
    await page.click('#share-close');
    const guest = await browser.newContext({ viewport: { width: 1000, height: 640 }, ignoreHTTPSErrors: true });
    await pinBlend(guest);
    const guestPage = await guest.newPage();
    const guestLogs = [];
    guestPage.on('console', (m) => guestLogs.push(`${m.type()}: ${m.text()}`));
    // The LAN address of the machine may not route in a sandbox: the same
    // listener answers on the loopback interface.
    const lanPort = new URL(link).port;
    await guestPage.goto(link.replace(/^https:\/\/[\d.]+:/, 'https://127.0.0.1:'));
    // (A still scene is drawn once: the viewer renders on demand.)
    let g = await waitState(guestPage, (s, n) => s.file && s.layers.static === n && s.viewer.framesRendered >= 1, 120000, result.gaussians);
    check('refine: the shared link opens the refined result in the embedded viewer, inside the room', g.embed && g.file.name === result.file && g.nav === 'fly' && g.eye[1] < 2.0 && (await guestPage.locator('#panel').count()) === 0 && lanPort.length > 0, `${JSON.stringify(g.file)}, eye ${JSON.stringify(g.eye)}`);
    await sleep(500);
    await guestPage.screenshot({ path: path.join(OUT, 'refine-shared.png') });
    check('refine: the embedded viewer has no console error', consoleErrors(guestLogs).length === 0, consoleErrors(guestLogs).join(' | '));
    // A client of the LAN side lists the results and nothing else.
    await guestPage.goto(`https://127.0.0.1:${lanPort}/`);
    g = await waitState(guestPage, (s) => s.connected && s.refined.length === 1, 90000);
    await guestPage.click('#ws-rooms');
    await guestPage.waitForSelector('#gallery');
    check('refine: a LAN client sees the refined results, not the raw captures', (await guestPage.locator('.orphan').count()) === 1 && (await guestPage.locator('.room-card').count()) === 0 && (await guestPage.locator('#gallery-import').count()) === 0 && g.captures.length === 0);
    await guest.close();

    // ---- the capture as a dataset for another trainer
    await openRoom();
    await page.click('.version.add .version-open');
    await page.click('#export-colmap');
    s = await waitState(page, (s) => !s.job && s.jobEnd && s.jobEnd.kind === 'export', 180000);
    const dir = await page.locator('#dataset-dir').inputValue();
    check('refine: the capture is exported as a COLMAP dataset, in a directory the page shows', s.jobEnd.outcome === 'done' && s.jobEnd.dataset === dir && dir.startsWith(path.join(tmp, 'datasets')) && fs.existsSync(path.join(dir, 'sparse/0/cameras.txt')) && fs.readdirSync(path.join(dir, 'images')).length > 20 && /COLMAP/.test(await page.locator('#refine-summary').innerText()), dir);
    await page.click('#refine-dismiss');

    // ---- leaving the capture, and opening it again from its room
    await toDirect();
    await page.click('#mode-stage');
    s = await waitState(page, (s) => s.scene === 'stage' && !s.capture && s.layers.dynamic > 0, 60000);
    check('refine: the pause button is back with the simulation', await page.locator('#btn-pause').isVisible());
    await openRoom();
    await page.click('.version[data-kind=capture] .version-open');
    s = await waitState(page, (s) => s.capture && s.capture.stats && s.capture.stats.finished && s.import && s.import.progress.done, 180000);
    check('refine: a kept capture is reopened from its room', s.sceneName === 'capture' && s.import.upload && s.import.progress.frames === 80 && posts.includes('/api/captures/open'), JSON.stringify(s.import.progress));

    // ---- deleting asks once more
    await page.locator('.version[data-kind=refined] .version-open').first().click();
    s = await waitState(page, (s, name) => s.file && s.file.name === name, 120000, result.file);
    const bin = page.locator('.version-delete');
    await bin.click();
    check('refine: the bin of a result asks before it deletes', (await bin.getAttribute('class')).includes('confirm') && /Supprimer/.test(await bin.innerText()) && s.refined.length === 1);
    await bin.click();
    s = await waitState(page, (s) => s.refined.length === 0, 30000);
    check('refine: the result and its description are deleted from the disk', fs.readdirSync(path.join(tmp, 'refined')).length === 0);
    await page.click('.version[data-kind=capture] .version-open');
    const captureBin = page.locator('.room-delete');
    await captureBin.click();
    await captureBin.click();
    s = await waitState(page, (s) => s.captures.length === 0, 30000);
    check('refine: a kept capture can be deleted from its room', fs.readdirSync(path.join(tmp, 'captures')).filter((f) => f.endsWith('.gscap')).length === 0);

    s = await state(page);
    check('refine: no application error', s.errors.length === 0, s.errors.join(' | '));
    check('refine: no console error', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
    await ctx.close();

    // ---- without a server: one sentence
    const alone = await newPage(browser);
    await open(alone.page, staticUrl);
    await alone.page.click('#ws-rooms');
    await alone.page.waitForSelector('#gallery');
    const absent = await alone.page.locator('#gallery').innerText();
    check('refine: without a server the feature is absent, with one sentence saying why', /application de bureau/.test(absent) && /ne tourne pas dans une page web/.test(absent) && (await alone.page.locator('.room-card').count()) === 0 && (await alone.page.locator('#refine-start').count()) === 0, absent.replace(/\s+/g, ' '));
    await shot(alone.page, 'refine-absent.png');
    await alone.ctx.close();
  } finally {
    await server.stop();
    fs.rmSync(tmp, { recursive: true, force: true });
  }
  return measurements;
}

module.exports = { run, bedroom };
if (require.main === module) require('./standalone.cjs')(run);
