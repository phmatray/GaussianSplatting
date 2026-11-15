# Splat Live: architecture

Live Gaussian splatting from a ring of cameras around a subject, viewable on a
desktop and in a VR headset. Built with Rust, Tauri 2 and Leptos.

There is no real capture hardware behind the two demos. The "cameras" are
simulated RGB-D sensors looking at a procedural animated 3D scene. Everything
downstream of the sensors is a real pipeline: it only ever sees calibrated
RGB + depth images, exactly what a rig of depth cameras (or RGB cameras
followed by a feed-forward depth network) would deliver.

The same pipeline takes real frames from an iPhone with a LiDAR scanner
("Real capture"), records what viewers receive and replays it anywhere ("4D
recording and sharing"), and a kept capture can be optimised offline against
its photographs ("Offline refinement"). None of it has met real hardware
yet: no macOS, no Xcode build, no iPhone, no GPU, no headset. Every number in
this document was measured on simulated data.

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
| `crates/gs-server` | native | Pipeline thread + axum HTTP/HTTPS + WebSocket broadcast; phone ingest, recorder, files on disk, share archives, the offline job slot. Library and headless `gs-server` binary |
| `crates/gs-refine` | native | Offline mode: CPU Gaussian-splatting trainer on a `.gscap` (differentiable rasterizer, loss, Adam, densification), dataset export (COLMAP, nerfstudio). Library and `gs-refine` command-line tool |
| `crates/gs-render` | wasm | WebGL2 Gaussian splat renderer, camera controls, gizmos, WebXR session |
| `crates/gs-worker` | wasm (Web Worker) | Runs `Pipeline` in the browser when no native server is available; in server mode it relays the WebSocket to the page. Also replays recordings, records in memory and ingests a `.gscap` without a server |
| `crates/gs-render-harness` | wasm | Trunk test page for `gs-render` (GPU against CPU reference, live layer, WebXR, performance), driven by `e2e/render` |
| `crates/gs-app` | wasm | Leptos CSR user interface, built with Trunk |
| `src-tauri` | native | Tauri 2 desktop shell. Starts `gs-server`, exposes three commands (`server_info`, `export_recording`, `reveal`). Workspace member outside `default-members` (needs WebKitGTK; root `cargo build` / `cargo test` skip it) |
| `e2e/render`, `e2e/app`, `e2e/server` | node, python | End-to-end checks: the renderer against its CPU reference, the application in headless Chromium (static build and with `gs-server`, emulated VR), the server binary with the reference tools |
| `ios/SplatCapture` | iOS (Swift) | Companion capture app: streams or records LiDAR RGB-D frames with ARKit poses. Built with Xcode on a Mac, not part of the Cargo workspace |
| `tools` | python | Reference implementation of the capture and recording file formats (`gscap.py`, `gsrec.py`), test fixtures, a simulated phone and a reference ingest server |

The workspace declares `rust-version = "1.87"` and every crate inherits it.

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
| `GET /api/pair[?i=n]`, `GET /api/pair.svg[?i=n]` | Loopback only. The pairing URI of LAN address `n` (`{"uri", "uris"}`) and its QR code. 404 `lan_off`, 404 `no_address` |
| `GET /ingest` | LAN listener only (403 `lan_only` on loopback). The phone's WebSocket, see "Real capture" |
| `POST /api/captures` | Body: a `.gscap`. Loopback: `Origin` check; LAN: `Authorization: Bearer <token>`. Imports it and shows it |
| `GET /api/captures`, `POST /api/captures/open` `{"file"}`, `GET`/`DELETE /captures/<file>.gscap` | Loopback only. The kept raw captures: list, ingest again, download (`Range`), delete |
| `POST /api/record` `{"action":"start"|"stop"}` | Loopback only. The record button |
| `GET /api/record` | Recorder state `{"recording", "last"}`; on loopback also `storage` (`free_bytes`, `low`, `problem`) |
| `GET /api/recordings`, `GET`/`HEAD /recordings/<name>.gsrec` | The recordings: list (newest first; `dir` on loopback only) and files with `Range` |
| `DELETE /recordings/<name>.gsrec` | Loopback only |
| `GET`/`HEAD /api/share/<name>.zip` | Share archive of a recording |
| `POST /api/refine` `{"action":"start"|"export"|"cancel"}`, `GET /api/refine` | Loopback only. The offline job slot, see "Offline refinement" |
| `GET /api/refined`, `GET`/`HEAD /refined/<name>.ply`, `GET`/`HEAD /api/share/refined/<name>.zip` | The refined results: list, files with `Range`, share archive |
| `DELETE /refined/<name>.ply` | Loopback only. Removes the result and its description |
| anything else | The web build (`gs-app` dist), `index.html` fallback, correct MIME types (`application/wasm`). Paths under `/api/`, `/recordings/`, `/captures/` and `/refined/` that match nothing answer 404; `GET /share.json` answers 204 when the web build has none |

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
address, link-local addresses `169.254/16` excepted: they come and go with
a USB cable and no device on the Wi-Fi uses them). The LAN listener has no authentication: anyone on the network can
watch and send control messages, which is why it is off by default.

