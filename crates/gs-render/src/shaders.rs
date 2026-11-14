//! GLSL ES 3.00 sources.
//!
//! The splat shaders implement, step by step, the reference algorithms of
//! `gs_core`: the projection and the sorted mode of `gs_core::splat` (the
//! numbers in the comments are the steps of that recipe), and the normalised
//! mode of `gs_core::experimental::wblend`. Keep them in sync: the
//! end-to-end tests compare these shaders with the CPU rasterizers pixel by
//! pixel.
//!
//! # Programs
//!
//! One vertex shader source serves every splat program; `#define SORTED`
//! selects how a vertex finds its splat.
//!
//! | Program | Vertex | Fragment | Output |
//! |---|---|---|---|
//! | sorted | `SORTED` | [`SORTED_FS`] | premultiplied colour, blended "over" |
//! | visibility | plain | [`VISIBILITY_FS`] | depth only |
//! | second visibility | plain | [`VISIBILITY_FS`] + `SECOND` | depth only |
//! | accumulation, hard | plain | [`ACCUMULATE_FS`] | `(Σ w·c, Σ w)` |
//! | accumulation, soft | plain | [`ACCUMULATE_FS`] + `SOFT` | the same for two surfaces, and their coverage |
//! | resolve | [`RESOLVE_VS`] | [`RESOLVE_FS`] (+ `SOFT`) | premultiplied colour, blended "over" |
//!
//! # Fragments of the normalised mode
//!
//! The mode makes yes/no decisions per fragment (inside a visibility disc
//! or not, on the visible surface or behind it), so its fragment shaders do
//! not interpolate anything over the quad: a rasterizer snaps vertices to a
//! sub-pixel grid (1/16 px in SwiftShader), which would move every disc
//! edge by that much. The vertex shader passes the center of the splat in
//! pixels and the two rows that map a pixel offset to the quad coordinate
//! (the same numbers for all four vertices), and the fragment shader
//! evaluates them at `gl_FragCoord`, exactly like the CPU reference does at
//! its pixel centers. The quad itself only has to cover the ellipse.
//!
//! The fragment depth is the depth where the pixel ray meets the plane of
//! the surfel: with `n` the plane normal and `p` the center in camera space,
//! `n·d` of the pixel ray `d` is linear in the pixel offset, and
//! `z = (n·p) / (n·d)` is one division. A world-space quad would give the
//! same depth for free, but not in the encoding below, it would need
//! clipping at the near plane, and sub-pixel splats would fall between the
//! pixel centers; planar footprints measured no better than EWA ones either
//! (±0.1 dB), so there is one footprint, EWA, in both modes.
//!
//! Depths are written **logarithmically**: `e = log2(z / near) / 21`. The
//! depth test of the mode is relative (a fragment belongs to the visible
//! surface when its depth is within 1 % of it), which in this encoding is a
//! constant offset; and 24 bits resolve `z` to 9·10⁻⁷ of itself at any
//! distance, four orders of magnitude finer than the tolerance, whether the
//! view is a house at 1:1 or its miniature 20 cm from the eye. 21 octaves
//! from a 5 cm near plane reach 100 km; the projection's far plane plays no
//! role. `1.0` is the cleared value and means "no surface here".

/// Octaves of depth the encoding spans, from the near plane on.
pub(crate) const DEPTH_OCTAVES: f32 = 21.0;

/// The splat vertex shader. Four vertices per splat, drawn as indexed
/// triangles: vertex `4·q + k` is corner `k` of the `q`-th quad.
///
/// Deliberately not instanced. Instancing is the obvious way to draw one
/// quad per splat and costs the same on a hardware GPU, but software
/// rasterizers (SwiftShader, which headless Chromium and therefore every
/// automated check of this project runs on) execute an instanced draw as one
/// draw call per instance: measured 25× slower than this formulation.
///
/// With `SORTED`, quad `q` shows the splat the order texture names, from
/// either layer. Without, one layer is drawn per call and quad `q` is its
/// splat `q`: no order is needed, and the shader also emits the plane of
/// the surfel for the fragment depth.
const SPLAT_VS_BODY: &str = r#"
precision highp float;
precision highp int;
precision highp usampler2D;

