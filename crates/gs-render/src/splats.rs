//! Splat storage (CPU + GPU), depth sorting and the splat draw call.

use glam::{Mat4, Vec3};
use gs_core::codec::{self, LayerHeader, LayerMode};
use gs_core::sort::DepthSorter;
use gs_core::splat::{ViewParams, CUTOFF_SIGMA};
use gs_core::Splat;
use wasm_bindgen::JsValue;
use web_sys::{WebGl2RenderingContext as GL, WebGlBuffer, WebGlProgram, WebGlTexture, WebGlUniformLocation, WebGlVertexArrayObject};

use crate::depth::{DepthWarp, RobustBounds};
use crate::glutil::{link_program, uniform};
use crate::pack::{pack_splats, TextureLayout, MIN_ROWS, WORDS_PER_SPLAT};
use crate::params::{RenderParams, LAYER_COUNT};
use crate::shaders::{SPLAT_FS, SPLAT_VS};

/// Splats packed per staging-buffer fill when (re)uploading a large range:
/// bounds the staging memory to a few megabytes.
const UPLOAD_CHUNK_ROWS: usize = 64;

/// A sort slower than this is not repeated on the very next frame.
const SLOW_SORT_MS: f64 = 4.0;

/// The order is kept while the view direction stays within about a quarter
/// of a degree (`1 − cos`), which hides nothing visible but lets a still or
/// slowly drifting head skip most sorts.
const SORT_DIRECTION_TOLERANCE: f32 = 1.0e-5;

/// What, besides the data and the view direction, determines the order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SortKey {
    pub show: [bool; LAYER_COUNT],
    /// Splats centered above this height are left out (`+∞` = none).
    pub clip_y: f32,
}

/// What [`SplatScene::sort`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SortOutcome {
    /// The order on the GPU is still valid; nothing was done.
    Fresh,
    /// A new order was computed and uploaded.
    Sorted,
    /// The order is stale, but the previous frame already paid for a slow
    /// sort: this frame draws with the old order and the next one sorts.
    Deferred,
}

struct Layer {
    splats: Vec<Splat>,
    /// Centers only, densely packed for the sorter.
    positions: Vec<[f32; 3]>,
    /// Where the bulk of the centers is (`None` for an empty layer).
    bounds: Option<RobustBounds>,
    texture: WebGlTexture,
    /// Allocated texture height.
    rows: u32,
}

struct Uniforms {
    order_width: WebGlUniformLocation,
    split: WebGlUniformLocation,
    counts: WebGlUniformLocation,
    spr: WebGlUniformLocation,
    view: WebGlUniformLocation,
    focal: WebGlUniformLocation,
    center: WebGlUniformLocation,
    viewport: WebGlUniformLocation,
    near: WebGlUniformLocation,
    splat_scale: WebGlUniformLocation,
    max_radius: WebGlUniformLocation,
    point_radius: WebGlUniformLocation,
    clip_y: WebGlUniformLocation,
}

/// State of the draw order on the GPU.
struct Sorted {
    /// Data version the order was computed for.
    version: u64,
    forward: Vec3,
    key: SortKey,
    /// Indices `>= split` address layer 1 (see the vertex shader).
    split: u32,
    /// Quads to draw.
    instances: u32,
    /// The previous frame sorted.
    sorted_last_frame: bool,
}

/// The layer-independent GL objects (everything but the data textures).
struct Pipeline {
    program: WebGlProgram,
    uniforms: Uniforms,
    /// Holds only the element buffer binding: the shader has no attributes.
    vao: WebGlVertexArrayObject,
    /// `0 1 2 2 1 3` + `4·q` for every quad `q` below `quad_capacity`.
    quad_indices: WebGlBuffer,
    quad_capacity: usize,
    /// `R32UI`, `order_width` texels per row: the sorted splat indices.
    order_texture: WebGlTexture,
    order_width: u32,
    order_rows: u32,
}

/// Both splat layers, their GPU copies and the sorted draw order.
pub(crate) struct SplatScene {
    gl: GL,
    pipeline: Pipeline,
    layout: TextureLayout,
    layers: [Layer; LAYER_COUNT],
    /// Packed words on their way to the GPU.
    staging: Vec<u32>,
    /// Decode target for layer messages; swapped with the layer on `Replace`.
    scratch: Vec<Splat>,
    sorter: DepthSorter,
    order: Vec<u32>,
    /// The GL objects do not exist (context lost, or destroyed): data
    /// changes only reach the CPU copies and nothing is sorted or drawn.
    detached: bool,
    /// Bumped on every change of the splat data.
    version: u64,
    sorted: Sorted,
    /// Duration of the last sort, milliseconds.
    pub sort_ms: f64,
    /// Number of sorts so far.
    pub sort_count: u64,
}

