// A narrow window (a phone, or the 2D browser window of a headset): the
// top bar fits, the panel is a sheet at the bottom, everything is reachable.
const { check, newPage, consoleErrors, state, waitState, open, shot, viewShot, content, setSlider, sleep } = require('./lib.cjs');

// Elements that stick out of the window horizontally.
const overflowing = (page, selector) =>
  page.evaluate((selector) => {
    const out = [];
    for (const el of document.querySelectorAll(selector)) {
      const box = el.getBoundingClientRect();
      if (box.width === 0 || getComputedStyle(el).visibility === 'hidden') continue;
      if (box.left < -0.5 || box.right > window.innerWidth + 0.5) out.push(`${el.id || el.className}: ${box.left.toFixed(0)}..${box.right.toFixed(0)}`);
    }
    return out;
  }, selector);

async function run({ browser, url }) {
  const { ctx, page, logs } = await newPage(browser, { width: 400, height: 760, hasTouch: true });
  let s = await open(page, url);
  check('narrow: the panel starts closed', !s.panelOpen);
  const scroll = await page.evaluate(() => ({ w: document.documentElement.scrollWidth, h: document.documentElement.scrollHeight, iw: innerWidth, ih: innerHeight }));
  check('narrow: the page does not scroll', scroll.w <= scroll.iw && scroll.h <= scroll.ih, JSON.stringify(scroll));
  let out = await overflowing(page, '.topbar *');
  check('narrow: the top bar fits in the window', out.length === 0, out.join(' | '));
  // The badge is an indicator, not a control: it only has to be visible.
  const reachable = await page.evaluate(() => ['#mode-stage', '#mode-house', '#btn-pause', '#btn-vr', '#btn-panel', '#source-badge'].map((q) => {
    const el = document.querySelector(q);
    const box = el.getBoundingClientRect();
    const top = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
    const large = q === '#source-badge' || (box.width >= 32 && box.height >= 32);
    return el.contains(top) && large ? null : q;
  }).filter(Boolean));
  check('narrow: every top bar control is visible and large enough to touch', reachable.length === 0, reachable.join(', '));
  const labels = await page.evaluate(() => [...document.querySelectorAll('.mode')].map((m) => m.innerText.trim()));
  check('narrow: the mode switch keeps its labels', labels.join('|') === 'Scène en direct|Scan de maison', labels.join('|'));
  await shot(page, 'narrow.png');
  const view = await viewShot(page, 'narrow-view.png');
  const c = await content(page, view);
  check('narrow: the scene renders', c.colors > 60 && s.viewer.canvasWidth === 400, `colors ${c.colors}, canvas ${s.viewer.canvasWidth}x${s.viewer.canvasHeight}`);

  // ---- the panel as a bottom sheet
  await page.tap('#btn-panel');
  await page.waitForFunction(() => window.__splatLive.state().panelOpen);
  await sleep(350);
  const sheet = await page.evaluate(() => {
    const box = document.getElementById('panel').getBoundingClientRect();
    return { left: box.left, right: box.right, top: box.top, bottom: box.bottom, ih: innerHeight, iw: innerWidth };
  });
  check('narrow: the panel is a sheet across the bottom, the scene stays visible above', sheet.left === 0 && sheet.right === sheet.iw && Math.abs(sheet.bottom - sheet.ih) < 1 && sheet.top > sheet.ih * 0.3, JSON.stringify(sheet));
  out = await overflowing(page, '#panel *');
  check('narrow: nothing in the panel is cut off sideways', out.length === 0, out.slice(0, 5).join(' | '));
  await shot(page, 'narrow-panel.png');

  // ---- controls work by touch
  await page.locator('#toggle-points').scrollIntoViewIfNeeded();
  await page.tap('#toggle-points');
  await waitState(page, (s) => s.display.pointMode);
  await page.tap('#toggle-points');
  await setSlider(page, '#cam-count', 4);
  s = await waitState(page, (s) => s.config.cameras === 4);
  check('narrow: panel controls work', s.config.cameras === 4 && !s.display.pointMode);
  for (const id of ['section-capture', 'section-display', 'section-cameras', 'section-stats', 'section-vr', 'section-file']) {
    await page.locator(`#${id} .section-head`).scrollIntoViewIfNeeded();
  }
  await page.locator('#import-btn').scrollIntoViewIfNeeded();
  const last = await page.evaluate(() => {
    const box = document.querySelector('#import-btn').getBoundingClientRect();
    const sheet = document.getElementById('panel').getBoundingClientRect();
    return { top: box.top, bottom: box.bottom, width: box.width, sheetTop: sheet.top, ih: innerHeight };
  });
  check('narrow: the whole panel can be scrolled through', last.top >= last.sheetTop && last.bottom <= last.ih && last.width > 200, JSON.stringify(last));
  await page.locator('#section-vr').scrollIntoViewIfNeeded();
  await shot(page, 'narrow-panel-vr.png');

  // ---- the house on a narrow window
  await page.tap('#mode-house');
  s = await waitState(page, (s) => s.scene === 'house' && s.display.dollhouse && s.layers.static > 20000, 90000);
  out = await overflowing(page, '.pill, .pill *');
  check('narrow: the scan pill fits', out.length === 0 && (await page.locator('#scan-pill').isVisible()), out.join(' | '));
  await page.tap('#btn-panel');
  await page.waitForFunction(() => !window.__splatLive.state().panelOpen);
  await sleep(350);
  await shot(page, 'narrow-house.png');

  s = await state(page);
  check('narrow: no application error', s.errors.length === 0, s.errors.join(' | '));
  check('narrow: no console error', consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
  await ctx.close();
}

module.exports = { run };
if (require.main === module) require('./standalone.cjs')(run);