#ifdef SORTED
uniform usampler2D u_data0;   // layer 0, three RGBA32UI texels per splat
uniform usampler2D u_data1;   // layer 1
uniform usampler2D u_order;   // sorted splat indices, one R32UI texel per quad
uniform int u_order_width;    // texels per row of u_order
uniform uint u_split;         // indices >= u_split address layer 1
uniform uvec2 u_counts;       // splats currently stored in each layer
uniform vec2 u_layer_scale;   // extra factor on the splat scale, per layer
uniform vec2 u_layer_classic; // per layer, 1: plain dilation (imported files)
#else
uniform usampler2D u_data;    // the layer being drawn
uniform float u_extent;       // half-size of the quad in standard deviations
uniform float u_cull_back;    // 1: skip surfels that face away from the eye
uniform vec3 u_eye;           // eye position, world space
uniform float u_view_scale;   // view units per world unit
#endif
uniform int u_spr;            // splats per data texture row
uniform mat4 u_view;          // world -> camera, may contain a uniform scale
uniform vec2 u_focal;         // fx, fy in pixels
uniform vec2 u_center;        // cx, cy in pixels, top-left origin
uniform vec2 u_viewport;      // W, H in pixels
uniform float u_near;
uniform float u_splat_scale;  // k
uniform float u_max_radius;   // clamp of the 3-sigma half-axes, pixels
uniform float u_point_radius; // > 0: point mode, 3-sigma radius of every dot in pixels
uniform float u_clip_y;       // splats centered above this world height are hidden

// Constant over the quad. Deliberately not `flat`: flat varyings force a slow
// provoking-vertex emulation on ANGLE/Direct3D.
out vec4 v_color;
#ifdef SORTED
out vec2 v_corner;
#else
// All constant over the quad: the fragment shader evaluates the splat at
// gl_FragCoord (see the module documentation).
out vec2 v_center;            // center in pixels, top-left origin
out vec4 v_axes;              // rows of the pixel offset -> quad coordinate map
out vec4 v_plane;             // n.d = x + dot(yz, offset); n.p in w
out vec3 v_depth;             // nearest and farthest depth a fragment may take, depth of the center
#endif

// Outside the clip volume: all four vertices of a culled splat go here.
const vec4 CULLED = vec4(0.0, 0.0, 2.0, 1.0);

#ifndef SORTED
// Inverse of `pack::encode_normal` (octahedral, 16 bits per coordinate).
vec3 decode_normal(uint word) {
  vec2 f = vec2(float(word & 65535u), float(word >> 16)) / 65535.0 * 2.0 - 1.0;
  vec3 n = vec3(f, 1.0 - abs(f.x) - abs(f.y));
  float t = max(-n.z, 0.0);
  n.x += n.x >= 0.0 ? -t : t;
  n.y += n.y >= 0.0 ? -t : t;
  return normalize(n);
}
#endif

