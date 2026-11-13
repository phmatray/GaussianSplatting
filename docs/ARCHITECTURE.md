# Splat Live: architecture

Live Gaussian splatting from a ring of cameras around a subject, viewable on a
desktop and in a VR headset. Built with Rust, Tauri 2 and Leptos.

There is no real capture hardware behind this demo. The "cameras" are simulated
RGB-D sensors looking at a procedural animated 3D scene. Everything downstream
of the sensors is a real pipeline: it only ever sees calibrated RGB + depth
images, exactly what a rig of depth cameras (or RGB cameras followed by a
feed-forward depth network) would deliver.

```
 procedural scene (t)          simulated sensors          reconstruction            transport              viewer
┌───────────────────┐   ┌──────────────────────────┐   ┌────────────────────┐   ┌──────────────┐   ┌────────────────────────┐
│ stage + dancer    │──▶│ N pinhole RGB-D cameras  │──▶│ depth → surfel     │──▶│ binary frames│──▶│ WebGL2 splat renderer  │
│ house interior    │   │ (CPU ray caster, noise)  │   │ Gaussians, fusion  │   │ WS / worker  │   │ orbit / fly / WebXR    │
└───────────────────┘   └──────────────────────────┘   └────────────────────┘   └──────────────┘   └────────────────────────┘
        gs-core                  gs-core                      gs-core            gs-server/worker         gs-render + gs-app
```

## Two applications

1. **Live stage** (`SceneKind::Stage`). A ring of N synchronized static cameras
   surrounds a stage with an animated dancer. Every tick all cameras capture
   the same instant, the frames are turned into Gaussians, and the viewer
   replaces the dynamic layer. The empty stage is captured once (background
   plate) and sent as a static layer; per tick only foreground pixels are sent.
2. **Home scan** (`SceneKind::House`). One handheld camera is walked along a
   path through a furnished house. Each RGB-D frame is fused into a voxel hash
   and only newly observed surface is appended, so the house builds up
   progressively. Afterwards you walk through it (fly controls or VR).

## Workspace layout

| Path | Target | Role |
|---|---|---|
| `crates/gs-core` | native + wasm | Math, scenes, sensors, reconstruction, wire codec, sorting, CPU reference rasterizer, PLY/.splat I/O, `Pipeline` |
| `crates/gs-server` | native | Pipeline thread + axum HTTP/HTTPS + WebSocket broadcast. Library and headless `gs-server` binary |
| `crates/gs-render` | wasm | WebGL2 Gaussian splat renderer, camera controls, gizmos, WebXR session |
| `crates/gs-worker` | wasm (Web Worker) | Runs `Pipeline` in the browser when no native server is available |
| `crates/gs-app` | wasm | Leptos CSR user interface, built with Trunk |
| `src-tauri` | native | Tauri 2 desktop shell. Starts `gs-server`, exposes info commands |
| `e2e` | node | Playwright end-to-end checks (desktop render, WS mode, emulated VR) |

## Conventions (binding for every crate)

- Units are meters. World is right-handed, **+Y up**. The floor is `y = 0`.
- Camera space is OpenGL/WebXR style: +X right, +Y up, **-Z forward**.
- Matrices are `glam` column-major. `world_from_cam` maps camera space to world.
- Image pixel `(0,0)` is the **top-left**; pixel centers are at `+0.5`.
  Image `v` grows downward, so camera-space `y = -(v - cy) / fy * z`.
- `depth` is the distance along the camera forward axis (not ray length),
  positive, in meters. `0.0` means invalid (no return).
- Colors are sRGB 8-bit. Blending in the viewer is done directly on sRGB values
  (same as the reference 3DGS implementations).
- Quaternions are stored `[x, y, z, w]`.
- A splat is `pos`, `scale` (standard deviations along its local axes), `rot`,
  `rgba` (opacity in `a`). Covariance is `R · diag(scale²) · Rᵀ`.

## gs-core modules

