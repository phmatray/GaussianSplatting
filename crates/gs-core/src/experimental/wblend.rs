//! Experimental CPU rasterizer: normalised depth-tested blending (C1) and
//! perspective-correct planar splats with a normalised mip filter (C2) of
//! `docs/RECHERCHE_QUALITE.md`, next to a copy of today's sorted "over"
//! compositing so that every combination can be compared on equal terms.
//!
//! [`rasterize`] has the interface of [`crate::raster::rasterize`]; with
//! [`WblendOptions::sorted`] it reproduces that function, with
//! [`WblendOptions::default`] its `classic_dilation` variant, the renderer
//! of the product before the quality round (unit tests pin both down).
//! [`render`] additionally returns the coverage of every pixel and accepts
//! several layers with a blending priority each.
//!
//! # The two modes of the product
//!
//! [`WblendOptions::normalized_hard`] and [`WblendOptions::normalized_soft`]
//! are the CPU reference of the "Normalised" blending mode of
//! `docs/ARCHITECTURE.md` at its two quality levels; `e2e/render` compares
//! the WebGL2 viewer against them pixel by pixel, with the layers weighted
//! `1` and [`LIVE_LAYER_WEIGHT`]. [`WblendOptions::sorted`] is the "Sorted"
//! mode. The section "What a WebGL2 port needs" below is the lab's original
//! sketch; `gs-render` documents what the port actually does (a logarithmic
//! depth encoding, a two-channel target for the coverage at both levels, no
//! depth attachment in the accumulation pass).
//!
//! # Normalised blending ([`Blend::Normalized`])
//!
//! Sensor surfels describe opaque surfaces. The colour of a pixel should be
//! the weighted mean of the splats of the *visible* surface (Zwicker 2001,
//! Botsch 2005), not a stack of translucent discs in sort order:
//!
//! 1. **Visibility.** Every splat is drawn as an opaque disc of
//!    [`Normalized::vis_cutoff`] standard deviations into a depth buffer
//!    (nearest wins).
//! 2. **Accumulation.** Every fragment within [`WblendOptions::cutoff`]
//!    standard deviations whose depth is at most the buffered depth plus a
//!    tolerance adds `w·colour` and `w`, with
//!    `w = layer weight · opacity · exp(−m/2)`. No order is involved.
//! 3. **Resolve.** `colour = Σ w·colour / Σ w`. The coverage of the pixel
//!    follows from the accumulated weight ([`Coverage`]) and the rule for
//!    silhouettes ([`Silhouette`]): opaque inside the visibility discs, or
//!    soft everywhere, over the background or over the next surface behind.
//!
//! The fragment depth is either the depth of the splat center
//! ([`DepthModel::Center`], all a screen-space EWA quad knows) or the depth
//! where the pixel ray meets the splat's plane ([`DepthModel::Plane`]).
//!
//! # Planar splats ([`Footprint::Planar`])
//!
//! A surfel is a flat disc. Instead of projecting its covariance with the
//! affine approximation of EWA, which is exact only at the center, the
//! Gaussian is evaluated where the pixel ray meets the disc's plane (2DGS,
//! Zwicker 2004). With `Tu`, `Tv` the two scaled tangent axes in camera
//! space, `pc` the center, `n = Tu × Tv` and the pixel ray `d = (x', y', −1)`:
//! `t = (n·pc)/(n·d)`, `u = (t·d − pc)·Tu/|Tu|²`, likewise `v`, `m = u² + v²`.
//! All three are ratios of functions that are linear in the pixel
//! coordinates, which is exactly what a rasterized world-space quad
//! interpolates. [`WblendOptions::screen_floor_px`] keeps splats that are
//! smaller than a pixel, or seen edge-on, from vanishing.
//!
//! # What a WebGL2 port needs
//!
//! * Pass 1: depth-only draw (colour mask off, `depthFunc(LESS)`, depth
//!   write on), fragment shader discards outside the visibility radius.
//!   With planar quads the rasterizer interpolates the depth; with EWA
//!   quads [`DepthModel::Plane`] needs `gl_FragDepth`.
//! * Pass 2: the same geometry into an `RGBA16F` colour attachment
//!   (`EXT_color_buffer_half_float` or `EXT_color_buffer_float`) sharing the
//!   depth attachment, `blendFunc(ONE, ONE)`, `depthMask(false)`,
//!   `depthFunc(LEQUAL)`, the tolerance applied as a depth offset in the
//!   vertex shader (the splat is pulled toward the eye by the tolerance).
//!   Output `vec4(colour·w, w)`.
//! * Pass 3: one full-screen triangle that divides by the weight. To know
//!   "inside a visibility disc" it samples the depth attachment, which must
//!   therefore be a texture (`DEPTH_COMPONENT24`), not a renderbuffer.
//! * [`Silhouette::SoftOverNextSurface`] adds a second depth-only pass whose
//!   fragment shader discards fragments on the first surface (it samples the
//!   first depth texture) and a second `RGBA16F` colour attachment
//!   (`drawBuffers`); the colour pass routes each fragment to the attachment
//!   of its surface by comparing its depth with the two depth textures (no
//!   hardware depth test), still with `blendFunc(ONE, ONE)` on both.
//! * No CPU sort and no index texture for layers drawn this way. Layers
//!   that are translucent by nature (imported 3DGS scenes) keep the sorted
//!   path.
//! * Per-layer weight is a uniform; per-splat weight rides in the opacity
//!   byte of the 16-byte wire splat.
//! * The coverage of a pixel is accumulated without the layer weights (a
//!   layer with a high priority would otherwise get a fat outline). With
//!   soft silhouettes that is one more accumulated value per surface: the
//!   two `RGBA16F` attachments hold `Σ w·colour` and `Σ w`, a third `RG16F`
//!   one the two coverages. With hard silhouettes and no fringe coverage
//!   ([`Coverage::None`]) the coverage is not needed at all.

#[cfg(feature = "par")]
use rayon::prelude::*;

use crate::raster::Image;
use crate::sort::DepthSorter;
use crate::splat::{cov6, DEFAULT_NEAR, FRUSTUM_CLAMP, MIN_ALPHA};
use crate::types::{Camera, Splat};
use glam::{DVec3, Quat, Vec3};

/// How the colours of overlapping splats are combined.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Blend {
    /// Back-to-front "over" on the opacity, sorted by the depth of the splat
    /// centers: what the product does today.
    Over,
    /// Visibility pre-pass, weighted sum, division by the weight.
    Normalized(Normalized),
}

/// Parameters of [`Blend::Normalized`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Normalized {
    /// Radius of the opaque discs of the visibility pass, in standard
    /// deviations (clamped to the kernel cutoff).
    pub vis_cutoff: f32,
    /// A fragment is blended when its depth is at most
    /// `(1 + depth_tol)·` the depth of the visible surface …
    pub depth_tol: f32,
    /// … plus this many of its splat's larger standard deviation.
    pub depth_tol_sigma: f32,
    /// Coverage of pixels outside every visibility disc.
    pub coverage: Coverage,
    /// Subtract the kernel's value at the cutoff, so that the weight reaches
    /// zero there instead of jumping.
    pub taper: bool,
    /// What the visible surface is drawn over; see [`Silhouette`].
    pub silhouette: Silhouette,
    /// Blend fragments that lie *in front of* the visible surface (the
    /// kernel tails of a nearer object, outside its own visibility discs)
    /// into that surface. This is what a one-sided depth test
    /// (`depthFunc(LEQUAL)`) does, and it smears every nearer object over
    /// what is behind it, all the more when its layer has a larger weight.
    /// `false` discards them (a two-sided test against the depth texture in
    /// the fragment shader).
    pub near_fringe: bool,
}

impl Default for Normalized {
    fn default() -> Self {
        Self {
            vis_cutoff: 2.0,
            depth_tol: 0.01,
            depth_tol_sigma: 0.0,
            coverage: Coverage::Linear(0.5),
            taper: true,
            silhouette: Silhouette::Hard,
            near_fringe: true,
        }
    }
}

