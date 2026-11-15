//! Writes a capture as a standard dataset, so that existing trainers can be
//! run on it and their `.ply` opened in the viewer.
//!
//! Both formats get the photographs as recorded (the JPEG bytes are copied,
//! not re-encoded), one pinhole camera per image (ARKit's intrinsics move a
//! little with the focus), the poses in the world viewers see (the capture's
//! world with the floor at `y = 0`, meters, +Y up) and the fused cloud as
//! initial points.
//!
//! **COLMAP** (`sparse/0/{cameras,images,points3D}.{txt,bin}`, `images/`).
//! COLMAP stores the world-to-camera transform of a camera that looks along
//! +Z with +X right and +Y *down*: with `F = diag(1, −1, −1)` and our
//! world-to-camera `[R | t]`, the file holds `F·R` as a Hamilton quaternion
//! `qw qx qy qz` and `F·t`. Pixel coordinates need no change: both put the
//! center of the top-left pixel at `(0.5, 0.5)`.
//!
//! **nerfstudio** (`transforms.json`, `images/`, `depth/`, `masks/`,
//! `sparse_pc.ply`). `transform_matrix` is camera-to-world in the OpenGL
//! convention, which is ours: the matrix is written as it is, row by row.
//! Depth maps are 16-bit PNGs in millimeters (`depth_unit_scale_factor`
//! 0.001), `0` where the sensor gave nothing or had low confidence.
//!
//! Trainers must be told to leave the world alone or their result comes
//! back turned, centered and rescaled: for nerfstudio
//! `--orientation-method none --center-method none --auto-scale-poses False`.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use glam::{Mat3, Quat, Vec3};
use gs_core::ingest::frame::CaptureFrame;
use gs_core::{Camera, Splat};

use crate::dataset::{select_views, views_of, Capture, SelectParams, ViewInfo};
use crate::init::{fuse, InitParams};
use crate::job::{CancelToken, RefineError};

/// Dataset layouts [`export_dataset`] can write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatasetFormat {
    Colmap,
    Nerfstudio,
}

impl DatasetFormat {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "colmap" => Some(Self::Colmap),
            "nerfstudio" => Some(Self::Nerfstudio),
            _ => None,
        }
    }
}

/// What was written.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportSummary {
    pub dir: PathBuf,
    pub images: usize,
    pub points: usize,
    pub depth_maps: usize,
    pub masks: usize,
}

/// The pose of a camera as COLMAP stores it: `(qw, qx, qy, qz)` and
/// `(tx, ty, tz)` of the world-to-camera transform of a camera that looks
/// along +Z with +Y down.
pub fn colmap_pose(cam: &Camera) -> ([f64; 4], [f64; 3]) {
    let cam_from_world = cam.world_from_cam.inverse();
    let flip = Mat3::from_diagonal(Vec3::new(1.0, -1.0, -1.0));
    let rotation = flip * Mat3::from(cam_from_world.matrix3);
    let translation = flip * Vec3::from(cam_from_world.translation);
    let q = Quat::from_mat3(&rotation).normalize();
    // One of the two quaternions of a rotation, the one with qw >= 0.
    let q = if q.w < 0.0 { -q } else { q };
    ([q.w as f64, q.x as f64, q.y as f64, q.z as f64], [translation.x as f64, translation.y as f64, translation.z as f64])
}

/// The pose of a camera as nerfstudio's `transform_matrix`: camera-to-world,
/// OpenGL axes, four rows of four.
pub fn nerfstudio_matrix(cam: &Camera) -> [[f32; 4]; 4] {
    let m = glam::Mat4::from(cam.world_from_cam);
    let row = |i: usize| [m.x_axis[i], m.y_axis[i], m.z_axis[i], m.w_axis[i]];
    [row(0), row(1), row(2), row(3)]
}

fn image_name(view: &ViewInfo, capture: &Capture) -> String {
    format!("frame_{:06}.jpg", capture.frames[view.frame].header.index)
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), RefineError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(std::fs::write(path, bytes)?)
}

/// A PNG of one channel, 8 or 16 bits.
fn gray_png(width: u32, height: u32, depth: png::BitDepth, data: &[u8]) -> Result<Vec<u8>, RefineError> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(depth);
    let io = |e: png::EncodingError| RefineError::Io(std::io::Error::other(e.to_string()));
    encoder.write_header().map_err(io)?.write_image_data(data).map_err(io)?;
    Ok(out)
}

