// Virtual reality through the IWER WebXR emulator (Meta Quest 3): the VR
// panel, entering from both buttons, stereo rendering, locomotion,
// miniature, leaving from both sides; and what the panel says without a
// headset.
const { check, iwerBundle, newPage, consoleErrors, state, waitState, open, shot, imageStats, hasContent, openPanel, sleep } = require('./lib.cjs');

const xrFrames = async (page, n) => {
  const start = (await state(page)).xr.session.frames;
  return waitState(page, (s, target) => s.xr.session && s.xr.session.frames >= target, 30000, start + n);
};
const stick = (page, hand, x, y) => page.evaluate(([h, x, y]) => xrdev.controllers[h].updateAxes('thumbstick', x, y), [hand, x, y]);
async function press(page, hand, button) {
  await page.evaluate(([h, b]) => xrdev.controllers[h].updateButtonValue(b, 1), [hand, button]);
  await xrFrames(page, 3);
  await page.evaluate(([h, b]) => xrdev.controllers[h].updateButtonValue(b, 0), [hand, button]);
  return xrFrames(page, 3);
}
const entered = (page) => waitState(page, (s) => s.xr.active && s.xr.session && s.xr.session.frames > 5 && s.xr.session.viewports.length === 2, 30000);
const dist = (a, b) => Math.hypot(...a.map((v, i) => v - b[i]));