void main() {
  int quad = gl_VertexID >> 2;
  int k = gl_VertexID & 3;
  vec2 corner = vec2((k & 1) != 0 ? 1.0 : -1.0, (k & 2) != 0 ? 1.0 : -1.0);   // in {-1, +1}^2
  uvec4 t0, t1, t2;
#ifdef SORTED
  uint index = texelFetch(u_order, ivec2(quad % u_order_width, quad / u_order_width), 0).x;
  bool second = index >= u_split;
  uint local = second ? index - u_split : index;
  // The order can be one frame older than the data.
  if (local >= (second ? u_counts.y : u_counts.x)) { gl_Position = CULLED; return; }
  ivec2 at = ivec2((int(local) % u_spr) * 3, int(local) / u_spr);
  if (second) {
    t0 = texelFetch(u_data1, at, 0);
    t1 = texelFetch(u_data1, at + ivec2(1, 0), 0);
    t2 = texelFetch(u_data1, at + ivec2(2, 0), 0);
  } else {
    t0 = texelFetch(u_data0, at, 0);
    t1 = texelFetch(u_data0, at + ivec2(1, 0), 0);
    t2 = texelFetch(u_data0, at + ivec2(2, 0), 0);
  }
  float scale = u_splat_scale * (second ? u_layer_scale.y : u_layer_scale.x);
  const float extent = 3.0;
#else
  ivec2 at = ivec2((quad % u_spr) * 3, quad / u_spr);
  t0 = texelFetch(u_data, at, 0);
  t1 = texelFetch(u_data, at + ivec2(1, 0), 0);
  t2 = texelFetch(u_data, at + ivec2(2, 0), 0);
  float scale = u_splat_scale;
  float extent = u_extent;
#endif
  vec3 p = uintBitsToFloat(t0.xyz);
  vec4 color = vec4(float(t0.w & 255u), float((t0.w >> 8) & 255u), float((t0.w >> 16) & 255u), float(t0.w >> 24)) / 255.0;
  if (p.y > u_clip_y || color.a < 1.0 / 255.0) { gl_Position = CULLED; return; }

  // 1. camera-space center and depth
  vec4 t = u_view * vec4(p, 1.0);
  float d = -t.z;
  if (!(d >= u_near)) { gl_Position = CULLED; return; }
#ifndef SORTED
  vec3 normal = decode_normal(t2.z);
  if (u_cull_back > 0.5 && dot(normal, u_eye - p) < 0.0) { gl_Position = CULLED; return; }
#endif

  // 2. center in pixels (y down)
  vec2 uv = t.xy / d;
  vec2 px = vec2(u_center.x + u_focal.x * uv.x, u_center.y - u_focal.y * uv.y);

  vec2 major, minor;
  float gain = 1.0;
  if (u_point_radius > 0.0) {
    major = vec2(u_point_radius * extent / 3.0, 0.0);
    minor = vec2(0.0, u_point_radius * extent / 3.0);
  } else {
    // 3. clamp the Jacobian direction to 1.3x the frustum
    float W = u_viewport.x, H = u_viewport.y;
    uv = clamp(uv,
               vec2((-u_center.x - 0.15 * W) / u_focal.x, (u_center.y - 1.15 * H) / u_focal.y),
               vec2((W - u_center.x + 0.15 * W) / u_focal.x, (u_center.y + 0.15 * H) / u_focal.y));
    // 4./5. T = J * mat3(view), one row per screen axis
    mat3 Wt = transpose(mat3(u_view));
    vec3 T0 = (u_focal.x / d) * (Wt[0] + uv.x * Wt[2]);
    vec3 T1 = -(u_focal.y / d) * (Wt[1] + uv.y * Wt[2]);
    vec4 ca = uintBitsToFloat(t1);
    vec2 cb = uintBitsToFloat(t2.xy);
    mat3 S = mat3(ca.x, ca.y, ca.z,  ca.y, ca.w, cb.x,  ca.z, cb.x, cb.y);
    float k2 = scale * scale;
    vec3 S0 = S * T0;
    vec3 S1 = S * T1;
    float ma = max(k2 * dot(T0, S0), 0.0);
    float mb = k2 * dot(T0, S1);
    float mc = max(k2 * dot(T1, S1), 0.0);
    float a = ma + 0.3, b = mb, c = mc + 0.3;
    // 6. determinant that cannot cancel below the low-pass, and the gain
    //    that makes the low-pass conserve the energy of the splat
    float det_m = max(ma * mc - mb * mb, 0.0);
    float det = det_m + 0.3 * (ma + mc) + 0.09;
    gain = sqrt(det_m / det);
    // 7. eigen-decomposition
    float hd = 0.5 * (a - c);
    float rad = length(vec2(hd, b));
    float l1 = 0.5 * (a + c) + rad;
    float l2 = det / l1;
    vec2 v1 = a >= c ? vec2(hd + rad, b) : vec2(b, rad - hd);
    float len = length(v1);
    v1 = len > 1e-12 ? v1 / len : vec2(1.0, 0.0);
    // Not finite (NaN compares false): cull.
    if (!(l1 > 0.0 && l1 < 1e30)) { gl_Position = CULLED; return; }
    // The clamp shrinks a huge splat as a whole: the same factor whatever
    // part of it this pass draws.
    float s1 = sqrt(l1), s2 = sqrt(l2);
    major = extent * s1 * min(1.0, u_max_radius / (3.0 * s1)) * v1;
    minor = extent * s2 * min(1.0, u_max_radius / (3.0 * s2)) * vec2(-v1.y, v1.x);
  }
#ifdef SORTED
  color.a *= mix(gain, 1.0, second ? u_layer_classic.y : u_layer_classic.x);
  // Too faint to change an 8-bit pixel anywhere.
  if (color.a < 1.0 / 255.0) { gl_Position = CULLED; return; }
#else
  // Not culled however faint: its disc still takes part in the visibility.
  color.a *= gain;
#endif

  // Entirely off-screen: skip the rasterizer.
  float reach = length(major);
  if (any(greaterThan(abs(px - 0.5 * u_viewport), 0.5 * u_viewport + reach))) { gl_Position = CULLED; return; }

  // 8. quad around the center; pixels have y down, NDC has y up
  vec2 ndc = vec2(2.0 * px.x / u_viewport.x - 1.0, 1.0 - 2.0 * px.y / u_viewport.y);
  v_color = color;
#ifdef SORTED
  vec2 offset = corner.x * major + corner.y * minor;
  v_corner = corner;
#else
  // Half a pixel wider than the ellipse: the fragment shader decides what
  // is inside, the quad must merely not cut it wherever its edges snap to.
  vec2 offset = corner.x * major * (1.0 + 0.5 / length(major)) + corner.y * minor * (1.0 + 0.5 / length(minor));
  v_center = px;
  v_axes = vec4(major / dot(major, major), minor / dot(minor, minor));
  // The plane of the surfel in camera space. A dot of the point mode and a
  // splat without a plane face the camera: n = +z gives n.p / n.d = d.
  float sigma = uintBitsToFloat(t2.w);
  vec3 n = vec3(0.0, 0.0, 1.0);
  if (u_point_radius <= 0.0 && sigma != 0.0) n = normalize(mat3(u_view) * normal);
  float np = dot(n, t.xyz);
  // The pixel ray is d(offset) = p / d + (offset.x / fx, -offset.y / fy, 0).
  v_plane = vec4(np / d, n.x / u_focal.x, -n.y / u_focal.y, np);
  // A fragment never leaves the reach of its splat (3 sigma, in view units).
  float r = u_point_radius > 0.0 ? 0.0 : 3.0 * scale * sigma * u_view_scale;
  v_depth = vec3(max(d - r, u_near), d + r, d);
#endif
  gl_Position = vec4(ndc + offset * vec2(2.0, -2.0) / u_viewport, 0.0, 1.0);
}
"#;