/// How the rim of the visible surface is treated under [`Blend::Normalized`].
///
/// The visibility discs decide which surface is in front; they say nothing
/// reliable about where exactly that surface ends, which is known only to
/// within a sensor pixel. Treating the discs as the outline
/// ([`Silhouette::Hard`]) gives every object a hard, jagged and slightly fat
/// rim. The soft variants give the front surface the coverage its
/// accumulated weight earns ([`Normalized::coverage`]), everywhere, and
/// differ in what shows through where that coverage is below one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Silhouette {
    /// Opaque inside the visibility discs; the coverage function only
    /// applies outside every disc.
    Hard,
    /// Soft rim over the empty background, even where another surface lies
    /// behind (a single colour target, but dark halos in front of surfaces).
    SoftOverBackground,
    /// Soft rim over the *next* surface: a second visibility pass finds the
    /// nearest surface behind the first, a second accumulation target
    /// receives its fragments, and the resolve composites front over back
    /// over the background.
    SoftOverNextSurface,
}

/// Opacity of a pixel from its accumulated weight `W`, where no visibility
/// disc covers it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Coverage {
    /// Always transparent: silhouettes end at the visibility discs.
    None,
    /// `min(1, W / τ)`.
    Linear(f32),
    /// `smoothstep(0, τ, W)`.
    Smooth(f32),
}

impl Coverage {
    #[inline]
    fn alpha(self, weight: f32) -> f32 {
        match self {
            Self::None => 0.0,
            Self::Linear(tau) => (weight / tau.max(1.0e-6)).clamp(0.0, 1.0),
            Self::Smooth(tau) => {
                let t = (weight / tau.max(1.0e-6)).clamp(0.0, 1.0);
                t * t * (3.0 - 2.0 * t)
            }
        }
    }
}

/// Screen-space shape of a splat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Footprint {
    /// Affine (EWA) projection of the 3D covariance: an ellipse.
    Ewa,
    /// Exact intersection of the pixel ray with the splat's plane.
    Planar,
}

/// Depth of a fragment in the visibility test of [`Blend::Normalized`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepthModel {
    /// The depth of the splat center, for all its fragments.
    Center,
    /// The depth where the pixel ray meets the splat's plane.
    Plane,
}

/// Anti-aliasing of the EWA footprint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LowPass {
    /// Add this many pixel² to the diagonal of the 2D covariance and leave
    /// the opacity alone (the product: 0.3). Thin structures get fatter.
    Dilate(f32),
    /// Mip-Splatting's 2D filter: add the variance and scale the opacity by
    /// `√(det Σ / det(Σ + s·I))`, which conserves the splat's energy.
    Normalized(f32),
}

/// Options of [`render`] and [`rasterize`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WblendOptions {
    /// Colour of pixels no splat covers ([`rasterize`] only).
    pub background: [u8; 3],
    pub blend: Blend,
    pub footprint: Footprint,
    pub depth: DepthModel,
    /// Low-pass of the EWA footprint (ignored by [`Footprint::Planar`]).
    pub low_pass: LowPass,
    /// [`Footprint::Planar`]: a splat is never smaller on screen than an
    /// isotropic Gaussian of this standard deviation in pixels around its
    /// center (`0` disables the floor).
    pub screen_floor_px: f32,
    /// Multiplies every standard deviation.
    pub kernel_scale: f32,
    /// Fragments farther than this many standard deviations from the splat
    /// center are discarded.
    pub cutoff: f32,
    /// Skip splats whose normal (their third axis) points away from the eye.
    pub cull_backfaces: bool,
    /// Splats whose center is closer than this are not drawn (meters).
    pub near: f32,
    /// Point-cloud view: when positive, every splat is drawn as a dot with
    /// this standard deviation in pixels, facing the camera, instead of its
    /// real footprint (the `point_mode` of [`crate::raster::RasterOptions`]).
    pub point_sigma_px: f32,
}

impl Default for WblendOptions {
    /// The renderer of the product before the quality round, which the lab
    /// calls "today": sorted "over", EWA, plain 0.3 px² dilation, 3σ cutoff.
    fn default() -> Self {
        Self {
            background: [0, 0, 0],
            blend: Blend::Over,
            footprint: Footprint::Ewa,
            depth: DepthModel::Center,
            low_pass: LowPass::Dilate(0.3),
            screen_floor_px: 0.0,
            kernel_scale: 1.0,
            cutoff: 3.0,
            cull_backfaces: false,
            near: DEFAULT_NEAR,
            point_sigma_px: 0.0,
        }
    }
}

/// Blending weight of the live layer (layer 1) relative to the static one
/// under [`Blend::Normalized`]. The live layer carries the shadows the dancer
/// casts on the floor of the static plate; at equal weights they come out at
/// half strength and patchy.
pub const LIVE_LAYER_WEIGHT: f32 = 16.0;

impl WblendOptions {
    /// The "Sorted" blending mode of `docs/ARCHITECTURE.md`: back-to-front
    /// "over" with the energy-conserving low-pass. Reproduces
    /// [`crate::raster::rasterize`] with its default options.
    /// ([`Self::default`] is the same with the plain dilation: the renderer
    /// the lab calls "today".)
    pub fn sorted() -> Self {
        Self { low_pass: LowPass::Normalized(crate::splat::LOW_PASS_PX2), ..Self::default() }
    }

    /// The "Normalised" blending mode of `docs/ARCHITECTURE.md` with hard
    /// silhouettes: the cheaper of the two quality levels, the configuration
    /// of the lab's "C1 hard" rows and exactly what the WebGL2 viewer renders
    /// at `Quality::Hard` (one visibility pass, an `RGBA16F` accumulation
    /// target and a two-channel one for the coverage).
    ///
    /// Opaque inside visibility discs of 1.6 standard deviations. Where no
    /// disc reaches (the rim of an object against the empty background, a
    /// pinhole between the discs of a sparsely sampled surface) the pixel
    /// gets the coverage `min(1, W / 0.5)` of the weight accumulated without
    /// the layer weights; an object in front of another surface ends at its
    /// discs. Depth of a fragment on the plane of its splat, two-sided 1 %
    /// depth test, tapered 3σ kernel, EWA footprint with the normalised
    /// low-pass filter ([`crate::splat::LOW_PASS_PX2`]) and no screen-space
    /// floor.
    pub fn normalized_hard() -> Self {
        let params = Normalized {
            vis_cutoff: 1.6,
            depth_tol: 0.01,
            depth_tol_sigma: 0.0,
            coverage: Coverage::Linear(0.5),
            taper: true,
            silhouette: Silhouette::Hard,
            near_fringe: false,
        };
        Self {
            blend: Blend::Normalized(params),
            depth: DepthModel::Plane,
            low_pass: LowPass::Normalized(crate::splat::LOW_PASS_PX2),
            ..Self::default()
        }
    }

    /// The same mode with soft silhouettes over the next surface: the
    /// configuration behind the lab's headline figures and what the viewer
    /// renders at `Quality::Soft` (two visibility passes, three accumulation
    /// targets).
    ///
    /// Visibility discs of 2 standard deviations; the coverage
    /// `min(1, W / 0.5)` applies to the first surface everywhere, also
    /// inside its discs: first surface over second surface over the
    /// background. Everything else as in [`Self::normalized_hard`].
    pub fn normalized_soft() -> Self {
        let hard = Self::normalized_hard();
        let Blend::Normalized(params) = hard.blend else { unreachable!("normalized_hard is normalised") };
        let params = Normalized { vis_cutoff: 2.0, silhouette: Silhouette::SoftOverNextSurface, ..params };
        Self { blend: Blend::Normalized(params), ..hard }
    }
}

/// A set of splats and its blending priority.
#[derive(Clone, Copy, Debug)]
pub struct Layer<'a> {
    pub splats: &'a [Splat],
    /// Multiplies the blending weight of every splat of the layer under
    /// [`Blend::Normalized`] (ignored by [`Blend::Over`]).
    pub weight: f32,
}

/// Work done by a render, for fill-rate estimates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FragmentStats {
    /// Splats that passed culling.
    pub splats: u64,
    /// Fragments evaluated by the visibility pass.
    pub visibility: u64,
    /// Fragments that passed the cutoff in the colour pass.
    pub shaded: u64,
    /// Of those, the ones that contributed (passed the depth or alpha test).
    pub blended: u64,
}

