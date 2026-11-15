//! Depth upsampling to the resolution of the colour image (recommendation
//! A2 of `docs/RECHERCHE_QUALITE.md`).
//!
//! Every real RGB-D device has a colour image several times finer than its
//! depth map. To place one splat per *colour* pixel the depth has to be
//! brought to the colour grid. Three ways are compared:
//!
//! * [`Upsampling::Nearest`]: every colour pixel takes the depth of the depth
//!   pixel it falls into (blocks of constant depth);
//! * [`Upsampling::Bilinear`]: bilinear interpolation of the inverse depth,
//!   blind to edges (it invents depths between foreground and background);
//! * [`Upsampling::Guided`]: joint bilateral upsampling (Kopf 2007) of local
//!   planes. Each of the four surrounding depth pixels predicts the inverse
//!   depth at the colour pixel by extrapolating its own plane, and is
//!   weighted by its bilinear weight times its colour similarity to the
//!   colour pixel. When the predictions belong to several surfaces, only the
//!   surface with the largest total weight is interpolated: depth edges snap
//!   to colour edges and no depth is ever invented across an edge. Depth
//!   pixels without a return take part in the vote as the surface "nothing
//!   there".
//!
//! The depth map and the colour image are assumed registered: both seen from
//! the same pinhole, the colour grid `factor` times finer.

use super::metrics::is_depth_step;
use crate::types::RgbdFrame;

/// How [`upsample_depth`] fills the colour grid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Upsampling {
    Nearest,
    Bilinear,
    /// `sigma_color` is the standard deviation of the colour similarity in
    /// 8-bit levels (10 to 20 is typical).
    Guided { sigma_color: f32 },
}

/// The low-resolution depth map prepared for upsampling.
struct Coarse {
    width: usize,
    height: usize,
    /// Inverse depth, `0.0` where invalid.
    inv: Vec<f32>,
    /// Gradient of the inverse depth per depth pixel along `u` and `v`.
    grad: Vec<[f32; 2]>,
    /// Colour at the center of every depth pixel.
    color: Vec<[f32; 3]>,
}

impl Coarse {
    fn new(depth: &RgbdFrame, color: &RgbdFrame, factor: usize, edge: f32) -> Self {
        let (width, height) = (depth.width as usize, depth.height as usize);
        let inv: Vec<f32> = depth.depth.iter().map(|&z| if z > 0.0 && z.is_finite() { 1.0 / z } else { 0.0 }).collect();
        let at = |x: isize, y: isize| {
            (x >= 0 && y >= 0 && x < width as isize && y < height as isize)
                .then(|| inv[y as usize * width + x as usize])
                .filter(|&w| w > 0.0)
        };
        // Slope toward a neighbour that continues the pixel's surface.
        let slope = |x: isize, y: isize, dx: isize, dy: isize, step: isize| {
            let c = at(x, y)?;
            let n = at(x + dx * step, y + dy * step)?;
            let beyond = at(x + 2 * dx * step, y + 2 * dy * step);
            let behind = at(x - dx * step, y - dy * step);
            (!is_depth_step(c, n, behind, beyond, edge)).then_some((n - c) * step as f32)
        };
        let mut grad = vec![[0.0f32; 2]; width * height];
        for y in 0..height as isize {
            for x in 0..width as isize {
                let axis = |dx: isize, dy: isize| match (slope(x, y, dx, dy, -1), slope(x, y, dx, dy, 1)) {
                    (Some(a), Some(b)) => 0.5 * (a + b),
                    (Some(a), None) | (None, Some(a)) => a,
                    (None, None) => 0.0,
                };
                grad[y as usize * width + x as usize] = [axis(1, 0), axis(0, 1)];
            }
        }
        // The colour pixels around the center of the depth pixel: one for an
        // odd factor, the central 2x2 for an even one.
        let (cw, ch) = (color.width as usize, color.height as usize);
        let span = if factor % 2 == 1 { factor / 2..factor / 2 + 1 } else { factor / 2 - 1..factor / 2 + 1 };
        let mut mean = vec![[0.0f32; 3]; width * height];
        for y in 0..height {
            for x in 0..width {
                let (mut sum, mut count) = ([0.0f32; 3], 0.0f32);
                for oy in span.clone() {
                    for ox in span.clone() {
                        let (px, py) = ((x * factor + ox).min(cw - 1), (y * factor + oy).min(ch - 1));
                        for (c, s) in sum.iter_mut().enumerate() {
                            *s += color.rgb[(py * cw + px) * 3 + c] as f32;
                        }
                        count += 1.0;
                    }
                }
                mean[y * width + x] = sum.map(|s| s / count);
            }
        }
        Self { width, height, inv, grad, color: mean }
    }
}

