//! Anti-aliased line renderer for the gizmos.

use glam::Mat4;
use wasm_bindgen::JsValue;
use web_sys::{WebGl2RenderingContext as GL, WebGlBuffer, WebGlProgram, WebGlUniformLocation, WebGlVertexArrayObject};

use crate::gizmo::{LineBatch, FLOATS_PER_SEGMENT};
use crate::glutil::{link_program, uniform};
use crate::shaders::{LINE_FS, LINE_VS};

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

/// Draws [`LineBatch`]es as screen-space quads of constant pixel width,
/// blended on top of whatever is in the framebuffer (no depth test).
///
/// Two batches are kept: the user's gizmo scene, uploaded when it changes,
/// and a small per-frame one (the VR controllers).
pub(crate) struct LineRenderer {
    gl: GL,
    program: WebGlProgram,
    u_view_proj: WebGlUniformLocation,
    u_viewport: WebGlUniformLocation,
    u_near: WebGlUniformLocation,
    u_px_scale: WebGlUniformLocation,
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
        let program = link_program(gl, LINE_VS, LINE_FS)?;
        Ok(Self {
            gl: gl.clone(),
            u_view_proj: uniform(gl, &program, "u_view_proj")?,
            u_viewport: uniform(gl, &program, "u_viewport")?,
            u_near: uniform(gl, &program, "u_near")?,
            u_px_scale: uniform(gl, &program, "u_px_scale")?,
            scene: create_batch(gl)?,
            overlay: create_batch(gl)?,
            program,
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
    /// `near` is the view-space distance of the near plane (segments are
    /// clipped against it in the vertex shader) and `px_scale` the number of
    /// device pixels per unit of line width.
    pub fn draw(&self, view_proj: &Mat4, viewport: [f32; 2], near: f32, px_scale: f32, with_scene: bool) {
        let scene = if with_scene { self.scene.vertices } else { 0 };
        if scene + self.overlay.vertices == 0 {
            return;
        }
        let gl = &self.gl;
        gl.use_program(Some(&self.program));
        gl.uniform_matrix4fv_with_f32_array(Some(&self.u_view_proj), false, &view_proj.to_cols_array());
        gl.uniform2f(Some(&self.u_viewport), viewport[0], viewport[1]);
        gl.uniform1f(Some(&self.u_near), near);
        gl.uniform1f(Some(&self.u_px_scale), px_scale);
        gl.disable(GL::DEPTH_TEST);
        gl.disable(GL::CULL_FACE);
        gl.enable(GL::BLEND);
        gl.blend_func(GL::ONE, GL::ONE_MINUS_SRC_ALPHA);
        for (batch, count) in [(&self.scene, scene), (&self.overlay, self.overlay.vertices)] {
            if count > 0 {
                gl.bind_vertex_array(Some(&batch.vao));
                gl.draw_arrays(GL::TRIANGLES, 0, count);
            }
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
        gl.delete_program(Some(&self.program));
    }
}