fn create_data_texture(gl: &GL, layout: &TextureLayout, rows: u32) -> Result<WebGlTexture, JsValue> {
    let texture = gl.create_texture().ok_or("WebGL: createTexture failed")?;
    gl.bind_texture(GL::TEXTURE_2D, Some(&texture));
    gl.tex_storage_2d(GL::TEXTURE_2D, 1, GL::RGBA32UI, layout.width() as i32, rows as i32);
    // Integer textures are incomplete with any filtering but NEAREST.
    gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_MIN_FILTER, GL::NEAREST as i32);
    gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_MAG_FILTER, GL::NEAREST as i32);
    gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_WRAP_S, GL::CLAMP_TO_EDGE as i32);
    gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_WRAP_T, GL::CLAMP_TO_EDGE as i32);
    Ok(texture)
}

/// Texture unit of the order texture (units 0 and 1 hold the layers).
const ORDER_UNIT: u32 = 2;
/// Preferred width of the order texture, texels.
const ORDER_WIDTH: u32 = 4096;
/// Quads the buffers are sized for at first.
const MIN_QUADS: usize = 1 << 14;
/// Quads generated per staging-buffer fill when the quad index buffer grows.
const INDEX_CHUNK_QUADS: usize = 1 << 16;

fn create_order_texture(gl: &GL, width: u32, rows: u32) -> Result<WebGlTexture, JsValue> {
    let texture = gl.create_texture().ok_or("WebGL: createTexture failed")?;
    gl.active_texture(GL::TEXTURE0 + ORDER_UNIT);
    gl.bind_texture(GL::TEXTURE_2D, Some(&texture));
    gl.tex_storage_2d(GL::TEXTURE_2D, 1, GL::R32UI, width as i32, rows as i32);
    gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_MIN_FILTER, GL::NEAREST as i32);
    gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_MAG_FILTER, GL::NEAREST as i32);
    gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_WRAP_S, GL::CLAMP_TO_EDGE as i32);
    gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_WRAP_T, GL::CLAMP_TO_EDGE as i32);
    gl.active_texture(GL::TEXTURE0);
    Ok(texture)
}

impl Pipeline {
    fn new(gl: &GL, max_texture_size: u32) -> Result<Self, JsValue> {
        let program = link_program(gl, SPLAT_VS, SPLAT_FS)?;
        let u = |name: &str| uniform(gl, &program, name);
        let uniforms = Uniforms {
            order_width: u("u_order_width")?,
            split: u("u_split")?,
            counts: u("u_counts")?,
            spr: u("u_spr")?,
            view: u("u_view")?,
            focal: u("u_focal")?,
            center: u("u_center")?,
            viewport: u("u_viewport")?,
            near: u("u_near")?,
            splat_scale: u("u_splat_scale")?,
            max_radius: u("u_max_radius")?,
            point_radius: u("u_point_radius")?,
            clip_y: u("u_clip_y")?,
        };
        // The samplers never change units.
        gl.use_program(Some(&program));
        gl.uniform1i(Some(&u("u_data0")?), 0);
        gl.uniform1i(Some(&u("u_data1")?), 1);
        gl.uniform1i(Some(&u("u_order")?), ORDER_UNIT as i32);

        let vao = gl.create_vertex_array().ok_or("WebGL: createVertexArray failed")?;
        let quad_indices = gl.create_buffer().ok_or("WebGL: createBuffer failed")?;
        gl.bind_vertex_array(Some(&vao));
        gl.bind_buffer(GL::ELEMENT_ARRAY_BUFFER, Some(&quad_indices));
        gl.bind_vertex_array(None);

        let order_width = ORDER_WIDTH.min(max_texture_size.max(1));
        let order_rows = (MIN_QUADS as u32).div_ceil(order_width);
        let order_texture = create_order_texture(gl, order_width, order_rows)?;
        let mut pipeline = Self { program, uniforms, vao, quad_indices, quad_capacity: 0, order_texture, order_width, order_rows };
        pipeline.reserve_quads(gl, MIN_QUADS, &mut Vec::new());
        Ok(pipeline)
    }

