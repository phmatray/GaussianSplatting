//! The exported datasets, checked from their files alone.
//!
//! The readers below are written from the documentation of the formats
//! (COLMAP's `read_write_model.py`, nerfstudio's data conventions), not
//! from the exporter. They never look at the capture: a dataset is right
//! when its own points and depth maps land on the same colours in its own
//! photographs, across frames. A mirrored, transposed or shifted pose puts
//! them somewhere else, which the tests demonstrate by trying exactly those
//! mistakes on the files they have just accepted.

mod common;

use std::path::Path;

use common::{scratch, small_sim, write_capture};
use gs_core::ingest::image::decode_jpeg;
use gs_refine::dataset::{Capture, SelectParams};
use gs_refine::export::{export_dataset, DatasetFormat};
use gs_refine::init::InitParams;
use gs_refine::job::CancelToken;

type M3 = [[f64; 3]; 3];

/// One image of a dataset in a neutral form: world-to-camera `x = R·X + t`
/// in the axes of COLMAP (x right, y down, z forward) and a pinhole.
#[derive(Clone, Debug)]
struct Posed {
    name: String,
    r: M3,
    t: [f64; 3],
    f: [f64; 2],
    c: [f64; 2],
    size: [usize; 2],
}

impl Posed {
    /// Pixel and depth of a world point, when it is in front of the camera.
    fn project(&self, p: [f64; 3]) -> Option<([f64; 2], f64)> {
        let x: Vec<f64> = (0..3).map(|i| self.r[i][0] * p[0] + self.r[i][1] * p[1] + self.r[i][2] * p[2] + self.t[i]).collect();
        (x[2] > 0.05).then(|| ([self.f[0] * x[0] / x[2] + self.c[0], self.f[1] * x[1] / x[2] + self.c[1]], x[2]))
    }

    /// The world point at a pixel and a depth along the camera axis.
    fn unproject(&self, pixel: [f64; 2], depth: f64) -> [f64; 3] {
        let x = [(pixel[0] - self.c[0]) / self.f[0] * depth - self.t[0], (pixel[1] - self.c[1]) / self.f[1] * depth - self.t[1], depth - self.t[2]];
        // Rᵀ·(x − t).
        [0, 1, 2].map(|j| self.r[0][j] * x[0] + self.r[1][j] * x[1] + self.r[2][j] * x[2])
    }
}

/// Rotation matrix of a Hamilton quaternion `w, x, y, z` (COLMAP's order).
fn rotation_of(q: [f64; 4]) -> M3 {
    let [w, x, y, z] = q;
    [
        [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - w * z), 2.0 * (x * z + w * y)],
        [2.0 * (x * y + w * z), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - w * x)],
        [2.0 * (x * z - w * y), 2.0 * (y * z + w * x), 1.0 - 2.0 * (x * x + y * y)],
    ]
}

struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let (head, tail) = self.0.split_at(N);
        self.0 = tail;
        head.try_into().unwrap()
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.bytes())
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.bytes())
    }
    fn i32(&mut self) -> i32 {
        i32::from_le_bytes(self.bytes())
    }
    fn f64(&mut self) -> f64 {
        f64::from_le_bytes(self.bytes())
    }
}

/// `cameras.bin` and `images.bin` as `read_write_model.py` reads them.
fn read_colmap_binary(model: &Path) -> Vec<Posed> {
    let data = std::fs::read(model.join("cameras.bin")).unwrap();
    let mut c = Cursor(&data);
    let mut cameras = std::collections::HashMap::new();
    for _ in 0..c.u64() {
        let (id, model_id, width, height) = (c.u32(), c.i32(), c.u64(), c.u64());
        assert_eq!(model_id, 1, "PINHOLE is model 1 and has four parameters");
        cameras.insert(id, ([c.f64(), c.f64()], [c.f64(), c.f64()], [width as usize, height as usize]));
    }
    assert!(c.0.is_empty(), "cameras.bin has trailing bytes");
    let data = std::fs::read(model.join("images.bin")).unwrap();
    let mut c = Cursor(&data);
    let mut images = Vec::new();
    for _ in 0..c.u64() {
        let _image_id = c.u32();
        let q = [c.f64(), c.f64(), c.f64(), c.f64()];
        let t = [c.f64(), c.f64(), c.f64()];
        let camera_id = c.u32();
        let end = c.0.iter().position(|b| *b == 0).expect("a NUL after the name");
        let name = String::from_utf8(c.0[..end].to_vec()).unwrap();
        c.0 = &c.0[end + 1..];
        // x, y, point3D id per 2D point.
        let points = c.u64() as usize;
        c.0 = &c.0[points * 24..];
        assert!((q.iter().map(|v| v * v).sum::<f64>() - 1.0).abs() < 1e-6, "not a unit quaternion");
        let (f, pp, size) = cameras[&camera_id];
        images.push(Posed { name, r: rotation_of(q), t, f, c: pp, size });
    }
    assert!(c.0.is_empty(), "images.bin has trailing bytes");
    images
}

