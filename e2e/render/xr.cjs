// WebXR through the IWER emulator (Meta Quest 3): stereo rendering, poses,
// locomotion, snap turn, miniature mode, controllers, session end from both
// sides.
//   node e2e/render/xr.cjs        (BUILD=1 to rebuild the harness first)
const fs = require('fs');
const path = require('path');
const { OUT, check, newPage, consoleErrors, state, cmd, waitLive, shot, imageStats, hasContent, iwerBundle, main } = require('./lib.cjs');

// ---- column-major 4x4 helpers (same layout as WebXR / glam)
const mul = (a, b) => {
  const o = new Array(16).fill(0);
  for (let c = 0; c < 4; c++) for (let r = 0; r < 4; r++) for (let k = 0; k < 4; k++) o[c * 4 + r] += a[k * 4 + r] * b[c * 4 + k];
  return o;
};
const translation = (x, y, z) => [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, x, y, z, 1];
const scaling = (s) => [s, 0, 0, 0, 0, s, 0, 0, 0, 0, s, 0, 0, 0, 0, 1];
const rotationY = (a) => [Math.cos(a), 0, -Math.sin(a), 0, 0, 1, 0, 0, Math.sin(a), 0, Math.cos(a), 0, 0, 0, 0, 1];
const fromQuat = ([x, y, z, w], [tx, ty, tz]) => [
  1 - 2 * (y * y + z * z), 2 * (x * y + z * w), 2 * (x * z - y * w), 0,
  2 * (x * y - z * w), 1 - 2 * (x * x + z * z), 2 * (y * z + x * w), 0,
  2 * (x * z + y * w), 2 * (y * z - x * w), 1 - 2 * (x * x + y * y), 0,
  tx, ty, tz, 1,
];
// General inverse by Gauss-Jordan elimination (the rig matrix has a scale).
function invert(m) {
  const a = [0, 1, 2, 3].map((r) => [0, 1, 2, 3].map((c) => m[c * 4 + r]).concat([0, 1, 2, 3].map((c) => (c === r ? 1 : 0))));
  for (let i = 0; i < 4; i++) {
    let p = i;
    for (let r = i + 1; r < 4; r++) if (Math.abs(a[r][i]) > Math.abs(a[p][i])) p = r;
    [a[i], a[p]] = [a[p], a[i]];
    const d = a[i][i];
    for (let c = 0; c < 8; c++) a[i][c] /= d;
    for (let r = 0; r < 4; r++) if (r !== i) { const f = a[r][i]; for (let c = 0; c < 8; c++) a[r][c] -= f * a[i][c]; }
  }
  const o = new Array(16);
  for (let r = 0; r < 4; r++) for (let c = 0; c < 4; c++) o[c * 4 + r] = a[r][4 + c];
  return o;
}
const point = (m, [x, y, z]) => [0, 1, 2].map((r) => m[r] * x + m[4 + r] * y + m[8 + r] * z + m[12 + r]);
const maxAbsDiff = (a, b) => Math.max(...a.map((v, i) => Math.abs(v - b[i])));
const dist = (a, b) => Math.hypot(...a.map((v, i) => v - b[i]));
const f3 = (a) => '[' + a.map((v) => v.toFixed(3)).join(', ') + ']';

// world_from_rig = T(pos) * R_y(yaw) * S(scale), from the reported rig.
const worldFromRig = (xr) => mul(mul(translation(xr.rig[0], xr.rig[1], xr.rig[2]), rotationY(xr.rig[3])), scaling(xr.scale));
// Expected view matrix of the left eye for the emulated device pose.
function expectedLeftView(dev, xr) {
  const worldFromEye = mul(mul(worldFromRig(xr), fromQuat(dev.q, dev.p)), translation(-dev.ipd / 2, 0, 0));
  return invert(worldFromEye);
}
const device = (page) =>
  page.evaluate(() => ({
    p: [xrdev.position.x, xrdev.position.y, xrdev.position.z],
    q: [xrdev.quaternion.x, xrdev.quaternion.y, xrdev.quaternion.z, xrdev.quaternion.w],
    ipd: xrdev.ipd,
    left: [xrdev.controllers.left.position.x, xrdev.controllers.left.position.y, xrdev.controllers.left.position.z],
    right: [xrdev.controllers.right.position.x, xrdev.controllers.right.position.y, xrdev.controllers.right.position.z],
  }));
