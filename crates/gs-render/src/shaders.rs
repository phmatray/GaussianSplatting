//! GLSL ES 3.00 sources.
//!
//! The splat shaders implement, step by step, the reference algorithm
//! documented in `gs_core::splat` (the numbers in the comments are the steps
//! of that recipe). Keep the two in sync: the end-to-end test compares this
//! shader with `gs_core::raster::rasterize` pixel by pixel.

/// Splat vertex shader. Four vertices per splat, drawn as indexed triangles:
/// vertex `4·q + k` is corner `k` of the `q`-th quad in draw order, and the
/// splat that quad shows is looked up in the order texture.
///
/// Deliberately not instanced. Instancing is the obvious way to draw one
/// quad per sorted index and costs the same on a hardware GPU, but software
/// rasterizers (SwiftShader, which headless Chromium and therefore every
/// automated check of this project runs on) execute an instanced draw as one
/// draw call per instance: measured 25× slower than this formulation.
pub(crate) const SPLAT_VS: &str = r#"#version 300 es
precision highp float;
precision highp int;
precision highp usampler2D;

uniform usampler2D u_data0;   // layer 0, three RGBA32UI texels per splat
uniform usampler2D u_data1;   // layer 1
uniform usampler2D u_order;   // sorted splat indices, one R32UI texel per quad
uniform int u_order_width;    // texels per row of u_order
uniform uint u_split;         // indices >= u_split address layer 1
uniform uvec2 u_counts;       // splats currently stored in each layer
uniform int u_spr;            // splats per data texture row
uniform mat4 u_view;          // world -> camera, may contain a uniform scale
uniform vec2 u_focal;         // fx, fy in pixels
uniform vec2 u_center;        // cx, cy in pixels, top-left origin
uniform vec2 u_viewport;      // W, H in pixels
uniform float u_near;
uniform float u_splat_scale;  // k
uniform float u_max_radius;   // clamp of the 3-sigma half-axes, pixels
uniform float u_point_radius; // > 0: point mode, radius of every dot in pixels
uniform float u_clip_y;       // splats centered above this world height are hidden

out vec2 v_corner;
out vec4 v_color;

// Outside the clip volume: all four vertices of a culled splat go here.
const vec4 CULLED = vec4(0.0, 0.0, 2.0, 1.0);

void main() {
  int quad = gl_VertexID >> 2;
  int k = gl_VertexID & 3;
  vec2 corner = vec2((k & 1) != 0 ? 1.0 : -1.0, (k & 2) != 0 ? 1.0 : -1.0);   // in {-1, +1}^2
  uint index = texelFetch(u_order, ivec2(quad % u_order_width, quad / u_order_width), 0).x;
  bool second = index >= u_split;
  uint local = second ? index - u_split : index;
  // The order can be one frame older than the data.
  if (local >= (second ? u_counts.y : u_counts.x)) { gl_Position = CULLED; return; }
  ivec2 at = ivec2((int(local) % u_spr) * 3, int(local) / u_spr);
  uvec4 t0, t1, t2;
  if (second) {
    t0 = texelFetch(u_data1, at, 0);
    t1 = texelFetch(u_data1, at + ivec2(1, 0), 0);
    t2 = texelFetch(u_data1, at + ivec2(2, 0), 0);
  } else {
    t0 = texelFetch(u_data0, at, 0);
    t1 = texelFetch(u_data0, at + ivec2(1, 0), 0);
    t2 = texelFetch(u_data0, at + ivec2(2, 0), 0);
  }
  vec3 p = uintBitsToFloat(t0.xyz);
  vec4 color = vec4(float(t0.w & 255u), float((t0.w >> 8) & 255u), float((t0.w >> 16) & 255u), float(t0.w >> 24)) / 255.0;
  if (p.y > u_clip_y || color.a < 1.0 / 255.0) { gl_Position = CULLED; return; }

  // 1. camera-space center and depth
  vec4 t = u_view * vec4(p, 1.0);
  float d = -t.z;
  if (!(d >= u_near)) { gl_Position = CULLED; return; }

  // 2. center in pixels (y down)
  vec2 uv = t.xy / d;
  vec2 px = vec2(u_center.x + u_focal.x * uv.x, u_center.y - u_focal.y * uv.y);

  vec2 major, minor;
  if (u_point_radius > 0.0) {
    major = vec2(u_point_radius, 0.0);
    minor = vec2(0.0, u_point_radius);
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
    float k2 = u_splat_scale * u_splat_scale;
    vec3 S0 = S * T0;
    vec3 S1 = S * T1;
    float ma = max(k2 * dot(T0, S0), 0.0);
    float mb = k2 * dot(T0, S1);
    float mc = max(k2 * dot(T1, S1), 0.0);
    float a = ma + 0.3, b = mb, c = mc + 0.3;
    // 6. determinant that cannot cancel below the low-pass
    float det = max(ma * mc - mb * mb, 0.0) + 0.3 * (ma + mc) + 0.09;
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
    major = min(3.0 * sqrt(l1), u_max_radius) * v1;
    minor = min(3.0 * sqrt(l2), u_max_radius) * vec2(-v1.y, v1.x);
  }

  // Entirely off-screen: skip the rasterizer.
  float reach = length(major);
  if (any(greaterThan(abs(px - 0.5 * u_viewport), 0.5 * u_viewport + reach))) { gl_Position = CULLED; return; }

  // 8. quad around the center; pixels have y down, NDC has y up
  vec2 offset = corner.x * major + corner.y * minor;
  vec2 ndc = vec2(2.0 * px.x / u_viewport.x - 1.0, 1.0 - 2.0 * px.y / u_viewport.y);
  gl_Position = vec4(ndc + offset * vec2(2.0, -2.0) / u_viewport, 0.0, 1.0);
  v_corner = corner;
  v_color = color;
}
"#;

/// Splat fragment shader: steps 9-11, premultiplied output.
pub(crate) const SPLAT_FS: &str = r#"#version 300 es
precision highp float;

in vec2 v_corner;
// Constant over the quad. Deliberately not `flat`: flat varyings force a slow
// provoking-vertex emulation on ANGLE/Direct3D.
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
