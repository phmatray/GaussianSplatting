// CPU-side cost of the viewer (?test=perf&n=...): sort, per-frame work and
// the layer update paths. GPU time is not measured (software rasterizer).
//   node e2e/render/perf.cjs        (PERF_N=200000,1000000 to choose sizes)
const fs = require('fs');
const path = require('path');
const { OUT, check, newPage, consoleErrors, main } = require('./lib.cjs');

const SIZES = (process.env.PERF_N || '200000').split(',').map(Number);

async function run({ browser, url }) {
  const reports = [];
  for (const n of SIZES) {
    console.log(`\n=== perf: ${n} splats ===`);
    // A small canvas: the software rasterizer's fill rate is not the subject.
    const { ctx, page, logs } = await newPage(browser, { width: 640, height: 360 });
    await page.goto(`${url}/?test=perf&n=${n}`);
    await page.waitForFunction(() => window.__perf || window.__error, null, { timeout: 600000 });
    const error = await page.evaluate(() => window.__error || null);
    check(`perf ${n}: harness finished without error`, !error, error || '');
    if (error) {
      await ctx.close();
      continue;
    }
    const p = await page.evaluate(() => JSON.parse(JSON.stringify(window.__perf)));
    reports.push(p);
    console.log(`      sort (incl. order upload): mean ${p.sortMsMean.toFixed(2)} ms, max ${p.sortMsMax.toFixed(2)} ms, ${p.sorts} sorts in ${p.frames} frames`);
    console.log(`      viewer CPU per frame (smoothed): ${p.viewerCpuMs.toFixed(2)} ms; frame interval in this browser: ${p.frameIntervalMs.toFixed(1)} ms (${p.canvasWidth}x${p.canvasHeight}, software GL)`);
    console.log(`      set_layer of ${n} splats: ${p.uploadStaticMs.toFixed(1)} ms`);
    console.log(`      Replace of the dynamic layer (${p.dynamicSplats} splats, decode + pack + upload): ${p.replaceDynamicMs.toFixed(2)} ms`);
    console.log(`      Append of ${p.appendChunkSplats} splats to the static layer: ${p.appendChunkMs.toFixed(2)} ms`);
    check(`perf ${n}: every splat is drawn`, p.splatsDrawn === n + p.dynamicSplats && p.splatsTotal === p.splatsDrawn, `${p.splatsDrawn} drawn`);
    check(`perf ${n}: a turning camera is re-sorted, at most once per frame`, p.sorts >= p.frames / 2 - 1 && p.sorts <= p.framesRendered + 1, `${p.sorts} sorts, ${p.framesRendered} frames rendered`);
    // The static layer is n splats; updating the dynamic one must not scale with it.
    check(`perf ${n}: replacing the dynamic layer costs O(dynamic), appending O(chunk)`, p.replaceDynamicMs < 25 && p.appendChunkMs < 25, `replace ${p.replaceDynamicMs.toFixed(2)} ms, append ${p.appendChunkMs.toFixed(2)} ms`);
    const errs = consoleErrors(logs);
    check(`perf ${n}: no console errors or panics`, errs.length === 0, errs.join(' | '));
    await page.screenshot({ path: path.join(OUT, `perf-${n}.png`) });
    await ctx.close();
  }
  fs.mkdirSync(OUT, { recursive: true });
  fs.writeFileSync(path.join(OUT, 'perf.json'), JSON.stringify(reports, null, 2));
  return reports;
}

module.exports = { run };
if (require.main === module) main(run);