    /// Makes sure the quad index buffer covers `quads` quads. Grows
    /// geometrically; `staging` is scratch memory.
    fn reserve_quads(&mut self, gl: &GL, quads: usize, staging: &mut Vec<u32>) {
        if quads <= self.quad_capacity {
            return;
        }
        let capacity = quads.next_power_of_two().max(MIN_QUADS);
        gl.bind_vertex_array(Some(&self.vao));
        gl.buffer_data_with_i32(GL::ELEMENT_ARRAY_BUFFER, (capacity * 6 * 4) as i32, GL::STATIC_DRAW);
        let mut first = 0;
        while first < capacity {
            let end = capacity.min(first + INDEX_CHUNK_QUADS);
            staging.clear();
            staging.extend((first..end).flat_map(|quad| {
                let v = 4 * quad as u32;
                [v, v + 1, v + 2, v + 2, v + 1, v + 3]
            }));
            // SAFETY: the view aliases wasm memory and is only valid until
            // the next allocation; bufferSubData copies it synchronously.
            let view = unsafe { js_sys::Uint32Array::view(staging) };
            gl.buffer_sub_data_with_i32_and_array_buffer_view(GL::ELEMENT_ARRAY_BUFFER, (first * 6 * 4) as i32, &view);
            first = end;
        }
        gl.bind_vertex_array(None);
        self.quad_capacity = capacity;
    }

    /// Uploads a draw order (`order` is not empty). `max_rows` is the
    /// texture size limit.
    fn upload_order(&mut self, gl: &GL, order: &[u32], max_rows: u32) -> Result<(), JsValue> {
        let width = self.order_width as usize;
        let needed = order.len().div_ceil(width) as u32;
        if needed > self.order_rows {
            let rows = needed.max(self.order_rows.saturating_mul(2)).min(max_rows.max(needed));
            let texture = create_order_texture(gl, self.order_width, rows)?;
            gl.delete_texture(Some(&self.order_texture));
            self.order_texture = texture;
            self.order_rows = rows;
        }
        gl.active_texture(GL::TEXTURE0 + ORDER_UNIT);
        gl.bind_texture(GL::TEXTURE_2D, Some(&self.order_texture));
        // Full rows first, then the partial last row.
        let full_rows = order.len() / width;
        let (full, rest) = order.split_at(full_rows * width);
        for (slice, y, w, h) in [(full, 0, width, full_rows), (rest, full_rows, rest.len(), 1)] {
            if slice.is_empty() {
                continue;
            }
            // SAFETY: as above; texSubImage2D copies synchronously.
            let view = unsafe { js_sys::Uint32Array::view(slice) };
            gl.tex_sub_image_2d_with_i32_and_i32_and_u32_and_type_and_opt_array_buffer_view(
                GL::TEXTURE_2D,
                0,
                0,
                y as i32,
                w as i32,
                h as i32,
                GL::RED_INTEGER,
                GL::UNSIGNED_INT,
                Some(&view),
            )?;
        }
        gl.active_texture(GL::TEXTURE0);
        Ok(())
    }

    fn destroy(&self, gl: &GL) {
        gl.delete_texture(Some(&self.order_texture));
        gl.delete_buffer(Some(&self.quad_indices));
        gl.delete_vertex_array(Some(&self.vao));
        gl.delete_program(Some(&self.program));
    }
}

impl SplatScene {
    pub fn new(gl: &GL) -> Result<Self, JsValue> {
        let max_texture_size = gl.get_parameter(GL::MAX_TEXTURE_SIZE)?.as_f64().unwrap_or(2048.0) as u32;
        let pipeline = Pipeline::new(gl, max_texture_size)?;
        let layout = TextureLayout::new(max_texture_size);
        let layer = || -> Result<Layer, JsValue> {
            Ok(Layer {
                splats: Vec::new(),
                positions: Vec::new(),
                bounds: None,
                texture: create_data_texture(gl, &layout, MIN_ROWS)?,
                rows: MIN_ROWS,
            })
        };
        Ok(Self {
            gl: gl.clone(),
            pipeline,
            layout,
            layers: [layer()?, layer()?],
            staging: Vec::new(),
            scratch: Vec::new(),
            sorter: DepthSorter::new(),
            order: Vec::new(),
            detached: false,
            version: 1,
            sorted: Sorted {
                version: 0,
                forward: Vec3::ZERO,
                key: SortKey { show: [true; LAYER_COUNT], clip_y: f32::INFINITY },
                split: 0,
                instances: 0,
                sorted_last_frame: false,
            },
            sort_ms: 0.0,
            sort_count: 0,
        })
    }