| Module | Contents |
|---|---|
| `types` | `Camera`, `RgbdFrame`, `Splat`, `Aabb` (shared, stable) |
| `scene` | Analytic primitives (sphere/ellipsoid, box, cylinder, capsule, cone) under affine transforms, procedural materials, ray casting with an acceleration structure, diffuse shading with shadows. `Scene` trait: `snapshot(t)` and `snapshot_static()` |
| `scene::stage` | Round stage, articulated dancer (forward kinematics, procedural dance loop), juggled balls |
| `scene::house` | Furnished single-storey interior with several rooms |
| `rig` | Ring rig generation, handheld scan path (spline through the rooms) |
| `capture` | `render_rgbd(snapshot, camera, opts) -> RgbdFrame`, sensor noise model |
| `reconstruct` | Depth map → oriented surfel Gaussians, background subtraction, voxel-hash fusion |
| `splat` | Covariance math, EWA projection helpers |
| `sort` | Back-to-front counting/radix sort of splat indices |
| `raster` | CPU reference splat rasterizer (tests, golden images, PSNR) |
| `codec` | Binary wire messages (quantized 16-byte splats, previews) |
| `protocol` | serde types: `PipelineConfig`, `SessionState`, `ControlMsg`, `Stats` |
| `pipeline` | `Pipeline`: owns scene, rig, fusion state; `tick(dt) -> Vec<Outgoing>` (one encoded message each, with `reliable` and a latest-wins `slot`), `snapshot()` for late joiners, `handle_control` |
| `io` | Import/export of INRIA 3DGS `.ply` and antimatter15 `.splat` |

### Reconstruction (per valid depth pixel)

1. Unproject to a world position `p`.
2. Estimate the normal `n` from depth differences of the 4-neighbours,
   rejecting neighbours across a depth discontinuity. Orient it to the camera.
3. Pixel footprint at that depth: `r = z / fx`.
4. Emit a flat Gaussian: tangent axes sized `k·r` and `k·r / clamp(cos θ)`
   (so its projection covers the source pixel), thin along `n`.
5. Drop grazing observations and pixels on depth edges (flying pixels).
6. Color is the pixel color, opacity is a config constant.

Live mode: a pixel is foreground when it is closer than the background plate
(or its color differs from the plate, which keeps contact shadows).
Scan mode: positions are hashed into voxels; the first good observation of a
voxel produces a round disc splat (radius `0.6 ×` the voxel size) and closer
views refine it in place. New cells are streamed as `Append` chunks; because
refinement rewrites earlier splats, layer 0 gets one full `Replace` when the
scan ends or is paused. The handheld camera pose comes from
`pipeline::scan_camera` (walk along `ScanPath` plus a look-around sweep).

### Wire format

One self-describing little-endian binary message per WebSocket frame or
`postMessage`. The same bytes travel over both transports.

- `Hello` / `State`: JSON `SessionState` (scene, config, camera poses, bounds).
- `Layer`: `layer_id`, mode `Replace | Append`, AABB, then `count × 16 bytes`
  (`pos` as 3×u16 in the AABB, 3×u8 log-scale, RGBA, 3-byte quaternion).
- `Preview`: one camera's downsampled RGB + depth for the feed thumbnails.
- `Stats`: JSON timings and counts.

Layer 0 is static (background plate, or the accumulated scan), layer 1 is the
live dynamic layer. Clients send JSON `ControlMsg` text frames back
(`set_config`, `patch_config`, `set_scene`, `set_paused`, `set_time_scale`,
`restart`, `scan_start`, `scan_pause`, `scan_reset`, `scan_finish`).

Delivery contract: `State` and everything on layer 0 are *reliable* and
ordered. Layer 0 may receive a full `Replace` at any time. Layer 1, `Stats`
and `Preview` are *latest wins* per slot: a slow client skips stale ones.

## Transport and deployment modes

| Mode | Pipeline runs in | Viewer gets frames by |
|---|---|---|
| Tauri desktop app | native thread (rayon) inside the app | `ws://127.0.0.1:<port>/ws` |
| Headset on the LAN | the same native thread | `wss://<lan-ip>:<port>/ws` (self-signed HTTPS, required by WebXR) |
| Static web build (no server) | `gs-worker` Web Worker (WASM) | `postMessage` with transferables |

