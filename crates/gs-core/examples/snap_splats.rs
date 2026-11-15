//! Renders hand-built splat clouds with the CPU reference rasterizer and
//! writes PNG snapshots, including the same cloud after a wire-codec round
//! trip and after `.ply` / `.splat` file round trips.
//!
//! ```text
//! cargo run -p gs-core --release --example snap_splats -- <output directory>
//! ```

use std::error::Error;
use std::f32::consts::{PI, TAU};
use std::path::{Path, PathBuf};
use std::time::Instant;

use gs_core::codec::{self, LayerMode};
use gs_core::glam::{Quat, Vec3};
use gs_core::io;
use gs_core::raster::{mean_abs_error, psnr, rasterize, Image, RasterOptions};
use gs_core::{Camera, Splat};

const LIGHT: Vec3 = Vec3::new(0.35, 0.85, 0.4);

/// Diffuse shading of an sRGB base color, good enough for a test picture.
fn shade(base: [f32; 3], normal: Vec3) -> [u8; 4] {
    let diffuse = normal.dot(LIGHT.normalize()).max(0.0);
    let k = 0.22 + 0.78 * diffuse;
    [(base[0] * k * 255.0) as u8, (base[1] * k * 255.0) as u8, (base[2] * k * 255.0) as u8, 255]
}

fn hsv(h: f32, s: f32, v: f32) -> [f32; 3] {
    let f = |n: f32| {
        let k = (n + h * 6.0) % 6.0;
        v - v * s * k.min(4.0 - k).clamp(0.0, 1.0)
    };
    [f(5.0), f(3.0), f(1.0)]
}

/// Sphere made of `n` surfels on a Fibonacci lattice, with colored bands.
fn sphere(center: Vec3, radius: f32, n: usize) -> Vec<Splat> {
    let spacing = (4.0 * PI * radius * radius / n as f32).sqrt();
    let golden = PI * (3.0 - 5f32.sqrt());
    (0..n)
        .map(|i| {
            let y = 1.0 - 2.0 * (i as f32 + 0.5) / n as f32;
            let r = (1.0 - y * y).sqrt();
            let phi = golden * i as f32;
            let normal = Vec3::new(r * phi.cos(), y, r * phi.sin());
            let longitude = normal.z.atan2(normal.x) / TAU + 0.5;
            let band = ((y * 0.5 + 0.5) * 7.0) as i32 + (longitude * 12.0) as i32;
            let base = if band % 2 == 0 { [0.95, 0.35, 0.15] } else { [0.95, 0.9, 0.8] };
            Splat::surfel(center + normal * radius, normal, 0.75 * spacing, 0.1 * spacing, shade(base, normal))
        })
        .collect()
}

/// Checkerboard floor in the plane `y = 0`.
fn floor(half_size: f32, step: f32) -> Vec<Splat> {
    let n = (half_size / step) as i32;
    let mut splats = Vec::new();
    for iz in -n..=n {
        for ix in -n..=n {
            let (x, z) = (ix as f32 * step, iz as f32 * step);
            let tile = ((x / 0.25).floor() + (z / 0.25).floor()) as i32;
            let base = if tile.rem_euclid(2) == 0 { [0.25, 0.35, 0.55] } else { [0.8, 0.8, 0.75] };
            splats.push(Splat::surfel(Vec3::new(x, 0.0, z), Vec3::Y, 0.75 * step, 0.1 * step, shade(base, Vec3::Y)));
        }
    }
    splats
}

