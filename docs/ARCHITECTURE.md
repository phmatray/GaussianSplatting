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
| `ios/SplatCapture` | iOS (Swift) | Companion capture app: streams or records LiDAR RGB-D frames with ARKit poses. Built with Xcode on a Mac, not part of the Cargo workspace |
| `tools` | python | Reference implementation of the capture file format, test fixtures, a simulated phone |

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
| `capture` | `render_rgbd(snapshot, camera, opts) -> RgbdFrame`, sensor noise model. `StaticRender` / `PartialCapture`: an exact partial capture that only recasts the pixels a moving part can change (bit-identical to a full render; about 6 % of the stage's pixels per tick) |
| `reconstruct` | Depth map → oriented surfel Gaussians, background subtraction, voxel-hash fusion |
| `multiview` | Multi-view fusion of the ring cameras: every surface point keeps the splat of the camera that sees it best |
| `ingest` | Real capture: frames from the phone companion, `.gscap` files, capture sessions |
| `record` | 4D recording and replay: the `.gsrec` format, writer, reader, player |
| `experimental` | Measurement lab: metrics (PSNR by region, SSIM), `wblend` (CPU reference of the blending modes), prototypes |
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

Live mode, multi-view fusion (`gs_core::multiview`, `PipelineConfig.fusion`,
on by default): where several ring cameras see the same surface point, only
the splat of the camera that sees it best is kept (closest and most frontal,
with a margin so that near-ties keep both). Two splats are the same surface
when one lies on the other's tangent plane within a tolerance and their
normals agree; the rim pixels of a camera's silhouette never replace
anything, and a splat that only a near-rim pixel beats is kept with a quarter
of its opacity (a low blending weight) so that thin limbs stay closed. The
fused cloud has about half the splats of the stacked one, which pays for
finer sensors: the default ring is 8 cameras of 240x180
(`PipelineConfig::RIG_RESOLUTIONS` lists 160x120, 240x180, 320x240).

Live mode: a pixel is foreground when it is closer than the background plate
(or its color differs from the plate, which keeps contact shadows).
Scan mode: positions are hashed into voxels; the first good observation of a
voxel produces a round disc splat (standard deviation `scan.kernel`, 0.45,
times the voxel size; cells are 2.4 cm by default) and closer
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

- Splat data lives in a GPU data texture; quads are indexed triangles that
  derive splat and corner from `gl_VertexID`. The vertex shader projects the
  3D covariance to a screen-space ellipse (EWA), the fragment shader
  evaluates the Gaussian.
- Sorted mode: the sorted order lives in a second (`R32UI`) texture; blending
  is back-to-front premultiplied alpha. Sorting is a 16-bit counting sort on
  view depth, throttled to camera motion.
- Normalised mode (see "Blending modes"): no sort. Each layer is drawn in
  storage order in three (hard) or four (soft) passes into off-screen
  targets: one or two depth textures, half-float accumulation targets and a
  two-channel coverage target, then a resolve to the canvas or to each eye.
  Depth is written logarithmically on the splat's plane. Gizmo lines are
  hidden where they lie behind the resolved surface.
- `RenderParams`: `blend` (`Auto`, `Sorted`, `Normalized`), `quality`
  (`Hard`, `Soft`), `cull_backfaces`, `layer_weights`, `sorted_surfel_scale`.
  `Auto` draws pipeline layers (`LayerContent::Surfels`) normalised and
  imported files (`LayerContent::Imported`) sorted; `Viewer::blend_status()`
  reports the active mode and, when it fell back, why.
- Desktop: orbit + zoom (stage), fly WASD + mouse look (house).
- WebXR `immersive-vr`, `local-floor`: at most one sort per frame, one draw
  per eye.
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

## Real capture: the phone companion (binding for `ios/`, `gs-core::ingest`, `gs-server`)

An iPhone or iPad with a LiDAR scanner delivers, through ARKit, exactly what
the pipeline consumes: a colour image, a depth map and the camera pose for
every frame. The companion app `ios/SplatCapture` (Swift, SwiftUI, ARKit)
streams those frames to `gs-server`, or records them to a file when no server
is in reach. The server fuses them with the same voxel fusion as the simulated
house scan; viewers see an ordinary session (`State`, layer 0 `Append` /
`Replace`, `Stats`, `Preview`).

ARKit's conventions equal ours and need no conversion: the world is
right-handed with +Y up (gravity), the camera looks along -Z with +X right
and +Y up, pixel `(0,0)` is the top-left of the image in the sensor's native
(landscape) orientation, and the depth map holds the distance along the
camera axis in meters, aligned with the colour image. Only the origin
differs: ARKit's is where the session started, so the server shifts the world
vertically once the floor height is known (`floor` message, or its own
estimate) so that the floor is `y = 0`.

**Capture modes.** `room` (walk through rooms; range up to 5 m), `object`
and `person` (walk around a subject that holds still; fusion is cropped to a
region of interest with finer cells), `live` (no fusion: every frame replaces
layer 1 with the surfels of that one view, a relief seen from the phone's
side; a full live volume needs several devices and is out of scope).

**Pairing.** `GET /api/pair` (loopback listener only, LAN must be on) answers
`{"uri": "splatlive://pair?host=<ip>&port=<lan port>&fp=<sha256>&token=<t>"}`
and `GET /api/pair.svg` the QR code of that URI. `fp` is the SHA-256 of the
server certificate (DER) as 64 lowercase hex digits; the app accepts exactly
that certificate and no other (the certificate is self-signed, so the system
trust store cannot help). `token` is 32 lowercase hex digits, random per
server start; `/ingest` refuses a `hello` without it.

**Stream.** `GET /ingest` on the LAN HTTPS listener, a WebSocket.

- Client text frames (JSON): first `{"type":"hello","protocol":1,
  "token":"…","device":"iPhone17,2","app":"0.1.0","mode":"room",
  "name":"Salon"}`; then, at any time, `{"type":"region","center":[x,y,z],
  "radius":r}` (world meters; `object` and `person`), `{"type":"floor",
  "y":h}` (world height of the floor ARKit detected), `{"type":"pause"}`,
  `{"type":"resume"}`, `{"type":"reset"}`, `{"type":"finish"}`.
- Server text frames (JSON): `{"type":"welcome","protocol":1,
  "max_inflight":4}` or `{"type":"error","code":"…","message":"…"}` followed
  by a close; `{"type":"ack","frame":n,"used":bool}` for every frame;
  `{"type":"progress","frames":n,"splats":n}` about once a second.
- Client binary frames: one capture frame each, little-endian:

  | Offset | Size | Field |
  |---|---|---|
  | 0 | 4 | magic `GSCF` |
  | 4 | 1 | version, `1` |
  | 5 | 1 | flags: bit 0 confidence present, bit 1 depth, confidence and mask are raw-deflate compressed (RFC 1951), bit 2 mask present |
  | 6 | 1 | tracking: 0 not available, 1 limited, 2 normal |
  | 7 | 1 | display orientation of the device (hint for previews only): 0 sensor native (landscape), 1 portrait, 2 landscape flipped, 3 portrait upside down |
  | 8 | 4 | frame index `u32`, increasing |
  | 12 | 4 | reserved, 0 |
  | 16 | 8 | timestamp `f64`, seconds (ARKit frame time) |
  | 24 | 64 | `world_from_cam`, 16 × `f32`, column-major |
  | 88 | 16 | `fx, fy, cx, cy` as `f32`, in pixels of the colour image as sent |
  | 104 | 4 | colour width, height as `u16` |
  | 108 | 4 | colour byte length `u32` (a JPEG) |
  | 112 | 4 | depth width, height as `u16` |
  | 116 | 4 | depth byte length `u32` |
  | 120 | 4 | confidence byte length `u32` (0 if absent) |
  | 124 | 4 | mask byte length `u32` (0 if absent) |
  | 128 | … | colour, then depth, then confidence, then mask |

  Depth is `u16` millimeters, row-major from the top-left, `0` = no value.
  Confidence is one byte per depth pixel (0 low, 1 medium, 2 high, as
  ARKit's). The mask is one byte per depth pixel (0 background, 255 person;
  ARKit's person segmentation) and is optional in every mode.
- Flow control: the app never has more than `max_inflight` frames
  unacknowledged; it skips frames instead of queueing them. In the fusing
  modes it sends a frame when the camera moved 5 cm or turned 5 degrees since
  the last one sent, or after 0.5 s, at most 10 per second; in `live` mode up
  to 15 per second.

**Capture file `.gscap`.** What the app records offline, and what the
simulator can write for tests: 8 bytes `GSCAP1\0\0`, a `u32` length and that
many bytes of JSON (the `hello` fields plus `created`, RFC 3339, and
optionally `region` and `floor`), then for every frame a `u32` length and the
frame exactly as on the wire. `POST /api/captures` (body: the file) imports
one into the running server; the desktop app offers it as a file to open.
On the LAN listener that request must carry the pairing token
(`Authorization: Bearer <token>`); on the loopback listener the usual
`Origin` check applies.

**Details fixed by the reference implementation.** `tools/gscap.py` is the
reference for the frame and the file, and the fixtures under
`ios/SplatCapture/Fixtures` (a symbolic link into the Swift test target;
`Fixtures/README.md` describes every file) are shared by the Python, Swift
and Rust tests. Where this section is silent, they decide:

- Deflate: depth, confidence and mask are three independent raw-deflate
  streams; header lengths are the stored sizes, inflated sizes follow from
  the depth size (2 bytes per pixel for depth, 1 for the others). The JPEG is
  never deflated. The phone may fall back to an uncompressed frame, so a
  reader accepts both. Inflation stops one byte past the expected size.
- Validation order and error codes: `too_short`, `bad_magic`, `bad_version`,
  `bad_flags` (bits 3 to 7 set), `bad_tracking`, `bad_orientation`,
  `bad_dimensions` (depth below 1x1 or above 2^22 pixels, checked before any
  allocation), `bad_length` (flag and length agreement, plane sizes, total
  size = 128 + the four lengths added without wrapping), `bad_deflate`. The
  reserved field is ignored on read. A colour length of 0 is allowed.
- Depth conversion on the phone: `floor(meters * 1000 + 0.5)`; values that
  are not positive, not finite or above 65535 mm become 0.
- Intrinsics refer to the colour image as sent; a reader scales them by depth
  size over colour size to unproject the depth map.
- The frame index starts at 0 at app launch, only grows, is never reset
  (pause, reset, reconnection, new capture) and may have gaps. Acks are
  matched by index. The timestamp is seconds since device boot: only
  differences are meaningful.
- `.gscap`: lengths are little-endian; the JSON header holds `protocol,
  device, app, mode, name, created` and optionally `region`
  (`{"center":[x,y,z],"radius":r}`) and `floor` (`{"y":h}` or a bare number),
  never the token; it may be padded with spaces inside its declared length
  (the phone rewrites it in place) and readers ignore unknown keys. A header
  above 1 MiB, not UTF-8 or not a JSON object is `bad_header`. A file that
  ends inside a record or inside a record length is a recording cut short:
  readers keep the complete frames. A record below 128 bytes or above 64 MiB
  is `bad_length`. Nothing is allocated from an announced length before the
  bytes are known to be there. JSON with `NaN`, `Infinity` or pathological
  nesting is invalid.
- Pairing URI: the exact parsing rules are in the docstring of
  `parse_pairing_uri` and in `pairing_uris.json`.
- Stream behaviour the server must honour: `welcome` within 8 s of the
  WebSocket opening; nothing but `hello` arrives before it; pings answered
  with pongs (the phone pings every 5 s); no `Origin` check on `/ingest` nor
  on the token-authenticated `POST /api/captures` (a native client sends no
  `Origin`); frame intervals down to 96 ms accepted; error codes `bad_hello`,
  `bad_protocol`, `bad_token`, `bad_message`, `bad_frame`, `flow_control`;
  `finish` is answered with the outstanding acks and a last `progress`;
  after `reset` the scan starts again from nothing (the phone drops older
  frames and resets its world origin); on reconnection the phone sends the
  same `hello`, then `region` and `floor` again, and the server continues the
  scan if the `name` and `mode` match the current capture session. In the
  fusing modes frames with tracking 0 never arrive and frames with tracking 1
  are acknowledged with `used: false`; in `live` mode every frame is used.
  `POST /api/captures` answers `{"ok":true,"frames":n,"truncated":bool,
  "name":"…"}`, 400 for a damaged file, 401 for a missing or wrong token.

## 4D recording and sharing (binding for `gs-core::record`, `gs-server`, `gs-app`)

**Recording file `.gsrec`.** A recording is the stream a viewer receives,
with time stamps, so it replays in any viewer without the pipeline:

- 8 bytes `GSREC1\0\0`, a `u32` length and that many bytes of JSON
  (`name`, `created`, `scene`, `protocol`, `duration_s`, 0 while unknown).
- Records: `u32` length of what follows, `u8` kind, `u8` flags (bit 0:
  payload is raw-deflate compressed), `u16` reserved, `f64` time in seconds
  since the start, payload.
  - kind 0, *message*: one wire message exactly as sent to viewers.
  - kind 1, *keyframe*: a complete snapshot (`State`, layer 0 `Replace`,
    layer 1 `Replace`), each message prefixed by its `u32` length. Playback
    can start at any keyframe. The first record is a keyframe.
  - kind 2, *index*: `(f64 time, u64 file offset)` of every keyframe.
- The file ends with the index record, the `u64` offset of that record and 8
  bytes `GSRECEND`. A file cut short (no index) still plays by reading it
  from the start.

A still scan is a recording with one keyframe. Seeking goes to the nearest
earlier keyframe and replays the messages up to the wanted time. Replay is
done by the viewer side (`gs-app`, in the worker), so every viewer has its
own time line: play, pause, speed, loop, scrub; in VR as well.

**Server.** `POST /api/record` `{"action":"start","name":"…"}` /
`{"action":"stop"}` (loopback only) records the live stream to the recordings
directory; `GET /api/recordings` lists them (name, size, duration, scene);
`GET /recordings/<name>.gsrec` serves one, with `Range` support; `DELETE`
of the same path removes it (loopback only). Without a server the app
records in memory, up to a bound, and offers the file as a download.

**Sharing.** The web build is also the shareable viewer. URL parameters:
`file=<url>` (`.gsrec`, `.ply` or `.splat`; relative or absolute),
`embed=1` (canvas only, with small playback controls, full screen and VR
buttons: for an `<iframe>`), `autoplay`, `loop`, `t=<seconds>`,
`view=<eye x,y,z,target x,y,z>`, `nav=orbit|fly`. The desktop app shares a
recording in two ways: a link (and QR code) on the local network,
`https://<lan address>/?file=recordings/<name>.gsrec&embed=1`; and
`GET /api/share/<name>.zip`, the web build with the recording inside and a
`share.json` that names it, to unpack onto any static web host.

## Verification

- `cargo test -p gs-core`: unit tests plus novel-view quality tests
  (`tests/novel_view.rs`). The reconstructed cloud is rendered at 640x480 by
  the CPU reference in both blending modes from cameras that are *not* part
  of the rig and compared with the ground-truth ray cast (PSNR by region,
  SSIM, coverage, a temporal flicker measure, holes opened by the fusion).
- `cargo run --release -p gs-core --example quality_report`: dumps clouds,
  scores them at 1280x960 and renders before / after sheets.
- `e2e/`: Playwright drives the real web build in headless Chromium: desktop
  render is non-blank and animated, WebSocket mode against `gs-server`, and a
  VR session through the IWER WebXR emulator.
