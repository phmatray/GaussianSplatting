//! The normalised blending mode on WebGL2: programs, render targets and the
//! passes of one view.
//!
//! The mode is specified in `docs/ARCHITECTURE.md` ("Blending modes") and
//! its reference implementation is `gs_core::experimental::wblend` with the
//! presets `WblendOptions::normalized_hard` and `normalized_soft`; the
//! numbers used here are read from those presets ([`Recipe`]), so the two
//! cannot drift apart silently.
//!
//! # Passes of one view
//!
//! All off-screen targets have the size of the view and are shared by every
//! view of that size (both eyes of a headset, one after the other).
//!
//! 1. **Visibility.** Depth-only draw of every splat as an opaque disc
//!    (1.6 σ hard, 2 σ soft) into a `DEPTH_COMPONENT24` texture,
//!    `depthFunc(LESS)`. The fragment depth is taken on the plane of the
//!    surfel and written logarithmically (`gl_FragDepth`, see
//!    [`crate::shaders`]).
//! 2. **Second visibility** (soft only). The same draw into a second depth
//!    texture; fragments on the first surface are discarded, which leaves
//!    the nearest surface behind it.
//! 3. **Accumulation.** Every splat again, 3 σ quads, `blendFunc(ONE, ONE)`,
//!    no depth attachment: the fragment shader compares its depth two-sided
//!    with the visibility texture(s) and adds `(w·colour, w)` to the
//!    `RGBA16F` target of the surface it lies on (the soft level has two).
//!    One more target (`RG16F` where the GPU renders to it, else `RGBA16F`)
//!    takes the coverage, which is accumulated without the layer weight:
//!    of each surface at the soft level; at the hard level of the pixels no
//!    visibility disc reaches, with a flag for the others.
//! 4. **Resolve.** One triangle into the destination (canvas, XR layer
//!    framebuffer or snapshot target) with `blendFunc(ONE,
//!    ONE_MINUS_SRC_ALPHA)`: premultiplied colour over whatever was cleared
//!    there. The destination needs no depth buffer and may be multisampled.
//!
//! Nothing is sorted and no order texture is touched: a layer is drawn in
//! storage order, one draw call per layer and pass, with the layer weight
//! as a uniform.
//!
//! # Requirements
//!
//! Rendering to `RGBA16F` needs `EXT_color_buffer_half_float` or
//! `EXT_color_buffer_float`; blending on it is core once it is renderable.
//! Everything else (depth textures, `gl_FragDepth`, three draw buffers,
//! `texelFetch`) is core WebGL2. Without the extension, or when a target
//! cannot be allocated, the viewer falls back to the sorted mode and says
//! why (`BlendStatus`).

use gs_core::experimental::wblend::{Blend, Coverage, DepthModel, Footprint, LowPass, Silhouette, WblendOptions};
use gs_core::splat::{ViewParams, CUTOFF_SIGMA, LOW_PASS_PX2};

use crate::params::{BlendQuality, LAYER_COUNT};
use crate::shaders::DEPTH_OCTAVES;

/// The numbers of a quality level, taken from its `wblend` preset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Recipe {
    /// Radius of the visibility discs, standard deviations.
    pub visibility_sigma: f32,
    /// Kernel cutoff, standard deviations.
    pub cutoff_sigma: f32,
    /// A fragment belongs to a surface when its depth is within this
    /// fraction of the surface's, on either side.
    pub depth_tolerance: f32,
    /// Accumulated weight at which a soft surface becomes opaque.
    pub coverage_tau: f32,
}

