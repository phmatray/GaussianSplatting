//! Small WebGL2 helpers shared by the splat and line renderers.

use wasm_bindgen::JsValue;
use web_sys::{WebGl2RenderingContext as GL, WebGlProgram, WebGlShader, WebGlUniformLocation};

fn compile(gl: &GL, kind: u32, source: &str) -> Result<WebGlShader, JsValue> {
    let shader = gl.create_shader(kind).ok_or("WebGL: createShader failed")?;
    gl.shader_source(&shader, source);
    gl.compile_shader(&shader);
    if gl.get_shader_parameter(&shader, GL::COMPILE_STATUS).as_bool() != Some(true) {
        let log = gl.get_shader_info_log(&shader).unwrap_or_default();
        gl.delete_shader(Some(&shader));
        let stage = if kind == GL::VERTEX_SHADER { "vertex" } else { "fragment" };
        return Err(format!("WebGL: {stage} shader failed to compile: {log}").into());
    }
    Ok(shader)
}

/// Compiles and links a program. The attribute locations come from the
/// `layout(location = ..)` qualifiers of the vertex shader.
pub(crate) fn link_program(gl: &GL, vertex: &str, fragment: &str) -> Result<WebGlProgram, JsValue> {
    let vs = compile(gl, GL::VERTEX_SHADER, vertex)?;
    let fs = compile(gl, GL::FRAGMENT_SHADER, fragment)?;
    let program = gl.create_program().ok_or("WebGL: createProgram failed")?;
    gl.attach_shader(&program, &vs);
    gl.attach_shader(&program, &fs);
    gl.link_program(&program);
    // The program keeps the compiled code alive; the shader objects can go.
    gl.delete_shader(Some(&vs));
    gl.delete_shader(Some(&fs));
    if gl.get_program_parameter(&program, GL::LINK_STATUS).as_bool() != Some(true) {
        let log = gl.get_program_info_log(&program).unwrap_or_default();
        gl.delete_program(Some(&program));
        return Err(format!("WebGL: program failed to link: {log}").into());
    }
    Ok(program)
}

/// Looks up a uniform that must exist (a missing one is a bug in the shader
/// source, reported at start-up instead of silently ignored at draw time).
pub(crate) fn uniform(gl: &GL, program: &WebGlProgram, name: &str) -> Result<WebGlUniformLocation, JsValue> {
    gl.get_uniform_location(program, name).ok_or_else(|| format!("WebGL: uniform {name} not found").into())
}

/// The two-triangle strip `(-1,-1) (1,-1) (-1,1) (1,1)` every instanced quad
/// of this crate is built from.
pub(crate) const QUAD_CORNERS: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
