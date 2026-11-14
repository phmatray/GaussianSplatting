//! Anti-aliased line renderer for the gizmos.

use glam::Mat4;
use wasm_bindgen::JsValue;
use web_sys::{WebGl2RenderingContext as GL, WebGlBuffer, WebGlProgram, WebGlTexture, WebGlUniformLocation, WebGlVertexArrayObject};

use crate::gizmo::{LineBatch, FLOATS_PER_SEGMENT};
use crate::glutil::{link_program, uniform};
use crate::shaders::{line_fs, DEPTH_OCTAVES, LINE_VS};

/// A line is hidden where it lies more than this fraction of its depth
/// behind the visible surface. Three times the tolerance of the splats
/// themselves, so that a grid lying *on* a reconstructed floor (whose
/// surfels sit a few millimeters above or below it) stays in one piece.
const OCCLUSION_TOLERANCE: f32 = 0.03;

/// Texture unit of the surface depth while lines are drawn.
const SURFACE_UNIT: u32 = 1;

/// The visible surface of the view the lines are drawn over: what the
/// normalised blending mode left in its first visibility texture.
#[derive(Clone, Copy)]
pub(crate) struct Occlusion<'a> {
    /// Encoded depth of the nearest surface per pixel (`1.0` = none).
    pub depth: &'a WebGlTexture,
    /// Near plane the depths were encoded with, view units.
    pub near: f32,
}

/// Floats per vertex: the whole segment (`FLOATS_PER_SEGMENT`) plus the corner.
const FLOATS_PER_VERTEX: usize = FLOATS_PER_SEGMENT + 2;
/// Corners of the two triangles of a segment quad: `(end, side)`.
const CORNERS: [[f32; 2]; 6] = [[-1.0, -1.0], [1.0, -1.0], [-1.0, 1.0], [-1.0, 1.0], [1.0, -1.0], [1.0, 1.0]];

/// One vertex buffer of segments with its own vertex array.
struct Batch {
    vao: WebGlVertexArrayObject,
    buffer: WebGlBuffer,
    /// Allocated size in floats.
    capacity: usize,
    vertices: i32,
}

/// The line program in one of its two variants, with its uniforms.
struct Program {
    program: WebGlProgram,
    view_proj: WebGlUniformLocation,
    viewport: WebGlUniformLocation,
    near: WebGlUniformLocation,
    px_scale: WebGlUniformLocation,
    /// Only in the occluded variant.
    origin: Option<WebGlUniformLocation>,
    occlusion: Option<WebGlUniformLocation>,
}

impl Program {
    fn new(gl: &GL, occluded: bool) -> Result<Self, JsValue> {
        let program = link_program(gl, LINE_VS, &line_fs(occluded))?;
        gl.use_program(Some(&program));
        // The sampler never changes units.
        gl.uniform1i(gl.get_uniform_location(&program, "u_surface").as_ref(), SURFACE_UNIT as i32);
        Ok(Self {
            view_proj: uniform(gl, &program, "u_view_proj")?,
            viewport: uniform(gl, &program, "u_viewport")?,
            near: uniform(gl, &program, "u_near")?,
            px_scale: uniform(gl, &program, "u_px_scale")?,
            origin: gl.get_uniform_location(&program, "u_origin"),
            occlusion: gl.get_uniform_location(&program, "u_occlusion"),
            program,
        })
    }
}

/// Draws [`LineBatch`]es as screen-space quads of constant pixel width,
/// blended on top of whatever is in the framebuffer.
///
/// Two batches are kept: the user's gizmo scene, uploaded when it changes,
/// and a small per-frame one (the VR controllers).
///
/// # Lines and splats
///
/// The destination framebuffers have no depth buffer. In the sorted
/// blending mode the splats have no depth either, so lines are simply drawn
/// on top of them. The normalised mode knows the visible surface of every
/// pixel: there the gizmo scene is hidden where it lies behind that surface
/// ([`Occlusion`]), so that a camera frustum behind the dancer or the scan
/// path behind a wall no longer shows through. The per-frame overlay (the
/// user's own controllers) always stays visible.
pub(crate) struct LineRenderer {
    gl: GL,
    plain: Program,
    occluded: Program,
    scene: Batch,
    overlay: Batch,
    /// Expanded vertices on their way to the GPU.
    staging: Vec<f32>,
}

fn create_batch(gl: &GL) -> Result<Batch, JsValue> {
    let vao = gl.create_vertex_array().ok_or("WebGL: createVertexArray failed")?;
    gl.bind_vertex_array(Some(&vao));
    let buffer = gl.create_buffer().ok_or("WebGL: createBuffer failed")?;
    gl.bind_buffer(GL::ARRAY_BUFFER, Some(&buffer));
    let stride = (FLOATS_PER_VERTEX * 4) as i32;
    // a_p0, a_p1, a_color, a_width, a_corner.
    for (location, size, offset) in [(0u32, 3, 0), (1, 3, 12), (2, 4, 24), (3, 1, 40), (4, 2, 44)] {
        gl.enable_vertex_attrib_array(location);
        gl.vertex_attrib_pointer_with_i32(location, size, GL::FLOAT, false, stride, offset);
    }
    gl.bind_vertex_array(None);
    Ok(Batch { vao, buffer, capacity: 0, vertices: 0 })
}

