// "Fichier": export of the reconstruction as .ply and .splat, re-import by
// file picker and by drag and drop, return to the live view, broken files.
const fs = require('fs');
const os = require('os');
const path = require('path');
const { check, newPage, consoleErrors, state, waitState, open, shot, viewShot, content, sleep } = require('./lib.cjs');

async function exportFile(page, button, dir) {
  const [download] = await Promise.all([page.waitForEvent('download'), page.click(button)]);
  const file = path.join(dir, download.suggestedFilename());
  await download.saveAs(file);
  return file;
}

// A point cloud in the format of the INRIA 3D Gaussian Splatting trainer
// (62 float properties per point): a torus knot coloured along its length.
function inriaPly(count) {
  const names = ['x', 'y', 'z', 'nx', 'ny', 'nz', 'f_dc_0', 'f_dc_1', 'f_dc_2', ...Array.from({ length: 45 }, (_, i) => `f_rest_${i}`), 'opacity', 'scale_0', 'scale_1', 'scale_2', 'rot_0', 'rot_1', 'rot_2', 'rot_3'];
  const header = Buffer.from(`ply\nformat binary_little_endian 1.0\nelement vertex ${count}\n${names.map((n) => `property float ${n}`).join('\n')}\nend_header\n`);
  const data = Buffer.alloc(count * names.length * 4);
  const SH_C0 = 0.28209479177387814;
  let first = null;
  for (let i = 0; i < count; i++) {
    const t = (i / count) * Math.PI * 2;
    const jitter = (k) => Math.sin(i * 12.9898 + k * 78.233) * 0.03;
    const r = 1 + 0.4 * Math.cos(3 * t);
    const pos = [r * Math.cos(2 * t) + jitter(1), 1.2 + 0.4 * Math.sin(3 * t) + jitter(2), r * Math.sin(2 * t) + jitter(3)];
    const rgb = [0.5 + 0.5 * Math.cos(t), 0.5 + 0.5 * Math.cos(t + 2.1), 0.5 + 0.5 * Math.cos(t + 4.2)];
    const alpha = 0.9;
    const scale = [0.03, 0.02, 0.01];
    const half = t / 2;
    const values = [...pos, 0, 0, 1, ...rgb.map((c) => (c - 0.5) / SH_C0), ...Array.from({ length: 45 }, (_, k) => 0.05 * Math.sin(i + k)), Math.log(alpha / (1 - alpha)), ...scale.map(Math.log), Math.cos(half), 0, Math.sin(half), 0];
    values.forEach((v, k) => data.writeFloatLE(v, (i * names.length + k) * 4));
    if (i === 0) first = { pos, scale, rgba: [...rgb.map((c) => Math.round(c * 255)), Math.round(alpha * 255)] };
  }
  return { bytes: Buffer.concat([header, data]), count, first };
}