/// `cameras.txt` and `images.txt`.
fn read_colmap_text(model: &Path) -> Vec<Posed> {
    let lines = |name: &str| -> Vec<String> { std::fs::read_to_string(model.join(name)).unwrap().lines().filter(|l| !l.starts_with('#')).map(str::to_string).collect() };
    let mut cameras = std::collections::HashMap::new();
    for line in lines("cameras.txt") {
        let w: Vec<&str> = line.split_whitespace().collect();
        assert_eq!((w.len(), w[1]), (8, "PINHOLE"));
        let n = |i: usize| w[i].parse::<f64>().unwrap();
        cameras.insert(w[0].to_string(), ([n(4), n(5)], [n(6), n(7)], [n(2) as usize, n(3) as usize]));
    }
    // Two lines per image; the second lists its 2D points and may be empty.
    lines("images.txt")
        .chunks(2)
        .map(|pair| {
            let w: Vec<&str> = pair[0].split_whitespace().collect();
            assert_eq!(w.len(), 10, "{}", pair[0]);
            let n = |i: usize| w[i].parse::<f64>().unwrap();
            let (f, c, size) = cameras[w[8]];
            Posed { name: w[9].to_string(), r: rotation_of([n(1), n(2), n(3), n(4)]), t: [n(5), n(6), n(7)], f, c, size }
        })
        .collect()
}

/// Points with colours: `(position, rgb)`.
type Points = Vec<([f64; 3], [u8; 3])>;

fn read_points_binary(model: &Path) -> Points {
    let data = std::fs::read(model.join("points3D.bin")).unwrap();
    let mut c = Cursor(&data);
    let mut points = Vec::new();
    for _ in 0..c.u64() {
        let _id = c.u64();
        let p = [c.f64(), c.f64(), c.f64()];
        let rgb: [u8; 3] = c.bytes();
        let _error = c.f64();
        let track = c.u64() as usize;
        c.0 = &c.0[track * 8..];
        points.push((p, rgb));
    }
    assert!(c.0.is_empty(), "points3D.bin has trailing bytes");
    points
}

fn read_points_text(model: &Path) -> Points {
    std::fs::read_to_string(model.join("points3D.txt"))
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let w: Vec<&str> = l.split_whitespace().collect();
            let n = |i: usize| w[i].parse::<f64>().unwrap();
            ([n(1), n(2), n(3)], [n(4) as u8, n(5) as u8, n(6) as u8])
        })
        .collect()
}

/// A binary little-endian PLY of `x y z` floats and `red green blue` bytes.
fn read_points_ply(path: &Path) -> Points {
    let data = std::fs::read(path).unwrap();
    let end = data.windows(11).position(|w| w == b"end_header\n").unwrap() + 11;
    let header = String::from_utf8_lossy(&data[..end]).to_string();
    assert!(header.starts_with("ply\nformat binary_little_endian 1.0\n"));
    let properties: Vec<&str> = header.lines().filter(|l| l.starts_with("property")).collect();
    assert_eq!(properties, ["property float x", "property float y", "property float z", "property uchar red", "property uchar green", "property uchar blue"]);
    let count: usize = header.lines().find_map(|l| l.strip_prefix("element vertex ")).unwrap().parse().unwrap();
    assert_eq!(data.len() - end, count * 15);
    data[end..]
        .chunks_exact(15)
        .map(|b| {
            let f = |i: usize| f32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as f64;
            ([f(0), f(4), f(8)], [b[12], b[13], b[14]])
        })
        .collect()
}

/// The photographs of a dataset, decoded at full size.
fn photographs(dir: &Path, images: &[Posed]) -> Vec<Vec<u8>> {
    images
        .iter()
        .map(|image| {
            let decoded = decode_jpeg(&std::fs::read(dir.join("images").join(&image.name)).unwrap(), image.size[0] as u32, image.size[1] as u32).unwrap();
            assert_eq!([decoded.width as usize, decoded.height as usize], image.size, "{}", image.name);
            decoded.rgb
        })
        .collect()
}

