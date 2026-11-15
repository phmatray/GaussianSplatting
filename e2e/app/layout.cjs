// Layout at phone, small-window and large-desktop widths: nothing leaves
// the window, no text is cut, the floating blocks (top bar, panel, chips,
// scan pill, notices) do not cover one another, with and without the panel,
// on the stage and in the house with an error notice on screen.
const { check, newPage, consoleErrors, state, waitState, open, shot, sleep } = require('./lib.cjs');

// Everything that looks wrong in the current layout, as a list of findings.
const audit = (page) =>
  page.evaluate(() => {
    const out = [];
    const visible = (el) => {
      const box = el.getBoundingClientRect();
      const style = getComputedStyle(el);
      return box.width > 0 && box.height > 0 && style.visibility !== 'hidden' && style.display !== 'none';
    };
    const name = (el) => el.id || el.className || el.tagName;
    for (const el of document.querySelectorAll('.topbar *, #hud, #hud *, .pill, .pill *, #toast, #toast *')) {
      if (!visible(el)) continue;
      const box = el.getBoundingClientRect();
      if (box.left < -0.5 || box.right > innerWidth + 0.5 || box.top < -0.5 || box.bottom > innerHeight + 0.5) out.push(`outside the window: ${name(el)}`);
    }
    // The panel scrolls vertically; sideways nothing may stick out of it.
    const panel = document.querySelector('#panel.open');
    if (panel) {
      const frame = panel.getBoundingClientRect();
      for (const el of panel.querySelectorAll('*')) {
        if (!visible(el) || el.closest('.visually-hidden')) continue;
        const box = el.getBoundingClientRect();
        if (box.left < frame.left - 0.5 || box.right > frame.right + 0.5) out.push(`sticks out of the panel: ${name(el)}`);
      }
    }
    for (const el of document.querySelectorAll('#app button, #app .badge, #app .badge-name, #app .pill, #app dd, #app dt, #app output, #app .toggle-label, #app code, #app p, #app h1')) {
      if (!visible(el)) continue;
      if (el.scrollWidth > el.clientWidth + 1 && getComputedStyle(el).overflowX !== 'visible') out.push(`text cut: ${name(el)} "${el.textContent.trim().slice(0, 24)}"`);
    }
    const blocks = ['.topbar .brand', '.topbar .modes', '.topbar .actions', '#panel.open', '#hud', '.pill', '#toast'].flatMap((q) => [...document.querySelectorAll(q)]).filter(visible);
    for (let i = 0; i < blocks.length; i++) {
      for (let j = i + 1; j < blocks.length; j++) {
        const [a, b] = [blocks[i].getBoundingClientRect(), blocks[j].getBoundingClientRect()];
        const w = Math.min(a.right, b.right) - Math.max(a.left, b.left);
        const h = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top);
        if (w > 1 && h > 1) out.push(`${name(blocks[i])} and ${name(blocks[j])} overlap by ${w.toFixed(0)}x${h.toFixed(0)}`);
      }
    }
    const doc = document.documentElement;
    if (doc.scrollWidth > innerWidth || doc.scrollHeight > innerHeight) out.push(`the page scrolls (${doc.scrollWidth}x${doc.scrollHeight})`);
    return out;
  });

async function run({ browser, url }) {
  // The three widths of the brief, then awkward ones: the smallest phone,
  // a short landscape window on each side of the sheet breakpoint.
  for (const [width, height] of [[400, 760], [800, 600], [1600, 900], [320, 560], [760, 480], [1024, 500]]) {
    const tag = `${width}x${height}`;
    const { ctx, page, logs } = await newPage(browser, { width, height });
    let s = await open(page, `${url}?source=worker`);
    let found = await audit(page);
    check(`layout ${tag}: the stage as it opens`, found.length === 0, found.join(' | '));
    if (width === 800 || width === 1600) {
      const badge = await page.locator('#source-badge').innerText();
      check(`layout ${tag}: the source badge is readable in full`, badge.trim() === 'Local (WASM)', badge);
    }
    await page.click('#btn-panel');
    await sleep(400);
    found = await audit(page);
    s = await state(page);
    check(`layout ${tag}: the stage with the panel ${s.panelOpen ? 'open' : 'closed'}`, found.length === 0, found.join(' | '));
    if (!s.panelOpen) await page.click('#btn-panel');
    await sleep(400);

    // The house, scanning, with an error notice: the busiest screen.
    await page.click('#mode-house');
    await waitState(page, (s) => s.scene === 'house' && s.display.dollhouse && s.layers.static > 5000, 90000);
    await page.setInputFiles('#import-input', { name: 'cassé.ply', mimeType: 'application/octet-stream', buffer: Buffer.from('nope') });
    await page.waitForSelector('#toast');
    await sleep(400);
    found = await audit(page);
    check(`layout ${tag}: the house with its scan pill, the panel and an error notice`, found.length === 0 && (await page.locator('#scan-pill').isVisible()) && (await page.locator('#toast').isVisible()), found.join(' | '));
    const toast = await page.evaluate(() => {
      const box = document.querySelector('#toast').getBoundingClientRect();
      const line = parseFloat(getComputedStyle(document.querySelector('#toast')).lineHeight) || 20;
      return { width: box.width, lines: Math.round((box.height - 22) / line) };
    });
    check(`layout ${tag}: the notice uses the width it has`, toast.lines <= 3, `${toast.width.toFixed(0)} px wide, about ${toast.lines} line(s)`);
    if (width === 400 || width === 800 || width === 1600) await shot(page, `layout-${width}-house.png`);
    for (const section of ['section-display', 'section-cameras', 'section-stats', 'section-vr', 'section-file']) {
      await page.locator(`#${section} .section-head`).scrollIntoViewIfNeeded();
      found = (await audit(page)).filter((f) => /sticks out|text cut/.test(f));
      if (found.length) check(`layout ${tag}: section ${section}`, false, found.join(' | '));
    }
    await page.locator('#file-error').scrollIntoViewIfNeeded();
    await sleep(200);
    if (width === 400 || width === 800 || width === 1600) await shot(page, `layout-${width}-panel-end.png`);
    check(`layout ${tag}: no console error`, consoleErrors(logs).length === 0, consoleErrors(logs).join(' | '));
    await ctx.close();
  }

  // The top bar across every width above the sheet breakpoint, connected
  // and with the longest badge ("Serveur natif" and its link state).
  for (const [what, query, ready] of [
    ['connected', '?source=worker', (page) => open(page, `${url}?source=worker`)],
    ['offline', '?source=server', (page) => page.goto(`${url}?source=server`).then(() => page.waitForSelector('#source-badge[data-link="failed"]'))],
  ]) {
    const { ctx, page } = await newPage(browser, { width: 1300, height: 500 });
    await ready(page);
    const bad = [];
    for (let width = 1300; width >= 761; width -= 7) {
      await page.setViewportSize({ width, height: 500 });
      const found = (await audit(page)).filter((f) => /brand|modes|actions|badge|outside|text cut/.test(f) && !/toast/.test(f));
      if (found.length) bad.push(`${width}: ${found[0]}`);
    }
    check(`layout: the top bar fits at every width from 761 to 1300 px (${what}, ${query})`, bad.length === 0, bad.slice(0, 4).join(' | '));
    await ctx.close();
  }
}

module.exports = { run };
if (require.main === module) require('./standalone.cjs')(run);