const xrFrames = async (page, n) => {
  const start = (await state(page)).xr.frames;
  await page.waitForFunction((t) => { const s = window.__harness.state(); return s.xr && s.xr.frames >= t; }, start + n, { timeout: 30000 });
  return state(page);
};
// A button press long enough to be seen by at least one XR frame.
async function press(page, hand, button) {
  await page.evaluate(([h, b]) => xrdev.controllers[h].updateButtonValue(b, 1), [hand, button]);
  await xrFrames(page, 3);
  await page.evaluate(([h, b]) => xrdev.controllers[h].updateButtonValue(b, 0), [hand, button]);
  return xrFrames(page, 3);
}
const stick = (page, hand, x, y) => page.evaluate(([h, x, y]) => xrdev.controllers[h].updateAxes('thumbstick', x, y), [hand, x, y]);
const enterVr = async (page) => {
  await page.click('#enter-vr');
  await page.waitForFunction(() => { const s = window.__harness.state(); return s.xrActive && s.xr && s.xr.frames > 5 && s.xr.views.length > 0; }, null, { timeout: 30000 });
};
// NDC x of a world point for one eye.
const ndcX = (view, p) => {
  const vp = mul(view.projection, view.view);
  const w = vp[3] * p[0] + vp[7] * p[1] + vp[11] * p[2] + vp[15];
  return (vp[0] * p[0] + vp[4] * p[1] + vp[8] * p[2] + vp[12]) / w;
};

async function withoutEmulator(browser, url) {
  console.log('\n=== xr: no WebXR runtime ===');
  const { ctx, page, logs } = await newPage(browser, { width: 640, height: 400 });
  await page.goto(`${url}/?test=live&static_cams=3&capture_scale=0.35`);
  await waitLive(page);
  await page.waitForFunction(() => window.__harness.state().xrSupported !== null);
  const s = await state(page);
  check('xr: immersive-vr is reported unsupported without a runtime', s.xrSupported === false && (await page.locator('#enter-vr').isDisabled()), await page.locator('#enter-vr').textContent());
  await cmd(page, 'enterXr');
  await page.waitForTimeout(500);
  const s2 = await state(page);
  check('xr: enter_xr fails gracefully without a runtime', !s2.xrActive && typeof s2.xrError === 'string' && s2.framesRendered > 0, `xrError: ${s2.xrError}`);
  // Chromium itself logs the rejected requestSession as a console error.
  const errs = consoleErrors(logs).filter((l) => !/Could not create a session/.test(l));
  check('xr: no panic without a runtime', errs.length === 0, errs.join(' | '));
  await ctx.close();
}