/// Depth map on the grid of `color` from the `factor` times coarser `depth`
/// frame (`0.0` where no depth pixel nearby is valid).
///
/// `edge` is the relative depth step that separates two surfaces (the
/// reconstruction's `edge_threshold`).
///
/// # Panics
/// When `color` is not exactly `factor` times the size of `depth`.
pub fn upsample_depth(depth: &RgbdFrame, color: &RgbdFrame, factor: u32, edge: f32, mode: Upsampling) -> Vec<f32> {
    let factor = factor.max(1) as usize;
    let (cw, ch) = (color.width as usize, color.height as usize);
    assert!(
        cw == depth.width as usize * factor && ch == depth.height as usize * factor,
        "the colour image must be {factor} times the depth map"
    );
    let coarse = Coarse::new(depth, color, factor, edge);
    let (lw, lh) = (coarse.width as isize, coarse.height as isize);
    let mut out = vec![0.0f32; cw * ch];
    for y in 0..ch {
        for x in 0..cw {
            // Position in depth-pixel units; depth pixel q has its center at q + 0.5.
            let (gx, gy) = ((x as f32 + 0.5) / factor as f32, (y as f32 + 0.5) / factor as f32);
            if mode == Upsampling::Nearest {
                let w = coarse.inv[(gy as usize).min(coarse.height - 1) * coarse.width + (gx as usize).min(coarse.width - 1)];
                out[y * cw + x] = if w > 0.0 { 1.0 / w } else { 0.0 };
                continue;
            }
            let (x0, y0) = ((gx - 0.5).floor(), (gy - 0.5).floor());
            let (tx, ty) = (gx - 0.5 - x0, gy - 0.5 - y0);
            let pixel = &color.rgb[(y * cw + x) * 3..(y * cw + x) * 3 + 3];
            // (predicted inverse depth, weight) of the up to four neighbours.
            let mut candidates = [(0.0f32, 0.0f32); 4];
            let mut count = 0;
            // Total weight of the neighbours without depth (guided mode).
            let mut void = 0.0f32;
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (qx, qy) = (x0 as isize + dx, y0 as isize + dy);
                if qx < 0 || qy < 0 || qx >= lw || qy >= lh {
                    continue;
                }
                let q = qy as usize * coarse.width + qx as usize;
                let w = coarse.inv[q];
                let bilinear = (if dx == 0 { 1.0 - tx } else { tx }) * (if dy == 0 { 1.0 - ty } else { ty });
                let candidate = match mode {
                    Upsampling::Guided { sigma_color } => {
                        let distance2: f32 = (0..3).map(|c| (pixel[c] as f32 - coarse.color[q][c]).powi(2)).sum();
                        let similarity = (-distance2 / (2.0 * sigma_color.max(1.0e-3).powi(2))).exp();
                        // The floor lets the bilinear weight decide where no
                        // neighbour has the pixel's colour.
                        let weight = (bilinear + 1.0e-4) * (similarity + 1.0e-3);
                        if w <= 0.0 {
                            // "Nothing there" is a surface like any other: a
                            // colour pixel that looks like the neighbour
                            // without depth has no depth either. Leaving
                            // these out of the vote paints the background's
                            // colour onto the rim of every object that
                            // stands before empty space.
                            void += weight;
                            continue;
                        }
                        let [a, b] = coarse.grad[q];
                        let predicted = w + a * (gx - (qx as f32 + 0.5)) + b * (gy - (qy as f32 + 0.5));
                        (if predicted > 0.0 { predicted } else { w }, weight)
                    }
                    _ if w <= 0.0 => continue,
                    _ => (w, bilinear + 1.0e-6),
                };
                candidates[count] = candidate;
                count += 1;
            }
            let candidates = &mut candidates[..count];
            if candidates.is_empty() {
                continue;
            }
            let mean = |list: &[(f32, f32)]| {
                let weight: f32 = list.iter().map(|c| c.1).sum();
                list.iter().map(|c| c.0 * c.1).sum::<f32>() / weight
            };
            let w = if matches!(mode, Upsampling::Bilinear) {
                mean(candidates)
            } else {
                // Group the predictions into surfaces and keep the heaviest.
                candidates.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
                let (mut best, mut best_weight) = (0..0, void);
                let mut start = 0;
                for i in 1..=candidates.len() {
                    let split = i == candidates.len() || candidates[i].0 - candidates[i - 1].0 > edge * candidates[i].0;
                    if split {
                        let weight: f32 = candidates[start..i].iter().map(|c| c.1).sum();
                        if weight > best_weight {
                            (best, best_weight) = (start..i, weight);
                        }
                        start = i;
                    }
                }
                if best.is_empty() {
                    // The neighbours without depth outweigh every surface.
                    continue;
                }
                mean(&candidates[best])
            };
            out[y * cw + x] = if w > 0.0 { 1.0 / w } else { 0.0 };
        }
    }
    out
}