The Tauri shell starts the server in-process, keeps its files under the
application data directory (`recordings/`, `captures/`, `refined/`,
`datasets/`, the certificate) and exposes three commands: `server_info`
(the same info JSON); `export_recording {name, kind}` with `kind` one of
`recording`, `share`, `refined`, `refined_share`, which writes the file into
the user's download folder without overwriting anything and returns its path
(a web view has no download manager; the application calls it instead of
following a download link); `reveal {target}` for `recordings`, `captures`,
`refined`, `datasets`, `exports` or a path `export_recording` returned. The
frontend resolves its source in this order: Tauri command (then uses
`http_url` as base), else `GET /api/info` on its own origin, else the local
`gs-worker`.

### Details fixed by `gs-server`

- **Errors.** Every route answers an error as JSON `{"error": "<English
  sentence>", "code": "<stable code>"}` with `Cache-Control: no-store`; an
  interface shows its own text for the code. Codes: `loopback_only`,
  `lan_only`, `origin_not_allowed`, `host_not_allowed`, `lan_off`,
  `no_address`, `bad_request`, `timeout`, `already_recording`,
  `not_recording`, `no_recordings_dir`, `not_found`, `in_use`, `busy`,
  `import_in_progress`, `too_large`, `disk_full` (507), `bad_token`,
  `no_frontend`, `unavailable`, `internal`, the format codes of a refused
  capture file, and those of the offline job (`job_running`, `no_job`,
  `unsupported_mode`, `no_refined_dir`, `disk_error`).
- **Loopback listener.** A request whose `Host` is present and is not
  `127.0.0.1`, `localhost` or `[::1]` (with or without port), or that carries
  two `Host` headers, is answered 403 `host_not_allowed` (DNS rebinding).
  CORS allows the Tauri origins and `extra_origins`: methods GET, HEAD, POST,
  DELETE; request headers `Content-Type`, `Range`, `If-Range`,
  `If-None-Match`; exposed `Content-Range`, `Content-Length`,
  `Accept-Ranges`, `ETag`, `Content-Disposition`.
- **LAN listener.** At most `max_lan_connections` (128) connections; one
  beyond is closed at accept, before any TLS work. Read-only routes of the
  recordings, of the refined results and of their share archives are served
  there; nothing there changes the server except `/ingest` and the
  token-authenticated `POST /api/captures`.
- **Capture and simulation.** A capture replaces the simulation for every
  viewer and the pipeline is parked. Control messages of viewers are ignored
  while a capture is shown, except `set_scene`, which leaves the capture and
  returns every viewer to the simulation; `set_scene` is ignored too while
  the phone is connected and has not finished. A page must therefore never
  send `set_scene` by itself at connect.
- **One phone at a time.** A second phone gets the error `busy` (close
  1008). A new `hello` takes over the attached connection when it is the
  same hello (device, app, name, mode), when that connection has been
  silent for 12 s, or when its capture is finished. Continuing a capture
  needs the same name, mode and device; an imported capture is never
  continued. No `hello` within 10 s, or a binary frame before it, is
  `bad_hello`; at most 8 connections may be waiting for their hello (503
  beyond); one frame beyond `max_inflight` is `flow_control`; a message
  above 8 MiB closes with 1009; the server pings the phone and closes one
  that is silent for `idle_timeout`. `progress` goes out every second while
  a phone is attached and not finished.
- **Live over a scan.** A `live` capture that follows a finished scan of the
  same device plays in front of that scan; the scan is dropped if the first
  live frame's index is not above the scan's last (the app was relaunched).