fn color_at(rgb: &[u8], size: [usize; 2], pixel: [f64; 2]) -> Option<[u8; 3]> {
    let (x, y) = (pixel[0].floor(), pixel[1].floor());
    (x >= 2.0 && y >= 2.0 && x < size[0] as f64 - 2.0 && y < size[1] as f64 - 2.0).then(|| {
        let i = (y as usize * size[0] + x as usize) * 3;
        [rgb[i], rgb[i + 1], rgb[i + 2]]
    })
}

/// The median of the samples; when a dataset leaves almost nothing to
/// compare (the points fall outside the photographs), the largest
/// difference there is.
fn median(mut values: Vec<f64>) -> f64 {
    if values.len() < 200 {
        return 255.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// Median colour difference (8-bit levels, mean over the channels) between
/// the points of a dataset and the pixels they project to. Only the points
/// nearest to the camera in their 3x3 pixel cell count: the others are
/// hidden behind them.
fn point_consistency(images: &[Posed], photos: &[Vec<u8>], points: &Points) -> f64 {
    let mut differences = Vec::new();
    for (image, rgb) in images.iter().zip(photos) {
        let cells = [image.size[0].div_ceil(3), image.size[1].div_ceil(3)];
        let cell = |pixel: [f64; 2]| (pixel[0] >= 0.0 && pixel[1] >= 0.0 && pixel[0] < image.size[0] as f64 && pixel[1] < image.size[1] as f64).then(|| pixel[1] as usize / 3 * cells[0] + pixel[0] as usize / 3);
        let mut nearest = vec![f64::INFINITY; cells[0] * cells[1]];
        for (p, _) in points {
            if let Some((at, depth)) = image.project(*p).and_then(|(pixel, depth)| cell(pixel).map(|at| (at, depth))) {
                nearest[at] = nearest[at].min(depth);
            }
        }
        for (p, color) in points.iter().step_by(3) {
            let Some((pixel, depth)) = image.project(*p) else { continue };
            if cell(pixel).is_none_or(|at| depth > nearest[at] + 0.03) {
                continue;
            }
            let Some(seen) = color_at(rgb, image.size, pixel) else { continue };
            differences.push((0..3).map(|k| (seen[k] as f64 - color[k] as f64).abs()).sum::<f64>() / 3.0);
        }
    }
    median(differences)
}

/// The mistakes a pose export can make, applied to a correct dataset.
fn wrong_poses(images: &[Posed]) -> Vec<(&'static str, Vec<Posed>)> {
    let map = |f: &dyn Fn(&Posed) -> Posed| images.iter().map(f).collect::<Vec<_>>();
    let transpose = |r: &M3| [0, 1, 2].map(|i| [0, 1, 2].map(|j| r[j][i]));
    vec![
        // Camera-to-world written where world-to-camera belongs.
        ("rotation transposed", map(&|p| Posed { r: transpose(&p.r), ..p.clone() })),
        // The camera position written as the translation: t = C instead of −R·C.
        ("position as translation", map(&|p| Posed { t: [0, 1, 2].map(|j| -(p.r[0][j] * p.t[0] + p.r[1][j] * p.t[1] + p.r[2][j] * p.t[2])), ..p.clone() })),
        // OpenGL axes left in: y up, z backward.
        ("axes not flipped", map(&|p| Posed { r: [p.r[0], p.r[1].map(|v| -v), p.r[2].map(|v| -v)], t: [p.t[0], -p.t[1], -p.t[2]], ..p.clone() })),
        // A mirrored world.
        ("mirrored", map(&|p| Posed { r: p.r.map(|row| [-row[0], row[1], row[2]]), ..p.clone() })),
        // The floor shift applied to the points but not to the cameras.
        ("vertical shift", map(&|p| Posed { t: [0, 1, 2].map(|i| p.t[i] + p.r[i][1] * 0.35), ..p.clone() })),
        // The poses of the neighbouring frames.
        ("off by one frame", (0..images.len()).map(|i| Posed { name: images[i].name.clone(), ..images[(i + 1) % images.len()].clone() }).collect()),
    ]
}

fn every_frame() -> SelectParams {
    SelectParams { max_views: 0, holdout_every: 0, min_move: 0.0, min_turn: 0.0, blur_ratio: 0.0 }
}

fn export(capture: &Path, dir: &Path, format: DatasetFormat) -> gs_refine::export::ExportSummary {
    let capture = Capture::open(capture).unwrap();
    export_dataset(&capture, dir, format, &every_frame(), &InitParams::default(), &CancelToken::default(), &mut |_, _| {}).unwrap()
}

#[test]
fn colmap_points_land_on_their_colours_in_every_photograph() {
    let dir = scratch("colmap-files");
    // An orbit around a subject: the views differ a lot, so a wrong pose
    // cannot hide.
    let written = write_capture(&dir.join("o.gscap"), "object", 16, small_sim(), |_, _| {});
    let out = dir.join("colmap");
    let summary = export(&written.path, &out, DatasetFormat::Colmap);
    let model = out.join("sparse").join("0");
    let (binary, text) = (read_colmap_binary(&model), read_colmap_text(&model));
    assert_eq!((binary.len(), text.len(), summary.images), (16, 16, 16));
    for (b, t) in binary.iter().zip(&text) {
        assert_eq!(b.name, t.name);
        assert_eq!((b.size, b.size), (t.size, [160, 120]));
        // The text file prints 32-bit floats: equal to that precision.
        let close = |x: f64, y: f64| (x - y).abs() < 1e-5 * (1.0 + x.abs());
        assert!((0..3).all(|i| close(b.t[i], t.t[i]) && (0..3).all(|j| close(b.r[i][j], t.r[i][j]))), "{}", b.name);
        assert!(close(b.f[0], t.f[0]) && close(b.f[1], t.f[1]) && close(b.c[0], t.c[0]) && close(b.c[1], t.c[1]));
        // A rotation, not a reflection.
        let r = &b.r;
        let det = r[0][0] * (r[1][1] * r[2][2] - r[1][2] * r[2][1]) - r[0][1] * (r[1][0] * r[2][2] - r[1][2] * r[2][0]) + r[0][2] * (r[1][0] * r[2][1] - r[1][1] * r[2][0]);
        assert!((det - 1.0).abs() < 1e-5, "determinant {det}");
    }
    let (points, points_text) = (read_points_binary(&model), read_points_text(&model));
    assert_eq!((points.len(), points_text.len()), (summary.points, summary.points));
    assert!(points.iter().zip(&points_text).all(|(a, b)| a.1 == b.1 && (0..3).all(|k| (a.0[k] - b.0[k]).abs() < 1e-4)));

    let photos = photographs(&out, &binary);
    let right = point_consistency(&binary, &photos, &points);
    eprintln!("COLMAP: median colour difference of the points in the photographs {right:.1} levels");
    let wrong: Vec<(&str, f64)> = wrong_poses(&binary).into_iter().map(|(mistake, poses)| (mistake, point_consistency(&poses, &photos, &points))).collect();
    eprintln!("  with mistakes: {wrong:.1?}");
    assert!(right < 20.0, "the points do not land on their colours: {right:.1} levels");
    for (mistake, wrong) in wrong {
        assert!(wrong > 1.5 * right && wrong > 24.0, "the check cannot tell {mistake} ({wrong:.1}) from the truth ({right:.1})");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// `transforms.json` in the neutral form: nerfstudio stores camera-to-world
/// with the camera looking along −z and y up; COLMAP's axes are those with
/// y and z negated.
fn read_nerfstudio(dir: &Path) -> (Vec<Posed>, Vec<String>, f64, String) {
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("transforms.json")).unwrap()).unwrap();
    assert_eq!(meta["camera_model"], "OPENCV");
    for key in ["k1", "k2", "p1", "p2"] {
        assert_eq!(meta[key].as_f64(), Some(0.0), "the photographs are not distorted");
    }
    let mut images = Vec::new();
    let mut depths = Vec::new();
    for entry in meta["frames"].as_array().unwrap() {
        let n = |key: &str| entry[key].as_f64().unwrap_or_else(|| panic!("no {key}"));
        let m: Vec<Vec<f64>> = entry["transform_matrix"].as_array().unwrap().iter().map(|row| row.as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect()).collect();
        assert_eq!((m.len(), m[3].clone()), (4, vec![0.0, 0.0, 0.0, 1.0]));
        // Columns of the camera-to-world rotation are the camera's axes in
        // the world; flip y and z, then invert.
        let axes = [[m[0][0], m[1][0], m[2][0]], [-m[0][1], -m[1][1], -m[2][1]], [-m[0][2], -m[1][2], -m[2][2]]];
        let center = [m[0][3], m[1][3], m[2][3]];
        let t = [0, 1, 2].map(|i| -(axes[i][0] * center[0] + axes[i][1] * center[1] + axes[i][2] * center[2]));
        let file = entry["file_path"].as_str().unwrap();
        images.push(Posed { name: file.trim_start_matches("images/").to_string(), r: axes, t, f: [n("fl_x"), n("fl_y")], c: [n("cx"), n("cy")], size: [n("w") as usize, n("h") as usize] });
        depths.push(entry["depth_file_path"].as_str().unwrap().to_string());
    }
    (images, depths, meta["depth_unit_scale_factor"].as_f64().unwrap(), meta["ply_file_path"].as_str().unwrap().to_string())
}

fn read_depth_png(path: &Path) -> (usize, usize, Vec<u16>) {
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
    let mut reader = decoder.read_info().unwrap();
    let mut buffer = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buffer).unwrap();
    assert_eq!((info.bit_depth, info.color_type), (png::BitDepth::Sixteen, png::ColorType::Grayscale));
    (info.width as usize, info.height as usize, buffer.chunks_exact(2).map(|b| u16::from_be_bytes([b[0], b[1]])).collect())
}

/// Median colour difference between a pixel of one photograph and the pixel
/// of the *next* photograph that its depth sends it to.
fn depth_consistency(images: &[Posed], photos: &[Vec<u8>], depths: &[(usize, usize, Vec<u16>)], unit: f64) -> f64 {
    let mut differences = Vec::new();
    for i in 0..images.len() {
        let (j, (dw, dh, depth)) = ((i + 1) % images.len(), &depths[i]);
        let (from, to) = (&images[i], &images[j]);
        for at in (0..dw * dh).step_by(5) {
            if depth[at] == 0 {
                continue;
            }
            // The depth map covers the field of view of the photograph.
            let pixel = [((at % dw) as f64 + 0.5) * from.size[0] as f64 / *dw as f64, ((at / dw) as f64 + 0.5) * from.size[1] as f64 / *dh as f64];
            let world = from.unproject(pixel, depth[at] as f64 * unit);
            let (Some(here), Some((there, _))) = (color_at(&photos[i], from.size, pixel), to.project(world)) else { continue };
            let Some(seen) = color_at(&photos[j], to.size, there) else { continue };
            differences.push((0..3).map(|k| (seen[k] as f64 - here[k] as f64).abs()).sum::<f64>() / 3.0);
        }
    }
    median(differences)
}

#[test]
fn nerfstudio_depth_maps_carry_pixels_into_the_next_photograph() {
    let dir = scratch("ns-files");
    let written = write_capture(&dir.join("o.gscap"), "object", 24, small_sim(), |_, _| {});
    let out = dir.join("ns");
    let summary = export(&written.path, &out, DatasetFormat::Nerfstudio);
    let (images, depth_files, unit, ply) = read_nerfstudio(&out);
    assert_eq!((images.len(), summary.depth_maps, unit), (24, 24, 0.001));
    let photos = photographs(&out, &images);
    let depths: Vec<_> = depth_files.iter().map(|f| read_depth_png(&out.join(f))).collect();
    assert!(depths.iter().all(|d| (d.0, d.1) == (64, 48)));

    let right = depth_consistency(&images, &photos, &depths, unit);
    eprintln!("nerfstudio: median colour difference of a pixel carried into the next photograph {right:.1} levels");
    assert!(right < 10.0, "depth and poses do not agree: {right:.1} levels");
    for (mistake, poses) in wrong_poses(&images) {
        let wrong = depth_consistency(&poses, &photos, &depths, unit);
        eprintln!("  with {mistake}: {wrong:.1} levels");
        // Carrying a pixel from one camera to the next only sees the pose
        // of one relative to the other, which a common shift, a mirror or
        // the same step along a regular orbit leave alone; the points below
        // catch those.
        if matches!(mistake, "axes not flipped" | "rotation transposed" | "position as translation") {
            assert!(wrong > 2.0 * right && wrong > 15.0, "the check cannot tell {mistake} ({wrong:.1}) from the truth ({right:.1})");
        }
    }
    // Depth in other units than the file announces.
    for scale in [0.5, 2.0] {
        let wrong = depth_consistency(&images, &photos, &depths, unit * scale);
        assert!(wrong > 2.0 * right && wrong > 15.0, "depth times {scale} goes unnoticed: {wrong:.1} against {right:.1}");
    }

    // The initial points are in the same world as the cameras.
    let points = read_points_ply(&out.join(&ply));
    assert_eq!(points.len(), summary.points);
    let right = point_consistency(&images, &photos, &points);
    eprintln!("nerfstudio: median colour difference of the points {right:.1} levels");
    let wrong: Vec<(&str, f64)> = wrong_poses(&images).into_iter().map(|(mistake, poses)| (mistake, point_consistency(&poses, &photos, &points))).collect();
    eprintln!("  with mistakes: {wrong:.1?}");
    assert!(right < 20.0, "the points do not land on their colours: {right:.1} levels");
    for (mistake, wrong) in wrong {
        assert!(wrong > 1.5 * right && wrong > 24.0, "the check cannot tell {mistake} ({wrong:.1}) from the truth ({right:.1})");
    }
    let _ = std::fs::remove_dir_all(dir);
}