/// The vertex shader of the sorted program.
pub(crate) fn sorted_vs() -> String {
    format!("#version 300 es\n#define SORTED\n{SPLAT_VS_BODY}")
}

/// The vertex shader of the visibility and accumulation programs.
pub(crate) fn surfel_vs() -> String {
    format!("#version 300 es\n{SPLAT_VS_BODY}")
}

/// Sorted mode, fragment stage: steps 9-11, premultiplied output.
pub(crate) const SORTED_FS: &str = r#"#version 300 es
precision highp float;

in vec2 v_corner;
in vec4 v_color;
out vec4 o_color;

void main() {
  float r2 = dot(v_corner, v_corner);
  if (r2 > 1.0) discard;                 // outside 3 sigma
  float alpha = v_color.a * exp(-4.5 * r2);
  if (alpha < 1.0 / 255.0) discard;
  o_color = vec4(v_color.rgb * alpha, alpha);   // blendFunc(ONE, ONE_MINUS_SRC_ALPHA)
}
"#;

/// Shared by the fragment shaders of the normalised mode: where the fragment
/// is on its splat, its depth on the plane of the surfel, the encoding of
/// that depth, and the two-sided test against a visibility depth.
const SURFACE_GLSL: &str = r#"
precision highp float;
precision highp sampler2D;

