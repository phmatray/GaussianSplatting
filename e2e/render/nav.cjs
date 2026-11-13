// Desktop navigation on the live page: orbit, zoom, pan, fly, touch, resize,
// render-on-demand and WebGL context restore.
//   node e2e/render/nav.cjs        (BUILD=1 to rebuild the harness first)
const fs = require('fs');
const path = require('path');
const { OUT, check, newPage, consoleErrors, state, cmd, waitLive, settle, shot, imageStats, hasContent, main } = require('./lib.cjs');

const sub = (a, b) => a.map((v, i) => v - b[i]);
const len = (a) => Math.hypot(...a);
const dist = (a, b) => len(sub(a, b));
// Unit view direction and heading from the state.
const forward = (s) => sub(s.target, s.eye).map((v) => v / dist(s.target, s.eye));
const yawOf = (s) => Math.atan2(-forward(s)[0], -forward(s)[2]);
const f3 = (a) => '[' + a.map((v) => v.toFixed(3)).join(', ') + ']';

async function drag(page, from, to, options = {}) {
  await page.mouse.move(from[0], from[1]);
  await page.mouse.down(options);
  await page.mouse.move(to[0], to[1], { steps: 8 });
  await page.mouse.up(options);
}

async function run({ browser, url }) {
  console.log('\n=== nav: desktop navigation ===');
  const { ctx, page, logs } = await newPage(browser, { width: 960, height: 600, hasTouch: true });
  // Few, coarse cameras: headless Chromium rasterizes in software.
  await page.goto(`${url}/?test=live&static_cams=4&capture_scale=0.5`);
  await waitLive(page);
  // Freeze the dancer: from here on only navigation changes the picture.
  await cmd(page, 'pause', true);
  let s0 = await settle(page);
  check('nav: drawing buffer follows the CSS size (DPR 1)', s0.canvasWidth === 960 && s0.canvasHeight === 600, `${s0.canvasWidth}x${s0.canvasHeight}`);
  const f0 = await shot(page, 'nav-0-initial.png');
  let img = await imageStats(page, f0);
  check('nav: initial frame has content', hasContent(img.left) && hasContent(img.right), JSON.stringify(img.left));

  // ---- render on demand: nothing changes => nothing is redrawn
  await page.waitForTimeout(300);
  const idle0 = await state(page);
  await page.waitForTimeout(500);
  const idle1 = await state(page);
  check('nav: idle frames are not redrawn', idle1.frames > idle0.frames + 10 && idle1.framesRendered === idle0.framesRendered, `frames +${idle1.frames - idle0.frames}, rendered +${idle1.framesRendered - idle0.framesRendered}`);

  // ---- orbit: left drag rotates around the target
  await drag(page, [480, 300], [680, 340]);
  let s1 = await settle(page);
  const dYaw = yawOf(s1) - yawOf(s0);
  check('nav: orbit drag turns the camera by 0.005 rad/px around a fixed target',
    Math.abs(dYaw - -1.0) < 0.02 && dist(s1.target, s0.target) < 1e-3 && Math.abs(dist(s1.eye, s1.target) - dist(s0.eye, s0.target)) < 1e-3 && s1.eye[1] > s0.eye[1] + 0.3,
    `yaw ${yawOf(s0).toFixed(3)} -> ${yawOf(s1).toFixed(3)}, eye ${f3(s0.eye)} -> ${f3(s1.eye)}`);
  const f1 = await shot(page, 'nav-1-orbited.png');
  img = await imageStats(page, f1, f0);
  check('nav: the picture changed and is not blank after orbiting', img.diffMeanAbs > 3 && hasContent(img.left) && hasContent(img.right), `mean abs diff ${img.diffMeanAbs.toFixed(2)}`);

  // ---- wheel zooms (factor exp(0.0012 * deltaY))
  await page.mouse.move(480, 300);
  await page.mouse.wheel(0, -400);
  let s2 = await settle(page);
  const ratio = dist(s2.eye, s2.target) / dist(s1.eye, s1.target);
  check('nav: wheel zooms in toward the target', Math.abs(ratio - Math.exp(-0.48)) < 0.01 && dist(s2.target, s1.target) < 1e-3, `distance ratio ${ratio.toFixed(4)} (expected ${Math.exp(-0.48).toFixed(4)})`);

  // ---- pan: right drag and shift+drag move the target in the view plane
  await drag(page, [480, 300], [380, 300], { button: 'right' });
  let s3 = await settle(page);
  const moved = sub(s3.target, s2.target);
  const fwd = forward(s2);
  const along = moved[0] * fwd[0] + moved[1] * fwd[1] + moved[2] * fwd[2];
  // 100 px of 600 at 50 degrees: 2 * d * tan(25 deg) / 6.
  const expectedPan = (2 * dist(s2.eye, s2.target) * Math.tan((25 * Math.PI) / 180)) / 6;
  check('nav: right drag pans perpendicular to the view, the scene follows the pointer', Math.abs(len(moved) - expectedPan) < 0.02 && Math.abs(along) < 1e-3 && Math.abs(dist(s3.eye, s3.target) - dist(s2.eye, s2.target)) < 1e-3,
    `target moved ${len(moved).toFixed(3)} m (expected ${expectedPan.toFixed(3)}), along view ${along.toExponential(1)}`);
  await page.keyboard.down('Shift');
  await drag(page, [480, 300], [480, 360]);
  await page.keyboard.up('Shift');
  let s4 = await settle(page);
  check('nav: shift+drag pans too', s4.target[1] > s3.target[1] + 0.05 && Math.abs(yawOf(s4) - yawOf(s3)) < 1e-4, `target y ${s3.target[1].toFixed(3)} -> ${s4.target[1].toFixed(3)}`);
  await shot(page, 'nav-2-zoomed-panned.png');

  // ---- set_view
  await cmd(page, 'view', [3, 2, 4.5, 0, 1, 0]);
  let s5 = await settle(page);
  check('nav: set_view places the camera exactly', dist(s5.eye, [3, 2, 4.5]) < 1e-4 && dist(s5.target, [0, 1, 0]) < 1e-4, `eye ${f3(s5.eye)} target ${f3(s5.target)}`);

  // ---- touch: one finger rotates, two fingers pinch
  const cdp = await ctx.newCDPSession(page);
  const touch = (type, points) => cdp.send('Input.dispatchTouchEvent', { type, touchPoints: points.map(([x, y], id) => ({ x, y, id })) });
  await touch('touchStart', [[480, 300]]);
  for (let i = 1; i <= 6; i++) await touch('touchMove', [[480 + 20 * i, 300]]);
  await touch('touchEnd', []);
  let s6 = await settle(page);
  check('nav: one-finger drag orbits', Math.abs(yawOf(s6) - yawOf(s5) - -0.6) < 0.03 && dist(s6.target, s5.target) < 1e-3, `yaw ${yawOf(s5).toFixed(3)} -> ${yawOf(s6).toFixed(3)}`);
  await touch('touchStart', [[430, 300], [530, 300]]);
  for (let i = 1; i <= 5; i++) await touch('touchMove', [[430 - 10 * i, 300], [530 + 10 * i, 300]]);
  await touch('touchEnd', []);
  let s7 = await settle(page);
  const pinch = dist(s7.eye, s7.target) / dist(s6.eye, s6.target);
  check('nav: two-finger pinch zooms (fingers twice as far apart = half the distance)', Math.abs(pinch - 0.5) < 0.03, `distance ratio ${pinch.toFixed(3)}`);

  // ---- fly mode
  await cmd(page, 'view', [3, 2, 4.5, 0, 1, 0]);
  await cmd(page, 'nav', 'fly');
  let fl0 = await settle(page);
  check('nav: switching to fly keeps the camera', fl0.navMode === 'fly' && dist(fl0.eye, [3, 2, 4.5]) < 1e-3 && dist(forward(fl0), forward(s5)) < 1e-3, `eye ${f3(fl0.eye)}`);
  await page.keyboard.down('KeyW');
  await page.waitForTimeout(700);
  await page.keyboard.up('KeyW');
  let fl1 = await settle(page);
  const step = sub(fl1.eye, fl0.eye);
  const f = forward(fl0);
  const cos = (step[0] * f[0] + step[1] * f[1] + step[2] * f[2]) / len(step);
  check('nav: W flies along the view direction at about the fly speed', len(step) > 0.5 && len(step) < 2.0 && cos > 0.999, `moved ${len(step).toFixed(2)} m in 0.7 s (speed ${fl0.flySpeed}), cos ${cos.toFixed(5)}`);
  await page.keyboard.down('Shift');
  await page.keyboard.down('KeyS');
  await page.waitForTimeout(700);
  await page.keyboard.up('KeyS');
  await page.keyboard.up('Shift');
  let fl2 = await settle(page);
  check('nav: shift+S flies back faster', dist(fl2.eye, fl1.eye) > 1.6 * len(step), `moved back ${dist(fl2.eye, fl1.eye).toFixed(2)} m`);
  await page.keyboard.down('KeyE');
  await page.keyboard.down('KeyD');
  await page.waitForTimeout(500);
  await page.keyboard.up('KeyE');
  await page.keyboard.up('KeyD');
  let fl3 = await settle(page);
  check('nav: E rises and D strafes right', fl3.eye[1] > fl2.eye[1] + 0.3 && dist([fl3.eye[0], fl3.eye[2]], [fl2.eye[0], fl2.eye[2]]) > 0.3, `eye ${f3(fl2.eye)} -> ${f3(fl3.eye)}`);
  await page.keyboard.down('ArrowUp');
  await page.waitForTimeout(300);
  await page.keyboard.up('ArrowUp');
  await page.keyboard.down('KeyQ');
  await page.waitForTimeout(300);
  await page.keyboard.up('KeyQ');
  let fl4 = await settle(page);
  check('nav: arrow keys move and Q descends', dist(fl4.eye, fl3.eye) > 0.3 && fl4.eye[1] < fl3.eye[1], `eye ${f3(fl3.eye)} -> ${f3(fl4.eye)}`);
  await drag(page, [480, 300], [580, 300]);
  let fl5 = await settle(page);
  check('nav: drag looks around in place (mouse right = turn right)', Math.abs(yawOf(fl5) - yawOf(fl4) - -0.5) < 0.02 && dist(fl5.eye, fl4.eye) < 1e-3, `yaw ${yawOf(fl4).toFixed(3)} -> ${yawOf(fl5).toFixed(3)}`);
  await page.mouse.wheel(0, -500);
  let fl6 = await settle(page);
  check('nav: wheel changes the fly speed, not the position', fl6.flySpeed > fl5.flySpeed * 1.5 && dist(fl6.eye, fl5.eye) < 1e-3, `speed ${fl5.flySpeed.toFixed(2)} -> ${fl6.flySpeed.toFixed(2)}`);
  const f3shot = await shot(page, 'nav-3-fly.png');
  img = await imageStats(page, f3shot);
  check('nav: fly view is not blank', hasContent(img.left) || hasContent(img.right), JSON.stringify([img.left, img.right]));

  // ---- gizmos on/off changes the picture
  await cmd(page, 'nav', 'orbit');
  await cmd(page, 'view', [3, 2, 4.5, 0, 1, 0]);
  await settle(page);
  const g1 = await shot(page, 'nav-4-gizmos-on.png');
  await cmd(page, 'gizmos', false);
  await page.waitForTimeout(200);
  const g0 = await shot(page, 'nav-5-gizmos-off.png');
  img = await imageStats(page, g0, g1);
  check('nav: gizmos are drawn on top and can be hidden', img.diffChangedFrac > 0.002 && img.diffChangedFrac < 0.3, `${(img.diffChangedFrac * 100).toFixed(2)} % of the pixels differ`);
  await cmd(page, 'gizmos', true);

  // ---- resize and render scale
  await page.setViewportSize({ width: 700, height: 500 });
  await page.waitForTimeout(250);
  let r1 = await state(page);
  check('nav: canvas follows a window resize', r1.canvasWidth === 700 && r1.canvasHeight === 500, `${r1.canvasWidth}x${r1.canvasHeight}`);
  await cmd(page, 'renderScale', 0.5);
  await page.waitForTimeout(250);
  let r2 = await state(page);
  const cssSize = await page.evaluate(() => { const c = document.getElementById('gl'); return [c.clientWidth, c.clientHeight, c.width, c.height]; });
  check('nav: render scale 0.5 halves the drawing buffer, not the element', r2.canvasWidth === 350 && r2.canvasHeight === 250 && cssSize[0] === 700 && cssSize[2] === 350, `buffer ${r2.canvasWidth}x${r2.canvasHeight}, css ${cssSize[0]}x${cssSize[1]}`);
  const lo = await shot(page, 'nav-6-half-resolution.png');
  img = await imageStats(page, lo);
  check('nav: half-resolution frame has content', hasContent(img.left) && hasContent(img.right));
  await cmd(page, 'renderScale', 1);
  await page.setViewportSize({ width: 960, height: 600 });
  await page.waitForTimeout(250);

  // ---- WebGL context loss and restore
  const before = await shot(page, 'nav-7-before-context-loss.png');
  await page.evaluate(() => {
    const gl = document.getElementById('gl').getContext('webgl2');
    window.__lose = gl.getExtension('WEBGL_lose_context');
    window.__lose.loseContext();
  });
  await page.waitForTimeout(300);
  await page.evaluate(() => window.__lose.restoreContext());
  await page.waitForTimeout(600);
  const after = await shot(page, 'nav-8-after-context-restore.png');
  img = await imageStats(page, after, before);
  const lost = await page.evaluate(() => document.getElementById('gl').getContext('webgl2').isContextLost());
  check('nav: the picture comes back after a WebGL context loss', !lost && img.diffMeanAbs < 0.5 && hasContent(img.left), `mean abs diff to before ${img.diffMeanAbs.toFixed(3)}`);

  fs.writeFileSync(path.join(OUT, 'console-nav.log'), logs.join('\n'));
  const errs = consoleErrors(logs).filter((l) => !/CONTEXT_LOST_WEBGL/.test(l));
  check('nav: no console errors or panics', errs.length === 0, errs.join(' | '));
  await ctx.close();
}

module.exports = { run };
if (require.main === module) main(run);