- **Uploads.** `POST /api/captures` is streamed to a hidden `.part` file.
  Its first 8 bytes are checked on arrival (400 `bad_file_magic`); the limit
  is `max_upload_bytes` (16 GiB, 413); an announced length that does not fit
  on the disk (told once the magic bytes have passed), or a write the disk
  refuses, is 507 `disk_full`; a stalled
  body is 408 at `request_timeout`; 409 `busy` while a phone captures, 409
  `import_in_progress` (one import at a time). An early refusal over HTTP/1
  first discards up to 64 MiB of the body for at most 10 s so that the
  client receives the answer. The answer is `{"ok", "frames",
  "frames_used", "truncated", "name", "file", "recording", "shown"}`.
- **Files of a capture.** The raw stream is kept as
  `<slug(name)>-<YYYYMMDD-HHMMSS UTC>.gscap` in `captures` (next to the
  recordings directory), `-2`, `-3` when taken; its header is rewritten in
  place with the region and floor the phone sent; a reset replaces the file;
  a capture without a frame leaves none. A finished capture is saved as
  `<same stem>.gsrec`: a still (one keyframe) for the fusing modes, the
  whole stream for `live` (an imported live capture on its frames' own time
  stamps, gaps cut to 2 s). The files of one capture share their stem,
  number suffix included. A scan whose phone never sent `finish` is finished
  and saved when its capture is closed (`set_scene`, another capture taking
  its place, shutdown). The raw file of a phone's capture stays the
  server's (`DELETE` answers 409 `in_use`) for as long as that capture is on
  show, because the phone may take it up again. `capturing` in
  `GET /api/captures` is true only while the file grows: once the phone has
  said `finish` everything received is on disk, the capture is listed with
  `capturing: false` and can be reopened, refined and exported there and
  then (409 `in_use` again if the phone resumes).
- **Recorder.** The file is a slug of the given name (letters and digits of
  any script, `-`, `_`), `enregistrement-<timestamp>` without one; titles are
  cut to 120 characters; the recorder counts as a spectator, so the scene
  ticks for it alone; a recording ends by itself, complete, at
  `max_recording_bytes` (8 GiB) with reason `full`; `saved.reason` is
  `stopped`, `full`, `disk_error` or `too_large`. No fsync on the pipeline
  thread.
- **Names.** A file name accepted by a route is one path segment with the
  right extension, no leading dot or space, no trailing dot or space, none
  of `/ \ : * ? " < > |`, no control character, not a Windows device name, at
  most 200 bytes, and a regular file directly in its directory (symbolic
  links are not followed).
- **Downloads.** Single byte ranges only (several ranges, another unit or a
  malformed header get the whole file; beyond the end 416 with
  `Content-Range: bytes */len`); a strong `ETag` of length and modification
  time; `If-Range` accepts an ETag only; `?download` adds
  `Content-Disposition: attachment`; never compressed. A download whose
  client takes nothing for `send_timeout` (30 s) is abandoned.
- **Share archive.** The web build at the root (deflated), `share.json`,
  and the shared file stored under a fixed name: `recording.gsrec`, or
  `scene.ply` for a refined result. ZIP64 only when needed; the whole
  archive has a `Content-Length` (also on `HEAD`). `share.json` holds
  `file`, `name`, `scene`, `created`, `size`, plus `autoplay`, `loop` and
  `duration_s` for a recording, and `nav` / `view` for a refined room. 409
  while a recording is written, 503 `no_frontend` without a web build.
- **Disk.** `GET /api/record` on loopback carries `storage`: free bytes
  where captures are written, `low` under 2 GiB, and the latest `problem`
  (`serial`, `code` `capture_not_saved` or `recording_not_saved`, `file`)
  when the disk refused a capture or a recording. The capture itself goes on.
  `GET /api/captures` carries `free_bytes` as well. A refinement is not
  started with less than 256 MiB free, a dataset export with less than the
  size of the capture plus that.
- **Log.** The debug access log replaces the pairing token with `<token>`.

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
  reports the active mode and, when it fell back, why. The status can change
  from frame to frame, so a UI reads it regularly rather than once.
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
turning to mush. Imported files are the exception: they were
trained with the plain 3DGS dilation and keep it (`classic_dilation`).

Only the ratio of the two layer weights matters; the viewer rescales them so
that the half-float targets cannot overflow. When the off-screen targets
cannot be allocated the viewer falls back to sorted blending and tries the
normalised mode again when the views change size, a VR session starts or
ends, or the context is restored.

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