in vec2 v_center;
in vec4 v_axes;
in vec4 v_plane;
in vec3 v_depth;

uniform vec2 u_viewport;      // W, H of the target in pixels
uniform float u_near;
uniform float u_extent;       // half-size of the quad in standard deviations
uniform vec2 u_tolerance;     // how far behind / in front of a surface a fragment may be, encoded

// Offset of this pixel center from the center of the splat, pixels, y down.
// The targets of the normalised mode have their viewport at the origin.
vec2 pixel_offset() {
  return vec2(gl_FragCoord.x, u_viewport.y - gl_FragCoord.y) - v_center;
}

// Squared length of the quad coordinate: 1 on the contour the quad was
// sized for (u_extent standard deviations).
float quad_radius2(vec2 offset) {
  vec2 q = vec2(dot(v_axes.xy, offset), dot(v_axes.zw, offset));
  return dot(q, q);
}

// Depth where the pixel ray meets the plane of the surfel, kept inside the
// reach of the splat; the depth of the center when the ray misses the plane.
float fragment_depth(vec2 offset) {
  float t = v_plane.w / (v_plane.x + dot(v_plane.yz, offset));
  // False for NaN, for infinity and for intersections behind the eye.
  return (t >= u_near && t < 1.0e30) ? min(max(t, v_depth.x), v_depth.y) : v_depth.z;
}

// Logarithmic encoding: see the module documentation. Never 1.0, which
// means "nothing here".
float encode_depth(float z) {
  return clamp(log2(z / u_near) * (1.0 / DEPTH_OCTAVES), 0.0, 0.99999);
}

// Is a fragment at encoded depth `e` part of the surface whose nearest
// visibility fragment is at `surface`? Everything is, where no visibility
// disc reaches (1.0).
bool on_surface(float e, float surface) {
  return surface >= 1.0 || (e <= surface + u_tolerance.x && e >= surface - u_tolerance.y);
}
"#;

/// Normalised mode, visibility pass: every splat is an opaque disc of
/// `u_extent` standard deviations; the nearest fragment wins
/// (`depthFunc(LESS)`, colour writes off). With `SECOND` the fragments of
/// the first surface are skipped, which leaves the nearest surface behind it.
const VISIBILITY_FS_BODY: &str = r#"
#ifdef SECOND
uniform sampler2D u_first;    // depth of the first surface
#endif

void main() {
  vec2 offset = pixel_offset();
  // Written so that NaN is discarded too.
  if (!(quad_radius2(offset) <= 1.0)) discard;
  float e = encode_depth(fragment_depth(offset));
#ifdef SECOND
  if (on_surface(e, texelFetch(u_first, ivec2(gl_FragCoord.xy), 0).r)) discard;
#endif
  gl_FragDepth = e;
}
"#;

/// Normalised mode, accumulation pass: `blendFunc(ONE, ONE)` into half-float
/// targets, no depth attachment (the two-sided test samples the visibility
/// depth instead). The weight of a fragment is
/// `layer weight · opacity · gain · kernel`, with the kernel tapered so that
/// it reaches zero at the cutoff. With `SOFT` a fragment goes to the target
/// of the surface it lies on, and the coverage of each surface, which must
/// not see the layer weight, is accumulated apart.
const ACCUMULATE_FS_BODY: &str = r#"
in vec4 v_color;

