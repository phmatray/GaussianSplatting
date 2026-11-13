// The dynamic layer: wire messages through Viewer::apply_message change the
// picture over time while the static layer stays put.
//   node e2e/render/live.cjs        (BUILD=1 to rebuild the harness first)
const fs = require('fs');
const path = require('path');
const { OUT, check, newPage, consoleErrors, state, cmd, waitLive, shot, imageStats, hasContent, main } = require('./lib.cjs');

async function run({ browser, url }) {
  console.log('\n=== live: dynamic layer through apply_message ===');
  const { ctx, page, logs } = await newPage(browser, { width: 960, height: 600 });
  await page.goto(`${url}/?test=live&static_cams=4&capture_scale=0.5`);
  await waitLive(page);
  const s0 = await state(page);
  check('live: both layers are filled from wire messages', s0.staticSplats > 5000 && s0.dynamicSplats > 200 && s0.splatsTotal === s0.staticSplats + s0.dynamicSplats,
    `static ${s0.staticSplats}, dynamic ${s0.dynamicSplats}, total ${s0.splatsTotal}`);
  check('live: the static layer arrived in several Append chunks', s0.messages - s0.ticks >= 2, `${s0.messages - s0.ticks} static messages`);

  const f0 = await shot(page, 'live-0.png');
  // The dancer needs a moment to move visibly.
  await page.waitForTimeout(1500);
  const f1 = await shot(page, 'live-1.png');
  const s1 = await state(page);
  let img = await imageStats(page, f1, f0);
  check('live: the picture changes over time', img.diffChangedFrac > 0.003 && hasContent(img.left) && hasContent(img.right), `${(img.diffChangedFrac * 100).toFixed(2)} % of the pixels changed, mean abs diff ${img.diffMeanAbs.toFixed(2)}`);
  check('live: ticks keep coming and each one redraws', s1.ticks > s0.ticks + 5 && s1.framesRendered > s0.framesRendered + 5 && s1.sorts > s0.sorts + 5,
    `ticks +${s1.ticks - s0.ticks}, rendered +${s1.framesRendered - s0.framesRendered}, sorts +${s1.sorts - s0.sorts}`);
  check('live: only the dynamic layer changes', s1.staticSplats === s0.staticSplats && s1.dynamicSplats !== s0.dynamicSplats, `static ${s1.staticSplats}, dynamic ${s0.dynamicSplats} -> ${s1.dynamicSplats}`);
  check('live: all visible splats are drawn', s1.splatsDrawn === s1.splatsTotal || Math.abs(s1.splatsDrawn - s1.splatsTotal) < 2000, `drawn ${s1.splatsDrawn} of ${s1.splatsTotal}`);
  console.log(`      tick ${s1.tickMs.toFixed(1)} ms (capture + encode + apply), apply_message ${s1.applyMs.toFixed(2)} ms, sort ${s1.sortMs.toFixed(2)} ms, viewer CPU ${s1.frameMs.toFixed(2)} ms/frame`);

  // ---- paused: the picture is stable
  await cmd(page, 'pause', true);
  await page.waitForTimeout(300);
  const p0 = await shot(page, 'live-2-paused.png');
  await page.waitForTimeout(600);
  const p1 = await shot(page, 'live-3-paused-later.png');
  img = await imageStats(page, p1, p0);
  check('live: paused animation gives a stable picture', img.diffMeanAbs === 0, `mean abs diff ${img.diffMeanAbs}`);

  // ---- layer visibility
  const all = await state(page);
  await cmd(page, 'showDynamic', false);
  await page.waitForTimeout(250);
  const noDyn = await state(page);
  const fStatic = await shot(page, 'live-4-static-only.png');
  await cmd(page, 'showDynamic', true);
  await cmd(page, 'showStatic', false);
  await page.waitForTimeout(250);
  const noStatic = await state(page);
  const fDyn = await shot(page, 'live-5-dynamic-only.png');
  await cmd(page, 'showStatic', true);
  check('live: layer visibility toggles select what is drawn', noDyn.splatsDrawn === all.staticSplats && noStatic.splatsDrawn === all.dynamicSplats, `static only ${noDyn.splatsDrawn}, dynamic only ${noStatic.splatsDrawn}`);
  img = await imageStats(page, fDyn, fStatic);
  check('live: static-only and dynamic-only pictures differ', img.diffChangedFrac > 0.1, `${(img.diffChangedFrac * 100).toFixed(1)} % differ`);

  // ---- height clip and point mode
  await cmd(page, 'clipY', 1.0);
  await page.waitForTimeout(250);
  const clipped = await state(page);
  await shot(page, 'live-6-clip-y.png');
  check('live: clip_y_max hides the splats above it', clipped.splatsDrawn < all.splatsTotal - 100 && clipped.splatsDrawn > all.splatsTotal / 4, `drawn ${clipped.splatsDrawn} of ${all.splatsTotal}`);
  await cmd(page, 'clipY', null);
  await cmd(page, 'points', true);
  await page.waitForTimeout(250);
  const fPoints = await shot(page, 'live-7-points.png');
  img = await imageStats(page, fPoints, p1);
  check('live: point mode draws small dots', img.diffChangedFrac > 0.05 && hasContent(img.left), `${(img.diffChangedFrac * 100).toFixed(1)} % differ from the splat picture`);
  await cmd(page, 'points', false);

  // ---- destroy stops everything
  await cmd(page, 'pause', false);
  await page.waitForTimeout(300);
  await cmd(page, 'destroy');
  const d0 = await state(page);
  await page.waitForTimeout(400);
  const d1 = await state(page);
  check('live: destroy stops the frame loop', d1.frames === d0.frames, `frames ${d0.frames} -> ${d1.frames}`);

  fs.writeFileSync(path.join(OUT, 'console-live.log'), logs.join('\n'));
  // After destroy the harness keeps ticking into a dead viewer on purpose; GL
  // calls on deleted objects would show up here as errors.
  const errs = consoleErrors(logs);
  check('live: no console errors or panics', errs.length === 0, errs.join(' | '));
  await ctx.close();
}

module.exports = { run };
if (require.main === module) main(run);