- The app (`ios/SplatCapture`, never compiled): every capture name gets its
  start time appended (`CaptureEngine.stampedName`), so that two captures
  never share a name and the server never continues the wrong scan; after
  `finish` the app waits up to 6 s for the server's last `progress` before
  it closes. `tools/fake_phone.py` waits 8 s.

### Details fixed by `gs_core::ingest`

The library has no sockets, files or clock and compiles for wasm32; the
server owns those. It has only ever seen synthetic data.

- **What viewers see.** A capture looks like a home scan: `SessionState.scene`
  is `"capture"`, `config.scene` is house, `cameras` is empty; the phone is
  `Stats.scan.camera` and `Preview` camera 0, its path is
  `SessionState.scan.path` (at most 1024 points). `SessionState.capture` and
  `Stats.capture` carry mode, name, device, app, region, `floor_known`,
  `imported`, `frames_received`, `frames_used`, `paused`, `finished`, `full`,
  `tracking`.
- **Coordinates.** Fusion works in ARKit's world; everything sent to viewers
  is shifted so that the floor is `y = 0`. The floor is, in this order: the
  phone's `floor` message or the `.gscap` header; an estimate from the data
  (lowest upward-facing level of at least 0.25 m2); a guess (first camera
  height minus 1.3 m). A change of 1 cm or more re-sends `State` and layer 0.
  In live mode the estimate only moves down and is not made over a backdrop.
- **Modes.** room: 2.4 cm cells, 0.25 to 5 m; object: 8 mm, 0.15 to 2.5 m;
  person: 1 cm, 0.25 to 4 m; live: 0.25 to 5 m. The cap is 1 000 000 cells:
  beyond it new surface is dropped, existing cells are still refined and
  `Stats.capture.full` is true (the UI must say so).
- **Region (object, person).** Until the phone sends one: a sphere of 0.6 m
  (object) or 1.2 m (person) around the point the first frame looks at.
  Frames are fused within 1.5 times the radius; viewers get what is inside it.
- **Person mode.** With a mask plane only pixels of 128 or more are fused;
  once a mask has been seen, frames without one are not used.
- **Depth filter.** Depth 0, out of range, or of confidence below
  `min_confidence` (1) is dropped. The depth-step filter runs only for frames
  without a confidence plane.
- **Fusion policy for the phone.** The same `VoxelFusion` as the simulated
  scan with two real-sensor settings on: a cell averages up to 24 frames
  (`blend_frames`), and a frame's colours are scaled per channel to agree
  with the map where they overlap (`exposure_compensation`).
- **Layer merge.** At progress ticks and at finish, same-facing cells closer
  than `layer_window` (3) cells in a column are merged into one, which removes
  the doubled surfaces that depth noise creates. Layer 0 then goes out as a
  `Replace`. Finish also drops lone single-frame cells.
- **Acks.** `used` is false when the capture is paused or finished, tracking
  is not normal (fusing modes), pose or intrinsics are not a rigid transform
  or a camera, the colour does not decode, no depth survives, person mode
  sees nobody, or the depth map exceeds 2^20 pixels. Live mode uses frames
  of any tracking state.
- **Restart rule.** A frame whose index is lower than one already taken means
  the app was relaunched: the session restarts (State, two empty layer
  Replaces, Stats, then the frame). A reconnection with higher indices and
  the same name and mode continues the scan.
- **Colour.** The JPEG is decoded at the smallest DCT scale that still covers
  the depth grid on both axes, then area-averaged onto it. JPEGs above 2^23
  pixels, in CMYK, or with more than 64 scans make the frame unused.
- **Limits.** Text messages above 16 KiB are refused; WebSocket messages are
  capped at 8 MiB; `max_inflight` is 4; names are cut to 120 characters.
  Error texts sent to the phone are French; the close code is 1007 for
  `bad_frame` and 1008 otherwise.
- **Timing.** A frame costs 7 to 18 ms (median, 4-core VM); a progress tick
  near the cap up to 160 ms and finish about 1 s, so the server runs all
  three off the async executor. `CaptureSession::snapshot()` goes to the
  joining viewer only.