/// Fan of thin, strongly anisotropic Gaussians ("needles") in the plane
/// through `center` facing +Z, plus a ring of small round ones.
fn needles(center: Vec3, length: f32) -> Vec<Splat> {
    let count = 16;
    let mut splats = Vec::new();
    for i in 0..count {
        let angle = PI * i as f32 / count as f32;
        let rot = Quat::from_rotation_z(angle);
        let c = hsv(i as f32 / count as f32, 0.85, 1.0);
        splats.push(Splat {
            pos: center.to_array(),
            scale: [length / 3.0, length / 150.0, length / 150.0],
            rot: rot.to_array(),
            rgba: [(c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8, 230],
        });
    }
    for i in 0..2 * count {
        let angle = PI * i as f32 / count as f32;
        let pos = center + Vec3::new(angle.cos(), angle.sin(), 0.0) * (length * 1.12);
        // Flat discs seen under varying tilt.
        let normal = Vec3::new(angle.cos() * 0.8, angle.sin() * 0.8, 0.6);
        splats.push(Splat::surfel(pos, normal, length / 40.0, length / 400.0, [240, 240, 240, 255]));
    }
    splats
}

fn save_png(dir: &Path, name: &str, image: &Image) -> Result<PathBuf, Box<dyn Error>> {
    let path = dir.join(name);
    let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
    let mut encoder = png::Encoder::new(file, image.width, image.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&image.rgb)?;
    Ok(path)
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "snap_splats_out".to_string()));
    std::fs::create_dir_all(&dir)?;

    let sphere_center = Vec3::new(0.0, 0.5, 0.0);
    let needle_center = Vec3::new(-1.0, 0.7, -0.6);
    let mut scene = floor(1.6, 0.02);
    scene.extend(sphere(sphere_center, 0.5, 12_000));
    scene.extend(needles(needle_center, 0.5));
    println!("scene: {} splats", scene.len());

    let opts = RasterOptions { background: [18, 20, 28], ..Default::default() };
    let fov = 50f32.to_radians();
    let cam_a = Camera::look_at(Vec3::new(1.7, 1.4, 2.4), Vec3::new(-0.2, 0.4, 0.0), Vec3::Y, fov, 640, 480);
    let cam_b = Camera::look_at(Vec3::new(-2.4, 0.7, 1.6), Vec3::new(-0.3, 0.45, -0.2), Vec3::Y, fov, 640, 480);
    let cam_close = Camera::look_at(Vec3::new(0.55, 0.8, 0.95), sphere_center, Vec3::Y, fov, 640, 480);
    let cam_needles = Camera::look_at(needle_center + Vec3::new(0.0, 0.0, 1.5), needle_center, Vec3::Y, fov, 480, 480);

    // Reference renders.
    let mut reference = None;
    for (name, cam) in [
        ("scene_a.png", &cam_a),
        ("scene_b.png", &cam_b),
        ("sphere_close.png", &cam_close),
        ("needles.png", &cam_needles),
    ] {
        let start = Instant::now();
        let image = rasterize(&scene, cam, &opts);
        let ms = start.elapsed().as_secs_f64() * 1e3;
        println!("{:<28} {ms:7.1} ms  -> {}", name, save_png(&dir, name, &image)?.display());
        if name == "scene_a.png" {
            reference = Some(image);
        }
    }
    let reference = reference.expect("scene_a is rendered");

    // Display variants.
    let points = rasterize(&scene, &cam_a, &RasterOptions { point_mode: true, ..opts });
    println!("{:<28} -> {}", "scene_a_points.png", save_png(&dir, "scene_a_points.png", &points)?.display());
    let small = rasterize(&scene, &cam_a, &RasterOptions { splat_scale: 0.4, ..opts });
    println!("{:<28} -> {}", "scene_a_scale04.png", save_png(&dir, "scene_a_scale04.png", &small)?.display());

    // Round trips: wire codec, .ply, .splat.
    let mut wire = Vec::new();
    codec::encode_layer(1, 0, 0, LayerMode::Replace, &scene, &mut wire);
    let mut from_wire = Vec::new();
    codec::decode_layer_into(&wire, &mut from_wire)?;
    let ply = io::write_ply(&scene);
    let from_ply = io::read_any(&ply)?;
    let splat_file = io::write_splat(&scene);
    let from_splat_file = io::read_any(&splat_file)?;

    for (name, bytes, cloud) in [
        ("scene_a_codec.png", wire.len(), &from_wire),
        ("scene_a_ply.png", ply.len(), &from_ply),
        ("scene_a_splatfile.png", splat_file.len(), &from_splat_file),
    ] {
        assert_eq!(cloud.len(), scene.len());
        let image = rasterize(cloud, &cam_a, &opts);
        save_png(&dir, name, &image)?;
        println!(
            "{:<28} {:>9} bytes ({:4.1} B/splat)  PSNR vs scene_a {:6.2} dB, mean abs error {:.3}",
            name,
            bytes,
            bytes as f64 / scene.len() as f64,
            psnr(&reference, &image),
            mean_abs_error(&reference, &image),
        );
    }
    Ok(())
}