/// Output of [`render`].
#[derive(Clone, Debug, PartialEq)]
pub struct Rendered {
    pub width: u32,
    pub height: u32,
    /// Colour premultiplied by the coverage, `0..=255` per channel.
    pub rgb: Vec<f32>,
    /// Coverage (accumulated opacity) per pixel, `0..=1`.
    pub alpha: Vec<f32>,
    pub stats: FragmentStats,
}

impl Rendered {
    /// The picture over a flat background.
    pub fn over(&self, background: [u8; 3]) -> Image {
        let mut rgb = Vec::with_capacity(self.rgb.len());
        for (pixel, &alpha) in self.rgb.chunks_exact(3).zip(&self.alpha) {
            for c in 0..3 {
                // Saturating cast: clamps to 0..=255.
                rgb.push((pixel[c] + (1.0 - alpha) * background[c] as f32 + 0.5) as u8);
            }
        }
        Image { width: self.width, height: self.height, rgb }
    }
}

/// Plane of a splat as seen from the pixel grid: every quantity is a linear
/// function of the pixel offset `(dx, dy)` from the projected center.
#[derive(Clone, Copy)]
struct Plane {
    /// `n·d = hn[0]·dx + hn[1]·dy + hn[2]` (with `n` normalised).
    hn: [f32; 3],
    /// `n·pc`: the fragment depth is `npc / (n·d)`.
    npc: f32,
    /// `u·(n·d) = hu[0]·dx + hu[1]·dy`, in standard deviations; likewise `v`.
    hu: [f32; 2],
    hv: [f32; 2],
    /// The fragment depth is kept inside the splat's reach.
    z_lo: f32,
    z_hi: f32,
}

/// One visible splat, ready to be drawn.
#[derive(Clone, Copy)]
struct Draw {
    px: f32,
    py: f32,
    /// Rows of the pixel-offset → standard-deviation map of the EWA ellipse.
    u1: [f32; 2],
    u2: [f32; 2],
    plane: Option<Plane>,
    color: [f32; 3],
    /// Opacity (over) or blending weight (normalised).
    weight: f32,
    /// The same without the layer's weight: what the splat adds to the
    /// coverage of a pixel.
    cover: f32,
    /// Depth of the center.
    depth: f32,
    /// Larger standard deviation in meters (for the depth tolerance).
    sigma: f32,
    x0: u32,
    x1: u32,
    y0: u32,
    y1: u32,
}

/// Everything [`prepare`] needs to know about the view.
struct View {
    r0: Vec3,
    r1: Vec3,
    r2: Vec3,
    t: Vec3,
    eye: Vec3,
    fx: f32,
    fy: f32,
    cx: f32,
    cy: f32,
    width: f32,
    height: f32,
    /// View units per world unit: 1 unless the camera pose carries a uniform
    /// scale (the miniature mode of the viewer).
    scale: f32,
}

impl View {
    fn new(cam: &Camera) -> Self {
        let m = cam.view_matrix();
        Self {
            r0: Vec3::new(m.x_axis.x, m.y_axis.x, m.z_axis.x),
            r1: Vec3::new(m.x_axis.y, m.y_axis.y, m.z_axis.y),
            r2: Vec3::new(m.x_axis.z, m.y_axis.z, m.z_axis.z),
            t: m.w_axis.truncate(),
            eye: cam.position(),
            fx: cam.fx,
            fy: cam.fy,
            cx: cam.cx,
            cy: cam.cy,
            width: cam.width as f32,
            height: cam.height as f32,
            scale: m.x_axis.truncate().length(),
        }
    }

    #[inline]
    fn to_camera(&self, v: Vec3) -> Vec3 {
        Vec3::new(self.r0.dot(v), self.r1.dot(v), self.r2.dot(v))
    }
}

/// Clips `center ± extent` to pixel indices `0..limit` (end-exclusive).
#[inline]
fn pixel_range(center: f32, extent: f32, limit: f32) -> (u32, u32) {
    // Pixel i has its center at i + 0.5. Saturating casts clip (NaN → empty).
    let lo = (center - extent - 0.5).ceil().max(0.0) as u32;
    let hi = ((center + extent - 0.5).floor() + 1.0).clamp(0.0, limit) as u32;
    (lo, hi)
}