/// The RGB-D frame a device with a fine colour image and a coarse depth map
/// delivers after upsampling: the colours of `color`, the depth of `depth`
/// brought to the colour grid. Pixels without colour information (`color`
/// has no depth there, i.e. the simulated sensor saw nothing) stay invalid
/// only if the upsampled depth is invalid too.
pub fn fuse_color_and_depth(depth: &RgbdFrame, color: &RgbdFrame, factor: u32, edge: f32, mode: Upsampling) -> RgbdFrame {
    RgbdFrame { width: color.width, height: color.height, rgb: color.rgb.clone(), depth: upsample_depth(depth, color, factor, edge, mode) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Camera;
    use glam::Vec3;

    /// A red square 2 m away in front of a slanted grey wall, at two
    /// resolutions of the same pinhole.
    fn scene(width: u32, height: u32) -> (Camera, RgbdFrame) {
        let cam = Camera::look_at(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y, 1.0, width, height);
        let mut frame = RgbdFrame::new(width, height);
        let wall_normal = Vec3::new(0.5, 0.0, 1.0).normalize();
        let wall_point = Vec3::new(0.0, 0.0, -3.0);
        for y in 0..height {
            for x in 0..width {
                let dir = cam.pixel_dir_cam(x as f32 + 0.5, y as f32 + 0.5);
                let i = frame.index(x, y);
                let square = dir * 2.0;
                if square.x.abs() < 0.40 && square.y.abs() < 0.37 {
                    frame.depth[i] = 2.0;
                    frame.rgb[i * 3..i * 3 + 3].copy_from_slice(&[220, 40, 40]);
                } else {
                    frame.depth[i] = wall_normal.dot(wall_point) / wall_normal.dot(dir);
                    frame.rgb[i * 3..i * 3 + 3].copy_from_slice(&[128, 128, 128]);
                }
            }
        }
        (cam, frame)
    }

    fn errors(truth: &RgbdFrame, depth: &[f32]) -> (f32, usize) {
        let mut worst_interior = 0.0f32;
        let mut wrong = 0;
        for (&z, &t) in depth.iter().zip(&truth.depth) {
            let relative = (z - t).abs() / t;
            // More than half-way to the other surface: assigned to the wrong one.
            if relative > 0.15 {
                wrong += 1;
            } else {
                worst_interior = worst_interior.max(relative);
            }
        }
        (worst_interior, wrong)
    }

    #[test]
    fn guided_upsampling_recovers_planes_and_snaps_edges_to_colour() {
        for factor in [2u32, 4] {
            let (_, coarse) = scene(40, 30);
            let (_, fine) = scene(40 * factor, 30 * factor);
            let guided = upsample_depth(&coarse, &fine, factor, 0.03, Upsampling::Guided { sigma_color: 15.0 });
            let (worst, wrong) = errors(&fine, &guided);
            // Planes are extrapolated exactly, and every colour pixel lands
            // on its own surface.
            assert!(worst < 2e-3, "factor {factor}: interior error {worst}");
            assert_eq!(wrong, 0, "factor {factor}");

            // Nearest: blocks of constant depth; the edge is off by up to
            // half a depth pixel.
            let nearest = upsample_depth(&coarse, &fine, factor, 0.03, Upsampling::Nearest);
            let (worst_nearest, wrong_nearest) = errors(&fine, &nearest);
            assert!(wrong_nearest > 20, "factor {factor}: {wrong_nearest}");
            assert!(worst_nearest > 5.0 * worst);

            // Bilinear: invents depths between the two surfaces.
            let bilinear = upsample_depth(&coarse, &fine, factor, 0.03, Upsampling::Bilinear);
            let between = bilinear.iter().zip(&fine.depth).filter(|&(&z, &t)| t == 2.0 && z > 2.1 && z < 0.95 * 2.6).count();
            assert!(between > 10, "factor {factor}: {between}");
            let between = guided.iter().zip(&fine.depth).filter(|&(&z, &t)| t == 2.0 && z > 2.1).count();
            assert_eq!(between, 0);
        }
    }

    #[test]
    fn empty_space_behind_an_object_stays_empty() {
        // The red square before nothing at all: colour pixels that show
        // nothing (black) must not get the square's depth.
        let void = |width: u32, height: u32| {
            let (_, mut frame) = scene(width, height);
            for i in 0..frame.depth.len() {
                if frame.depth[i] != 2.0 {
                    frame.depth[i] = 0.0;
                    frame.rgb[i * 3..i * 3 + 3].copy_from_slice(&[0, 0, 0]);
                }
            }
            frame
        };
        let (coarse, fine) = (void(40, 30), void(160, 120));
        let guided = upsample_depth(&coarse, &fine, 4, 0.03, Upsampling::Guided { sigma_color: 15.0 });
        let wrong = guided.iter().zip(&fine.depth).filter(|&(&z, &t)| (z > 0.0) != (t > 0.0)).count();
        assert_eq!(wrong, 0);
        // Bilinear interpolation of the valid neighbours fattens the square.
        let bilinear = upsample_depth(&coarse, &fine, 4, 0.03, Upsampling::Bilinear);
        let fattened = bilinear.iter().zip(&fine.depth).filter(|&(&z, &t)| z > 0.0 && t == 0.0).count();
        assert!(fattened > 100, "{fattened}");
    }

    #[test]
    fn invalid_depth_stays_invalid_and_does_not_poison_neighbours() {
        let (_, mut coarse) = scene(40, 30);
        let (_, fine) = scene(80, 60);
        // A hole of 3x3 depth pixels in the wall.
        for y in 2..5 {
            for x in 2..5 {
                let i = coarse.index(x, y);
                coarse.depth[i] = 0.0;
            }
        }
        for mode in [Upsampling::Nearest, Upsampling::Bilinear, Upsampling::Guided { sigma_color: 15.0 }] {
            let frame = fuse_color_and_depth(&coarse, &fine, 2, 0.03, mode);
            assert_eq!((frame.width, frame.height), (80, 60));
            // The center of the hole has no valid neighbour.
            assert_eq!(frame.depth[7 * 80 + 7], 0.0, "{mode:?}");
            // Everything else is valid and finite.
            assert!(frame.depth.iter().all(|z| z.is_finite() && *z >= 0.0));
            let valid = frame.depth.iter().filter(|&&z| z > 0.0).count();
            assert!(valid > 80 * 60 - 60, "{mode:?}: {valid}");
        }
    }
}