- **Import.** The `.gscap` header must name a mode and, if present, protocol
  1. A damaged frame record fails the import; a file cut inside a record
  keeps its complete frames.

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
`https://<lan address>/?file=recordings/<name>.gsrec&embed=1&autoplay=1&loop=1`;
and `GET /api/share/<name>.zip`, the web build with the recording inside and
a `share.json` that names it, to unpack onto any static web host. A refined
result is shared the same way: `?file=refined/<name>.ply&embed=1` (plus
`view` and `nav=fly` for a room) and `GET /api/share/refined/<name>.zip`.

### Details fixed by `gs_core::record`

- **Keyframes** hold `State`, layer 0 `Replace`, layer 1 `Replace`, then the
  latest `Stats` and the latest `Preview` of every camera when recorded.
  They are pure redundancy: forward playback skips them; a seek starts at the
  last keyframe not after the target and applies the records up to it. A
  layer built from `Append` chunks is merged into one `Replace`.
- **Keyframe policy.** One is due once the message bytes since the last one
  reach 8 times the size of a keyframe, and not before 1 s: about every
  1.2 s on the default stage (8 % of the file), none during a scan.
- **Compression.** Raw deflate level 1 per record for payloads of 64 bytes or
  more, kept only when it saves at least a ninth. Default stage: 5.8 MB/s;
  a complete house scan: 17.3 MB.
- **Header.** JSON padded with spaces to 1012 bytes (first record at offset
  1024) and rewritten in place at the end with `duration_s`. Record times are
  rebased so that the first is 0 and never decrease.
- **Framing limits.** A record is at most 128 MiB, stored or inflated; the
  writer refuses (`too_large`) a message or snapshot above that. Flag bits 1
  to 7 are 0. Validation order: `bad_length`, `bad_kind`, `bad_flags`,
  `bad_time`, `bad_deflate`, then `bad_message`, `bad_keyframe` or
  `bad_index`.
- **Damage.** Damage to the framing ends the readable part of the file; a
  record with a bad payload is reported and skipped; a file without index or
  trailer is a recording cut short and plays to its last complete record.
- **Index.** A player holding the whole file ignores it. A `Range` player
  uses it as a hint only: the span fetched at an index offset must begin with
  a keyframe of exactly that time, otherwise the entry is dropped. A wrong
  index costs a detour, never a wrong picture. A seek before the first known
  keyframe starts at the first record; a record later than the index's
  duration extends the duration.
- **Players.** `from_bytes`, `streaming` (a download in progress) and `ranged`
  (HTTP `Range`, bounded cache). The first two hold the whole file (about
  350 MB per minute of the default stage), so long stage recordings need
  `ranged`. One `Update` hands out at most 512 MiB. Hosts ask `wanted()`
  after every `update`, `supply`, `seek` and `advance`, and forget the
  statistics and thumbnails they show when `Update.reset` is set.
- **In-memory recording** (no server) writes to a `ChunkedSink` with a size
  bound; the recorder answers `Full` early enough that the bounded file is
  always complete and valid.
- **Second implementation.** `tools/gsrec.py` (`info`, `verify`, `synth`)
  reads and writes the same bytes.

### Details fixed by `gs-app` and `gs-worker`

- **Replay** runs in a second instance of the worker. Meanwhile the live
  source waits: the local pipeline is put on hold, the connection to a
  server is closed and opened again afterwards. A page opened on `file=`
  starts no live source until the user leaves the file.
- **Sources of a replay.** A `Blob` up to 256 MiB is read whole, a larger
  one through `Player::ranged` over `Blob.slice`. A URL is probed with
  `Range: bytes=0-4095`: 206 with a total above 64 MiB means Range playback
  in 4 MiB requests; otherwise one GET is streamed and plays while it
  downloads (refused above 1 GiB; a host without Range costs about three
  times the file in memory).
- **Link parameters.** `file` is a relative reference or an absolute
  http(s) URL without credentials, at most 2048 bytes, ending in `.gsrec`,
  `.ply` or `.splat`; any other scheme is refused before any fetch. Flags
  accept empty, `1`, `true`, `yes`, `on` and `0`, `false`, `no`, `off`;
  `autoplay` and `loop` default to off. A refused value is dropped and told
  in a French notice; the others still apply. `share.json` next to
  `index.html` (`file`, `embed`, `autoplay`, `loop`, `t`, `view`, `nav`; 16
  KiB at most) gets no more trust than a link, and link parameters override
  it one by one. It is fetched only without `file=`, `source=` and Tauri.