/// Projects one splat. The EWA part follows `splat::project_splat_with_axes`
/// step by step, with the low-pass and the cutoff made configurable.
fn prepare(splat: &Splat, layer_weight: f32, view: &View, opts: &WblendOptions) -> Option<Draw> {
    let opacity = splat.rgba[3] as f32 / 255.0;
    if opacity < MIN_ALPHA {
        return None;
    }
    let pos = splat.position();
    let pc = view.to_camera(pos) + view.t;
    let depth = -pc.z;
    if !(depth >= opts.near && depth > 0.0) {
        return None;
    }
    let q = Quat::from_array(splat.rot);
    let len2 = q.length_squared();
    let q = if len2.is_finite() && len2 > 1e-24 { q * len2.sqrt().recip() } else { Quat::IDENTITY };
    if opts.cull_backfaces && (q * Vec3::Z).dot(view.eye - pos) < 0.0 {
        return None;
    }
    let k = opts.kernel_scale;
    let cutoff = opts.cutoff.max(0.1);
    let inv_d = 1.0 / depth;
    let (u, v) = (pc.x * inv_d, pc.y * inv_d);
    let (px, py) = (view.cx + view.fx * u, view.cy - view.fy * v);

    // EWA: T = J·W with the direction clamped to 1.3x the frustum.
    let margin = 0.5 * (FRUSTUM_CLAMP - 1.0);
    let (inv_fx, inv_fy) = (1.0 / view.fx, 1.0 / view.fy);
    let uc = u.max((-view.cx - margin * view.width) * inv_fx).min((view.width - view.cx + margin * view.width) * inv_fx);
    let vc = v.max((view.cy - view.height - margin * view.height) * inv_fy).min((view.cy + margin * view.height) * inv_fy);
    let (j00, j11) = (view.fx * inv_d, -view.fy * inv_d);
    let t0 = view.r0 * j00 + view.r2 * (j00 * uc);
    let t1 = view.r1 * j11 + view.r2 * (j11 * vc);
    let [xx, xy, xz, yy, yz, zz] = cov6(splat);
    let sigma_mul = |t: Vec3| Vec3::new(xx * t.x + xy * t.y + xz * t.z, xy * t.x + yy * t.y + yz * t.z, xz * t.x + yz * t.y + zz * t.z);
    let (s0, s1) = (sigma_mul(t0), sigma_mul(t1));
    let positive = |x: f32| if x < 0.0 { 0.0 } else { x };
    let k2 = k * k;
    let (ma, mb, mc) = (positive(k2 * t0.dot(s0)), k2 * t0.dot(s1), positive(k2 * t1.dot(s1)));
    let (low, compensate) = match opts.low_pass {
        LowPass::Dilate(s) => (s.max(1.0e-4), false),
        LowPass::Normalized(s) => (s.max(1.0e-4), true),
    };
    let point = opts.point_sigma_px > 0.0;
    let (a, b, c, det_m, det) = if point {
        // A dot has the size it is asked to have: nothing is dilated.
        let var = (opts.point_sigma_px * opts.point_sigma_px).max(1.0e-6);
        (var, 0.0, var, var * var, var * var)
    } else {
        let det_m = positive(ma * mc - mb * mb);
        (ma + low, mb, mc + low, det_m, det_m + low * (ma + mc) + low * low)
    };
    if !(det > 0.0 && det.is_finite() && px.is_finite() && py.is_finite()) {
        return None;
    }
    let half_diff = 0.5 * (a - c);
    let rad = half_diff.hypot(b);
    let lambda1 = 0.5 * (a + c) + rad;
    let lambda2 = det / lambda1;
    let (ex, ey) = if a >= c { (half_diff + rad, b) } else { (b, rad - half_diff) };
    let len = ex.hypot(ey);
    let (vx, vy) = if len > 1e-12 { (ex / len, ey / len) } else { (1.0, 0.0) };
    let (i1, i2) = (1.0 / lambda1.sqrt(), 1.0 / lambda2.sqrt());
    let mut weight = opacity;
    if compensate && opts.footprint == Footprint::Ewa {
        weight *= (det_m / det).sqrt();
    }

    // Pixel rectangle of the EWA ellipse. Under "over" a faint splat drops
    // below the alpha threshold before the cutoff (as in `raster::prepare`).
    let reach = match opts.blend {
        Blend::Over => cutoff.min((2.0 * (weight.min(1.0) / MIN_ALPHA).ln()).max(0.0).sqrt()),
        Blend::Normalized(_) => cutoff,
    };
    let mut x_extent = reach * a.sqrt();
    let mut y_extent = reach * c.sqrt();

    // In view units, like the depths it is compared with. A dot has no
    // extent in depth: all its fragments take the depth of its center.
    let sigma = if point { 0.0 } else { k * splat.scale[0].max(splat.scale[1]) * view.scale };
    let needs_plane = !point && (opts.footprint == Footprint::Planar || opts.depth == DepthModel::Plane);
    let mut plane = None;
    let mut whole_screen = false;
    if needs_plane {
        // In double precision: the terms below cancel to the splat's size
        // out of the distance to the camera.
        let to_cam = |axis: Vec3| DVec3::new(view.r0.dot(axis) as f64, view.r1.dot(axis) as f64, view.r2.dot(axis) as f64);
        let tu = to_cam(q * Vec3::X) * (k * splat.scale[0]) as f64;
        let tv = to_cam(q * Vec3::Y) * (k * splat.scale[1]) as f64;
        let pcd = pc.as_dvec3();
        let n = tu.cross(tv);
        let (lu, lv, ln) = (tu.length_squared(), tv.length_squared(), n.length());
        if lu > 0.0 && lv > 0.0 && ln > 0.0 && ln.is_finite() {
            let n = n / ln;
            let npc = n.dot(pcd);
            let (fx, fy) = (view.fx as f64, view.fy as f64);
            // d(dx, dy) = pc/depth + (dx/fx, −dy/fy, 0).
            let linear = |w: DVec3| [w.x / fx, -w.y / fy];
            let hn = linear(n);
            let coefficients = |axis: DVec3, len2: f64| {
                let (g, along) = (linear(axis), pcd.dot(axis));
                [((npc * g[0] - along * hn[0]) / len2) as f32, ((npc * g[1] - along * hn[1]) / len2) as f32]
            };
            let reach = cutoff * sigma;
            plane = Some(Plane {
                hn: [hn[0] as f32, hn[1] as f32, (npc / depth as f64) as f32],
                npc: npc as f32,
                hu: coefficients(tu, lu),
                hv: coefficients(tv, lv),
                z_lo: (depth - reach).max(opts.near),
                z_hi: depth + reach,
            });
            if opts.footprint == Footprint::Planar {
                // Bounding rectangle of the projected cutoff square.
                let (mut ax, mut ay) = (0.0f32, 0.0f32);
                for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                    let corner = pcd + (tu * su + tv * sv) * cutoff as f64;
                    if -corner.z < opts.near as f64 {
                        whole_screen = true;
                        break;
                    }
                    let cpx = view.cx as f64 + fx * corner.x / -corner.z;
                    let cpy = view.cy as f64 - fy * corner.y / -corner.z;
                    ax = ax.max((cpx - px as f64).abs() as f32);
                    ay = ay.max((cpy - py as f64).abs() as f32);
                }
                let floor = cutoff * opts.screen_floor_px.max(0.0);
                x_extent = ax.max(floor) + 1.0;
                y_extent = ay.max(floor) + 1.0;
            }
        } else if opts.footprint == Footprint::Planar {
            // A degenerate disc: only its screen-space floor is left.
            let floor = cutoff * opts.screen_floor_px.max(0.0);
            (x_extent, y_extent) = (floor, floor);
        }
    }
    let ((x0, x1), (y0, y1)) = if whole_screen {
        ((0, view.width as u32), (0, view.height as u32))
    } else {
        (pixel_range(px, x_extent, view.width), pixel_range(py, y_extent, view.height))
    };
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    Some(Draw {
        px,
        py,
        u1: [vx * i1, vy * i1],
        u2: [-vy * i2, vx * i2],
        plane,
        color: [splat.rgba[0] as f32, splat.rgba[1] as f32, splat.rgba[2] as f32],
        weight: weight * layer_weight.max(0.0),
        cover: weight,
        depth,
        sigma,
        x0,
        x1,
        y0,
        y1,
    })
}

/// Per-render constants of the fragment evaluation.
#[derive(Clone, Copy)]
struct Shading {
    planar: bool,
    plane_depth: bool,
    cutoff2: f32,
    /// `1 / floor²`, or 0 without a screen-space floor.
    inv_floor2: f32,
    near: f32,
}

impl Shading {
    fn new(opts: &WblendOptions) -> Self {
        let floor = opts.screen_floor_px;
        Self {
            planar: opts.footprint == Footprint::Planar,
            plane_depth: opts.depth == DepthModel::Plane,
            cutoff2: opts.cutoff.max(0.1).powi(2),
            inv_floor2: if floor > 0.0 { 1.0 / (floor * floor) } else { 0.0 },
            near: opts.near,
        }
    }

    /// Squared Mahalanobis distance and depth of the fragment of `d` at the
    /// pixel offset `(dx, dy)` from its center.
    #[inline]
    fn fragment(&self, d: &Draw, dx: f32, dy: f32) -> (f32, f32) {
        let mut z = d.depth;
        let mut m = f32::INFINITY;
        if let Some(p) = &d.plane {
            let den = p.hn[0] * dx + p.hn[1] * dy + p.hn[2];
            let t = p.npc / den;
            // False for NaN and for intersections behind the eye.
            if t >= self.near && t.is_finite() {
                if self.plane_depth {
                    z = t.max(p.z_lo).min(p.z_hi);
                }
                if self.planar {
                    let inv = 1.0 / den;
                    let (u, v) = ((p.hu[0] * dx + p.hu[1] * dy) * inv, (p.hv[0] * dx + p.hv[1] * dy) * inv);
                    m = u * u + v * v;
                }
            }
        }
        if self.planar {
            if self.inv_floor2 > 0.0 {
                m = m.min((dx * dx + dy * dy) * self.inv_floor2);
            }
        } else {
            let q1 = dx * d.u1[0] + dy * d.u1[1];
            let q2 = dx * d.u2[0] + dy * d.u2[1];
            m = q1 * q1 + q2 * q2;
        }
        (m, z)
    }
}

/// Calls `f(pixel index in the band, dx, dy)` for the pixels of `d` in the
/// rows `row0..row_end`.
#[inline]
fn for_fragments(d: &Draw, row0: u32, row_end: u32, width: u32, mut f: impl FnMut(usize, f32, f32)) {
    for y in d.y0.max(row0)..d.y1.min(row_end) {
        let dy = y as f32 + 0.5 - d.py;
        let start = (y - row0) as usize * width as usize;
        for x in d.x0..d.x1 {
            f(start + x as usize, x as f32 + 0.5 - d.px, dy);
        }
    }
}

/// Sorted "over" of `order` (back to front) into one band.
fn band_over(rgb: &mut [f32], alpha: &mut [f32], row0: u32, width: u32, draws: &[Draw], order: &[u32], sh: &Shading) -> FragmentStats {
    let row_end = row0 + (alpha.len() / width as usize) as u32;
    let mut stats = FragmentStats::default();
    for &index in order {
        let d = &draws[index as usize];
        for_fragments(d, row0, row_end, width, |i, dx, dy| {
            let (m, _) = sh.fragment(d, dx, dy);
            // False for NaN too.
            if m <= sh.cutoff2 {
                stats.shaded += 1;
                let a = d.weight.min(1.0) * (-0.5 * m).exp();
                if a >= MIN_ALPHA {
                    stats.blended += 1;
                    let keep = 1.0 - a;
                    let dst = &mut rgb[i * 3..i * 3 + 3];
                    dst[0] = d.color[0] * a + dst[0] * keep;
                    dst[1] = d.color[1] * a + dst[1] * keep;
                    dst[2] = d.color[2] * a + dst[2] * keep;
                    alpha[i] = a + alpha[i] * keep;
                }
            }
        });
    }
    stats
}

