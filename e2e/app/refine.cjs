// "Affiner cette capture", end to end, with a native gs-server: a capture
// file goes to the server, is listed among the kept captures, is refined
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
    const section = page.locator('#section-captures');
    await section.scrollIntoViewIfNeeded();
    check('refine: with a server the panel lists the kept captures, none yet', (await page.locator('#captures-empty').count()) === 1 && (await page.locator('#captures-absent').count()) === 0 && s.captures.length === 0 && s.refined.length === 0, (await section.innerText()).replace(/\s+/g, ' ').slice(0, 160));

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

    // ---- the list and the form
    await section.scrollIntoViewIfNeeded();
    const row = page.locator('#captures .capture').first();
    const rowText = (await row.innerText()).replace(/\s+/g, ' ');
    check('refine: the kept capture is listed with its mode, size and date', (await page.locator('#captures .capture').count()) === 1 && /Simulation bedroom/.test(rowText) && /Pièce/.test(rowText) && /(ko|Mo)/.test(rowText) && /\d\d\/\d\d\/\d{4}/.test(rowText), rowText);
    const labels = await row.locator('.take-actions button').evaluateAll((buttons) => buttons.map((b) => b.getAttribute('aria-label')));
    check('refine: a capture can be reopened, refined and deleted', labels.length === 3 && /^Rouvrir/.test(labels[0]) && /^Affiner/.test(labels[1]) && /^Supprimer/.test(labels[2]), labels.join(' | '));
    await row.locator('.capture-refine').click();
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
    check('refine: no text of the form uses a dash as punctuation', !/[\u2013\u2014]/.test(await page.locator('#refine-form').innerText()));
    await page.click('#refine-preset-0');
    await shot(page, 'refine-form.png');

    // ---- a job is started, watched and cancelled: nothing is kept
    await page.click('#refine-start');
    s = await waitState(page, (s) => s.job && s.job.stage === 'optimise' && s.job.iteration >= 5, 120000);
    check('refine: the job runs on the server and the page follows it', s.job.kind === 'refine' && s.job.capture === kept.file && s.job.iterations === ITERATIONS && (await page.locator('#refine-form').count()) === 0, JSON.stringify(s.job));
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
    await row.locator('.capture-refine').click();
    await page.click('#refine-preset-0');
    const started = Date.now();
    await page.click('#refine-start');
    s = await waitState(page, (s) => s.job && s.job.stage === 'optimise' && s.job.iteration >= 10, 120000);
    await section.scrollIntoViewIfNeeded();
    const card = (await page.locator('#refine-job').innerText()).replace(/\s+/g, ' ');
    check('refine: the job card shows the stage, the iteration, the time and the way to stop', /Optimisation/.test(card) && /Rapide/.test(card) && /Itération \d+ sur 150/.test(card) && /gaussiennes/.test(card) && /Depuis/.test(card) && /Annuler/.test(card) && /Simulation bedroom/.test(card) && !/dB|PSNR/.test(card), card);
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
    check('refine: the result is listed with its preset and size', result.preset === 'rapide' && result.capture === kept.file && result.gaussians > 1000 && /-rapide\.ply$/.test(result.file) && /Rapide/.test(await page.locator('#refined .refined .take-meta').innerText()), JSON.stringify(result));
    const onDisk = fs.readdirSync(path.join(tmp, 'refined')).sort();
    check('refine: the server keeps the .ply and its description', onDisk.length === 2 && onDisk[0] === result.file.replace(/\.ply$/, '.json') && onDisk[1] === result.file, onDisk.join(','));

    // ---- "Voir le résultat"
    await page.click('#refine-view');
    s = await waitState(page, (s, n) => s.file && s.fileMode && s.layers.static === n, 120000, result.gaussians);
    check('refine: the result opens in the viewer as an imported file, drawn sorted', s.file.name === result.file && s.file.splats === result.gaussians && s.viewer.blend === 'sorted' && s.layers.dynamic === 0, `${s.file.splats} splats, blend ${s.viewer.blend}`);
    check('refine: a refined room is looked at from inside, for a walk', s.nav === 'fly' && s.eye[1] > 0.5 && s.eye[1] < 2.0 && Math.abs(s.eye[0]) < 4 && Math.abs(s.eye[2]) < 6, `nav ${s.nav}, eye ${JSON.stringify(s.eye)}`);
    await sleep(600);
    await section.scrollIntoViewIfNeeded();
    await shot(page, 'refine-result.png');
    const refinedView = await viewShot(page, 'refine-result-view.png');
    const refined = await content(page, refinedView);
    check('refine: the refined room is drawn', refined.colors > 40 && refined.bright > 0.05, `colors ${refined.colors}, bright ${refined.bright.toFixed(3)}`);
    check('refine: the row of the result on screen is marked', (await page.locator('#refined .refined').first().getAttribute('class')).includes('playing'));
    await page.locator('#section-file').scrollIntoViewIfNeeded();
    await page.click('#back-live');
    s = await waitState(page, (s) => !s.file && s.capture && s.layers.static > 0 && s.sceneName === 'capture', 60000);
    check('refine: "Revenir au direct" returns to the capture as it was fused', s.capture.stats === null || s.capture.stats.finished);

    // ---- download
    await section.scrollIntoViewIfNeeded();
    const resultRow = page.locator('#refined .refined').first();
    const [download] = await Promise.all([page.waitForEvent('download'), resultRow.locator('.refined-download').click()]);
    const saved = path.join(OUT, 'refine-result.ply');
    await download.saveAs(saved);
    const head = fs.readFileSync(saved).subarray(0, 4).toString('latin1');
    check('refine: the result downloads as a 3DGS .ply', download.suggestedFilename() === result.file && head === 'ply\n' && fs.statSync(saved).size === result.size, `${download.suggestedFilename()}, ${fs.statSync(saved).size} bytes`);
    fs.rmSync(saved, { force: true });

    // ---- shared like a recording: LAN link, embedded viewer, archive
    await resultRow.locator('.refined-share').click();
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
    check('refine: a LAN client sees the refined results, not the raw captures', (await guestPage.locator('#captures-remote').count()) === 1 && (await guestPage.locator('#captures .capture').count()) === 0 && g.captures.length === 0 && (await guestPage.locator('#refined .refined-view').count()) === 1 && (await guestPage.locator('#refined .refined-delete').count()) === 0);
    await guest.close();

    // ---- the capture as a dataset for another trainer
    await row.locator('.capture-refine').click();
    await page.click('#export-colmap');
    s = await waitState(page, (s) => !s.job && s.jobEnd && s.jobEnd.kind === 'export', 180000);
    const dir = await page.locator('#dataset-dir').inputValue();
    check('refine: the capture is exported as a COLMAP dataset, in a directory the page shows', s.jobEnd.outcome === 'done' && s.jobEnd.dataset === dir && dir.startsWith(path.join(tmp, 'datasets')) && fs.existsSync(path.join(dir, 'sparse/0/cameras.txt')) && fs.readdirSync(path.join(dir, 'images')).length > 20 && /COLMAP/.test(await page.locator('#refine-summary').innerText()), dir);
    await page.click('#refine-dismiss');

    // ---- leaving the capture, and opening it again from the list
    await page.click('#mode-stage');
    s = await waitState(page, (s) => s.scene === 'stage' && !s.capture && s.layers.dynamic > 0, 60000);
    check('refine: the pause button is back with the simulation', await page.locator('#btn-pause').isVisible());
    await section.scrollIntoViewIfNeeded();
    await row.locator('.capture-open').click();
    s = await waitState(page, (s) => s.capture && s.capture.stats && s.capture.stats.finished && s.import && s.import.progress.done, 180000);
    check('refine: a kept capture is reopened from the list', s.sceneName === 'capture' && s.import.upload && s.import.progress.frames === 80 && posts.includes('/api/captures/open'), JSON.stringify(s.import.progress));
    await page.click('#import-dismiss');

    // ---- deleting asks once more
    const bin = resultRow.locator('.refined-delete');
    await bin.click();
    check('refine: the bin of a result asks before it deletes', (await bin.getAttribute('class')).includes('confirm') && /Supprimer/.test(await bin.innerText()) && s.refined.length === 1);
    await bin.click();
    s = await waitState(page, (s) => s.refined.length === 0, 30000);
    check('refine: the result and its description are deleted from the disk', fs.readdirSync(path.join(tmp, 'refined')).length === 0);
    const captureBin = row.locator('.capture-delete');
    await captureBin.click();
    await captureBin.click();
    s = await waitState(page, (s) => s.captures.length === 0, 30000);
    check('refine: a kept capture can be deleted', fs.readdirSync(path.join(tmp, 'captures')).filter((f) => f.endsWith('.gscap')).length === 0);

    s = await state(page);
    check('refine: no application error', s.errors.length === 0, s.errors.join(' | '));
    check('refine: no console error', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
    await ctx.close();

    // ---- without a server: one sentence
    const alone = await newPage(browser);
    await open(alone.page, staticUrl);
    await alone.page.locator('#section-captures').scrollIntoViewIfNeeded();
    const absent = await alone.page.locator('#section-captures').innerText();
    check('refine: without a server the feature is absent, with one sentence saying why', (await alone.page.locator('#captures-absent').count()) === 1 && /application de bureau/.test(absent) && /ne tourne pas dans une page web/.test(absent) && (await alone.page.locator('#captures').count()) === 0 && (await alone.page.locator('#refine-start').count()) === 0, absent.replace(/\s+/g, ' '));
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
