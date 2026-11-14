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
| `compare.cjs` | `?test=compare` | GPU image vs the CPU references in every blending mode: 15 hand-built cases as imported content against `gs_core::raster::rasterize` (sorted, PSNR >= 35 dB), and the product's stage and house clouds as surfels, sorted, normalised hard and normalised soft, against `gs_core::experimental::wblend` with its presets (PSNR >= 45 dB normalised): orbit, close-up, far, off-axis XR eye, miniature, points, splat scale, height clip, both faces, layer weights, the on-screen canvas |
| `modes.cjs` | `?test=scene` | what `Viewer::blend_status` reports; fallback to sorted without the float-target extensions (`mask_float=1`) and when the render targets cannot be allocated; no CPU sort in the normalised mode and a picture that depends on the pose alone; gizmo lines hidden behind the normalised surface; resize and WebGL context restore with the off-screen targets |
| `nav.cjs` | `?test=live` | orbit, zoom, pan, fly keys, touch, `set_view`, resize, render scale, render-on-demand, WebGL context restore |
| `live.cjs` | `?test=live` | dynamic layer through `apply_message` changes the picture; layer toggles, height clip, point mode, `destroy` |
| `xr.cjs` | `?test=live` + IWER | stereo viewports, view matrices vs the emulated headset, locomotion, snap turn, miniature, controllers, session end from both sides |
| `robust.cjs` | `?test=live` (+ IWER) | malformed, random and mutated wire messages; splats at 10^30 m; nonsense parameters; rapid configuration changes; empty layers; a canvas of size 0 and one without CSS size; DPR cap; idle auto-rotation; **both eyes of an off-axis stereo session against the CPU reference** (normalised soft and hard, life-size and miniature; sorted); context loss inside a session; repeated, cancelled and destroyed session starts |
| `perf.cjs` | `?test=perf&n=...` | reports sort / frame / layer-update times (CPU side) |
| `measure.cjs` | `?test=scene` (+ IWER) | not a check, a report (`ONLY=measure` or `node e2e/render/measure.cjs`): 1280x720 pictures of the stage and the house before and after (`sheet-*.png`: before, sorted, normalised hard, normalised soft), one stereo frame per mode, frame times, main-thread time and sorts per frame on the desktop and per XR frame, fragments per pixel, change between consecutive views of an orbit, flicker of a noisy live layer. See the header of the file for its options |

Each suite also runs alone (`node e2e/render/xr.cjs`, add `BUILD=1` to rebuild
first). Environment variables:

- `GS_RENDER_OUT`: directory for screenshots, console logs and JSON reports
  (default `out/render-e2e` at the repository root, git-ignored).
- `SKIP_BUILD=1`: reuse the existing `crates/gs-render-harness/dist`.
- `ONLY=compare,xr`: run a subset (`measure` only runs when named here).
- `PERF_N=200000,1000000`: splat counts of the perf suite.
- `GS_NPM_TOOLS` (a `node_modules` directory) or `IWER_BUNDLE` (path to
  `iwer.min.js`): where to find the emulator when it is not installed here.

A WebGL warning in the console (`INVALID_OPERATION` ...) fails the suite that
produced it, like an exception or a panic.

Headless Chromium rasterizes in software, so frame rates in these runs say
nothing about a real GPU; the suites use few, coarse simulated cameras for
that reason. Timings reported by `perf.cjs` are CPU-side only. The
normalised mode draws every splat three or four times and is several times
slower than the sorted one on this rasterizer; the checks that time the
camera against the wall clock (`nav.cjs`, two in `robust.cjs`) therefore
run with `blend=sorted`.