/// Is a fragment at depth `z` part of the surface whose nearest visibility
/// fragment is at `nearest`? Always true where no disc reaches (infinity).
#[inline]
fn on_surface(z: f32, nearest: f32, params: &Normalized, slack: f32) -> bool {
    z <= nearest * (1.0 + params.depth_tol) + slack
        && (params.near_fringe || !nearest.is_finite() || z >= nearest * (1.0 - params.depth_tol) - slack)
}

/// The three passes of normalised blending for one band.
#[allow(clippy::too_many_arguments)]
fn band_normalized(
    rgb: &mut [f32],
    alpha: &mut [f32],
    row0: u32,
    width: u32,
    draws: &[Draw],
    bin: &[u32],
    sh: &Shading,
    params: &Normalized,
) -> FragmentStats {
    let row_end = row0 + (alpha.len() / width as usize) as u32;
    let mut stats = FragmentStats::default();
    let vis2 = params.vis_cutoff.max(0.0).powi(2).min(sh.cutoff2);
    let two_layers = params.silhouette == Silhouette::SoftOverNextSurface;
    let mut nearest = vec![f32::INFINITY; alpha.len()];
    for &index in bin {
        let d = &draws[index as usize];
        for_fragments(d, row0, row_end, width, |i, dx, dy| {
            let (m, z) = sh.fragment(d, dx, dy);
            if m <= vis2 {
                stats.visibility += 1;
                if z < nearest[i] {
                    nearest[i] = z;
                }
            }
        });
    }
    // Second visibility pass: the nearest surface behind the first.
    let mut second = Vec::new();
    let (mut back_rgb, mut back_weight) = (Vec::new(), Vec::new());
    if two_layers {
        second = vec![f32::INFINITY; alpha.len()];
        back_rgb = vec![0.0f32; rgb.len()];
        back_weight = vec![0.0f32; alpha.len()];
        for &index in bin {
            let d = &draws[index as usize];
            let slack = params.depth_tol_sigma * d.sigma;
            for_fragments(d, row0, row_end, width, |i, dx, dy| {
                let (m, z) = sh.fragment(d, dx, dy);
                if m <= vis2 {
                    stats.visibility += 1;
                    if z < second[i] && !on_surface(z, nearest[i], params, slack) {
                        second[i] = z;
                    }
                }
            });
        }
    }
    let floor = if params.taper { (-0.5 * sh.cutoff2).exp() } else { 0.0 };
    let gain = 1.0 / (1.0 - floor);
    // `alpha` holds the weight sum until the resolve. The coverage is
    // accumulated apart, without the layer weights: a layer's priority must
    // not fatten its outline.
    let mut cover = vec![0.0f32; alpha.len()];
    let mut back_cover = vec![0.0f32; back_weight.len()];
    for &index in bin {
        let d = &draws[index as usize];
        let slack = params.depth_tol_sigma * d.sigma;
        for_fragments(d, row0, row_end, width, |i, dx, dy| {
            let (m, z) = sh.fragment(d, dx, dy);
            if m <= sh.cutoff2 {
                stats.shaded += 1;
                let kernel = ((-0.5 * m).exp() - floor) * gain;
                let w = d.weight * kernel;
                if on_surface(z, nearest[i], params, slack) {
                    stats.blended += 1;
                    let dst = &mut rgb[i * 3..i * 3 + 3];
                    dst[0] += d.color[0] * w;
                    dst[1] += d.color[1] * w;
                    dst[2] += d.color[2] * w;
                    alpha[i] += w;
                    cover[i] += d.cover * kernel;
                } else if two_layers && on_surface(z, second[i], params, slack) {
                    stats.blended += 1;
                    let dst = &mut back_rgb[i * 3..i * 3 + 3];
                    dst[0] += d.color[0] * w;
                    dst[1] += d.color[1] * w;
                    dst[2] += d.color[2] * w;
                    back_weight[i] += w;
                    back_cover[i] += d.cover * kernel;
                }
            }
        });
    }
    for (i, a) in alpha.iter_mut().enumerate() {
        let weight = *a;
        let front = if weight <= 0.0 {
            0.0
        } else if params.silhouette == Silhouette::Hard && nearest[i].is_finite() {
            1.0
        } else {
            params.coverage.alpha(cover[i])
        };
        let scale = if weight > 0.0 { front / weight } else { 0.0 };
        let mut coverage = front;
        let pixel = &mut rgb[i * 3..i * 3 + 3];
        for v in pixel.iter_mut() {
            *v *= scale;
        }
        if two_layers && back_weight[i] > 0.0 && front < 1.0 {
            // The surface behind is opaque where its own discs reach.
            let back = if second[i].is_finite() { 1.0 } else { params.coverage.alpha(back_cover[i]) };
            let through = (1.0 - front) * back;
            for (c, v) in pixel.iter_mut().enumerate() {
                *v += through * back_rgb[i * 3 + c] / back_weight[i];
            }
            coverage += through;
        }
        *a = coverage;
    }
    stats
}

/// The colour and coverage rows of one band.
type BandBuffers<'a> = (&'a mut [f32], &'a mut [f32]);

/// Renders several layers as seen by `cam`. See the module documentation.
pub fn render(layers: &[Layer], cam: &Camera, opts: &WblendOptions) -> Rendered {
    let (width, height) = (cam.width, cam.height);
    let pixels = width as usize * height as usize;
    let mut out = Rendered { width, height, rgb: vec![0.0; pixels * 3], alpha: vec![0.0; pixels], stats: FragmentStats::default() };
    if pixels == 0 {
        return out;
    }
    let view = View::new(cam);
    let mut draws: Vec<Draw> = Vec::new();
    for layer in layers {
        #[cfg(feature = "par")]
        draws.par_extend(layer.splats.par_iter().filter_map(|s| prepare(s, layer.weight, &view, opts)));
        #[cfg(not(feature = "par"))]
        draws.extend(layer.splats.iter().filter_map(|s| prepare(s, layer.weight, &view, opts)));
    }
    let mut order: Vec<u32> = Vec::new();
    match opts.blend {
        Blend::Over => DepthSorter::new().sort_by_depth(draws.iter().map(|d| d.depth), &mut order),
        Blend::Normalized(_) => order.extend(0..draws.len() as u32),
    }

    // Horizontal bands; every band gets, in order, the splats touching it.
    #[cfg(feature = "par")]
    let threads = rayon::current_num_threads();
    #[cfg(not(feature = "par"))]
    let threads = 1;
    let band_rows = (height as usize).div_ceil((threads * 4).clamp(1, height as usize)).max(1) as u32;
    let band_count = height.div_ceil(band_rows) as usize;
    let mut bins: Vec<Vec<u32>> = vec![Vec::new(); band_count];
    for &index in &order {
        let d = &draws[index as usize];
        for bin in &mut bins[(d.y0 / band_rows) as usize..=((d.y1 - 1) / band_rows) as usize] {
            bin.push(index);
        }
    }
    let shading = Shading::new(opts);
    let work = |(band, ((rgb, alpha), bin)): (usize, (BandBuffers, &Vec<u32>))| {
        let row0 = band as u32 * band_rows;
        match &opts.blend {
            Blend::Over => band_over(rgb, alpha, row0, width, &draws, bin, &shading),
            Blend::Normalized(params) => band_normalized(rgb, alpha, row0, width, &draws, bin, &shading, params),
        }
    };
    let (rgb_len, alpha_len) = (band_rows as usize * width as usize * 3, band_rows as usize * width as usize);
    #[cfg(feature = "par")]
    let per_band: Vec<FragmentStats> =
        out.rgb.par_chunks_mut(rgb_len).zip(out.alpha.par_chunks_mut(alpha_len)).zip(bins.par_iter()).enumerate().map(work).collect();
    #[cfg(not(feature = "par"))]
    let per_band: Vec<FragmentStats> =
        out.rgb.chunks_mut(rgb_len).zip(out.alpha.chunks_mut(alpha_len)).zip(bins.iter()).enumerate().map(work).collect();
    out.stats.splats = draws.len() as u64;
    for s in per_band {
        out.stats.visibility += s.visibility;
        out.stats.shaded += s.shaded;
        out.stats.blended += s.blended;
    }
    out
}