impl Recipe {
    /// The recipe of `quality`.
    ///
    /// # Panics
    /// When the preset asks for something the shaders do not implement: a
    /// preset may change its numbers, not its structure, without the
    /// shaders following (a unit test calls this for both levels).
    pub fn of(quality: BlendQuality) -> Self {
        let (preset, silhouette) = match quality {
            BlendQuality::Hard => (WblendOptions::normalized_hard(), Silhouette::Hard),
            BlendQuality::Soft => (WblendOptions::normalized_soft(), Silhouette::SoftOverNextSurface),
        };
        let Blend::Normalized(n) = preset.blend else { panic!("the preset is not normalised") };
        let coverage_tau = match n.coverage {
            Coverage::Linear(tau) if tau > 0.0 => tau,
            other => panic!("coverage rule {other:?} is not implemented by the shaders"),
        };
        assert!(
            n.silhouette == silhouette
                && !n.near_fringe
                && n.taper
                && n.depth_tol_sigma == 0.0
                && preset.depth == DepthModel::Plane
                && preset.footprint == Footprint::Ewa
                && preset.low_pass == LowPass::Normalized(LOW_PASS_PX2)
                && preset.screen_floor_px == 0.0
                && preset.cutoff == CUTOFF_SIGMA
                && preset.kernel_scale == 1.0,
            "the shaders do not implement this preset: {preset:?}"
        );
        assert!(n.depth_tol > 0.0 && n.depth_tol < 0.5 && n.vis_cutoff > 0.0 && n.vis_cutoff <= preset.cutoff);
        Self { visibility_sigma: n.vis_cutoff, cutoff_sigma: preset.cutoff, depth_tolerance: n.depth_tol, coverage_tau }
    }

    /// The two-sided depth tolerance in the logarithmic encoding: how far
    /// behind and how far in front of a surface a fragment may lie.
    pub fn encoded_tolerance(&self) -> [f32; 2] {
        [(1.0 + self.depth_tolerance).log2() / DEPTH_OCTAVES, -(1.0 - self.depth_tolerance).log2() / DEPTH_OCTAVES]
    }

    /// Value of the kernel at the cutoff and the factor that brings the
    /// tapered kernel back to 1 at the center.
    pub fn taper(&self) -> [f32; 2] {
        let floor = (-0.5 * self.cutoff_sigma * self.cutoff_sigma).exp();
        [floor, 1.0 / (1.0 - floor)]
    }
}

/// The larger of the two layer weights as the shaders see it.
///
/// Only the ratio of the layer weights matters (`Σ w·c / Σ w`; the coverage
/// is accumulated without them), but their size decides whether the sums fit
/// a half-float target: its largest value is 65504 and it has eleven bits of
/// precision. With the larger weight at 16 a pixel overflows only where four
/// thousand opaque splats lie on top of each other, and the weaker layer
/// keeps its precision down to a ratio of about a thousand to one.
const SHADER_WEIGHT: f32 = 16.0;

/// The layer weights the accumulation pass is given: `weights` rescaled so
/// that the larger one is [`SHADER_WEIGHT`]. A layer of weight zero stays at
/// zero (it hides what is behind it and adds no colour, as in the
/// reference); negative and non-finite weights count as zero.
pub(crate) fn shader_weights(weights: [f32; LAYER_COUNT]) -> [f32; LAYER_COUNT] {
    let weights = weights.map(|w| if w.is_finite() && w > 0.0 { w } else { 0.0 });
    let largest = weights.iter().copied().fold(0.0, f32::max);
    if largest > 0.0 {
        // Divide first: the quotient is at most 1, whatever the weights.
        weights.map(|w| w / largest * SHADER_WEIGHT)
    } else {
        weights
    }
}

/// View-dependent uniforms the surfel vertex shader needs beyond
/// [`ViewParams`]: the eye in world space and the view units per world unit.
pub(crate) fn eye_and_scale(vp: &ViewParams) -> (glam::Vec3, f32) {
    let scale = vp.view.x_axis.truncate().length();
    (vp.view.inverse().w_axis.truncate(), scale)
}

#[cfg(target_arch = "wasm32")]
pub(crate) use gl::{LayerDraw, Normalized, Output, ViewDraw};

#[cfg(target_arch = "wasm32")]
mod gl {
    use gs_core::splat::{ViewParams, CUTOFF_SIGMA};
    use wasm_bindgen::JsValue;
    use web_sys::{
        WebGl2RenderingContext as GL, WebGlFramebuffer, WebGlProgram, WebGlTexture, WebGlUniformLocation, WebGlVertexArrayObject,
    };