    /// The WebGL context is lost: every GL object is gone with it. Until
    /// [`Self::restore`], layer updates are kept on the CPU side only.
    pub fn context_lost(&mut self) {
        self.detached = true;
    }

    /// Recreates every GL object after a context loss and uploads the CPU
    /// copy of the splats again.
    pub fn restore(&mut self) -> Result<(), JsValue> {
        self.pipeline = Pipeline::new(&self.gl, self.layout.max_rows)?;
        self.detached = false;
        self.sorted.version = 0;
        self.sorted.instances = 0;
        for layer in 0..LAYER_COUNT {
            let l = &mut self.layers[layer];
            l.rows = self.layout.rows_for(l.splats.len()).max(MIN_ROWS);
            l.texture = create_data_texture(&self.gl, &self.layout, l.rows)?;
            self.upload(layer, 0).map_err(JsValue::from)?;
        }
        Ok(())
    }

    /// Splats in one layer.
    pub fn layer_len(&self, layer: usize) -> usize {
        self.layers[layer].splats.len()
    }

    /// Splats in all layers.
    pub fn total(&self) -> usize {
        self.layers.iter().map(|l| l.splats.len()).sum()
    }

    /// Instances the next draw submits.
    pub fn instances(&self) -> u32 {
        self.sorted.instances
    }

    /// Copy of all splats, layer 0 first.
    pub fn splats(&self) -> Vec<Splat> {
        let mut out = Vec::with_capacity(self.total());
        for layer in &self.layers {
            out.extend_from_slice(&layer.splats);
        }
        out
    }

    fn check_capacity(&self, count: usize) -> Result<(), String> {
        let max = self.layout.max_splats();
        if count > max {
            return Err(format!("layer would hold {count} splats, this GPU allows {max} per layer"));
        }
        Ok(())
    }

    /// Replaces the content of a layer. O(new size).
    pub fn replace(&mut self, layer: usize, splats: &[Splat]) -> Result<(), String> {
        self.check_capacity(splats.len())?;
        let l = &mut self.layers[layer];
        l.splats.clear();
        l.splats.extend_from_slice(splats);
        self.refresh(layer, 0)
    }

    /// Adds splats to a layer. O(added), amortized.
    pub fn append(&mut self, layer: usize, splats: &[Splat]) -> Result<(), String> {
        let start = self.layers[layer].splats.len();
        self.check_capacity(start + splats.len())?;
        self.layers[layer].splats.extend_from_slice(splats);
        self.refresh(layer, start)
    }

    /// Removes every splat. The textures keep their size.
    pub fn clear(&mut self) {
        for layer in &mut self.layers {
            layer.splats.clear();
            layer.positions.clear();
            layer.bounds = None;
        }
        self.version += 1;
    }

    /// Decodes a wire layer message straight into the layers and uploads it.
    /// On error nothing changes. Returns the message header and the size of
    /// the layer afterwards.
    pub fn apply_layer_message(&mut self, bytes: &[u8]) -> Result<(LayerHeader, usize), String> {
        self.scratch.clear();
        let header = codec::decode_layer_into(bytes, &mut self.scratch).map_err(|e| e.to_string())?;
        let layer = header.layer_id as usize;
        if layer >= LAYER_COUNT {
            return Err(format!("unknown layer id {}", header.layer_id));
        }
        let start = match header.mode {
            LayerMode::Replace => {
                self.check_capacity(self.scratch.len())?;
                // The decoded vector becomes the layer; the old one is reused
                // as the decode target of the next message. No copy.
                std::mem::swap(&mut self.layers[layer].splats, &mut self.scratch);
                0
            }
            LayerMode::Append => {
                let start = self.layers[layer].splats.len();
                self.check_capacity(start + self.scratch.len())?;
                self.layers[layer].splats.extend_from_slice(&self.scratch);
                start
            }
        };
        self.refresh(layer, start)?;
        Ok((header, self.layers[layer].splats.len()))
    }

