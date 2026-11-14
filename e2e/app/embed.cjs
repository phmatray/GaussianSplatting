// The shareable viewer in embed mode (`embed=1`): canvas and small
// controls only, alone and inside an <iframe> on a page of another origin
// (e2e/app/embed.html), with full screen and VR from inside the frame.
const fs = require('fs');
const path = require('path');
const { OUT, check, iwerBundle, newPage, serve, site, recording, consoleErrors, shot, content, sleep } = require('./lib.cjs');
const { splatFile } = require('./replay.cjs');

// The hook of the application inside a frame (or a page).
const stateOf = (target) => target.evaluate(() => window.__splatLive && window.__splatLive.state());
async function waitFor(target, predicate, timeout = 90000, arg = null) {
  await target.waitForFunction(
    ([source, arg]) => {
      const s = window.__splatLive && window.__splatLive.state();
      return s && new Function('s', 'arg', `return (${source})(s, arg)`)(s, arg);
    },
    [predicate.toString(), arg],
    { timeout, polling: 100 },
  );
  return stateOf(target);
}

async function alone(browser, base) {
  const { ctx, page, logs } = await newPage(browser, { width: 960, height: 540 });
  await page.goto(`${base}?file=recordings/stage.gsrec&embed=1`);
  let s = await waitFor(page, (s) => s.replay && s.replay.status.ready && s.viewer.framesRendered >= 1);
  const present = await page.evaluate(() => Object.fromEntries(['.topbar', '#panel', '#hud', '#timeline', '#tl-play', '#tl-scrub', '#tl-fullscreen', '#tl-close', '#tl-link', '#source-badge'].map((q) => [q, !!document.querySelector(q)])));
  check('embed: only the canvas and the small controls are there', s.embed && !present['.topbar'] && !present['#panel'] && !present['#source-badge'] && present['#timeline'] && present['#tl-play'] && present['#tl-scrub'] && present['#tl-fullscreen'] && !present['#tl-close'] && !present['#tl-link'], JSON.stringify(present));
  const box = await page.locator('#timeline').boundingBox();
  check('embed: the controls fit in a 960x540 frame', box.x >= 0 && box.x + box.width <= 960 && box.y + box.height <= 540 && box.height < 110, JSON.stringify(box));
  check('embed: no VR button without a headset', (await page.locator('#tl-vr').count()) === 0);
  await page.click('#tl-play');
  s = await waitFor(page, (s) => s.replay.status.playing && s.replay.status.time > 0.5);
  check('embed: the recording plays', s.replay.status.playing);
  // H must not hide the only controls there are.
  await page.keyboard.press('h');
  await sleep(200);
  check('embed: H does not hide the controls of the embedded viewer', !(await stateOf(page)).uiHidden && (await page.locator('#timeline').isVisible()));
  // Full screen from the button.
  await page.click('#tl-fullscreen');
  await page.waitForFunction(() => document.fullscreenElement !== null);
  const pressed = await page.waitForFunction(() => document.getElementById('tl-fullscreen').getAttribute('aria-pressed') === 'true', null, { timeout: 5000 }).then(() => true, () => false);
  check('embed: the full screen button enters full screen and shows it', pressed && /Quitter/.test(await page.locator('#tl-fullscreen').getAttribute('aria-label')));
  await page.click('#tl-fullscreen');
  await page.waitForFunction(() => document.fullscreenElement === null && document.getElementById('tl-fullscreen').getAttribute('aria-pressed') === 'false');

  // ---- narrow frame
  await page.setViewportSize({ width: 360, height: 300 });
  await sleep(300);
  const narrow = await page.evaluate(() => {
    const bar = document.getElementById('timeline').getBoundingClientRect();
    const out = [...document.querySelectorAll('#timeline button, #timeline input, #timeline select')].filter((el) => {
      const b = el.getBoundingClientRect();
      return b.left < bar.left - 0.5 || b.right > bar.right + 0.5 || b.width < 28 || b.height < 16;
    });
    return { left: bar.left, right: bar.right, bottom: bar.bottom, out: out.map((el) => el.id) };
  });
  check('embed: in a 360 px frame every control stays inside the bar', narrow.left >= 0 && narrow.right <= 360 && narrow.bottom <= 300 && narrow.out.length === 0, JSON.stringify(narrow));
  await shot(page, 'embed-narrow.png');

  // ---- a still scene: only the buttons
  await page.setViewportSize({ width: 960, height: 540 });
  await page.goto(`${base}?file=scenes/boule.splat&embed=1`);
  s = await waitFor(page, (s) => s.fileMode && s.viewer.framesRendered >= 1);
  check('embed: a splat scene shows the buttons alone, no time line and no way back to a live source', (await page.locator('#embed-bar #tl-fullscreen').count()) === 1 && (await page.locator('#timeline').count()) === 0 && (await page.locator('#file-pill .pill-action').count()) === 0 && s.source === null);
  check('embed: no console error', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
  await ctx.close();
}

// The viewer inside an <iframe> on a page of another origin.
async function framed(browser, base, hostUrl) {
  const { ctx, page, logs } = await newPage(browser, { width: 1100, height: 820, iwer: !!iwerBundle() });
  const src = `${base}?file=recordings/stage.gsrec&embed=1&autoplay=1&loop=1`;
  await page.goto(`${hostUrl}embed.html?src=${encodeURIComponent(src)}`);
  const frame = page.frames().find((f) => f !== page.mainFrame()) || (await page.waitForEvent('frameattached'));
  let s = await waitFor(frame, (s) => s.replay && s.replay.status.ready && s.replay.status.playing && s.replay.status.time > 0.3);
  check('embed: inside an <iframe> of another origin the recording loads and plays', new URL(frame.url()).origin !== new URL(page.url()).origin && s.embed && s.replay.status.looping && s.errors.length === 0, `${new URL(page.url()).origin} embeds ${new URL(frame.url()).origin}`);
  const picture = path.join(OUT, 'embed-iframe.png');
  await page.screenshot({ path: picture });
  const element = path.join(OUT, 'embed-iframe-view.png');
  await page.locator('#viewer').screenshot({ path: element });
  const c = await content(page, element);
  check('embed: the frame shows the scene', c.colors > 60 && c.bright > 0.15, `colors ${c.colors}`);
  // The controls work from inside the frame.
  await frame.click('#tl-play');
  s = await waitFor(frame, (s) => !s.replay.status.playing);
  await frame.focus('#tl-scrub');
  await frame.evaluate(() => {
    const el = document.getElementById('tl-scrub');
    el.value = '2.5';
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
  });
  s = await waitFor(frame, (s) => Math.abs(s.replay.status.time - 2.5) < 0.011 && !s.replay.status.buffering);
  check('embed: pause and scrub work inside the frame', !s.replay.status.playing);
  // Full screen: the frame carries allowfullscreen.
  await frame.click('#tl-fullscreen');
  await frame.waitForFunction(() => document.fullscreenElement !== null);
  check('embed: full screen works from inside the frame', await page.evaluate(() => document.fullscreenElement && document.fullscreenElement.id === 'viewer'));
  await frame.click('#tl-fullscreen');
  await frame.waitForFunction(() => document.fullscreenElement === null);
  // VR: the emulated headset is installed in the frame too.
  if (iwerBundle()) {
    s = await waitFor(frame, (s) => s.xr.supported === true);
    await frame.click('#tl-vr');
    s = await waitFor(frame, (s) => s.xr.active && s.xr.session && s.xr.session.frames > 5 && s.xr.session.viewports.length === 2);
    check('embed: the VR button starts an immersive session from inside the frame', s.xr.active && s.viewer.splatsDrawn > 5000, `${s.viewer.splatsDrawn} splats per eye`);
    await frame.click('#exit-vr');
    await waitFor(frame, (s) => !s.xr.active);
  }
  const errors = consoleErrors(logs);
  check('embed: no console error in the embedding page nor in the frame', errors.length === 0, errors.join(' | '));
  await ctx.close();
}

async function run({ browser }) {
  const dir = site('embed', { 'recordings/stage.gsrec': recording('stage'), 'scenes/boule.splat': splatFile(3000) });
  // The page that embeds the viewer lives on another origin (another port).
  const hostDir = path.join(OUT, 'site-embed-host');
  fs.rmSync(hostDir, { recursive: true, force: true });
  fs.mkdirSync(hostDir, { recursive: true });
  fs.copyFileSync(path.join(__dirname, 'embed.html'), path.join(hostDir, 'embed.html'));
  fs.writeFileSync(path.join(hostDir, 'index.html'), '<!doctype html><title>hôte</title>');
  const viewer = await serve({ dir, port: 8190 });
  const host = await serve({ dir: hostDir, port: 8195 });
  try {
    await alone(browser, viewer.url);
    await framed(browser, viewer.url, host.url);
  } finally {
    viewer.srv.close();
    host.srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
    fs.rmSync(hostDir, { recursive: true, force: true });
  }
}

module.exports = { run, waitFor, stateOf };
if (require.main === module) require('./standalone.cjs')(run);