- **Embed mode** has no top bar, panel, HUD or gizmos and no way back to a
  live source.
- **Imported files** (`.ply`, `.splat`) are framed from outside, in orbit.
  One opened with `nav=fly` and no `view` starts inside its bounds, at eye
  height (the link of a refined room).
- **Recording target.** The server's recorder when the page talks to the
  server over loopback; otherwise the memory of the worker behind the
  transport, bounded at 256 MiB, handed over as a `Blob`. A finished scan is
  saved on the server by a start followed by a stop; the interface calls any
  recording shorter than 0.25 s a still. Deleting a recording, a capture or
  a refined result asks once more (two clicks).
- **A capture in the page.** A capture session is shown in orbit navigation,
  from outside what was filmed: a room from high above (the viewer draws
  walls and ceiling from their inner side only), an object or a person from
  a three-quarter view; never from the session's own start view, which is
  where the phone stood. The camera follows the growing bounds until the
  user moves it. The pause button is hidden during a capture and the list of
  recordings is read again when a capture finishes or is left.
- **Capture files.** Before a `.gscap` is uploaded to the server the page
  checks its 8 magic bytes. Without a server the worker ingests the file
  itself, one frame at a time; it reads the header before it takes over, so
  a file that is not a capture leaves the screen untouched.
- **Bounds on what a stream may build.** A `State` is cut to 64 cameras,
  4096 path points and names of 200 characters; thumbnails above one
  megapixel are not painted; a layer may not grow beyond 4 000 000 splats.
- **Worker protocol additions** (page to worker): `hold`, `record_start`,
  `record_stop`, `record_still`, `replay_open`, `replay_play`,
  `replay_pause`, `replay_seek`, `replay_speed`, `replay_loop`,
  `replay_close`, `capture_cancel`, and `[json, Blob]` for `replay_file` and
  `capture_file`. Worker to page: `replay` (status), `replay_reset`,
  `record`, `capture`, and `["recording", meta, ArrayBuffer...]`.
- **Rendering choice.** Auto is normalised soft on a screen and hard inside
  a VR session; it is the only setting stored (`localStorage`,
  `splat-live.blend`).
- **VR time line.** Left Y plays and pauses; with the left grip held the
  left stick scrubs.
- **Desktop shell.** Inside Tauri every download goes through
  `export_recording`, then `reveal`.
- **Disk.** The page tells in French when the server's disk is nearly full
  (`storage.low`) and each time it refused a capture or a recording
  (`storage.problem`), and when an upload is refused for lack of room.

## Offline refinement (binding for `gs-refine`, `gs-server`, `gs-app`)

The real-time path never checks its picture against the photographs.
`crates/gs-refine` takes a kept capture (`.gscap`: photographs, LiDAR depth,
confidence, poses) and optimises 3D Gaussians against those photographs,
starting from the cloud the real-time fusion makes of the same file. The
result is an INRIA 3DGS `.ply` that the viewer draws like any imported file
(sorted blending). It runs on the CPU only (rayon); `raster::Rasterizer` is
the seam for a GPU implementation, which does not exist.

**What it optimises.** Per Gaussian 14 numbers: position, three log standard
deviations, a quaternion, an opacity logit, a colour (view independent,
spherical harmonics of degree 0).

- *Views.* Frames without normal tracking, blurred frames and near-repeats
  are dropped; one frame in 8 is held out for evaluation and never fused.
- *Start.* The other frames go through `gs_core::ingest::CaptureSession` as
  an import does; the fused surfels become Gaussians 1.4 times larger with
  0.7 of their opacity.
- *Loss.* 0.8 L1 + 0.2 (1 - SSIM) against the photograph through one gain
  per channel and image fitted in closed form; a depth term against the
  LiDAR weighted by its confidence (0.2); in `object` and `person` captures
  a coverage term outside the region (0.5).
- *Optimiser.* Adam on the visible Gaussians; the first 30 % of the run at
  half width; every 200 steps between 5 % and 80 % of the run, Gaussians
  with a large screen-space gradient are split along their longest axis or
  cloned, and those nearly transparent, larger than 0.5 m or not finite are
  pruned. Pose refinement exists and is off.
- *`photo_beyond_sensor`* (on by default): where the sensor measured nothing
  (glass, mirrors, beyond 5 m) the photographs are fitted all the same,
  which paints such places at a guessed depth; off, they stay holes.
- A run is bit-identical for any number of threads and across a checkpoint.