    use super::{eye_and_scale, Recipe};
    use crate::glutil::link_program;
    use crate::params::{BlendQuality, RenderParams};
    use crate::shaders::{accumulate_fs, resolve_fs, surfel_vs, visibility_fs, RESOLVE_VS};

    /// Texture units. Unit 0 holds the data texture of the layer being drawn.
    const UNIT_FIRST_DEPTH: u32 = 1;
    const UNIT_SECOND_DEPTH: u32 = 2;
    const UNIT_FRONT: u32 = 3;
    const UNIT_BACK: u32 = 4;
    const UNIT_COVER: u32 = 5;

    /// One layer to draw.
    pub(crate) struct LayerDraw<'a> {
        pub texture: &'a WebGlTexture,
        pub count: u32,
        /// Blending weight of the layer.
        pub weight: f32,
    }

    /// Where the resolved picture goes.
    pub(crate) struct Output<'a> {
        /// `None` is the canvas.
        pub framebuffer: Option<&'a WebGlFramebuffer>,
        /// `[x, y, width, height]` inside that framebuffer.
        pub viewport: [i32; 4],
    }

    /// Everything [`Normalized::draw`] needs to know about one view.
    pub(crate) struct ViewDraw<'a> {
        pub vp: &'a ViewParams,
        pub params: &'a RenderParams,
        /// Splats centered above this world height are hidden.
        pub clip_y: f32,
        pub quality: BlendQuality,
        pub layers: &'a [LayerDraw<'a>],
        /// The quad index buffer (covers every layer's splat count).
        pub vao: &'a WebGlVertexArrayObject,
        /// Splats per data texture row.
        pub splats_per_row: u32,
        pub output: Output<'a>,
    }

    /// A linked splat program with the uniforms of the shared vertex shader
    /// and of the surface fragment shaders. A uniform a variant does not use
    /// is optimized away: its location is `None` and setting it is a no-op.
    struct SurfelProgram {
        program: WebGlProgram,
        spr: Option<WebGlUniformLocation>,
        view: Option<WebGlUniformLocation>,
        focal: Option<WebGlUniformLocation>,
        center: Option<WebGlUniformLocation>,
        viewport: Option<WebGlUniformLocation>,
        near: Option<WebGlUniformLocation>,
        splat_scale: Option<WebGlUniformLocation>,
        max_radius: Option<WebGlUniformLocation>,
        point_radius: Option<WebGlUniformLocation>,
        clip_y: Option<WebGlUniformLocation>,
        extent: Option<WebGlUniformLocation>,
        cull_back: Option<WebGlUniformLocation>,
        eye: Option<WebGlUniformLocation>,
        view_scale: Option<WebGlUniformLocation>,
        tolerance: Option<WebGlUniformLocation>,
        weight: Option<WebGlUniformLocation>,
        taper: Option<WebGlUniformLocation>,
    }

    impl SurfelProgram {
        fn new(gl: &GL, vertex: &str, fragment: &str) -> Result<Self, JsValue> {
            let program = link_program(gl, vertex, fragment)?;
            let u = |name: &str| gl.get_uniform_location(&program, name);
            gl.use_program(Some(&program));
            // The samplers never change units.
            gl.uniform1i(u("u_data").as_ref(), 0);
            gl.uniform1i(u("u_first").as_ref(), UNIT_FIRST_DEPTH as i32);
            gl.uniform1i(u("u_second").as_ref(), UNIT_SECOND_DEPTH as i32);
            Ok(Self {
                spr: u("u_spr"),
                view: u("u_view"),
                focal: u("u_focal"),
                center: u("u_center"),
                viewport: u("u_viewport"),
                near: u("u_near"),
                splat_scale: u("u_splat_scale"),
                max_radius: u("u_max_radius"),
                point_radius: u("u_point_radius"),
                clip_y: u("u_clip_y"),
                extent: u("u_extent"),
                cull_back: u("u_cull_back"),
                eye: u("u_eye"),
                view_scale: u("u_view_scale"),
                tolerance: u("u_tolerance"),
                weight: u("u_weight"),
                taper: u("u_taper"),
                program,
            })
        }

        /// Makes the program current and sets everything that is the same
        /// for all layers of a pass. `extent` is the half-size of the quads
        /// in standard deviations.
        fn begin(&self, gl: &GL, view: &ViewDraw, recipe: &Recipe, extent: f32) {
            let (vp, params) = (view.vp, view.params);
            gl.use_program(Some(&self.program));
            gl.uniform1i(self.spr.as_ref(), view.splats_per_row as i32);
            gl.uniform_matrix4fv_with_f32_array(self.view.as_ref(), false, &vp.view.to_cols_array());
            gl.uniform2f(self.focal.as_ref(), vp.fx, vp.fy);
            gl.uniform2f(self.center.as_ref(), vp.cx, vp.cy);
            gl.uniform2f(self.viewport.as_ref(), vp.width, vp.height);
            gl.uniform1f(self.near.as_ref(), vp.near);
            gl.uniform1f(self.splat_scale.as_ref(), params.splat_scale);
            gl.uniform1f(self.max_radius.as_ref(), params.max_radius_px);
            gl.uniform1f(self.point_radius.as_ref(), if params.point_mode { CUTOFF_SIGMA * params.point_sigma_px } else { 0.0 });
            // Finite stand-in for "no clip": uniforms must not be infinite on every driver.
            gl.uniform1f(self.clip_y.as_ref(), view.clip_y.min(f32::MAX));
            gl.uniform1f(self.extent.as_ref(), extent);
            gl.uniform1f(self.cull_back.as_ref(), if params.cull_backfaces { 1.0 } else { 0.0 });
            let (eye, scale) = eye_and_scale(vp);
            gl.uniform3f(self.eye.as_ref(), eye.x, eye.y, eye.z);
            gl.uniform1f(self.view_scale.as_ref(), scale);
            let [behind, in_front] = recipe.encoded_tolerance();
            gl.uniform2f(self.tolerance.as_ref(), behind, in_front);
            let [floor, gain] = recipe.taper();
            gl.uniform2f(self.taper.as_ref(), floor, gain);
        }

        /// Draws every layer with the current program.
        fn draw_layers(&self, gl: &GL, layers: &[LayerDraw]) {
            gl.active_texture(GL::TEXTURE0);
            for layer in layers {
                gl.bind_texture(GL::TEXTURE_2D, Some(layer.texture));
                gl.uniform1f(self.weight.as_ref(), layer.weight);
                gl.draw_elements_with_i32(GL::TRIANGLES, 6 * layer.count as i32, GL::UNSIGNED_INT, 0);
            }
        }
    }

    struct ResolveProgram {
        program: WebGlProgram,
        origin: Option<WebGlUniformLocation>,
        inv_tau: Option<WebGlUniformLocation>,
    }

    impl ResolveProgram {
        fn new(gl: &GL, soft: bool) -> Result<Self, JsValue> {
            let program = link_program(gl, RESOLVE_VS, &resolve_fs(soft))?;
            let u = |name: &str| gl.get_uniform_location(&program, name);
            gl.use_program(Some(&program));
            gl.uniform1i(u("u_front").as_ref(), UNIT_FRONT as i32);
            gl.uniform1i(u("u_back").as_ref(), UNIT_BACK as i32);
            gl.uniform1i(u("u_cover").as_ref(), UNIT_COVER as i32);
            gl.uniform1i(u("u_second").as_ref(), UNIT_SECOND_DEPTH as i32);
            Ok(Self { origin: u("u_origin"), inv_tau: u("u_inv_tau"), program })
        }
    }

    /// The programs of one quality level.
    struct Level {
        /// One per visibility pass (the soft level has two).
        visibility: Vec<SurfelProgram>,
        accumulate: SurfelProgram,
        resolve: ResolveProgram,
    }

    impl Level {
        fn new(gl: &GL, soft: bool) -> Result<Self, JsValue> {
            let vs = surfel_vs();
            let mut visibility = vec![SurfelProgram::new(gl, &vs, &visibility_fs(false))?];
            if soft {
                visibility.push(SurfelProgram::new(gl, &vs, &visibility_fs(true))?);
            }
            Ok(Self { visibility, accumulate: SurfelProgram::new(gl, &vs, &accumulate_fs(soft))?, resolve: ResolveProgram::new(gl, soft)? })
        }

        fn destroy(&self, gl: &GL) {
            for p in self.visibility.iter().chain(std::iter::once(&self.accumulate)) {
                gl.delete_program(Some(&p.program));
            }
            gl.delete_program(Some(&self.resolve.program));
        }
    }

    /// The off-screen targets of one view size and quality level.
    struct Targets {
        width: u32,
        height: u32,
        soft: bool,
        /// One depth texture and its framebuffer per visibility pass.
        depth: Vec<(WebGlTexture, WebGlFramebuffer)>,
        /// `[front, cover]`, or `[front, back, cover]` for the soft level.
        color: Vec<WebGlTexture>,
        accumulate: WebGlFramebuffer,
    }

    /// Empties the error queue of the context. Bounded: a context holds one
    /// flag per error kind, and a lost one must not be able to hang the
    /// frame if it kept reporting its loss.
    fn clear_errors(gl: &GL) {
        for _ in 0..16 {
            if gl.get_error() == GL::NO_ERROR {
                break;
            }
        }
    }

    fn texture(gl: &GL, format: u32, width: u32, height: u32) -> Result<WebGlTexture, String> {
        let texture = gl.create_texture().ok_or("createTexture failed")?;
        gl.bind_texture(GL::TEXTURE_2D, Some(&texture));
        gl.tex_storage_2d(GL::TEXTURE_2D, 1, format, width as i32, height as i32);
        // Sampled with texelFetch only; depth and float textures must not be
        // filtered (the default MIN filter would make them incomplete).
        gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_MIN_FILTER, GL::NEAREST as i32);
        gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_MAG_FILTER, GL::NEAREST as i32);
        gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_WRAP_S, GL::CLAMP_TO_EDGE as i32);
        gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_WRAP_T, GL::CLAMP_TO_EDGE as i32);
        Ok(texture)
    }

    impl Targets {
        fn new(gl: &GL, width: u32, height: u32, soft: bool) -> Result<Self, String> {
            // Start from a clean error flag: an allocation failure below must
            // not be blamed on, or hidden by, an older error.
            clear_errors(gl);
            let mut targets = Self {
                width,
                height,
                soft,
                depth: Vec::new(),
                color: Vec::new(),
                accumulate: gl.create_framebuffer().ok_or("createFramebuffer failed")?,
            };
            let built = targets.build(gl);
            gl.bind_framebuffer(GL::FRAMEBUFFER, None);
            gl.bind_texture(GL::TEXTURE_2D, None);
            let error = gl.get_error();
            let built = match (built, error) {
                (Ok(()), GL::NO_ERROR) => Ok(()),
                (Ok(()), code) => Err(format!("WebGL error 0x{code:04x} while allocating {width}x{height} targets")),
                (Err(e), _) => Err(e),
            };
            match built {
                Ok(()) => Ok(targets),
                Err(e) => {
                    targets.destroy(gl);
                    Err(e)
                }
            }
        }

        fn build(&mut self, gl: &GL) -> Result<(), String> {
            let (width, height) = (self.width, self.height);
            let complete = |what: &str| match gl.check_framebuffer_status(GL::FRAMEBUFFER) {
                GL::FRAMEBUFFER_COMPLETE => Ok(()),
                status => Err(format!("{what} framebuffer of {width}x{height} is incomplete (0x{status:04x})")),
            };
            for _ in 0..if self.soft { 2 } else { 1 } {
                let depth = texture(gl, GL::DEPTH_COMPONENT24, width, height)?;
                let framebuffer = gl.create_framebuffer().ok_or("createFramebuffer failed")?;
                gl.bind_framebuffer(GL::FRAMEBUFFER, Some(&framebuffer));
                gl.framebuffer_texture_2d(GL::FRAMEBUFFER, GL::DEPTH_ATTACHMENT, GL::TEXTURE_2D, Some(&depth), 0);
                // Depth only.
                gl.draw_buffers(&js_sys::Array::of1(&GL::NONE.into()));
                gl.read_buffer(GL::NONE);
                self.depth.push((depth, framebuffer));
                complete("visibility")?;
            }

            gl.bind_framebuffer(GL::FRAMEBUFFER, Some(&self.accumulate));
            let attach = |index: u32, texture: &WebGlTexture| {
                gl.framebuffer_texture_2d(GL::FRAMEBUFFER, GL::COLOR_ATTACHMENT0 + index, GL::TEXTURE_2D, Some(texture), 0);
            };
            let buffers = js_sys::Array::new();
            for index in 0..if self.soft { 2 } else { 1 } {
                let sums = texture(gl, GL::RGBA16F, width, height)?;
                attach(index, &sums);
                self.color.push(sums);
                buffers.push(&(GL::COLOR_ATTACHMENT0 + index).into());
            }
            let cover_index = buffers.length();
            buffers.push(&(GL::COLOR_ATTACHMENT0 + cover_index).into());
            gl.draw_buffers(&buffers);
            // The coverage needs two channels. RG16F is renderable with
            // EXT_color_buffer_float; with only the half-float extension
            // that is up to the driver, so ask.
            let mut result = Ok(());
            for format in [GL::RG16F, GL::RGBA16F] {
                let cover = texture(gl, format, width, height)?;
                attach(cover_index, &cover);
                result = complete("accumulation");
                if result.is_ok() {
                    self.color.push(cover);
                    break;
                }
                gl.delete_texture(Some(&cover));
            }
            // Probing an unsupported format may have raised an error.
            if result.is_ok() {
                clear_errors(gl);
            }
            result
        }

        fn destroy(&self, gl: &GL) {
            for (texture, framebuffer) in &self.depth {
                gl.delete_framebuffer(Some(framebuffer));
                gl.delete_texture(Some(texture));
            }
            gl.delete_framebuffer(Some(&self.accumulate));
            for texture in &self.color {
                gl.delete_texture(Some(texture));
            }
        }
    }

    /// The GL side of the normalised mode.
    pub(crate) struct Normalized {
        hard: Level,
        soft: Level,
        targets: Option<Targets>,
        /// Near plane of the view whose surface the first visibility
        /// texture holds; `None` when it holds nothing usable.
        surface_near: Option<f32>,
    }

    impl Normalized {
        /// Compiles the programs. Call only when the context can render to
        /// half-float targets ([`Self::float_targets`]).
        pub fn new(gl: &GL) -> Result<Self, JsValue> {
            Ok(Self { hard: Level::new(gl, false)?, soft: Level::new(gl, true)?, targets: None, surface_near: None })
        }

        /// Enables rendering to half-float targets. `false` when the context
        /// offers neither extension.
        pub fn float_targets(gl: &GL) -> bool {
            ["EXT_color_buffer_half_float", "EXT_color_buffer_float"]
                .iter()
                .any(|name| matches!(gl.get_extension(name), Ok(Some(_))))
        }

        /// Size of the off-screen targets, if any are allocated.
        pub fn target_size(&self) -> Option<(u32, u32)> {
            self.targets.as_ref().map(|t| (t.width, t.height))
        }

        /// The visible surface of the view drawn last: its encoded depth
        /// texture and the near plane of the encoding. `None` when the last
        /// call of [`Self::draw`] drew nothing or [`Self::forget_surface`]
        /// was called since.
        pub fn surface(&self) -> Option<(&WebGlTexture, f32)> {
            let near = self.surface_near?;
            self.targets.as_ref().map(|t| (&t.depth[0].0, near))
        }

        /// Declares the surface of the last view stale (something else was
        /// drawn into the destination since).
        pub fn forget_surface(&mut self) {
            self.surface_near = None;
        }

        /// The targets for a view of `width × height`; reallocated when the
        /// size or the level changed.
        fn targets(&mut self, gl: &GL, width: u32, height: u32, soft: bool) -> Result<&Targets, String> {
            let reusable = self.targets.as_ref().is_some_and(|t| (t.width, t.height, t.soft) == (width, height, soft));
            if !reusable {
                if let Some(old) = self.targets.take() {
                    old.destroy(gl);
                }
                self.targets = Some(Targets::new(gl, width, height, soft)?);
            }
            Ok(self.targets.as_ref().expect("allocated above"))
        }

        /// Renders one view. On error (a target could not be allocated)
        /// nothing was drawn into the output and the GL state is usable.
        pub fn draw(&mut self, gl: &GL, view: &ViewDraw) -> Result<(), String> {
            let [x, y, width, height] = view.output.viewport;
            self.surface_near = None;
            if width <= 0 || height <= 0 || view.layers.is_empty() {
                return Ok(());
            }
            let soft = view.quality == BlendQuality::Soft;
            let recipe = Recipe::of(view.quality);
            self.targets(gl, width as u32, height as u32, soft)?;
            let targets = self.targets.as_ref().expect("allocated above");
            let level = if soft { &self.soft } else { &self.hard };

            gl.viewport(0, 0, width, height);
            gl.disable(GL::CULL_FACE);
            gl.bind_vertex_array(Some(view.vao));
            let bind = |unit: u32, texture: &WebGlTexture| {
                gl.active_texture(GL::TEXTURE0 + unit);
                gl.bind_texture(GL::TEXTURE_2D, Some(texture));
            };

            // 1./2. Visibility: nearest disc fragment per pixel, then the
            // nearest one that is not on that first surface.
            gl.enable(GL::DEPTH_TEST);
            gl.depth_func(GL::LESS);
            gl.depth_mask(true);
            gl.disable(GL::BLEND);
            gl.clear_depth(1.0);
            for (pass, (program, (_, framebuffer))) in level.visibility.iter().zip(&targets.depth).enumerate() {
                gl.bind_framebuffer(GL::FRAMEBUFFER, Some(framebuffer));
                gl.clear(GL::DEPTH_BUFFER_BIT);
                if pass == 1 {
                    bind(UNIT_FIRST_DEPTH, &targets.depth[0].0);
                }
                program.begin(gl, view, &recipe, recipe.visibility_sigma);
                program.draw_layers(gl, view.layers);
            }

            // 3. Accumulation.
            gl.bind_framebuffer(GL::FRAMEBUFFER, Some(&targets.accumulate));
            gl.disable(GL::DEPTH_TEST);
            gl.depth_mask(false);
            gl.enable(GL::BLEND);
            gl.blend_func(GL::ONE, GL::ONE);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(GL::COLOR_BUFFER_BIT);
            bind(UNIT_FIRST_DEPTH, &targets.depth[0].0);
            if soft {
                bind(UNIT_SECOND_DEPTH, &targets.depth[1].0);
            }
            level.accumulate.begin(gl, view, &recipe, recipe.cutoff_sigma);
            level.accumulate.draw_layers(gl, view.layers);

            // 4. Resolve into the destination.
            gl.bind_framebuffer(GL::FRAMEBUFFER, view.output.framebuffer);
            gl.viewport(x, y, width, height);
            gl.blend_func(GL::ONE, GL::ONE_MINUS_SRC_ALPHA);
            bind(UNIT_FRONT, &targets.color[0]);
            if soft {
                bind(UNIT_BACK, &targets.color[1]);
            }
            bind(UNIT_COVER, targets.color.last().expect("the coverage target"));
            gl.use_program(Some(&level.resolve.program));
            gl.uniform2i(level.resolve.origin.as_ref(), x, y);
            gl.uniform1f(level.resolve.inv_tau.as_ref(), 1.0 / recipe.coverage_tau);
            gl.draw_arrays(GL::TRIANGLES, 0, 3);

            gl.active_texture(GL::TEXTURE0);
            gl.depth_mask(true);
            gl.bind_vertex_array(None);
            self.surface_near = Some(view.vp.near);
            Ok(())
        }

        /// Frees the render targets (they are reallocated on demand).
        pub fn release_targets(&mut self, gl: &GL) {
            self.surface_near = None;
            if let Some(targets) = self.targets.take() {
                targets.destroy(gl);
            }
        }

        pub fn destroy(&mut self, gl: &GL) {
            self.release_targets(gl);
            self.hard.destroy(gl);
            self.soft.destroy(gl);
        }
    }
}