/// The depth map of a frame in millimeters, `0` where it is not to be
/// trusted, as a 16-bit PNG at the size of the depth grid.
fn depth_png(frame: &CaptureFrame) -> Result<Vec<u8>, RefineError> {
    let n = frame.depth_pixels();
    let confidence = frame.confidence.as_deref().filter(|c| c.len() == n);
    let mut data = Vec::with_capacity(n * 2);
    for i in 0..n {
        let mm = frame.depth_mm.get(i).copied().unwrap_or(0);
        let trusted = confidence.is_none_or(|c| c[i] >= 1);
        // PNG samples are big-endian.
        data.extend_from_slice(&(if trusted { mm } else { 0 }).to_be_bytes());
    }
    gray_png(frame.depth_width as u32, frame.depth_height as u32, png::BitDepth::Sixteen, &data)
}

/// The person mask of a frame at the size of its photograph (nearest
/// neighbour), white where the person is.
fn mask_png(frame: &CaptureFrame, mask: &[u8]) -> Result<Vec<u8>, RefineError> {
    let (w, h) = (frame.color_width as usize, frame.color_height as usize);
    let (dw, dh) = (frame.depth_width as usize, frame.depth_height as usize);
    let mut data = Vec::with_capacity(w * h);
    for y in 0..h {
        let sy = ((2 * y + 1) * dh / (2 * h)).min(dh - 1);
        for x in 0..w {
            let sx = ((2 * x + 1) * dw / (2 * w)).min(dw - 1);
            data.push(if mask[sy * dw + sx] >= 128 { 255 } else { 0 });
        }
    }
    gray_png(w as u32, h as u32, png::BitDepth::Eight, &data)
}

/// Points as a little-endian binary PLY with `x y z` floats and
/// `red green blue` bytes: what nerfstudio reads as `ply_file_path`.
pub fn points_ply(splats: &[Splat]) -> Vec<u8> {
    let mut out = format!(
        "ply\nformat binary_little_endian 1.0\nelement vertex {}\nproperty float x\nproperty float y\nproperty float z\n\
         property uchar red\nproperty uchar green\nproperty uchar blue\nend_header\n",
        splats.len()
    )
    .into_bytes();
    for s in splats {
        for v in s.pos {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&s.rgba[..3]);
    }
    out
}

/// The three text files of a COLMAP model.
fn colmap_text(views: &[ViewInfo], names: &[String], points: &[Splat]) -> (String, String, String) {
    let mut cameras = String::from("# Camera list with one line of data per camera:\n#   CAMERA_ID, MODEL, WIDTH, HEIGHT, PARAMS[]\n");
    let mut images = String::from(
        "# Image list with two lines of data per image:\n#   IMAGE_ID, QW, QX, QY, QZ, TX, TY, TZ, CAMERA_ID, NAME\n#   POINTS2D[] as (X, Y, POINT3D_ID)\n",
    );
    for (i, (view, name)) in views.iter().zip(names).enumerate() {
        let (id, cam) = (i + 1, &view.cam);
        let _ = writeln!(cameras, "{id} PINHOLE {} {} {} {} {} {}", cam.width, cam.height, cam.fx, cam.fy, cam.cx, cam.cy);
        let (q, t) = colmap_pose(cam);
        let _ = writeln!(images, "{id} {} {} {} {} {} {} {} {id} {name}\n", q[0], q[1], q[2], q[3], t[0], t[1], t[2]);
    }
    let mut points3d = String::from("# 3D point list with one line of data per point:\n#   POINT3D_ID, X, Y, Z, R, G, B, ERROR, TRACK[] as (IMAGE_ID, POINT2D_IDX)\n");
    for (i, s) in points.iter().enumerate() {
        let _ = writeln!(points3d, "{} {} {} {} {} {} {} 0", i + 1, s.pos[0], s.pos[1], s.pos[2], s.rgba[0], s.rgba[1], s.rgba[2]);
    }
    (cameras, images, points3d)
}

/// The three binary files of a COLMAP model (`cameras.bin`, `images.bin`,
/// `points3D.bin`), little-endian as COLMAP writes them.
fn colmap_binary(views: &[ViewInfo], names: &[String], points: &[Splat]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    const PINHOLE: i32 = 1;
    let mut cameras = (views.len() as u64).to_le_bytes().to_vec();
    let mut images = (views.len() as u64).to_le_bytes().to_vec();
    for (i, (view, name)) in views.iter().zip(names).enumerate() {
        let (id, cam) = ((i + 1) as i32, &view.cam);
        cameras.extend_from_slice(&id.to_le_bytes());
        cameras.extend_from_slice(&PINHOLE.to_le_bytes());
        cameras.extend_from_slice(&(cam.width as u64).to_le_bytes());
        cameras.extend_from_slice(&(cam.height as u64).to_le_bytes());
        for v in [cam.fx, cam.fy, cam.cx, cam.cy] {
            cameras.extend_from_slice(&(v as f64).to_le_bytes());
        }
        let (q, t) = colmap_pose(cam);
        images.extend_from_slice(&id.to_le_bytes());
        for v in q.iter().chain(&t) {
            images.extend_from_slice(&v.to_le_bytes());
        }
        images.extend_from_slice(&id.to_le_bytes());
        images.extend_from_slice(name.as_bytes());
        images.push(0);
        // No 2D points.
        images.extend_from_slice(&0u64.to_le_bytes());
    }
    let mut points3d = (points.len() as u64).to_le_bytes().to_vec();
    for (i, s) in points.iter().enumerate() {
        points3d.extend_from_slice(&(i as u64 + 1).to_le_bytes());
        for v in s.pos {
            points3d.extend_from_slice(&(v as f64).to_le_bytes());
        }
        points3d.extend_from_slice(&s.rgba[..3]);
        points3d.extend_from_slice(&0f64.to_le_bytes());
        // Empty track.
        points3d.extend_from_slice(&0u64.to_le_bytes());
    }
    (cameras, images, points3d)
}