`gs-server` serves the web build itself, so the headset needs nothing installed:
open the URL (or scan the QR code shown in the desktop app) and press
"Entrer en VR". The frontend probes `GET /api/info`; if it answers, it uses the
WebSocket, otherwise it starts the local worker.

## Server interface (binding for `gs-server`, `src-tauri`, `gs-app`)

`gs-server` runs one `Pipeline` on its own thread and two listeners:

- **Loopback HTTP**, `127.0.0.1:7878` by default (next free port if busy).
  Always on. Used by the desktop window and by a browser on the same machine.
- **LAN HTTPS**, `0.0.0.0:7879` by default, self-signed certificate (WebXR
  needs a secure context). **Off by default**; switched on at start
  (`--lan`) or at run time through `POST /api/lan` from loopback only.

Routes (identical on both listeners unless noted):

| Route | Answer |
|---|---|
| `GET /api/info` | JSON, see below |
| `POST /api/lan` body `{"enabled": bool}` | Loopback listener only (403 on the LAN one). Starts or stops the LAN listener, answers the new info JSON |
| `GET /api/qr.svg?i=<n>` | SVG QR code of `lan.urls[n]` (default 0). 404 while LAN is off |
| `GET /ws` | WebSocket. Server to client: binary frames, one codec message each. Client to server: text frames, one `ControlMsg` JSON each |
| anything else | The web build (`gs-app` dist), `index.html` fallback, correct MIME types (`application/wasm`) |

```json
{
  "name": "splat-live",
  "version": "0.1.0",
  "protocol": 1,
  "http_url": "http://127.0.0.1:7878",
  "ws_path": "/ws",
  "lan": { "enabled": true, "urls": ["https://192.168.1.20:7879"], "fingerprint": "AB:CD:…" },
  "clients": 1
}
```

`lan.urls` is empty while LAN is off. `fingerprint` is the SHA-256 of the
certificate so the user can compare it with what the headset browser shows.

WebSocket behaviour: on connect the client receives `Pipeline::snapshot()`
taken between two ticks, then the live stream. Reliable messages are queued
per client in order; latest-wins messages overwrite their slot. A client whose
reliable queue grows past a bound is resynchronised with a fresh snapshot
instead of being fed an ever-growing backlog. A control message from any
client applies to the shared pipeline; resulting reliable messages go to all
clients. The pipeline does not tick while no client is connected.

Hardening: the WebSocket and `POST` routes check the `Origin` header (same
host, or the Tauri origins `tauri://localhost` / `http://tauri.localhost`);
CORS on the loopback listener allows only those Tauri origins; control text
frames are capped at `pipeline::MAX_CONTROL_JSON`.

The Tauri shell starts the server in-process and exposes one command,
`server_info`, returning the same info JSON. The frontend resolves its
source in this order: Tauri command (then uses `http_url` as base), else
`GET /api/info` on its own origin, else the local `gs-worker`.

## Viewer

- Splat data lives in a GPU data texture; the sorted order lives in a second
  (`R32UI`) texture and quads are indexed triangles that derive splat and
  corner from `gl_VertexID`. The vertex shader projects the 3D covariance to a screen-space ellipse
  (EWA), the fragment shader evaluates the Gaussian, blending is back-to-front
  premultiplied alpha.
- Sorting is a 16-bit counting sort on view depth, throttled to camera motion.
- Desktop: orbit + zoom (stage), fly WASD + mouse look (house).
- WebXR `immersive-vr`, `local-floor`: one sort per frame, one draw per eye.
  Left stick moves, right stick snap-turns, a button toggles a 1:1 /
  miniature ("dollhouse") world scale.

## Verification

- `cargo test -p gs-core`: unit tests plus a novel-view quality test. The
  reconstructed cloud is rendered by the CPU rasterizer from a camera that is
  *not* part of the rig and compared (PSNR) with the ground-truth ray cast.
- `e2e/`: Playwright drives the real web build in headless Chromium: desktop
  render is non-blank and animated, WebSocket mode against `gs-server`, and a
  VR session through the IWER WebXR emulator.
