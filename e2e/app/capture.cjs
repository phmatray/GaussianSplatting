// A capture file of the phone companion opened without a server: the
// worker ingests the .gscap itself, frame by frame, and the page follows it
// like a live capture. Then: saving it as a recording, leaving it, damaged
// files.
const { spawnSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const { REPO, OUT, check, newPage, consoleErrors, state, waitState, open, shot, viewShot, content, sleep } = require('./lib.cjs');

// A synthetic capture written by the reference tool: a walk through a box
// room (4 x 2.6 x 5 m). `depth` is the width of the depth map (256 is what
// the phone's LiDAR delivers; such frames take the worker long enough to
// watch an import run).
function synth(name, { frames = 80, mode = 'room', title = 'Salon', depth = 64, color = 160 } = {}) {
  const file = path.join(OUT, name);
  const args = ['synth', file, '--frames', String(frames), '--mode', mode, '--name', title, '--depth-width', String(depth), '--color-width', String(color)];
  const r = spawnSync('python3', [path.join(REPO, 'tools/gscap.py'), ...args], { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`gscap.py synth failed: ${r.stderr}`);
  return file;
}

async function run({ browser, url }) {
  const room = synth('room.gscap', { frames: 150, depth: 256, color: 640 });
  const { ctx, page, logs } = await newPage(browser);
  let s = await open(page, url);
  await page.locator('#section-phone').scrollIntoViewIfNeeded();
  const lead = await page.locator('#section-phone').innerText();
  check('capture: without a server the iPhone section explains what works here', /Capturer avec un iPhone/.test(lead) && /application de bureau/.test(lead) && /\.gscap/.test(lead) && (await page.locator('#pair-start').count()) === 0, lead.replace(/\s+/g, ' ').slice(0, 200));

  // ---- import, followed while it runs
  await page.setInputFiles('#gscap-input', room);
  s = await waitState(page, (s) => s.import && s.capture && s.sceneName === 'capture', 60000);
  check('capture: the worker opens the capture file and the page shows a capture session', s.capture.info.mode === 'room' && s.capture.info.name === 'Salon' && s.capture.info.imported && s.scene === 'house' && s.import.file === 'room.gscap' && !s.import.upload, JSON.stringify(s.capture.info));
  s = await waitState(page, (s) => s.import.progress.frames >= 10 && s.layers.static > 1000 && s.capture.stats && s.capture.stats.frames_used >= 5, 120000);
  const mid = s.layers.static;
  check('capture: the reconstruction builds up while the file is read', !s.import.progress.done && s.import.progress.bytes > 0 && s.import.progress.total === fs.statSync(room).size && s.capture.state === 'running', JSON.stringify(s.import.progress));
  const card = await page.locator('#capture-card').innerText();
  check('capture: the panel follows the capture (mode, name, device, frames)', /Import en cours/.test(card) && /Pièce/.test(card) && /Salon/.test(card) && /Images reçues/.test(card) && /Images utilisées/.test(card) && (await page.locator('#cam-count').count()) === 0 && (await page.locator('#scan-start').count()) === 0, card.replace(/\s+/g, ' '));
  const pill = await page.locator('#capture-pill').innerText();
  check('capture: a pill over the view says what is going on', /Pièce/.test(pill) && /images/.test(pill) && (await page.locator('#scan-pill').count()) === 0, pill.replace(/\s+/g, ' '));
  check('capture: the import shows its progress and can be stopped', (await page.locator('#import [role=progressbar]').count()) === 1 && (await page.locator('#import-cancel').isVisible()) && /lue/.test(await page.locator('#import-status').innerText()), await page.locator('#import-status').innerText());
  check('capture: the phone has its preview tile', s.previews.ids.length === 1 && s.previews.painted['0'] > 0 && /Caméra mobile/.test(await page.locator('#tiles .tile-label').first().innerText()), JSON.stringify(s.previews));
  check('capture: neither simulation mode is selected, orbit navigation', (await page.locator('#mode-house').getAttribute('aria-selected')) === 'false' && s.nav === 'orbit' && !s.display.dollhouse);
  await page.locator('#section-phone').scrollIntoViewIfNeeded();
  await shot(page, 'capture-import.png');

  s = await waitState(page, (s) => s.import.progress.done, 300000);
  check('capture: the import ends with every frame read and the capture finished', s.import.progress.frames === 150 && s.import.progress.error === null && !s.import.progress.truncated && s.capture.stats.finished && s.capture.stats.frames_received === 150 && s.capture.state === 'finished' && s.layers.static > mid, `${s.layers.static} splats, ${JSON.stringify(s.import.progress)}`);
  check('capture: a notice sums the import up', /Capture importée/.test(s.notice.text) && /150 images lues/.test(s.notice.text) && !s.notice.error, s.notice.text);
  check('capture: Stats and viewer agree on the map', s.stats.splats_static === s.layers.static && s.stats.scan.state === 'done');
  await sleep(400);
  const view = await viewShot(page, 'capture-room-view.png');
  const c = await content(page, view);
  check('capture: the captured room is drawn', c.colors > 40 && c.bright > 0.05, `colors ${c.colors}, bright ${c.bright.toFixed(3)}`);
  await shot(page, 'capture-room.png');
  await page.click('#import-dismiss');
  await page.waitForFunction(() => !window.__splatLive.state().import);

  // ---- the finished capture saved as a recording in one click
  const splats = s.layers.static;
  await page.locator('#section-record').scrollIntoViewIfNeeded();
  await page.click('#save-still');
  s = await waitState(page, (s) => s.takes.length === 1, 30000);
  check('capture: the finished capture is saved as a still', s.takes[0].scene === 'capture' && s.takes[0].durationS === 0 && /^Capture /.test(s.takes[0].title), JSON.stringify(s.takes[0]));
  await page.locator('#takes .take-play').first().click();
  s = await waitState(page, (s, splats) => s.replay && s.replay.status.ready && s.layers.static === splats && s.capture, 60000, splats);
  check('capture: its recording replays as the same capture', s.sceneName === 'capture' && s.capture.info.name === 'Salon' && s.replay.status.scene === 'capture', `${s.layers.static} splats`);
  await page.click('#tl-close');
  // The live source is the capture session the worker still holds.
  s = await waitState(page, (s, splats) => !s.replay && s.capture && s.layers.static === splats && s.capture.stats && s.capture.stats.finished, 30000, splats);
  check('capture: closing the replay returns to the capture session', s.sceneName === 'capture');

  // ---- leaving the capture for the simulation
  await page.click('#mode-stage');
  s = await waitState(page, (s) => s.scene === 'stage' && !s.capture && s.layers.dynamic > 0 && s.sceneName === 'stage', 60000);
  check('capture: a mode button leaves the capture for the simulation', (await page.locator('#capture-card').count()) === 0 && (await page.locator('#cam-count').count()) === 1 && s.nav === 'orbit');

  // ---- an object capture, stopped half way
  const object = synth('object.gscap', { frames: 300, mode: 'object', title: 'Vase', depth: 192, color: 320 });
  await page.setInputFiles('#gscap-input', object);
  s = await waitState(page, (s) => s.import && s.capture && s.capture.info.mode === 'object' && s.import.progress.frames >= 5, 120000);
  await page.click('#import-cancel');
  s = await waitState(page, (s) => s.import.progress.done, 60000);
  check('capture: an import can be stopped, what was fused stays', s.import.progress.frames < 300 && s.capture.stats.finished && s.sceneName === 'capture' && /Objet/.test(await page.locator('#capture-mode').innerText()), `${s.import.progress.frames} of 300 frames`);
  await page.click('#import-dismiss');

  // ---- files that are not captures
  const bad = path.join(OUT, 'bad.gscap');
  fs.writeFileSync(bad, Buffer.from('GSREC1\0\0 not a capture at all, just some bytes'));
  await page.setInputFiles('#gscap-input', bad);
  s = await waitState(page, (s) => s.import && s.import.progress.done && s.import.file === 'bad.gscap', 60000);
  check('capture: a file that is not a capture is refused in French', s.import.progress.error === 'bad_file_magic' && /n'est pas une capture/.test(await page.locator('#import-status').innerText()) && s.notice.error, await page.locator('#import-status').innerText());
  const cut = path.join(OUT, 'cut.gscap');
  const whole = fs.readFileSync(room);
  fs.writeFileSync(cut, whole.subarray(0, Math.floor(whole.length * 0.4)));
  await page.click('#import-dismiss');
  await page.setInputFiles('#gscap-input', cut);
  s = await waitState(page, (s) => s.import && s.import.progress.done && s.import.file === 'cut.gscap', 120000);
  check('capture: a capture cut short keeps its complete frames and says so', s.import.progress.truncated && s.import.progress.error === null && s.import.progress.frames > 10 && s.import.progress.frames < 150 && /incomplet/.test(s.notice.text) && s.layers.static > 500, JSON.stringify(s.import.progress));
  for (const file of [bad, cut, object]) fs.rmSync(file, { force: true });

  s = await state(page);
  check('capture: no application error', s.errors.length === 0, s.errors.join(' | '));
  check('capture: no console error', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
  await ctx.close();
}

module.exports = { run, synth };
if (require.main === module) require('./standalone.cjs')(run);