**Presets.** The number of iterations is `passes x training views`, with a
minimum.

| Preset | Width | Views at most | Passes | Iterations at least | Room cells | Gaussians at most |
|---|---|---|---|---|---|---|
| `rapide` | 480 | 300 | 6 | 1 500 | 2.4 cm | 1 000 000 |
| `standard` | 640 | 600 | 15 | 4 000 | 1.6 cm | 2 000 000 |
| `fin` | 960 | 1 200 | 30 | 12 000 | 1.6 cm | 4 000 000 |

**Command line** (`gs-refine`, French messages; unknown options are errors).
`gs-refine <capture.gscap> -o <out.ply> [--preset rapide|standard|fin]
[--iterations n] [--mesure-seule] [--poses] [--splat] [--threads n]
[--set path=value]` writes `<out>.ply`, `<out>.json` (the report) and, while
unfinished, `<out>.reprise` (checkpoint, magic `GSRFCK2`); Ctrl-C stops
after the current iteration and writes the best state; `--reprise <file>`
goes on. `gs-refine info <capture>` describes a capture. `gs-refine export
<capture> --format colmap|nerfstudio -o <dir>` writes a dataset.

**Datasets.** Both formats get the photographs as recorded, one pinhole
camera per image, the poses in the capture's world with the floor at
`y = 0`, and the fused cloud as initial points. COLMAP: `images/`,
`sparse/0/{cameras,images,points3D}.{txt,bin}`, world-to-camera with +Z
forward and +Y down. nerfstudio: `transforms.json` (camera-to-world, OpenGL
axes), `images/`, `depth/*.png` (16-bit millimeters), `masks/*.png` (person
captures), `sparse_pc.ply`; to keep the capture's world, train with
`--orientation-method none --center-method none --auto-scale-poses False`.

**Server.** One offline job at a time, on a thread of its own inside the
server process (not a child process: the desktop application is one
binary), with `refine_threads` worker threads (default: every processor but
one, which stays with the pipeline and the listeners).

- `POST /api/refine` (loopback, `Origin` check):
  `{"action":"start","capture":"<file>.gscap","preset":"rapide|standard|fin",
  "beyond_sensor":bool}` (preset `standard` and `beyond_sensor` true when
  absent), `{"action":"export","capture":"…","format":"colmap|nerfstudio"}`,
  `{"action":"cancel"}`. 200 `{"job": status}`; 404 `not_found`; 409
  `job_running` (the body carries the job in the way); 409 `in_use` (the
  capture is still being written); 400 `unsupported_mode` (a `live` capture)
  or `bad_request`; 503 `no_refined_dir`; 507 `disk_full`; 409 `no_job`.
- `GET /api/refine` (loopback): `{"job": status|null, "last": end|null}`.
  `status`: `id`, `kind` (`refine` | `export`), `capture`, `title`, `preset`,
  `format`, `stage` (`prepare`, `select`, `fuse`, `exposure`, `poses`,
  `evaluate`, `optimise`, `write`, `export`), `done`, `total`, `iteration`,
  `iterations`, `elapsed_s`, `eta_s`, `loss`, `holdout_psnr`, `gaussians`,
  `width`, `threads`, `cancelling`, `started`. `end`: the identity of the
  job, `outcome` (`done`, `cancelled`, `failed`), `error` (`code`:
  `bad_capture`, `no_views`, `disk_full`, `disk_error`, `internal`),
  `result` (an entry of the list below), `dataset` (`name`, `dir`, `format`,
  `images`, `points`, `depth_maps`, `masks`), `iterations_done`,
  `iterations`, `elapsed_s`.
- A refinement writes `<capture stem>-<preset>.ply` (`-2`, `-3` when taken)
  into the directory `refined` next to the recordings, and beside it a
  `.json` with `capture`, `title`, `mode`, `preset`, `created`, `gaussians`,
  `iterations`, `elapsed_s`, `beyond_sensor`, `view` and, under `report`,
  the optimiser's own report. The result is the best state on the held-out
  views. `view` is where a refined room is first looked at from: the phone's
  first training position, looking toward the bulk of the cloud.
- **A cancelled refinement keeps nothing.** A run stopped in its first
  hundreds of iterations is worse than the real-time fusion; a stop takes a
  few seconds (the token is read at every iteration and every sixteen frames
  of the preparation). A server that shuts down cancels its job.