uniform sampler2D u_first;    // depth of the first surface
uniform float u_weight;       // blending weight of the layer being drawn
uniform vec2 u_taper;         // kernel value at the cutoff, 1 / (1 - that value)
layout(location = 0) out vec4 o_front;
#ifdef SOFT
uniform sampler2D u_second;   // depth of the surface behind it
layout(location = 1) out vec4 o_back;
layout(location = 2) out vec4 o_cover;
#endif

void main() {
  vec2 offset = pixel_offset();
  float r2 = quad_radius2(offset);
  if (!(r2 <= 1.0)) discard;
  float e = encode_depth(fragment_depth(offset));
  ivec2 at = ivec2(gl_FragCoord.xy);
  float first = texelFetch(u_first, at, 0).r;
  float kernel = (exp(-0.5 * u_extent * u_extent * r2) - u_taper.x) * u_taper.y;
  float cover = v_color.a * kernel;
  vec4 sum = vec4(v_color.rgb, 1.0) * (cover * u_weight);
#ifdef SOFT
  if (on_surface(e, first)) {
    o_front = sum;
    o_back = vec4(0.0);
    o_cover = vec4(cover, 0.0, 0.0, 0.0);
  } else if (on_surface(e, texelFetch(u_second, at, 0).r)) {
    o_front = vec4(0.0);
    o_back = sum;
    o_cover = vec4(0.0, cover, 0.0, 0.0);
  } else {
    discard;
  }
#else
  // Hard silhouettes: nothing shows outside the visibility discs.
  if (first >= 1.0 || !on_surface(e, first)) discard;
  o_front = sum;
#endif
}
"#;

fn surface_fs(defines: &str, body: &str) -> String {
    format!("#version 300 es\n#define DEPTH_OCTAVES {DEPTH_OCTAVES:.1}\n{defines}{SURFACE_GLSL}{body}")
}

/// Fragment shader of a visibility pass; `second` skips the first surface.
pub(crate) fn visibility_fs(second: bool) -> String {
    surface_fs(if second { "#define SECOND\n" } else { "" }, VISIBILITY_FS_BODY)
}

/// Fragment shader of the accumulation pass.
pub(crate) fn accumulate_fs(soft: bool) -> String {
    surface_fs(if soft { "#define SOFT\n" } else { "" }, ACCUMULATE_FS_BODY)
}

/// One triangle covering the viewport.
pub(crate) const RESOLVE_VS: &str = r#"#version 300 es
void main() {
  gl_Position = vec4(gl_VertexID == 1 ? 3.0 : -1.0, gl_VertexID == 2 ? 3.0 : -1.0, 0.0, 1.0);
}
"#;

/// Normalised mode, resolve: `colour = Σ w·c / Σ w`, premultiplied by the
/// coverage of the pixel and blended "over" whatever the framebuffer holds
/// (`blendFunc(ONE, ONE_MINUS_SRC_ALPHA)`).
///
/// Hard: the accumulation pass only kept fragments inside visibility discs,
/// so a pixel with any weight is opaque. Soft: the first surface gets the
/// coverage its accumulated weight earns, `min(1, W / 0.5)`, over the second
/// surface, which is opaque where its own discs reach.
const RESOLVE_FS_BODY: &str = r#"
precision highp float;
precision highp sampler2D;

uniform sampler2D u_front;
uniform ivec2 u_origin;       // lower-left corner of the viewport in the framebuffer
#ifdef SOFT
uniform sampler2D u_back;
uniform sampler2D u_cover;
uniform sampler2D u_second;   // depth of the second surface
uniform float u_inv_tau;      // 1 / weight at which a surface is opaque
#endif
out vec4 o_color;

