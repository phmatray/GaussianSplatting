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
2. **Home scan** (`SceneKind::House`). A handheld head of five rigidly mounted
   RGB-D sensors (ahead, 120 degrees to either side, tilted 60 degrees up and
   down) is carried along a path through a furnished house. Each RGB-D frame
   is fused into a voxel hash
   and only newly observed surface is appended, so the house builds up
   progressively. Afterwards you walk through it (fly controls or VR).

## Workspace layout

| Path | Target | Role |
|---|---|---|
| `crates/gs-core` | native + wasm | Math, scenes, sensors, reconstruction, wire codec, sorting, CPU reference rasterizer, PLY/.splat I/O, `Pipeline` |
| `crates/gs-server` | native | Pipeline thread + axum HTTP/HTTPS + WebSocket broadcast. Library and headless `gs-server` binary |
| `crates/gs-render` | wasm | WebGL2 Gaussian splat renderer, camera controls, gizmos, WebXR session |
| `crates/gs-worker` | wasm (Web Worker) | Runs `Pipeline` in the browser when no native server is available; in server mode it relays the WebSocket to the page |
| `crates/gs-app` | wasm | Leptos CSR user interface, built with Trunk |
| `src-tauri` | native | Tauri 2 desktop shell. Starts `gs-server`, exposes info commands. Workspace member outside `default-members` (needs WebKitGTK; root `cargo build` / `cargo test` skip it) |
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
| `rig` | Ring rig generation, handheld scan path (spline through the rooms, walking speed limited so the heading turns slowly) |
| `capture` | `render_rgbd(snapshot, camera, opts) -> RgbdFrame`, sensor noise model |
| `reconstruct` | Depth map → oriented surfel Gaussians, background subtraction, voxel-hash fusion |
| `splat` | Covariance math, EWA projection helpers |
| `sort` | Back-to-front counting/radix sort of splat indices |
| `raster` | CPU reference splat rasterizer (tests, golden images, PSNR) |
| `codec` | Binary wire messages (quantized 16-byte splats, previews) |
| `protocol` | serde types: `PipelineConfig`, `SessionState`, `ControlMsg`, `Stats`, `ScanHead` |
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
scan ends or is paused. The pose of every sensor comes from
`pipeline::scan_sensor` (`scan_camera` is sensor 0): the head (`scan.head`,
default `surround`, five 192x144 sensors with an 80 degree vertical field of
view) rides along `ScanPath` and faces the walking direction averaged over one
second, with a sway of 10 degrees. The sensors fire one at a time at 6 frames
per second in the order 0,1,0,2,0,3,0,4. The walk takes about 67 s; the head
turns at 20 degrees per second on average and 37 at most.

### Wire format

One self-describing little-endian binary message per WebSocket frame or
`postMessage`. The same bytes travel over both transports.

- `Hello` / `State`: JSON `SessionState` (scene, config, camera poses, bounds).
- `Layer`: `layer_id`, mode `Replace | Append`, AABB, then `count × 16 bytes`
  (`pos` as 3×u16 in the AABB, 3×u8 log-scale, RGBA, 3-byte quaternion).
- `Preview`: one camera's downsampled RGB + depth for the feed thumbnails.
  In the house the camera id is the sensor id (0 = forward).
- `Stats`: JSON timings and counts. In the house `scan.camera` is the forward
  sensor and `scan.extra_cameras` lists the others.

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

In server mode the page does not open the WebSocket itself: the worker opens
it in *relay* mode, coalesces messages by the delivery contract (a layer 0
`Replace` drops earlier layer 0 messages; layer 1, `Stats` and each `Preview`
are latest wins) and hands acknowledged batches to the page, at most two
outstanding. A page that renders slowly therefore skips ahead instead of
falling seconds behind. If the worker cannot load, the page falls back to a
socket on the main thread.

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
frames are capped at `pipeline::MAX_CONTROL_JSON`. A connection that sends no
complete request within 15 s is closed (`POST /api/lan` answers 408). While
LAN is on, `lan.urls` and `lan.fingerprint` follow the machine's addresses
(checked every 5 s; the certificate is renewed if it does not name the new
address). The LAN listener has no authentication: anyone on the network can
watch and send control messages, which is why it is off by default.

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

## Blending modes (binding for `gs-core` and `gs-render`)

Sensor surfels describe opaque surfaces, so stacking them as translucent
discs in sort order blurs textures and lets the splats behind a surface leak
through it. Measurements (`gs_core::experimental`, `examples/quality_lab.rs`)
led to two modes that every renderer of this project implements, the CPU
reference and the WebGL2 viewer alike:

- **Sorted**: back-to-front premultiplied alpha, as in the 3DGS reference
  implementations. Used for imported `.ply` / `.splat` scenes, which are
  translucent by nature, and as the fallback when the GPU cannot render to a
  half-float target.
- **Normalised** (default for the live and scan layers): order independent,
  no CPU sort.
  1. *Visibility*: every splat is drawn as an opaque disc (visibility radius
     in standard deviations) into a depth buffer; the fragment depth is taken
     on the splat's plane, not at its center.
  2. *Accumulation*: every fragment within a two-sided depth tolerance of the
     visible surface adds `w·colour` and `w`, with
     `w = layer weight · opacity · exp(−m/2)`.
  3. *Resolve*: `colour = Σ w·colour / Σ w`. Coverage is accumulated without
     the layer weights. Silhouettes are either hard (opaque inside the
     visibility discs) or soft (a second visibility pass finds the next
     surface; first surface over second surface over background).

  In this mode the opacity byte of a wire splat is a blending weight. The
  live layer (layer 1) carries a higher layer weight than the static plate so
  that the shadows it captures win over the plate underneath.

Both modes use the same normalised screen-space low-pass filter (a small
dilation of the projected covariance with the amplitude scaled so that the
splat keeps its energy), which keeps distant and miniature views from
turning to mush.

`gs_core::experimental::wblend` is the CPU reference for the normalised mode
and for the filter; the viewer is compared against it pixel by pixel in
`e2e/render`. The splat cloud is produced for the normalised mode; a viewer
in sorted mode compensates with its splat scale.

## Verification

- `cargo test -p gs-core`: unit tests plus a novel-view quality test. The
  reconstructed cloud is rendered by the CPU rasterizer from a camera that is
  *not* part of the rig and compared (PSNR) with the ground-truth ray cast.
- `e2e/`: Playwright drives the real web build in headless Chromium: desktop
  render is non-blank and animated, WebSocket mode against `gs-server`, and a
  VR session through the IWER WebXR emulator.