    /// Brings the sort positions and the GPU texture of `layer` up to date
    /// after its splats changed from index `start` on.
    fn refresh(&mut self, layer: usize, start: usize) -> Result<(), String> {
        let l = &mut self.layers[layer];
        l.positions.truncate(start);
        l.positions.extend(l.splats[start..].iter().map(|s| s.pos));
        // From a fixed-size sample: O(1) in the layer size.
        l.bounds = RobustBounds::of(&l.positions);
        self.version += 1;
        if self.detached {
            // `restore` uploads everything from the CPU copy.
            return Ok(());
        }

        let needed = self.layout.rows_for(l.splats.len());
        let mut start = start;
        if needed > l.rows {
            // Immutable storage cannot grow: allocate a bigger texture and
            // fill it from the CPU copy. Doubling keeps appends amortized O(1).
            let rows = self.layout.grown_rows(l.rows, needed);
            let texture = create_data_texture(&self.gl, &self.layout, rows).map_err(|e| format!("{e:?}"))?;
            self.gl.delete_texture(Some(&l.texture));
            l.texture = texture;
            l.rows = rows;
            start = 0;
        }
        self.upload(layer, start)
    }

    /// Packs the splats of `layer` from index `start` on and writes them into
    /// its texture, which must be large enough.
    fn upload(&mut self, layer: usize, mut start: usize) -> Result<(), String> {
        let gl = &self.gl;
        let layout = self.layout;
        let l = &self.layers[layer];
        let end = l.splats.len();
        gl.bind_texture(GL::TEXTURE_2D, Some(&l.texture));
        let spr = layout.splats_per_row as usize;
        while start < end {
            // Chunks end on a row boundary so that each is at most three rectangles.
            let chunk_end = end.min((start / spr + UPLOAD_CHUNK_ROWS) * spr);
            pack_splats(&l.splats[start..chunk_end], &mut self.staging);
            for rect in layout.rects(start, chunk_end) {
                let words = &self.staging[(rect.first_splat - start) * WORDS_PER_SPLAT..][..rect.splat_count * WORDS_PER_SPLAT];
                // SAFETY: the view aliases wasm memory and is only valid until
                // the next allocation; texSubImage2D copies it synchronously.
                let view = unsafe { js_sys::Uint32Array::view(words) };
                gl.tex_sub_image_2d_with_i32_and_i32_and_u32_and_type_and_opt_array_buffer_view(
                    GL::TEXTURE_2D,
                    0,
                    rect.x as i32,
                    rect.y as i32,
                    rect.width as i32,
                    rect.height as i32,
                    GL::RGBA_INTEGER,
                    GL::UNSIGNED_INT,
                    Some(&view),
                )
                .map_err(|e| format!("texSubImage2D failed: {e:?}"))?;
            }
            start = chunk_end;
        }
        Ok(())
    }

    /// Makes sure the order texture holds a back-to-front order that is valid
    /// for `view`. Call once per frame, before drawing any view of that frame
    /// (both eyes share the order of the head pose).
    ///
    /// The planar depth order only depends on the view *direction*, so
    /// translating the camera never triggers a sort. A slow sort (large
    /// scene) is done at most every other frame unless `force` is set.
    /// `now` returns milliseconds.
    pub fn sort(&mut self, view: &Mat4, key: SortKey, now: &dyn Fn() -> f64, force: bool) -> SortOutcome {
        // Row 2 of the view matrix, negated: depth = forward·p + offset. A
        // uniform world scale changes its length, not the order.
        let forward = Vec3::new(-view.x_axis.z, -view.y_axis.z, -view.z_axis.z);
        let offset = -view.w_axis.z;
        let direction = forward.normalize_or_zero();
        // False for a NaN direction too, which then sorts every frame.
        let same_direction = self.sorted.forward.dot(direction) >= 1.0 - SORT_DIRECTION_TOLERANCE;
        if self.detached {
            return SortOutcome::Fresh;
        }
        let stale = self.sorted.version != self.version || self.sorted.key != key || !same_direction;
        let throttled = self.sort_ms > SLOW_SORT_MS && self.sorted.sorted_last_frame;
        if !stale || (throttled && !force) {
            self.sorted.sorted_last_frame = false;
            return if stale { SortOutcome::Deferred } else { SortOutcome::Fresh };
        }

        let start = now();
        let visible = |layer: usize| key.show[layer].then_some(&self.layers[layer]);
        let first = visible(0).map_or(&[][..], |l| &l.positions);
        let second = visible(1).map_or(&[][..], |l| &l.positions);
        // The sorter spreads its 16-bit keys over the depth range it is
        // given: keep stray far-away splats from stretching that range.
        let bounds = RobustBounds::union(visible(0).and_then(|l| l.bounds), visible(1).and_then(|l| l.bounds));
        let warp = DepthWarp::new(bounds, forward, offset);
        let clip_y = key.clip_y;
        let mut hidden = 0u32;
        // Clipped splats get a NaN depth: the sorter puts non-finite depths
        // last, so dropping them is just a shorter draw call.
        let depths = first.iter().chain(second).map(|p| {
            let depth = if p[1] > clip_y { f32::NAN } else { warp.apply(forward.x * p[0] + forward.y * p[1] + forward.z * p[2] + offset) };
            hidden += !depth.is_finite() as u32;
            depth
        });
        self.sorter.sort_by_depth(depths, &mut self.order);
        let instances = self.order.len() - hidden as usize;

        if instances > 0 {
            self.pipeline.reserve_quads(&self.gl, instances, &mut self.staging);
            if let Err(e) = self.pipeline.upload_order(&self.gl, &self.order[..instances], self.layout.max_rows) {
                // Out of GPU memory at worst: keep drawing the old order.
                web_sys::console::error_2(&"gs-render: could not upload the draw order".into(), &e);
                return SortOutcome::Deferred;
            }
        }

        self.sorted = Sorted {
            version: self.version,
            forward: direction,
            key,
            split: first.len() as u32,
            instances: instances as u32,
            sorted_last_frame: true,
        };
        self.sort_ms = now() - start;
        self.sort_count += 1;
        SortOutcome::Sorted
    }