/// Renders `splats` as seen by `cam` over [`WblendOptions::background`]: the
/// interface of [`crate::raster::rasterize`].
pub fn rasterize(splats: &[Splat], cam: &Camera, opts: &WblendOptions) -> Image {
    render(&[Layer { splats, weight: 1.0 }], cam, opts).over(opts.background)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::{self, RasterOptions};
    use crate::splat::test_util::Rng;

    /// Camera at the origin looking down −Z, 200×160, fx = fy = 200.
    fn camera() -> Camera {
        let mut cam = Camera::look_at(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y, 1.0, 200, 160);
        cam.fx = 200.0;
        cam.fy = 200.0;
        cam
    }

    fn disc(pos: Vec3, normal: Vec3, sigma: f32, rgba: [u8; 4]) -> Splat {
        Splat::surfel(pos, normal, sigma, sigma * 0.05, rgba)
    }

    fn normalized(params: Normalized) -> WblendOptions {
        WblendOptions { blend: Blend::Normalized(params), ..WblendOptions::default() }
    }

    fn max_difference(a: &Image, b: &Image) -> u8 {
        a.rgb.iter().zip(&b.rgb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
    }

    #[test]
    fn sorted_options_reproduce_the_reference_rasterizer() {
        let cam = camera();
        let mut rng = Rng::new(8);
        // Down to a tenth of a pixel, where the gain of the low-pass matters.
        let splats: Vec<Splat> = (0..600)
            .map(|_| {
                let mut s = rng.splat(1.0, 0.001, 0.1);
                s.pos[2] -= 3.0;
                s
            })
            .collect();
        for background in [[0, 0, 0], [18, 20, 28]] {
            let expected = raster::rasterize(&splats, &cam, &RasterOptions { background, ..Default::default() });
            let got = rasterize(&splats, &cam, &WblendOptions { background, ..WblendOptions::sorted() });
            assert!(max_difference(&expected, &got) <= 1, "differs by {}", max_difference(&expected, &got));
            // And the two filters do differ on this cloud.
            let classic = rasterize(&splats, &cam, &WblendOptions { background, ..WblendOptions::default() });
            assert!(max_difference(&classic, &got) > 20);
        }
    }

    #[test]
    fn point_mode_draws_dots_of_fixed_size() {
        let cam = camera();
        // Very different world sizes and depths, the same dot on screen.
        let a = disc(cam.unproject(50.5, 80.5, 1.0), Vec3::new(0.6, 0.0, 0.8), 0.5, [255; 4]);
        let b = disc(cam.unproject(150.5, 80.5, 6.0), Vec3::Z, 0.001, [255; 4]);
        let sorted = WblendOptions { point_sigma_px: 1.0, ..WblendOptions::sorted() };
        let expected = raster::rasterize(&[a, b], &cam, &RasterOptions { point_mode: true, point_sigma_px: 1.0, ..Default::default() });
        assert!(max_difference(&rasterize(&[a, b], &cam, &sorted), &expected) <= 1);
        // Normalised: opaque dots of the visibility radius (1.6 px) with a
        // faint fringe that ends at the cutoff (3 px), the nearer one in
        // front, each at the depth of its center.
        let hard = WblendOptions { point_sigma_px: 1.0, ..WblendOptions::normalized_hard() };
        let image = rasterize(&[a, b], &cam, &hard);
        for cx in [50u32, 150] {
            assert_eq!(image.pixel(cx, 80), [255; 3]);
            assert_eq!(image.pixel(cx + 1, 80), [255; 3]);
            // (e⁻² − e⁻⁴·⁵) / (1 − e⁻⁴·⁵) of the weight, over τ = 0.5.
            assert!((image.pixel(cx + 2, 80)[0] as f32 - 64.2).abs() < 1.5, "{:?}", image.pixel(cx + 2, 80));
            assert_eq!(image.pixel(cx + 3, 80), [0; 3]);
        }
        let behind = disc(cam.unproject(50.5, 80.5, 1.2), Vec3::Z, 0.3, [255, 0, 0, 255]);
        assert_eq!(rasterize(&[behind, a], &cam, &hard).pixel(50, 80), [255; 3]);
    }

    #[test]
    fn presets_are_the_documented_configurations() {
        let hard = WblendOptions::normalized_hard();
        let Blend::Normalized(h) = hard.blend else { panic!("not normalised") };
        assert_eq!((h.vis_cutoff, h.silhouette, h.coverage, h.near_fringe), (1.6, Silhouette::Hard, Coverage::Linear(0.5), false));
        assert_eq!((hard.depth, hard.footprint, hard.screen_floor_px), (DepthModel::Plane, Footprint::Ewa, 0.0));
        assert_eq!(hard.low_pass, LowPass::Normalized(crate::splat::LOW_PASS_PX2));
        let soft = WblendOptions::normalized_soft();
        let Blend::Normalized(s) = soft.blend else { panic!("not normalised") };
        assert_eq!((s.vis_cutoff, s.silhouette, s.coverage), (2.0, Silhouette::SoftOverNextSurface, Coverage::Linear(0.5)));
        assert_eq!(WblendOptions { blend: hard.blend, ..soft }, hard);
        // The two levels differ in the silhouette rule and the disc radius only.
        assert_eq!(Normalized { vis_cutoff: h.vis_cutoff, silhouette: h.silhouette, ..s }, h);
        assert_eq!(WblendOptions::sorted().blend, Blend::Over);
        assert_eq!(LIVE_LAYER_WEIGHT, 16.0);
    }

    #[test]
    fn a_scaled_view_renders_like_a_scaled_scene() {
        // The miniature mode of the viewer puts a uniform scale into the
        // camera pose: the picture must be the one of the scaled scene.
        let cam = camera();
        let mut rng = Rng::new(9);
        let splats: Vec<Splat> = (0..300)
            .map(|_| {
                let mut s = Splat::surfel(rng.vec3(-0.6, 0.6), rng.quat() * Vec3::Z, rng.range(0.02, 0.08), 0.004, [0, 0, 0, 255]);
                s.pos[2] -= 3.0;
                s.rgba = [rng.below(256) as u8, rng.below(256) as u8, rng.below(256) as u8, 255];
                s
            })
            .collect();
        let factor = 15.0;
        let big: Vec<Splat> = splats.iter().map(|s| Splat { pos: s.pos.map(|v| v * factor), scale: s.scale.map(|v| v * factor), ..*s }).collect();
        let mut scaled = cam;
        scaled.world_from_cam = glam::Affine3A::from_mat4(glam::Mat4::from_scale(Vec3::splat(factor)) * glam::Mat4::from(cam.world_from_cam));
        for opts in [WblendOptions::normalized_hard(), WblendOptions::normalized_soft(), WblendOptions::sorted()] {
            let a = rasterize(&splats, &cam, &opts);
            let b = rasterize(&big, &scaled, &opts);
            let off = a.rgb.iter().zip(&b.rgb).filter(|(x, y)| x.abs_diff(**y) > 2).count();
            assert!(off < a.rgb.len() / 500, "{off} of {} channels differ", a.rgb.len());
            assert!(a.rgb.iter().filter(|&&v| v > 0).count() > 5_000);
        }
    }

    #[test]
    fn default_options_reproduce_the_reference_rasterizer() {
        let cam = camera();
        let mut rng = Rng::new(7);
        let splats: Vec<Splat> = (0..400)
            .map(|_| {
                let mut s = rng.splat(1.0, 0.005, 0.15);
                s.pos[2] -= 3.0;
                s
            })
            .collect();
        for background in [[0, 0, 0], [255, 255, 255], [18, 20, 28]] {
            let expected = raster::rasterize(&splats, &cam, &RasterOptions { background, classic_dilation: true, ..Default::default() });
            let got = rasterize(&splats, &cam, &WblendOptions { background, ..Default::default() });
            assert!(max_difference(&expected, &got) <= 1, "differs by {}", max_difference(&expected, &got));
        }
        // And the coverage is the accumulated opacity.
        let rendered = render(&[Layer { splats: &splats, weight: 1.0 }], &cam, &WblendOptions::default());
        let classic = RasterOptions { classic_dilation: true, ..Default::default() };
        let black = raster::rasterize(&splats, &cam, &RasterOptions { background: [0; 3], ..classic });
        let white = raster::rasterize(&splats, &cam, &RasterOptions { background: [255; 3], ..classic });
        for i in 0..rendered.alpha.len() {
            let transparency = (white.rgb[i * 3] as f32 - black.rgb[i * 3] as f32) / 255.0;
            assert!((1.0 - rendered.alpha[i] - transparency).abs() < 0.01);
        }
        assert!(rendered.stats.splats > 300 && rendered.stats.blended > 10_000 && rendered.stats.visibility == 0);
    }

    #[test]
    fn normalized_blending_averages_one_surface_whatever_the_order() {
        let cam = camera();
        // Two discs on the plane z = −2, 10 px apart, σ = 10 px.
        let red = disc(cam.unproject(95.5, 80.5, 2.0), Vec3::Z, 0.1, [255, 0, 0, 255]);
        let blue = disc(cam.unproject(105.5, 80.5, 2.0), Vec3::Z, 0.1, [0, 0, 255, 255]);
        let opts = normalized(Normalized { taper: false, ..Default::default() });
        let a = rasterize(&[red, blue], &cam, &opts);
        let b = rasterize(&[blue, red], &cam, &opts);
        assert_eq!(a, b);
        // Half-way both weigh the same: the mean colour, fully opaque.
        let mid = a.pixel(100, 80);
        assert!(mid[0].abs_diff(128) <= 2 && mid[2].abs_diff(128) <= 2 && mid[1] == 0, "{mid:?}");
        // At the red center: weights 1 and exp(−0.5) (low-pass aside).
        let w = (-0.5f32 * 100.0 / 100.3).exp();
        let expected = 255.0 / (1.0 + w);
        assert!((a.pixel(95, 80)[0] as f32 - expected).abs() < 1.5, "{:?} vs {expected}", a.pixel(95, 80));
        // "Over" depends on which of two splats at the same depth comes last.
        let over = rasterize(&[red, blue], &cam, &WblendOptions::default());
        assert!(over.pixel(100, 80) != mid);
    }

    #[test]
    fn hidden_surfaces_do_not_contribute_and_fringes_blend() {
        let cam = camera();
        let front = disc(cam.unproject(100.5, 80.5, 2.0), Vec3::Z, 0.05, [255, 0, 0, 255]);
        // A big green wall 0.5 m behind.
        let wall = disc(cam.unproject(100.5, 80.5, 2.5), Vec3::Z, 1.0, [0, 255, 0, 255]);
        let params = Normalized { vis_cutoff: 2.0, taper: false, ..Default::default() };
        let opts = WblendOptions { background: [0, 0, 255], ..normalized(params) };
        let image = rasterize(&[wall, front], &cam, &opts);
        // σ = 5 px: inside 2σ pure red, the wall is rejected by the depth test.
        assert_eq!(image.pixel(100, 80), [255, 0, 0]);
        assert_eq!(image.pixel(108, 80), [255, 0, 0]);
        // Between 2σ and 3σ the fringe of the front splat mixes with the wall.
        let fringe = image.pixel(112, 80);
        assert!(fringe[0] > 5 && fringe[0] < 60 && fringe[1] > 190, "{fringe:?}");
        // Beyond 3σ only the wall, and no background shines through it.
        assert_eq!(image.pixel(120, 80), [0, 255, 0]);
        // With a two-sided test the fringe is dropped: the wall is untouched.
        let two_sided = WblendOptions { background: [0, 0, 255], ..normalized(Normalized { near_fringe: false, ..params }) };
        let image = rasterize(&[wall, front], &cam, &two_sided);
        assert_eq!(image.pixel(112, 80), [0, 255, 0]);
        assert_eq!(image.pixel(108, 80), [255, 0, 0]);
        // A second surface inside the tolerance blends, outside it does not.
        let close = disc(cam.unproject(100.5, 80.5, 2.01), Vec3::Z, 0.05, [0, 0, 255, 255]);
        let both = rasterize(&[front, close], &cam, &opts);
        assert!(both.pixel(100, 80)[2] > 100, "{:?}", both.pixel(100, 80));
        let tight = normalized(Normalized { depth_tol: 0.001, ..params });
        assert_eq!(rasterize(&[front, close], &cam, &tight).pixel(100, 80), [255, 0, 0]);
    }

    #[test]
    fn coverage_is_full_inside_visibility_discs_and_soft_outside() {
        let cam = camera();
        let s = disc(cam.unproject(100.5, 80.5, 2.0), Vec3::Z, 0.05, [200, 100, 50, 255]);
        let params = Normalized { vis_cutoff: 1.5, coverage: Coverage::Linear(0.5), taper: false, ..Default::default() };
        let rendered = render(&[Layer { splats: &[s], weight: 1.0 }], &cam, &normalized(params));
        let at = |x: usize| rendered.alpha[80 * 200 + x];
        assert_eq!(at(100), 1.0);
        assert_eq!(at(107), 1.0); // 1.3σ
        // 2σ: outside the disc, weight exp(−2) = 0.135 → 0.27.
        assert!((at(110) - 2.0 * (-0.5f32 * 100.0 / 25.3).exp()).abs() < 0.02, "{}", at(110));
        assert_eq!(at(117), 0.0);
        // The colour is not darkened by the normalisation.
        let image = rendered.over([0, 0, 0]);
        assert_eq!(image.pixel(107, 80), [200, 100, 50]);
        let hard = Normalized { coverage: Coverage::None, ..params };
        let rendered = render(&[Layer { splats: &[s], weight: 1.0 }], &cam, &normalized(hard));
        assert_eq!(rendered.alpha[80 * 200 + 110], 0.0);
        assert!(rendered.stats.visibility > 0 && rendered.stats.blended >= rendered.stats.visibility);
    }

    #[test]
    fn soft_silhouettes_show_the_next_surface_not_the_background() {
        let cam = camera();
        let front = disc(cam.unproject(100.5, 80.5, 2.0), Vec3::Z, 0.05, [255, 0, 0, 255]);
        let wall = disc(cam.unproject(100.5, 80.5, 2.5), Vec3::Z, 1.0, [0, 255, 0, 255]);
        let hidden = disc(cam.unproject(100.5, 80.5, 3.5), Vec3::Z, 1.0, [255, 255, 255, 255]);
        let params = Normalized { vis_cutoff: 2.0, coverage: Coverage::Linear(0.5), taper: false, ..Default::default() };
        let render_with = |silhouette| {
            let opts = WblendOptions { background: [0, 0, 255], ..normalized(Normalized { silhouette, ..params }) };
            rasterize(&[hidden, wall, front], &cam, &opts)
        };
        // σ = 5 px. At 1.8σ (inside the visibility disc) the red splat weighs
        // exp(−1.62) = 0.198: coverage 0.396 under the soft rules.
        let hard = render_with(Silhouette::Hard).pixel(109, 80);
        assert_eq!(hard, [255, 0, 0]);
        let coverage = 2.0 * (-0.5f32 * 81.0 / 25.3).exp();
        let over_background = render_with(Silhouette::SoftOverBackground).pixel(109, 80);
        assert_close(over_background, [255.0 * coverage, 0.0, 255.0 * (1.0 - coverage)], 1.5);
        let over_next = render_with(Silhouette::SoftOverNextSurface).pixel(109, 80);
        assert_close(over_next, [255.0 * coverage, 255.0 * (1.0 - coverage), 0.0], 1.5);
        // At the center the front surface is opaque under every rule, and the
        // third surface never shows.
        for silhouette in [Silhouette::Hard, Silhouette::SoftOverBackground, Silhouette::SoftOverNextSurface] {
            let image = render_with(silhouette);
            assert_eq!(image.pixel(100, 80), [255, 0, 0]);
            assert_eq!(image.pixel(150, 80), [0, 255, 0]);
        }
    }

    fn assert_close(actual: [u8; 3], expected: [f32; 3], tolerance: f32) {
        for c in 0..3 {
            assert!((actual[c] as f32 - expected[c]).abs() <= tolerance, "{actual:?} vs {expected:?}");
        }
    }

    #[test]
    fn a_heavier_layer_dominates_a_coplanar_one() {
        let cam = camera();
        let floor = disc(cam.unproject(100.5, 80.5, 2.0), Vec3::Z, 0.2, [200, 200, 200, 255]);
        let shadow = disc(cam.unproject(100.5, 80.5, 1.995), Vec3::Z, 0.2, [40, 40, 40, 255]);
        let opts = normalized(Normalized::default());
        let equal = render(&[Layer { splats: &[floor], weight: 1.0 }, Layer { splats: &[shadow], weight: 1.0 }], &cam, &opts);
        let heavy = render(&[Layer { splats: &[floor], weight: 1.0 }, Layer { splats: &[shadow], weight: 16.0 }], &cam, &opts);
        let level = |r: &Rendered| r.over([0; 3]).pixel(100, 80)[0];
        assert!(level(&equal).abs_diff(120) <= 2, "{}", level(&equal));
        assert!(level(&heavy).abs_diff(49) <= 2, "{}", level(&heavy));
    }

    #[test]
    fn a_layer_weight_does_not_fatten_the_outline() {
        let cam = camera();
        let s = disc(cam.unproject(100.5, 80.5, 2.0), Vec3::Z, 0.05, [200, 100, 50, 255]);
        for silhouette in [Silhouette::Hard, Silhouette::SoftOverBackground, Silhouette::SoftOverNextSurface] {
            let opts = normalized(Normalized { silhouette, ..Default::default() });
            let light = render(&[Layer { splats: &[s], weight: 1.0 }], &cam, &opts);
            let heavy = render(&[Layer { splats: &[s], weight: 16.0 }], &cam, &opts);
            for (a, b) in light.alpha.iter().zip(&heavy.alpha) {
                assert!((a - b).abs() < 1e-5, "{silhouette:?}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn planar_footprint_is_the_ray_plane_intersection() {
        let cam = camera();
        // A big disc tilted 70 degrees about the vertical axis.
        let angle = 70f32.to_radians();
        let normal = Vec3::new(angle.sin(), 0.0, angle.cos());
        let center = Vec3::new(0.1, 0.05, -1.5);
        let s = disc(center, normal, 0.3, [255, 255, 255, 255]);
        let opts = WblendOptions { footprint: Footprint::Planar, ..WblendOptions::default() };
        let rendered = render(&[Layer { splats: &[s], weight: 1.0 }], &cam, &opts);
        let rotation = s.rotation();
        let (a1, a2) = (rotation * Vec3::X, rotation * Vec3::Y);
        let mut checked = 0;
        for (x, y) in [(100u32, 80u32), (90, 70), (120, 95), (60, 80), (140, 60), (114, 86), (30, 30)] {
            let (origin, dir) = cam.ray(x as f32 + 0.5, y as f32 + 0.5);
            let t = normal.dot(center - origin) / normal.dot(dir);
            let hit = origin + dir * t - center;
            let m = (hit.dot(a1) / 0.3).powi(2) + (hit.dot(a2) / 0.3).powi(2);
            let expected = if m <= 9.0 && (-0.5 * m).exp() >= MIN_ALPHA { (-0.5 * m).exp() } else { 0.0 };
            let got = rendered.alpha[(y * 200 + x) as usize];
            assert!((got - expected).abs() < 2e-3, "({x}, {y}): {got} vs {expected}");
            checked += usize::from(expected > 0.05);
        }
        assert!(checked >= 4);
        // Fronto-parallel and small, the two footprints agree (the EWA
        // low-pass aside).
        let flat = disc(cam.unproject(100.5, 80.5, 2.0), Vec3::Z, 0.05, [255, 255, 255, 255]);
        let ewa = WblendOptions { low_pass: LowPass::Dilate(1.0e-4), ..WblendOptions::default() };
        let a = rasterize(&[flat], &cam, &ewa);
        let b = rasterize(&[flat], &cam, &WblendOptions { footprint: Footprint::Planar, ..ewa });
        assert!(max_difference(&a, &b) <= 1);
    }

    #[test]
    fn plane_depth_follows_a_slanted_splat() {
        let cam = camera();
        // A red disc slanted 60 degrees, and a blue disc parallel to the
        // image plane through its center. Left of the center the red plane
        // is nearer, right of it the blue one.
        let angle = 60f32.to_radians();
        let center = cam.unproject(100.5, 80.5, 2.0);
        let red = disc(center, Vec3::new(angle.sin(), 0.0, angle.cos()), 0.2, [255, 0, 0, 255]);
        let blue = disc(center, Vec3::Z, 0.2, [0, 0, 255, 255]);
        let params = Normalized { depth_tol: 0.002, ..Default::default() };
        let plane = WblendOptions { depth: DepthModel::Plane, ..normalized(params) };
        let image = rasterize(&[red, blue], &cam, &plane);
        assert_eq!(image.pixel(92, 80), [255, 0, 0]);
        assert_eq!(image.pixel(108, 80), [0, 0, 255]);
        // With center depths the two discs are one surface everywhere.
        let image = rasterize(&[red, blue], &cam, &normalized(params));
        assert!(image.pixel(108, 80)[0] > 30 && image.pixel(108, 80)[2] > 30);
    }

    #[test]
    fn the_normalised_mip_filter_conserves_energy() {
        let cam = camera();
        // σ = 0.2 px: far smaller than a pixel.
        let tiny = Splat { pos: cam.unproject(100.5, 80.5, 2.0).to_array(), scale: [0.002; 3], rot: [0.0, 0.0, 0.0, 1.0], rgba: [255; 4] };
        // Averaged over sub-pixel positions of the center.
        let sum = |low_pass| {
            let opts = WblendOptions { low_pass, ..WblendOptions::default() };
            let mut total = 0.0;
            for k in 0..16 {
                let mut s = tiny;
                s.pos = cam.unproject(100.0 + (k % 4) as f32 / 4.0, 80.0 + (k / 4) as f32 / 4.0, 2.0).to_array();
                total += render(&[Layer { splats: &[s], weight: 1.0 }], &cam, &opts).alpha.iter().sum::<f32>();
            }
            total / 16.0
        };
        let exact = std::f32::consts::TAU * 0.04;
        // Dilation inflates the splat to 2π·(0.04 + 0.3) px².
        assert!(sum(LowPass::Dilate(0.3)) > 6.0 * exact);
        let filtered = sum(LowPass::Normalized(0.1));
        assert!((filtered / exact - 1.0).abs() < 0.1, "{filtered} vs {exact}");
    }

    #[test]
    fn backfaces_can_be_culled_and_planar_splats_keep_a_floor() {
        let cam = camera();
        let away = disc(cam.unproject(100.5, 80.5, 2.0), Vec3::NEG_Z, 0.05, [255; 4]);
        assert!(rasterize(&[away], &cam, &WblendOptions::default()).pixel(100, 80)[0] > 250);
        let cull = WblendOptions { cull_backfaces: true, ..WblendOptions::default() };
        assert_eq!(rasterize(&[away], &cam, &cull).pixel(100, 80), [0, 0, 0]);
        // Exactly edge-on (on the optical axis): nothing without a floor,
        // a dot with one.
        let edge_on = disc(Vec3::new(0.0, 0.0, -2.0), Vec3::X, 0.05, [255; 4]);
        let planar = WblendOptions { footprint: Footprint::Planar, ..WblendOptions::default() };
        assert!(rasterize(&[edge_on], &cam, &planar).rgb.iter().all(|&v| v == 0));
        let floored = WblendOptions { screen_floor_px: 0.7, ..planar };
        assert!(rasterize(&[edge_on], &cam, &floored).pixel(100, 80)[0] > 100);
    }
}