async function withoutHeadset(browser, url) {
  const { ctx, page, logs } = await newPage(browser, { width: 900, height: 700 });
  await open(page, url);
  let s = await waitState(page, (s) => s.xr.supported !== null);
  await page.locator('#section-vr').scrollIntoViewIfNeeded();
  const help = await page.locator('#vr-worker-help').innerText();
  check('vr: without a headset the panel explains what to do (worker source)', s.xr.supported === false && (await page.locator('#enter-vr').count()) === 0 && /navigateur du casque/.test(help) && /https/.test(help) && (await page.locator('#lan-toggle').count()) === 0, help);
  await shot(page, 'vr-panel-no-headset.png');
  await page.click('#btn-panel');
  await page.waitForFunction(() => !window.__splatLive.state().panelOpen);
  await page.click('#btn-vr');
  await page.waitForFunction(() => window.__splatLive.state().panelOpen);
  s = await state(page);
  check('vr: the VR button opens the panel when no session can start', s.panelOpen && !s.xr.active && s.xr.error === null);
  check('vr: no console error without a headset', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
  await ctx.close();
}

async function run({ browser, url }) {
  await withoutHeadset(browser, url);
  if (!iwerBundle()) {
    check('vr: IWER emulator bundle found', false, 'run `npm install` in e2e/render, or set GS_NPM_TOOLS / IWER_BUNDLE');
    return;
  }
  // 1280x640: each eye is square.
  const { ctx, page, logs } = await newPage(browser, { width: 1280, height: 640, iwer: true });
  await open(page, url);
  let s = await waitState(page, (s) => s.xr.supported !== null);
  await page.locator('#section-vr').scrollIntoViewIfNeeded();
  const reminder = await page.locator('#section-vr .controls').innerText();
  check('vr: with a headset the panel offers "Entrer en VR" and the controls', s.xr.supported === true && (await page.locator('#enter-vr').innerText()).trim() === 'Entrer en VR' && /Joystick gauche/.test(reminder) && /maquette/.test(reminder) && /repères/.test(reminder) && (await page.locator('#btn-vr').getAttribute('class')).includes('ready'),
    reminder.replace(/\s+/g, ' '));
  await shot(page, 'vr-panel.png');
  const desktop = s;

  // ---- a refused session is reported, and does not prevent the next try
  await page.evaluate(() => {
    window.__requestSession = navigator.xr.requestSession;
    navigator.xr.requestSession = () => Promise.reject(new DOMException('refus simulé', 'SecurityError'));
  });
  await page.click('#enter-vr');
  await page.waitForSelector('#xr-error', { timeout: 10000 });
  const refusal = await page.locator('#xr-error').innerText();
  s = await state(page);
  check('vr: a refused session shows the reason', !s.xr.active && typeof s.xr.error === 'string' && /n'a pas pu démarrer/.test(refusal) && /refus simulé/.test(refusal), refusal);
  await page.evaluate(() => { navigator.xr.requestSession = window.__requestSession; });

  // ---- enter with a real click on the panel button
  await page.click('#enter-vr');
  s = await entered(page);
  const session = s.xr.session;
  check('vr: "Entrer en VR" starts an immersive session', s.xr.active && session.referenceSpace === 'local-floor' && s.viewer.splatsDrawn > 5000, `frames ${session.frames}, ${s.viewer.splatsDrawn} splats per eye`);
  check('vr: two side-by-side eye viewports', session.viewports[0][0] === 0 && session.viewports[1][0] === session.viewports[0][2] && session.viewports[0][2] > 0, JSON.stringify(session.viewports));
  check('vr: the session starts at the desktop camera, on the floor, life-size', Math.abs(session.rig[0] - desktop.eye[0]) < 1e-3 && session.rig[1] === 0 && Math.abs(session.rig[2] - desktop.eye[2]) < 1e-3 && session.scale === 1 && !session.miniature, `rig ${session.rig.map((v) => v.toFixed(2))}`);
  const ui = await page.evaluate(() => ['.topbar', '#panel', '#hud'].map((q) => getComputedStyle(document.querySelector(q)).display));
  check('vr: the 2D interface steps aside during the session', ui.every((d) => d === 'none') && (await page.locator('#exit-vr').isVisible()), ui.join(','));
  // The exit card would sit in the middle of the stereo pair: hide it for the picture.
  await page.addStyleTag({ content: '#xr-card { visibility: hidden; }' });
  await sleep(300);
  let f = await shot(page, 'vr-stereo.png');
  let img = await imageStats(page, f);
  check('vr: both eyes render, similar but not identical', hasContent(img.left) && hasContent(img.right) && img.lrIdenticalFrac < 0.99 && img.lrMeanAbsDiff < 30, `left ${img.left.colors} colours, right ${img.right.colors} colours, identical ${(img.lrIdenticalFrac * 100).toFixed(1)} %`);

  // ---- the stream keeps flowing during the session
  const tick = s.stats.tick;
  s = await waitState(page, (s, tick) => s.stats.tick > tick + 5, 30000, tick);
  check('vr: the pipeline keeps running during the session', s.stats.tick > tick && s.layers.dynamic > 0, `tick ${tick} -> ${s.stats.tick}`);

  // ---- sticks
  const rig0 = s.xr.session.rig;
  await stick(page, 'left', 0, -1);
  await xrFrames(page, 25);
  await stick(page, 'left', 0, 0);
  s = await xrFrames(page, 3);
  const moved = dist(s.xr.session.rig.slice(0, 3), rig0.slice(0, 3));
  check('vr: the left stick moves the rig', moved > 0.2 && s.xr.session.rig[1] === rig0[1], `moved ${moved.toFixed(2)} m`);
  const yaw0 = s.xr.session.rig[3];
  await stick(page, 'right', 1, 0);
  s = await xrFrames(page, 10);
  await stick(page, 'right', 0, 0);
  check('vr: the right stick snap-turns', Math.abs(s.xr.session.rig[3] - (yaw0 - Math.PI / 6)) < 1e-4, `yaw ${yaw0.toFixed(3)} -> ${s.xr.session.rig[3].toFixed(3)}`);
  s = await press(page, 'right', 'a-button');
  check('vr: A switches to the miniature', s.xr.session.miniature && s.xr.session.scale > 5, `scale ${s.xr.session.scale}`);
  await sleep(200);
  f = await shot(page, 'vr-miniature.png');
  img = await imageStats(page, f);
  check('vr: the miniature renders in both eyes', img.left.colors > 20 && img.right.colors > 20, `left ${img.left.colors}, right ${img.right.colors}`);
  s = await press(page, 'right', 'a-button');
  check('vr: A again returns to life size', !s.xr.session.miniature && s.xr.session.scale === 1);
  s = await press(page, 'left', 'x-button');
  check('vr: X toggles the gizmos (and does not disturb the session)', s.xr.active);
  await press(page, 'left', 'x-button');

  // ---- leave with the page button
  await page.addStyleTag({ content: '#xr-card { visibility: visible !important; }' });
  const rendered = s.viewer.framesRendered;
  await page.click('#exit-vr');
  s = await waitState(page, (s, n) => !s.xr.active && s.xr.session === null && s.viewer.framesRendered > n + 3, 20000, rendered);
  await sleep(300);
  const after = await page.evaluate(() => {
    const c = document.getElementById('view');
    return { width: c.width, height: c.height, parent: c.parentElement.className, ui: ['.topbar', '#panel', '#hud'].map((q) => getComputedStyle(document.querySelector(q)).display), card: !!document.getElementById('xr-card') };
  });
  f = await shot(page, 'vr-after.png');
  img = await imageStats(page, f);
  check('vr: leaving returns to the desktop view', after.width === 1280 && after.height === 640 && after.parent === 'viewport' && after.ui.every((d) => d !== 'none') && !after.card && hasContent(img.left) && dist(s.eye, desktop.eye) < 1e-3,
    JSON.stringify(after));
  check('vr: the panel button is back to "Entrer en VR", the old error is gone', (await page.locator('#enter-vr').innerText()).trim() === 'Entrer en VR' && s.xr.error === null && (await page.locator('#xr-error').count()) === 0);

  // ---- the house: entering from the overview lands inside, life-size
  await page.click('#mode-house');
  await waitState(page, (s) => s.scene === 'house' && s.display.dollhouse && s.layers.static > 0, 60000);
  await page.click('#btn-vr');
  s = await entered(page);
  check('vr: from the house overview the session starts inside the house', !s.display.dollhouse && s.display.clipY === null && s.xr.session.scale === 1 && dist([s.xr.session.rig[0], s.xr.session.rig[2]], [-0.5, -0.45]) < 0.05 && s.xr.session.rig[1] === 0,
    `rig ${s.xr.session.rig.map((v) => v.toFixed(2))}`);
  s = await press(page, 'right', 'a-button');
  check('vr: the house miniature', s.xr.session.miniature && s.xr.session.scale > 5);
  // ---- ended by the runtime (system menu, headset taken off)
  await page.evaluate(() => xrdev.activeSession.end());
  s = await waitState(page, (s) => !s.xr.active, 20000);
  const rendered2 = s.viewer.framesRendered;
  s = await waitState(page, (s, n) => s.viewer.framesRendered > n + 2, 20000, rendered2);
  check('vr: a session ended by the headset returns to the desktop too', !s.xr.active && s.xr.session === null && (await page.locator('.topbar').isVisible()));

  check('vr: no application error', s.errors.length === 0, s.errors.join(' | '));
  check('vr: no console error', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
  await ctx.close();
}

module.exports = { run };
if (require.main === module) require('./standalone.cjs')(run);