void main() {
  ivec2 at = ivec2(gl_FragCoord.xy) - u_origin;
  vec4 front = texelFetch(u_front, at, 0);
#ifdef SOFT
  vec2 cover = texelFetch(u_cover, at, 0).rg;
  float alpha = front.a > 0.0 ? min(cover.x * u_inv_tau, 1.0) : 0.0;
  vec3 rgb = front.a > 0.0 ? front.rgb * (alpha / front.a) : vec3(0.0);
  vec4 back = texelFetch(u_back, at, 0);
  if (back.a > 0.0 && alpha < 1.0) {
    float behind = texelFetch(u_second, at, 0).r < 1.0 ? 1.0 : min(cover.y * u_inv_tau, 1.0);
    float through = (1.0 - alpha) * behind;
    rgb += back.rgb * (through / back.a);
    alpha += through;
  }
  if (alpha <= 0.0) discard;
  o_color = vec4(rgb, alpha);
#else
  if (front.a <= 0.0) discard;
  o_color = vec4(front.rgb / front.a, 1.0);
#endif
}
"#;

/// Fragment shader of the resolve pass.
pub(crate) fn resolve_fs(soft: bool) -> String {
    format!("#version 300 es\n{}{RESOLVE_FS_BODY}", if soft { "#define SOFT\n" } else { "" })
}

/// Line vertex shader: every segment is a screen-space quad of constant
/// pixel width (WebGL lines are always one pixel wide, which is invisible in
/// a headset and on high-DPI screens). Each of the six vertices of a quad
/// carries the whole segment; not instanced for the same reason as the splats.
pub(crate) const LINE_VS: &str = r#"#version 300 es
precision highp float;

layout(location = 0) in vec3 a_p0;
layout(location = 1) in vec3 a_p1;
layout(location = 2) in vec4 a_color;
layout(location = 3) in float a_width;
layout(location = 4) in vec2 a_corner;   // x: which end (-1, +1), y: side (-1, +1)

uniform mat4 u_view_proj;
uniform vec2 u_viewport;     // W, H in pixels
uniform float u_near;        // clip-space w of the near plane
uniform float u_px_scale;    // device pixels per width unit

out vec4 v_color;
out float v_side;            // signed distance from the center line, pixels
out float v_half_width;

void main() {
  vec4 c0 = u_view_proj * vec4(a_p0, 1.0);
  vec4 c1 = u_view_proj * vec4(a_p1, 1.0);
  // Clip the segment against the near plane here: the quad is built after
  // the perspective divide, which is meaningless behind the camera.
  if (c0.w < u_near && c1.w < u_near) { gl_Position = vec4(0.0, 0.0, 2.0, 1.0); return; }
  if (c0.w < u_near) c0 = mix(c0, c1, (u_near - c0.w) / (c1.w - c0.w));
  else if (c1.w < u_near) c1 = mix(c1, c0, (u_near - c1.w) / (c0.w - c1.w));
  vec2 s0 = c0.xy / c0.w * 0.5 * u_viewport;
  vec2 s1 = c1.xy / c1.w * 0.5 * u_viewport;
  vec2 dir = s1 - s0;
  float len = length(dir);
  dir = len > 1e-5 ? dir / len : vec2(1.0, 0.0);
  vec2 normal = vec2(-dir.y, dir.x);
  float half_width = 0.5 * a_width * u_px_scale;
  float reach = half_width + 1.0;          // one pixel of feather for anti-aliasing
  vec2 s = (a_corner.x < 0.0 ? s0 : s1) + dir * (a_corner.x * half_width) + normal * (a_corner.y * reach);
  gl_Position = vec4(s / (0.5 * u_viewport), 0.0, 1.0);
  v_color = a_color;
  v_side = a_corner.y * reach;
  v_half_width = half_width;
}
"#;

pub(crate) const LINE_FS: &str = r#"#version 300 es
precision highp float;

in vec4 v_color;
in float v_side;
in float v_half_width;
out vec4 o_color;

void main() {
  float coverage = clamp(v_half_width + 0.5 - abs(v_side), 0.0, 1.0);
  float alpha = v_color.a * coverage;
  o_color = vec4(v_color.rgb * alpha, alpha);
}
"#;