    /// Draws the sorted splats into the bound framebuffer and viewport.
    /// `clip_y` is the height above which splats are hidden (`+∞` = none).
    pub fn draw(&self, vp: &ViewParams, params: &RenderParams, clip_y: f32) {
        if self.sorted.instances == 0 || self.detached {
            return;
        }
        let gl = &self.gl;
        let u = &self.pipeline.uniforms;
        gl.use_program(Some(&self.pipeline.program));
        gl.active_texture(GL::TEXTURE0 + ORDER_UNIT);
        gl.bind_texture(GL::TEXTURE_2D, Some(&self.pipeline.order_texture));
        gl.active_texture(GL::TEXTURE1);
        gl.bind_texture(GL::TEXTURE_2D, Some(&self.layers[1].texture));
        gl.active_texture(GL::TEXTURE0);
        gl.bind_texture(GL::TEXTURE_2D, Some(&self.layers[0].texture));
        gl.uniform1i(Some(&u.order_width), self.pipeline.order_width as i32);
        gl.uniform1ui(Some(&u.split), self.sorted.split);
        gl.uniform2ui(Some(&u.counts), self.layers[0].splats.len() as u32, self.layers[1].splats.len() as u32);
        gl.uniform1i(Some(&u.spr), self.layout.splats_per_row as i32);
        gl.uniform_matrix4fv_with_f32_array(Some(&u.view), false, &vp.view.to_cols_array());
        gl.uniform2f(Some(&u.focal), vp.fx, vp.fy);
        gl.uniform2f(Some(&u.center), vp.cx, vp.cy);
        gl.uniform2f(Some(&u.viewport), vp.width, vp.height);
        gl.uniform1f(Some(&u.near), vp.near);
        gl.uniform1f(Some(&u.splat_scale), params.splat_scale);
        gl.uniform1f(Some(&u.max_radius), params.max_radius_px);
        gl.uniform1f(Some(&u.point_radius), if params.point_mode { CUTOFF_SIGMA * params.point_sigma_px } else { 0.0 });
        // Finite stand-in for "no clip": uniforms must not be infinite on every driver.
        gl.uniform1f(Some(&u.clip_y), clip_y.min(f32::MAX));

        // Back-to-front "over" on premultiplied alpha, no depth buffer.
        gl.disable(GL::DEPTH_TEST);
        gl.disable(GL::CULL_FACE);
        gl.enable(GL::BLEND);
        gl.blend_func(GL::ONE, GL::ONE_MINUS_SRC_ALPHA);
        gl.bind_vertex_array(Some(&self.pipeline.vao));
        gl.draw_elements_with_i32(GL::TRIANGLES, 6 * self.sorted.instances as i32, GL::UNSIGNED_INT, 0);
        gl.bind_vertex_array(None);
    }

    /// Forces the next [`Self::sort`] to run (the order texture was reused for
    /// another view, e.g. by a snapshot).
    pub fn invalidate_order(&mut self) {
        self.sorted.version = 0;
    }

    /// Frees the GPU objects, for good. The CPU copies stay readable.
    pub fn destroy(&mut self) {
        if std::mem::replace(&mut self.detached, true) {
            // Already gone with a lost context.
            return;
        }
        let gl = &self.gl;
        for layer in &self.layers {
            gl.delete_texture(Some(&layer.texture));
        }
        self.pipeline.destroy(gl);
    }
}