/// Writes `capture` under `dir` in `format`. The views are chosen like the
/// optimiser's (`select.holdout_every` is ignored: trainers make their own
/// split) and the initial points are the fused cloud of those views.
pub fn export_dataset(
    capture: &Capture,
    dir: &Path,
    format: DatasetFormat,
    select: &SelectParams,
    init: &InitParams,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<ExportSummary, RefineError> {
    let selection = select_views(capture, &SelectParams { holdout_every: 0, ..*select });
    if selection.train.is_empty() {
        return Err(RefineError::Empty("no usable view in the capture"));
    }
    let total = selection.fusion.len() + selection.train.len();
    let fused = fuse(capture, &selection.fusion, init, |done, _| {
        progress(done, total);
        !cancel.is_cancelled()
    })?;
    let Some(fused) = fused else { return Err(RefineError::Cancelled) };
    let views = views_of(capture, &selection.train, fused.floor_y);
    let names: Vec<String> = views.iter().map(|v| image_name(v, capture)).collect();
    std::fs::create_dir_all(dir.join("images"))?;

    let mut summary = ExportSummary { dir: dir.to_path_buf(), images: 0, points: fused.splats.len(), depth_maps: 0, masks: 0 };
    let mut frames_json = Vec::new();
    for (k, (view, name)) in views.iter().zip(&names).enumerate() {
        if cancel.is_cancelled() {
            return Err(RefineError::Cancelled);
        }
        progress(selection.fusion.len() + k, total);
        let frame = capture.frame(view.frame)?;
        write_file(&dir.join("images").join(name), &frame.color)?;
        summary.images += 1;
        if format == DatasetFormat::Nerfstudio {
            let stem = name.trim_end_matches(".jpg");
            let cam = &view.cam;
            let mut entry = serde_json::json!({
                "file_path": format!("images/{name}"),
                "fl_x": cam.fx, "fl_y": cam.fy, "cx": cam.cx, "cy": cam.cy, "w": cam.width, "h": cam.height,
                "transform_matrix": nerfstudio_matrix(cam),
            });
            if !frame.depth_mm.is_empty() {
                write_file(&dir.join("depth").join(format!("{stem}.png")), &depth_png(&frame)?)?;
                entry["depth_file_path"] = format!("depth/{stem}.png").into();
                summary.depth_maps += 1;
            }
            if let Some(mask) = frame.mask.as_deref().filter(|m| m.len() == frame.depth_pixels()) {
                write_file(&dir.join("masks").join(format!("{stem}.png")), &mask_png(&frame, mask)?)?;
                entry["mask_path"] = format!("masks/{stem}.png").into();
                summary.masks += 1;
            }
            frames_json.push(entry);
        }
    }

    match format {
        DatasetFormat::Colmap => {
            let model = dir.join("sparse").join("0");
            let (cameras, images, points3d) = colmap_text(&views, &names, &fused.splats);
            write_file(&model.join("cameras.txt"), cameras.as_bytes())?;
            write_file(&model.join("images.txt"), images.as_bytes())?;
            write_file(&model.join("points3D.txt"), points3d.as_bytes())?;
            let (cameras, images, points3d) = colmap_binary(&views, &names, &fused.splats);
            write_file(&model.join("cameras.bin"), &cameras)?;
            write_file(&model.join("images.bin"), &images)?;
            write_file(&model.join("points3D.bin"), &points3d)?;
        }
        DatasetFormat::Nerfstudio => {
            write_file(&dir.join("sparse_pc.ply"), &points_ply(&fused.splats))?;
            let transforms = serde_json::json!({
                "camera_model": "OPENCV",
                "k1": 0.0, "k2": 0.0, "p1": 0.0, "p2": 0.0,
                "depth_unit_scale_factor": 0.001,
                "ply_file_path": "sparse_pc.ply",
                "frames": frames_json,
            });
            let mut file = std::io::BufWriter::new(std::fs::File::create(dir.join("transforms.json"))?);
            serde_json::to_writer_pretty(&mut file, &transforms).map_err(|e| RefineError::Io(e.into()))?;
            file.flush()?;
        }
    }
    progress(total, total);
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies a COLMAP pose the way COLMAP does: `x_cam = R(q)·X + t`,
    /// with the Hamilton quaternion written out by hand.
    fn colmap_camera_point(q: [f64; 4], t: [f64; 3], p: [f64; 3]) -> [f64; 3] {
        let [w, x, y, z] = q;
        let r = [
            [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - w * z), 2.0 * (x * z + w * y)],
            [2.0 * (x * y + w * z), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - w * x)],
            [2.0 * (x * z - w * y), 2.0 * (y * z + w * x), 1.0 - 2.0 * (x * x + y * y)],
        ];
        [0, 1, 2].map(|i| r[i][0] * p[0] + r[i][1] * p[1] + r[i][2] * p[2] + t[i])
    }

    #[test]
    fn a_camera_at_the_origin_looks_along_plus_z_with_y_down_in_colmap() {
        // Our camera at the origin, looking along -Z, +Y up.
        let cam = Camera::look_at(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y, 1.0, 640, 480);
        let (q, t) = colmap_pose(&cam);
        assert!(t.iter().all(|v| v.abs() < 1e-6));
        // Two meters ahead and one up, half a meter to the right.
        let c = colmap_camera_point(q, t, [0.5, 1.0, -2.0]);
        assert!((c[0] - 0.5).abs() < 1e-6 && (c[1] + 1.0).abs() < 1e-6 && (c[2] - 2.0).abs() < 1e-6, "{c:?}");
        // A half turn about X: (qw, qx, qy, qz) = (0, 1, 0, 0).
        assert!(q[0].abs() < 1e-6 && (q[1].abs() - 1.0).abs() < 1e-6 && q[2].abs() < 1e-6 && q[3].abs() < 1e-6, "{q:?}");
    }

    #[test]
    fn colmap_poses_project_like_our_cameras() {
        let cam = Camera::look_at(Vec3::new(1.5, 1.2, -0.7), Vec3::new(-0.4, 0.6, 2.0), Vec3::Y, 0.9, 640, 480);
        let (q, t) = colmap_pose(&cam);
        assert!(((q.iter().map(|v| v * v).sum::<f64>()) - 1.0).abs() < 1e-6 && q[0] >= 0.0);
        for p in [Vec3::new(-0.4, 0.6, 2.0), Vec3::new(0.3, 1.4, 1.1), Vec3::new(-1.0, 0.1, 2.5)] {
            let (px, py, depth) = cam.project(p).unwrap();
            let c = colmap_camera_point(q, t, [p.x as f64, p.y as f64, p.z as f64]);
            // COLMAP's pinhole: u = fx·x/z + cx, v = fy·y/z + cy.
            let (u, v) = (cam.fx as f64 * c[0] / c[2] + cam.cx as f64, cam.fy as f64 * c[1] / c[2] + cam.cy as f64);
            assert!((u - px as f64).abs() < 1e-3 && (v - py as f64).abs() < 1e-3 && (c[2] - depth as f64).abs() < 1e-5, "{u} {v} vs {px} {py}");
        }
    }

    #[test]
    fn the_nerfstudio_matrix_is_camera_to_world_in_rows() {
        let eye = Vec3::new(1.0, 2.0, 3.0);
        let cam = Camera::look_at(eye, eye + Vec3::X, Vec3::Y, 1.0, 64, 48);
        let m = nerfstudio_matrix(&cam);
        // Last column: the camera position. Third column: the camera's +Z
        // (backward) axis in the world, here -X. Second: up. First: right,
        // which is +Z for a camera that looks along +X.
        assert_eq!([m[0][3], m[1][3], m[2][3]], [1.0, 2.0, 3.0]);
        assert_eq!(m[3], [0.0, 0.0, 0.0, 1.0]);
        let column = |j: usize| Vec3::new(m[0][j], m[1][j], m[2][j]);
        assert!(column(2).distance(Vec3::NEG_X) < 1e-6 && column(1).distance(Vec3::Y) < 1e-6 && column(0).distance(Vec3::Z) < 1e-6);
    }

    #[test]
    fn the_point_cloud_has_the_announced_size() {
        let splat = Splat { pos: [1.0, 2.0, 3.0], scale: [0.1; 3], rot: [0.0, 0.0, 0.0, 1.0], rgba: [10, 20, 30, 255] };
        let ply = points_ply(&[splat, splat]);
        let header_end = ply.windows(11).position(|w| w == b"end_header\n").unwrap() + 11;
        assert_eq!(ply.len() - header_end, 2 * 15);
        assert_eq!(&ply[header_end..header_end + 4], &1.0f32.to_le_bytes());
        assert_eq!(&ply[header_end + 12..header_end + 15], &[10, 20, 30]);
    }
}