- A dataset export writes the directory `<capture stem>-<format>` (`-2`,
  `-3` when taken) under `datasets`, next to the recordings; the interface
  shows where it is. It is a directory on the server's machine, not a
  download: datasets are made to be read by another program there.
- `GET /api/refined` (both listeners): `{"results": [{"name", "file",
  "size", "url", "writing"} + the fields of the description], "dir",
  "datasets_dir"}` (the directories on loopback only). `url` is
  `refined/<file>`, usable as the `file` parameter of the viewer.
- `ServerConfig::refine_iterations` (`--refine-iterations N`) forces the
  number of iterations of every job: for tests and demonstrations.

**Application.** With a server reached over loopback the panel has a section
"Captures et affinage": the kept captures (`GET /api/captures`; reopen one
with `POST /api/captures/open`, delete), and for each "Affiner cette
capture" with the preset, the `beyond_sensor` choice ("Peindre d'après les
photos" / "Laisser les trous") and the dataset exports. The job is polled
every second (stage, iteration, elapsed and remaining time, "Annuler").
"Voir le résultat" downloads the `.ply` and shows it as an imported file; a
room from its `view`, in fly navigation. A result is downloaded, shared
(LAN link, `<iframe>`, archive) and deleted like a recording. A LAN client
lists and views the results only. Without a server the section holds one
sentence saying why the feature is absent.

**What the interface must not claim.** The held-out PSNR is measured on
photographs that are neighbours of the training photographs: it is not a
free-viewpoint quality measure and is never shown as the quality of a
result. Every text about the refinement says that it was measured on
simulated captures only and that it runs on the CPU.

**What was measured** (simulated captures only, exact ray-cast ground truth
from viewpoints the phone never took, 640x480):

- Bedroom, 220 frames, `rapide`: PSNR where the phone has looked 25.11 to
  26.43 dB, SSIM 0.834 to 0.875, silhouettes 19.6 to 24.3 dB; at the
  phone's own poses 29.9 to 37.3 dB. Living room on another path: 23.32 to
  27.38 dB, SSIM 0.797 to 0.877. An object: 31.32 to 33.51 dB.
- Far from the phone's path, where the fused cloud is already closed, the
  gain is small or nil (`standard` on the bedroom: 29.40 to 29.43 dB); flat
  interiors lose 1 to 3 dB to a faint mottle; holes the phone never saw stay
  holes. Under simulated pose drift and exposure wander with another seed
  the far-view gain is 0.13 dB while the held-out photographs report +3.6.
- Time, through the server on the 4-core development VM with three threads:
  108 s for `rapide` on that bedroom (278 241 Gaussians, 19 MB), 587 s for
  `standard` (551 093 Gaussians, 37 MB; the machine was also compiling).
  `fin` was never run to its end. Memory stayed under 0.9 GB.

## Verification

- `cargo test -p gs-core`: unit tests plus novel-view quality tests
  (`tests/novel_view.rs`). The reconstructed cloud is rendered at 640x480 by
  the CPU reference in both blending modes from cameras that are *not* part
  of the rig and compared with the ground-truth ray cast (PSNR by region,
  SSIM, coverage, a temporal flicker measure, holes opened by the fusion).
- `cargo run --release -p gs-core --example quality_report`: dumps clouds,
  scores them at 1280x960 and renders before / after sheets.
- `cargo test --no-fail-fast` at the root runs every crate but the desktop
  shell (`cargo test -p splat-live` needs WebKitGTK or a Mac). The capture
  path is tested with a simulated phone (`crates/gs-core/tests/ingest_sim`),
  including frames of the size a real phone sends (1920x1440, a few hundred
  kilobytes), generated at test time.
- `python3 -m unittest discover -s tools`: the reference tools of the two
  file formats, the fixtures shared with the Swift and Rust tests, the
  simulated phone against the reference server.
- `e2e/render`: the WebGL2 viewer against the CPU reference, pixel by pixel.
- `e2e/app`: Playwright drives the real web build in headless Chromium
  (software rasterizer): both demos, files, replay, embed, capture,
  refinement, a VR session through the IWER WebXR emulator, and the suites
  that need the `gs-server` binary.
- `e2e/server`: the server binary with the reference tools and a browser
  (capture, hostile clients, sharing, refinement, the Tauri shell under
  Xvfb).
- Nothing above runs on real hardware: see the first section.