#[cfg(test)]
mod tests {
    use glam::{Mat4, Vec3};
    use gs_core::Camera;

    use super::*;

    #[test]
    fn recipes_follow_the_presets() {
        let hard = Recipe::of(BlendQuality::Hard);
        let soft = Recipe::of(BlendQuality::Soft);
        assert_eq!((hard.visibility_sigma, hard.cutoff_sigma, hard.depth_tolerance, hard.coverage_tau), (1.6, 3.0, 0.01, 0.5));
        assert_eq!((soft.visibility_sigma, soft.cutoff_sigma, soft.depth_tolerance, soft.coverage_tau), (2.0, 3.0, 0.01, 0.5));
    }

    #[test]
    fn encoded_tolerance_is_the_relative_test() {
        let recipe = Recipe::of(BlendQuality::Hard);
        let [behind, in_front] = recipe.encoded_tolerance();
        let encode = |z: f32| (z / 0.05).log2() / DEPTH_OCTAVES;
        // At any distance: 1 % behind or in front is exactly on the limit.
        for z in [0.06f32, 0.7, 3.0, 40.0, 900.0] {
            assert!((encode(z * 1.01) - encode(z) - behind).abs() < 2.0e-7, "{z}");
            assert!((encode(z) - encode(z * 0.99) - in_front).abs() < 2.0e-7, "{z}");
        }
        // 24 bits resolve the encoding far finer than the tolerance, and the
        // 21 octaves reach from the near plane to 100 km.
        assert!(behind > 5_000.0 / (1u32 << 24) as f32 && in_front > behind);
        assert!(encode(100_000.0) < 1.0 && encode(0.05) == 0.0);
        let [floor, gain] = recipe.taper();
        assert!((floor - (-4.5f32).exp()).abs() < 1e-9 && ((1.0 - floor) * gain - 1.0).abs() < 1e-6);
    }

