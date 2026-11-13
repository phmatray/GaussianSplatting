# gs-render end-to-end checks

Playwright scripts that drive the `crates/gs-render-harness` page in headless
Chromium (WebGL2 through SwiftShader, WebXR through the IWER emulator).

```bash
(cd e2e/render && npm install)   # once: fetches the IWER emulator
node e2e/render/run.cjs          # trunk build --release, then every suite
```

`playwright` itself must be resolvable by Node (it is not installed from
here, to avoid downloading browsers).

| Suite | Page | What it asserts |
|---|---|---|
| `compare.cjs` | `?test=compare` | GPU image vs `gs_core::raster::rasterize` on 15 cases (including a miniature-scale view, degenerate splats and a far outlier), PSNR >= 35 dB each |
| `nav.cjs` | `?test=live` | orbit, zoom, pan, fly keys, touch, `set_view`, resize, render scale, render-on-demand, WebGL context restore |
| `live.cjs` | `?test=live` | dynamic layer through `apply_message` changes the picture; layer toggles, height clip, point mode, `destroy` |
| `xr.cjs` | `?test=live` + IWER | stereo viewports, view matrices vs the emulated headset, locomotion, snap turn, miniature, controllers, session end from both sides |
| `robust.cjs` | `?test=live` (+ IWER) | malformed, random and mutated wire messages; splats at 10^30 m; nonsense parameters; rapid configuration changes; empty layers; a canvas of size 0 and one without CSS size; DPR cap; idle auto-rotation; **both eyes of an off-axis stereo session against the CPU reference** (life-size and miniature); context loss inside a session; repeated, cancelled and destroyed session starts |
| `perf.cjs` | `?test=perf&n=...` | reports sort / frame / layer-update times (CPU side) |

Each suite also runs alone (`node e2e/render/xr.cjs`, add `BUILD=1` to rebuild
first). Environment variables:

- `GS_RENDER_OUT`: directory for screenshots, console logs and JSON reports
  (default `out/render-e2e` at the repository root, git-ignored).
- `SKIP_BUILD=1`: reuse the existing `crates/gs-render-harness/dist`.
- `ONLY=compare,xr`: run a subset.
- `PERF_N=200000,1000000`: splat counts of the perf suite.
- `GS_NPM_TOOLS` (a `node_modules` directory) or `IWER_BUNDLE` (path to
  `iwer.min.js`): where to find the emulator when it is not installed here.

A WebGL warning in the console (`INVALID_OPERATION` ...) fails the suite that
produced it, like an exception or a panic.

Headless Chromium rasterizes in software, so frame rates in these runs say
nothing about a real GPU; the suites use few, coarse simulated cameras for
that reason. Timings reported by `perf.cjs` are CPU-side only.