async function run({ browser, url }) {
  if (!iwerBundle()) {
    check('xr: IWER emulator bundle found', false, 'run `npm install` in e2e/render, or set GS_NPM_TOOLS / IWER_BUNDLE');
    return;
  }
  await withoutEmulator(browser, url);

  console.log('\n=== xr: IWER emulated Quest 3 ===');
  // 1280x640: each eye is square. The ceiling hint makes miniature mode clip.
  const CEILING = 1.2;
  const { ctx, page, logs } = await newPage(browser, { width: 1280, height: 640, iwer: true });
  await page.goto(`${url}/?test=live&static_cams=4&capture_scale=0.5&ceiling=${CEILING}`);
  await waitLive(page);
  await page.waitForFunction(() => window.__harness.state().xrSupported !== null);
  let s = await state(page);
  check('xr: immersive-vr supported under the emulator, button enabled', s.xrSupported === true && (await page.locator('#enter-vr').isEnabled()), await page.locator('#enter-vr').textContent());
  const desktop = s;
  const desktopYaw = Math.atan2(-(s.target[0] - s.eye[0]), -(s.target[2] - s.eye[2]));
  await shot(page, 'xr-0-desktop-before.png');

  // ---- enter with a real click (user activation)
  await enterVr(page);
  s = await state(page);
  let xr = s.xr;
  check('xr: session started, frames reach the Rust frame callback', s.xrActive && xr.frames > 5 && JSON.stringify(s.xrChanges) === '[true]', `frames ${xr.frames}, changes ${JSON.stringify(s.xrChanges)}`);
  check('xr: local-floor reference space', xr.referenceSpace === 'local-floor');
  const vps = xr.views.map((v) => v.viewport);
  check('xr: two views with side-by-side viewports', xr.views.length === 2 && vps[0][0] === 0 && vps[1][0] === vps[0][2] && vps[0][2] > 0 && vps[1][2] === vps[0][2], JSON.stringify(vps));
  check('xr: the rig starts at the desktop camera, on the floor, facing the same way', Math.abs(xr.rig[0] - desktop.eye[0]) < 1e-4 && xr.rig[1] === 0 && Math.abs(xr.rig[2] - desktop.eye[2]) < 1e-4 && Math.abs(xr.rig[3] - desktopYaw) < 1e-4 && xr.scale === 1 && !xr.miniature,
    `rig ${f3(xr.rig)} desktop eye ${f3(desktop.eye)} yaw ${desktopYaw.toFixed(3)}`);
  let dev = await device(page);
  let err = maxAbsDiff(expectedLeftView(dev, xr), xr.views[0].view);
  check('xr: left-eye view matrix = inverse(world_from_rig * head * eye offset)', err < 1e-4, `max abs error ${err.toExponential(2)}`);
  // The emulator's frusta are symmetric (the off-axis case is covered by the
  // compare test); what can be checked is that the matrix is the runtime's:
  // perspective, with the near plane requested through updateRenderState.
  const proj = xr.views[0].projection;
  const near = proj[14] / (proj[10] - 1);
  check('xr: the runtime projection is used, with the requested near plane', proj[11] === -1 && proj[0] > 0.3 && proj[5] > 0.3 && Math.abs(near - 0.05) < 1e-4, `P00 ${proj[0].toFixed(3)}, P11 ${proj[5].toFixed(3)}, near ${near.toFixed(4)}`);
  const parallax = ndcX(xr.views[0], [0, 1, 0]) - ndcX(xr.views[1], [0, 1, 0]);
  check('xr: the stage center has positive parallax (left eye sees it further right)', parallax > 0.005 && parallax < 0.2, `NDC x difference ${parallax.toFixed(4)}`);
  // The grip pose sits a few centimeters from the emulated controller origin.
  const grip = (x, left) => x.controllers.find((c) => c.left === left).pose.slice(12, 15);
  check('xr: both controllers tracked in world space', xr.controllers.length === 2 && dist(grip(xr, true), point(worldFromRig(xr), dev.left)) < 0.12 && dist(grip(xr, false), point(worldFromRig(xr), dev.right)) < 0.12,
    `left grip ${f3(grip(xr, true))}, device ${f3(point(worldFromRig(xr), dev.left))}`);
  check('xr: stats report the eye buffer and the session', s.xrActive && s.canvasWidth === vps[0][2] && s.canvasHeight === vps[0][3] && s.splatsDrawn > 5000, `${s.canvasWidth}x${s.canvasHeight}, ${s.splatsDrawn} splats per eye`);

  let f = await shot(page, 'xr-1-stereo-initial.png');
  let img = await imageStats(page, f);
  check('xr: stereo frame: both eyes have content, similar but not identical', hasContent(img.left) && hasContent(img.right) && img.lrIdenticalFrac < 0.99 && img.lrIdenticalFrac > 0.2 && img.lrMeanAbsDiff < 30,
    `identical ${(img.lrIdenticalFrac * 100).toFixed(1)} %, L/R mean abs diff ${img.lrMeanAbsDiff.toFixed(2)}`);

  // ---- the picture is live in VR too (dynamic layer keeps updating)
  const ticks0 = s.ticks;
  s = await xrFrames(page, 20);
  check('xr: the dynamic layer keeps updating during the session', s.ticks > ticks0, `ticks ${ticks0} -> ${s.ticks}`);

  // ---- move and turn the headset, hold the controllers in view
  await page.evaluate(() => {
    xrdev.position.set(0.3, 1.5, -0.2);
    const yaw = 0.5, pitch = -0.25;
    const [ax, ay, az, aw] = [0, Math.sin(yaw / 2), 0, Math.cos(yaw / 2)];
    const [bx, by, bz, bw] = [Math.sin(pitch / 2), 0, 0, Math.cos(pitch / 2)];
    xrdev.quaternion.set(aw * bx + ax * bw + ay * bz - az * by, aw * by - ax * bz + ay * bw + az * bx, aw * bz + ax * by - ay * bx + az * bw, aw * bw - ax * bx - ay * by - az * bz);
    xrdev.controllers.left.position.set(0.0, 1.25, -0.65);
    xrdev.controllers.right.position.set(0.35, 1.25, -0.75);
  });
  s = await xrFrames(page, 5);
  xr = s.xr;
  dev = await device(page);
  check('xr: head pose follows the emulated headset', dist(xr.head.slice(12, 15), point(worldFromRig(xr), dev.p)) < 1e-4, `world head ${f3(xr.head.slice(12, 15))}`);
  err = maxAbsDiff(expectedLeftView(dev, xr), xr.views[0].view);
  check('xr: view matrix matches after moving and turning the headset', err < 1e-4, `max abs error ${err.toExponential(2)}`);
  f = await shot(page, 'xr-2-headset-moved-controllers.png');
  img = await imageStats(page, f);
  check('xr: stereo content after the headset move', hasContent(img.left) && hasContent(img.right) && img.lrIdenticalFrac < 0.99);

  // ---- left stick: smooth locomotion along the head direction
  const rig0 = xr.rig;
  const t0 = Date.now();
  await stick(page, 'left', 0, -1);
  await xrFrames(page, 30);
  await stick(page, 'left', 0, 0);
  s = await xrFrames(page, 3);
  const seconds = (Date.now() - t0) / 1000;
  xr = s.xr;
  let moved = [xr.rig[0] - rig0[0], xr.rig[2] - rig0[2]];
  // Head yaw in the world = rig yaw + 0.5; forward on the floor = (-sin, -cos).
  const headYaw = rig0[3] + 0.5;
  let cos = (moved[0] * -Math.sin(headYaw) + moved[1] * -Math.cos(headYaw)) / Math.hypot(...moved);
  check('xr: left stick moves the rig along the head direction, level, at about 2 m/s', Math.hypot(...moved) > 0.3 && Math.hypot(...moved) < 2.2 * seconds + 0.2 && cos > 0.9999 && xr.rig[1] === rig0[1] && xr.rig[3] === rig0[3],
    `moved ${Math.hypot(...moved).toFixed(2)} m in ${seconds.toFixed(2)} s, cos ${cos.toFixed(5)}`);
  dev = await device(page);
  err = maxAbsDiff(expectedLeftView(dev, xr), xr.views[0].view);
  check('xr: view matrix matches after locomotion', err < 1e-4, `max abs error ${err.toExponential(2)}`);
  // Strafe with the trigger held: faster, sideways.
  const rig1 = xr.rig;
  await page.evaluate(() => xrdev.controllers.left.updateButtonValue('trigger', 1));
  await stick(page, 'left', 1, 0);
  await xrFrames(page, 20);
  await stick(page, 'left', 0, 0);
  await page.evaluate(() => xrdev.controllers.left.updateButtonValue('trigger', 0));
  s = await xrFrames(page, 3);
  xr = s.xr;
  moved = [xr.rig[0] - rig1[0], xr.rig[2] - rig1[2]];
  cos = (moved[0] * Math.cos(headYaw) + moved[1] * -Math.sin(headYaw)) / Math.hypot(...moved);
  check('xr: stick right strafes to the right of the head', Math.hypot(...moved) > 0.3 && cos > 0.9999, `moved ${Math.hypot(...moved).toFixed(2)} m, cos ${cos.toFixed(5)}`);
  await shot(page, 'xr-3-after-walk.png');

  // ---- right stick x: snap turn with a latch, pivoting on the head
  const yaw0 = xr.rig[3];
  const head0 = xr.head.slice(12, 15);
  await stick(page, 'right', 1, 0);
  s = await xrFrames(page, 20);
  check('xr: holding the right stick turns exactly once by 30 degrees to the right', Math.abs(s.xr.rig[3] - (yaw0 - Math.PI / 6)) < 1e-5, `yaw ${yaw0.toFixed(4)} -> ${s.xr.rig[3].toFixed(4)}`);
  check('xr: the snap turn pivots around the head', dist(s.xr.head.slice(12, 15), head0) < 1e-4, `head drift ${dist(s.xr.head.slice(12, 15), head0).toExponential(2)}`);
  await stick(page, 'right', 0, 0);
  await xrFrames(page, 3);
  await stick(page, 'right', 1, 0);
  await xrFrames(page, 3);
  await stick(page, 'right', 0, 0);
  s = await xrFrames(page, 3);
  check('xr: a second flick turns again', Math.abs(s.xr.rig[3] - (yaw0 - Math.PI / 3)) < 1e-5, `yaw ${s.xr.rig[3].toFixed(4)}`);
  await stick(page, 'right', -1, 0);
  await xrFrames(page, 3);
  await stick(page, 'right', 0, 0);
  s = await xrFrames(page, 3);
  check('xr: a flick to the left turns back', Math.abs(s.xr.rig[3] - (yaw0 - Math.PI / 6)) < 1e-5, `yaw ${s.xr.rig[3].toFixed(4)}`);
  dev = await device(page);
  err = maxAbsDiff(expectedLeftView(dev, s.xr), s.xr.views[0].view);
  check('xr: view matrix matches after the snap turns', err < 1e-4, `max abs error ${err.toExponential(2)}`);
  await shot(page, 'xr-4-after-snap-turn.png');

  // ---- right stick y: up / down
  const y0 = s.xr.rig[1];
  await stick(page, 'right', 0, -1);
  await xrFrames(page, 15);
  await stick(page, 'right', 0, 0);
  s = await xrFrames(page, 3);
  check('xr: right stick forward raises the rig without turning', s.xr.rig[1] > y0 + 0.05 && Math.abs(s.xr.rig[3] - (yaw0 - Math.PI / 6)) < 1e-5, `rig y ${y0.toFixed(3)} -> ${s.xr.rig[1].toFixed(3)}`);
  const lifeSize = s.xr.rig;
  const drawnLifeSize = s.splatsDrawn;

  // ---- right A: miniature ("dollhouse") mode
  s = await press(page, 'right', 'a-button');
  xr = s.xr;
  dev = await device(page);
  const rigFromWorld = invert(worldFromRig(xr));
  const center = point(rigFromWorld, [0, 0, 0]);
  // Head forward on the floor in rig space (device yaw 0.5): (-sin, -cos).
  const ahead = (center[0] - dev.p[0]) * -Math.sin(0.5) + (center[2] - dev.p[2]) * -Math.cos(0.5);
  const side = (center[0] - dev.p[0]) * Math.cos(0.5) + (center[2] - dev.p[2]) * -Math.sin(0.5);
  check('xr: A switches to the 1:15 miniature', xr.miniature && xr.scale === 15, `scale ${xr.scale}`);
  check('xr: the model sits on a table in front of the user', Math.abs(center[1] - (dev.p[1] - 0.65)) < 1e-3 && Math.abs(ahead - (0.45 + 2.2 / 15)) < 1e-3 && Math.abs(side) < 1e-3,
    `stage center in room space ${f3(center)}: ${ahead.toFixed(3)} m ahead, ${(dev.p[1] - center[1]).toFixed(3)} m below the eyes`);
  err = maxAbsDiff(expectedLeftView(dev, xr), xr.views[0].view);
  check('xr: view matrix carries the world scale', err < 2e-4, `max abs error ${err.toExponential(2)}`);
  check('xr: the ceiling hint clips the miniature automatically', s.splatsDrawn < drawnLifeSize - 50, `splats drawn ${drawnLifeSize} -> ${s.splatsDrawn} (clip at ${CEILING} m - margin)`);
  // A hand movement of (0.1, 0.05, -0.2) real meters is 15 times that in the world.
  const grip0 = grip(xr, true);
  await page.evaluate(() => { const p = xrdev.controllers.left.position; p.set(p.x + 0.1, p.y + 0.05, p.z - 0.2); });
  s = await xrFrames(page, 3);
  xr = s.xr;
  const w = worldFromRig(xr);
  const expectedDelta = [0, 1, 2].map((r) => w[r] * 0.1 + w[4 + r] * 0.05 + w[8 + r] * -0.2);
  const delta = grip(xr, true).map((v, i) => v - grip0[i]);
  check('xr: controllers stay in the real room while the world is a miniature', dist(delta, expectedDelta) < 1e-3 && Math.abs(Math.hypot(...delta) - 15 * Math.hypot(0.1, 0.05, 0.2)) < 1e-3,
    `grip moved ${f3(delta)} world m, expected ${f3(expectedDelta)}`);
  f = await shot(page, 'xr-5-miniature.png');
  img = await imageStats(page, f);
  check('xr: miniature stereo frame has content in both eyes', img.left.colors > 20 && img.right.colors > 20 && img.lrIdenticalFrac < 0.995, JSON.stringify({ l: img.left, r: img.right }));
  // The miniature slides slowly under the stick.
  const mini0 = xr.rig;
  await stick(page, 'left', 0, -1);
  await xrFrames(page, 10);
  await stick(page, 'left', 0, 0);
  s = await xrFrames(page, 3);
  check('xr: locomotion works in the miniature', dist(s.xr.rig.slice(0, 3), mini0.slice(0, 3)) > 0.2, `rig moved ${dist(s.xr.rig.slice(0, 3), mini0.slice(0, 3)).toFixed(2)} world m`);
  // B puts the model back on the table.
  s = await press(page, 'right', 'b-button');
  check('xr: B recenters the miniature', s.xr.miniature && Math.abs(point(invert(worldFromRig(s.xr)), [0, 0, 0])[1] - (dev.p[1] - 0.65)) < 1e-3);
  s = await press(page, 'right', 'a-button');
  check('xr: A again returns to life size at the previous spot', !s.xr.miniature && s.xr.scale === 1 && maxAbsDiff(s.xr.rig, lifeSize) < 1e-5 && Math.abs(s.splatsDrawn - drawnLifeSize) < 500, `rig ${f3(s.xr.rig)}, splats drawn ${s.splatsDrawn}`);

  // ---- left X: gizmos, right B: reset, API toggle
  s = await press(page, 'left', 'x-button');
  const noGizmos = await shot(page, 'xr-6-gizmos-off.png');
  check('xr: X hides the gizmos', s.showGizmos === false);
  s = await press(page, 'left', 'x-button');
  check('xr: X shows them again', s.showGizmos === true);
  s = await press(page, 'right', 'b-button');
  check('xr: B resets the rig to where the session started', dist(s.xr.rig.slice(0, 3), [desktop.eye[0], 0, desktop.eye[2]]) < 1e-4 && Math.abs(s.xr.rig[3] - desktopYaw) < 1e-4, `rig ${f3(s.xr.rig)}`);
  await cmd(page, 'miniature', true);
  s = await xrFrames(page, 3);
  const apiMini = s.xr.miniature;
  await cmd(page, 'miniature', false);
  s = await xrFrames(page, 3);
  check('xr: set_xr_miniature switches modes from the API', apiMini && !s.xr.miniature);

  // ---- leave with the DOM button; the desktop loop takes over
  const rendered0 = s.framesRendered;
  check('xr: the button reads "Exit VR" during the session', (await page.locator('#enter-vr').textContent()) === 'Exit VR');
  await page.click('#enter-vr');
  await page.waitForFunction(() => !window.__harness.state().xrActive, null, { timeout: 10000 });
  await page.waitForFunction((n) => window.__harness.state().framesRendered > n + 3, rendered0);
  s = await state(page);
  check('xr: exit_xr ends the session and notifies', s.xr === null && JSON.stringify(s.xrChanges) === '[true,false]' && (await page.locator('#enter-vr').textContent()) === 'Enter VR', JSON.stringify(s.xrChanges));
  await page.waitForTimeout(300);
  f = await shot(page, 'xr-7-desktop-after.png');
  img = await imageStats(page, f);
  const canvas = await page.evaluate(() => { const c = document.getElementById('gl'); return { w: c.width, h: c.height, parent: c.parentElement.tagName }; });
  s = await state(page);
  check('xr: desktop view restored (full-size mono picture, same camera as before)', canvas.w === 1280 && canvas.h === 640 && canvas.parent === 'BODY' && hasContent(img.left) && img.lrMeanAbsDiff > 12 && dist(s.eye, desktop.eye) < 1e-4,
    JSON.stringify({ canvas, lrMeanAbsDiff: +img.lrMeanAbsDiff.toFixed(2) }));

  // ---- second session, ended from the runtime side (system menu / headset off)
  await enterVr(page);
  s = await state(page);
  check('xr: a second session works', s.xrActive && s.xr.views.length === 2 && s.xr.scale === 1, `frames ${s.xr.frames}`);
  await shot(page, 'xr-8-second-session.png');
  await page.evaluate(() => xrdev.activeSession.end());
  await page.waitForFunction(() => !window.__harness.state().xrActive, null, { timeout: 10000 });
  const rendered1 = (await state(page)).framesRendered;
  await page.waitForFunction((n) => window.__harness.state().framesRendered > n + 3, rendered1);
  s = await state(page);
  check('xr: a session ended by the runtime returns to the desktop too', s.xr === null && JSON.stringify(s.xrChanges) === '[true,false,true,false]' && (await page.locator('#enter-vr').textContent()) === 'Enter VR', JSON.stringify(s.xrChanges));

  // ---- third session, then destroy the viewer while it runs
  await enterVr(page);
  await cmd(page, 'destroy');
  await page.waitForTimeout(500);
  s = await state(page);
  const sessionGone = await page.evaluate(() => !xrdev.activeSession);
  check('xr: destroying the viewer ends the running session', !s.xrActive && sessionGone && s.xrChanges.length === 6 && s.xrChanges[5] === false, JSON.stringify(s.xrChanges));

  fs.writeFileSync(path.join(OUT, 'console-xr.log'), logs.join('\n'));
  const errs = consoleErrors(logs);
  check('xr: no console errors or panics', errs.length === 0, errs.join(' | '));
  await ctx.close();
  return noGizmos;
}

module.exports = { run };
if (require.main === module) main(run);