    #[test]
    fn layer_weights_reach_the_shaders_in_half_float_range() {
        // The default is passed through unchanged.
        assert_eq!(shader_weights([1.0, gs_core::experimental::wblend::LIVE_LAYER_WEIGHT]), [1.0, 16.0]);
        // Only the ratio survives; the larger weight is always 16.
        for weights in [[1.0, 1.0e4], [1.0e4, 1.0], [1.0e4, 1.0e4], [1.0e-3, 1.0e-3], [3.0, 0.5], [f32::MAX, 1.0e30]] {
            let [a, b] = shader_weights(weights);
            assert_eq!(a.max(b), 16.0, "{weights:?}");
            let (ratio, wanted) = (a as f64 / b as f64, weights[0] as f64 / weights[1] as f64);
            assert!((ratio / wanted - 1.0).abs() < 1.0e-6, "{weights:?} -> {a} : {b}");
        }
        // Zero stays zero, also next to a weight; nonsense counts as zero.
        assert_eq!(shader_weights([0.0, 0.0]), [0.0, 0.0]);
        assert_eq!(shader_weights([0.0, 2.0]), [0.0, 16.0]);
        assert_eq!(shader_weights([1.0, 0.0]), [16.0, 0.0]);
        assert_eq!(shader_weights([f32::NAN, 4.0]), [0.0, 16.0]);
        assert_eq!(shader_weights([-3.0, f32::INFINITY]), [0.0, 0.0]);
    }

    #[test]
    fn eye_and_scale_come_from_the_view_matrix() {
        let eye = Vec3::new(1.5, 2.0, -3.0);
        let cam = Camera::look_at(eye, Vec3::new(0.0, 1.0, 0.0), Vec3::Y, 1.0, 64, 48);
        let (e, s) = eye_and_scale(&ViewParams::from_camera(&cam));
        assert!(e.abs_diff_eq(eye, 1e-5) && (s - 1.0).abs() < 1e-6);
        // A miniature: the world shrunk 15 times around the origin.
        let mut vp = ViewParams::from_camera(&cam);
        vp.view *= Mat4::from_scale(Vec3::splat(1.0 / 15.0));
        let (e, s) = eye_and_scale(&vp);
        assert!((s - 1.0 / 15.0).abs() < 1e-7);
        assert!(e.abs_diff_eq(eye * 15.0, 1e-3), "{e:?}");
    }
}