/// Expands every segment into the six vertices of its quad and uploads them.
fn upload(gl: &GL, batch: &mut Batch, lines: &LineBatch, staging: &mut Vec<f32>, usage: u32) {
    batch.vertices = (lines.len() * CORNERS.len()) as i32;
    if lines.len() == 0 {
        return;
    }
    staging.clear();
    for segment in lines.data().chunks_exact(FLOATS_PER_SEGMENT) {
        for corner in &CORNERS {
            staging.extend_from_slice(segment);
            staging.extend_from_slice(corner);
        }
    }
    gl.bind_buffer(GL::ARRAY_BUFFER, Some(&batch.buffer));
    if staging.len() > batch.capacity {
        batch.capacity = staging.len().next_power_of_two();
        gl.buffer_data_with_i32(GL::ARRAY_BUFFER, (batch.capacity * 4) as i32, usage);
    }
    // SAFETY: the view is only used by bufferSubData, which copies the data
    // before any allocation can move wasm memory.
    let view = unsafe { js_sys::Float32Array::view(staging) };
    gl.buffer_sub_data_with_i32_and_array_buffer_view(GL::ARRAY_BUFFER, 0, &view);
}

impl LineRenderer {
    pub fn new(gl: &GL) -> Result<Self, JsValue> {
        Ok(Self {
            gl: gl.clone(),
            plain: Program::new(gl, false)?,
            occluded: Program::new(gl, true)?,
            scene: create_batch(gl)?,
            overlay: create_batch(gl)?,
            staging: Vec::new(),
        })
    }

    /// Replaces the persistent gizmo scene.
    pub fn set_scene(&mut self, lines: &LineBatch) {
        upload(&self.gl, &mut self.scene, lines, &mut self.staging, GL::STATIC_DRAW);
    }

    /// Replaces the per-frame overlay (may be empty).
    pub fn set_overlay(&mut self, lines: &LineBatch) {
        upload(&self.gl, &mut self.overlay, lines, &mut self.staging, GL::DYNAMIC_DRAW);
    }

    /// Draws both batches into the bound framebuffer and viewport.
    ///
    /// `viewport` is `[x, y, width, height]` inside the framebuffer, `near`
    /// the view-space distance of the near plane (segments are clipped
    /// against it in the vertex shader) and `px_scale` the number of device
    /// pixels per unit of line width. With `occlusion`, the gizmo scene is
    /// hidden behind the splats of the view just drawn.
    pub fn draw(&self, view_proj: &Mat4, viewport: [i32; 4], near: f32, px_scale: f32, with_scene: bool, occlusion: Option<Occlusion>) {
        let scene = if with_scene { self.scene.vertices } else { 0 };
        if scene + self.overlay.vertices == 0 {
            return;
        }
        let gl = &self.gl;
        gl.disable(GL::DEPTH_TEST);
        gl.disable(GL::CULL_FACE);
        gl.enable(GL::BLEND);
        gl.blend_func(GL::ONE, GL::ONE_MINUS_SRC_ALPHA);
        let begin = |p: &Program| {
            gl.use_program(Some(&p.program));
            gl.uniform_matrix4fv_with_f32_array(Some(&p.view_proj), false, &view_proj.to_cols_array());
            gl.uniform2f(Some(&p.viewport), viewport[2] as f32, viewport[3] as f32);
            gl.uniform1f(Some(&p.near), near);
            gl.uniform1f(Some(&p.px_scale), px_scale);
        };
        let mut current = None;
        for (batch, count, hidden_by) in [(&self.scene, scene, occlusion), (&self.overlay, self.overlay.vertices, None)] {
            if count == 0 {
                continue;
            }
            if current != Some(hidden_by.is_some()) {
                current = Some(hidden_by.is_some());
                match hidden_by {
                    Some(surface) => {
                        begin(&self.occluded);
                        gl.active_texture(GL::TEXTURE0 + SURFACE_UNIT);
                        gl.bind_texture(GL::TEXTURE_2D, Some(surface.depth));
                        gl.active_texture(GL::TEXTURE0);
                        gl.uniform2i(self.occluded.origin.as_ref(), viewport[0], viewport[1]);
                        let tolerance = (1.0 + OCCLUSION_TOLERANCE).log2() / DEPTH_OCTAVES;
                        gl.uniform2f(self.occluded.occlusion.as_ref(), surface.near, tolerance);
                    }
                    None => begin(&self.plain),
                }
            }
            gl.bind_vertex_array(Some(&batch.vao));
            gl.draw_arrays(GL::TRIANGLES, 0, count);
        }
        gl.bind_vertex_array(None);
    }

    /// Frees the GPU objects.
    pub fn destroy(&self) {
        let gl = &self.gl;
        for batch in [&self.scene, &self.overlay] {
            gl.delete_buffer(Some(&batch.buffer));
            gl.delete_vertex_array(Some(&batch.vao));
        }
        gl.delete_program(Some(&self.plain.program));
        gl.delete_program(Some(&self.occluded.program));
    }
}