async function run({ browser, url }) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'splat-live-files-'));
  const { ctx, page, logs } = await newPage(browser);
  await open(page, url);
  // Paused: the splat count on screen is the one of the exported file.
  await page.click('#btn-pause');
  let s = await waitState(page, (s) => s.config.paused);
  const total = s.layers.static + s.layers.dynamic;

  // ---- export .ply (INRIA 3DGS)
  const ply = await exportFile(page, '#export-ply', dir);
  const plyBytes = fs.readFileSync(ply);
  const header = plyBytes.subarray(0, plyBytes.indexOf('end_header')).toString('latin1');
  const vertices = Number((header.match(/element vertex (\d+)/) || [])[1]);
  check('files: .ply export is a valid INRIA 3DGS file', header.startsWith('ply\nformat binary_little_endian 1.0') && vertices === total && /property float f_dc_0/.test(header) && /property float rot_3/.test(header) && /\.ply$/.test(ply),
    `${path.basename(ply)}: ${plyBytes.length} bytes, ${vertices} vertices for ${total} splats`);
  const dataStart = plyBytes.indexOf('end_header') + 'end_header\n'.length;
  check('files: .ply size matches its header', plyBytes.length === dataStart + vertices * 17 * 4, `${plyBytes.length} bytes`);

  // ---- export .splat
  const splat = await exportFile(page, '#export-splat', dir);
  const splatBytes = fs.readFileSync(splat);
  check('files: .splat export has 32 bytes per splat', splatBytes.length === total * 32 && /\.splat$/.test(splat), `${path.basename(splat)}: ${splatBytes.length} bytes`);

  // ---- import the .ply with the file picker
  await page.click('#btn-pause');
  await waitState(page, (s) => !s.config.paused);
  await page.setInputFiles('#import-input', ply);
  s = await waitState(page, (s) => s.fileMode && s.file);
  check('files: the imported .ply shows the same number of splats', s.file.splats === total && s.layers.static === total && s.layers.dynamic === 0, `${s.file.splats} splats, layers ${JSON.stringify(s.layers)}`);
  s = await waitState(page, (s) => s.config.paused);
  check('files: importing pauses the pipeline and frames the file in orbit mode', s.config.paused && s.nav === 'orbit' && (await page.locator('#back-live').isVisible()) && /splat-live-stage/.test(await page.locator('#file-card .file-name').innerText()));
  await sleep(600);
  s = await state(page);
  check('files: the live stream does not overwrite the file', s.layers.static === total && s.layers.dynamic === 0, JSON.stringify(s.layers));
  await page.locator('#section-file').scrollIntoViewIfNeeded();
  await shot(page, 'file-imported.png');

  // ---- back to the live view
  await page.click('#back-live');
  s = await waitState(page, (s) => !s.fileMode && !s.config.paused && s.layers.dynamic > 0 && s.layers.static > 0, 30000);
  const tick = s.stats.tick;
  s = await waitState(page, (s, tick) => s.stats.tick > tick + 3, 20000, tick);
  check('files: "Revenir au direct" restores the live reconstruction', !s.fileMode && s.file === null && s.nav === 'orbit' && Math.abs(s.eye[2] - 4) < 0.01, JSON.stringify(s.layers));

  // ---- import the .splat by drag and drop
  await page.evaluate(async ([name, b64]) => {
    const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    const data = new DataTransfer();
    data.items.add(new File([bytes], name));
    for (const type of ['dragover', 'drop']) window.dispatchEvent(new DragEvent(type, { dataTransfer: data, bubbles: true, cancelable: true }));
  }, [path.basename(splat), splatBytes.toString('base64')]);
  s = await waitState(page, (s) => s.fileMode && s.file);
  check('files: a dropped .splat shows the same number of splats', s.file.splats === total && s.layers.static === total && s.layers.dynamic === 0, `${s.file.splats} splats`);
  await page.click('#file-pill .pill-action');
  await waitState(page, (s) => !s.fileMode && !s.config.paused && s.layers.dynamic > 0, 30000);

  // ---- broken files: an error, and nothing else changes
  const broken = {
    'truncated.ply': plyBytes.subarray(0, Math.floor(plyBytes.length / 2)),
    'not-a-ply.ply': Buffer.from('this is certainly not a PLY file\n'.repeat(20)),
    'odd-length.splat': splatBytes.subarray(0, 32 * 10 + 7),
    'notes.txt': Buffer.from('hello'),
    'empty.splat': Buffer.alloc(0),
  };
  for (const [name, bytes] of Object.entries(broken)) {
    const file = path.join(dir, name);
    fs.writeFileSync(file, bytes);
    const before = await state(page);
    await page.setInputFiles('#import-input', file);
    await page.waitForFunction(() => window.__splatLive.state().fileError);
    const message = await page.locator('#file-error').innerText();
    const toast = await page.locator('#toast').innerText();
    s = await waitState(page, (s, tick) => s.stats.tick > tick + 2, 20000, before.stats.tick);
    check(`files: ${name} shows an error and breaks nothing`, message.length > 10 && toast.includes(message) && !s.fileMode && !s.config.paused && s.layers.dynamic > 0 && s.layers.static === before.layers.static && s.errors.length === 0, message);
    if (name === 'truncated.ply') await shot(page, 'file-error.png');
    // A good file clears the error.
    await page.evaluate(() => document.querySelector('#toast .toast-close').click());
  }
  await page.setInputFiles('#import-input', splat);
  s = await waitState(page, (s) => s.fileMode);
  check('files: a good file after a bad one clears the error', s.fileError === null && (await page.locator('#file-error').count()) === 0);
  // Switching mode leaves the file as well.
  await page.click('#mode-house');
  s = await waitState(page, (s) => s.scene === 'house' && !s.fileMode && s.layers.static > 0 && !s.config.paused, 60000);
  check('files: a mode switch leaves the imported file', s.file === null && s.scan !== null);

  // ---- a file as the INRIA trainer writes it: normals, spherical harmonics
  // of degree 3 (45 f_rest_* values), opacity as a logit, log scales,
  // rotation as (w, x, y, z)
  await page.click('#mode-stage');
  await waitState(page, (s) => s.scene === 'stage' && s.layers.dynamic > 0, 60000);
  const trained = inriaPly(20000);
  const trainedFile = path.join(dir, 'point_cloud.ply');
  fs.writeFileSync(trainedFile, trained.bytes);
  await page.setInputFiles('#import-input', trainedFile);
  s = await waitState(page, (s) => s.fileMode && s.file && s.file.name === 'point_cloud.ply');
  check('files: an INRIA .ply with spherical harmonics imports completely', s.file.splats === trained.count && s.layers.static === trained.count && s.fileError === null, `${s.file.splats} of ${trained.count} splats, ${trained.bytes.length} bytes`);
  await sleep(500);
  const trainedView = await viewShot(page, 'file-inria-sh.png');
  const trainedContent = await content(page, trainedView);
  check('files: the imported cloud is on screen, in colour', trainedContent.colors > 100 && trainedContent.bright > 0.02, `colours ${trainedContent.colors}, covered ${(trainedContent.bright * 100).toFixed(1)} %`);
  // Out again as .splat: positions, colours, opacity and scales of the
  // first splat survive the two conversions.
  const reexported = fs.readFileSync(await exportFile(page, '#export-splat', dir));
  const first = { pos: [0, 4, 8].map((o) => reexported.readFloatLE(o)), scale: [12, 16, 20].map((o) => reexported.readFloatLE(o)), rgba: [...reexported.subarray(24, 28)] };
  const close = (a, b, eps) => a.every((v, i) => Math.abs(v - b[i]) <= eps);
  check('files: values survive .ply (SH) to .splat', reexported.length === trained.count * 32 && close(first.pos, trained.first.pos, 1e-5) && close(first.scale, trained.first.scale, 1e-5) && close(first.rgba, trained.first.rgba, 2),
    `pos ${first.pos.map((v) => v.toFixed(3))}, rgba ${first.rgba} (expected ${trained.first.rgba})`);
  // And the exported .ply of that cloud comes back identical in count.
  const second = await exportFile(page, '#export-ply', dir);
  await page.setInputFiles('#import-input', second);
  s = await waitState(page, (s, name) => s.fileMode && s.file && s.file.name === name, 30000, path.basename(second));
  check('files: re-exporting an imported file and importing it again keeps every splat', s.file.splats === trained.count && /splat-live-import-20000\.ply/.test(second), path.basename(second));
  await page.click('#back-live');
  await waitState(page, (s) => !s.fileMode && !s.config.paused && s.layers.dynamic > 0, 30000);

  check('files: no console error', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
  await ctx.close();
  fs.rmSync(dir, { recursive: true, force: true });
}

module.exports = { run };
if (require.main === module) require('./standalone.cjs')(run);
