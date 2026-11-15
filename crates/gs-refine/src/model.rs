//! The parameters the optimiser works on.
//!
//! One Gaussian is [`PARAMS`] floats in a fixed layout, so that the model,
//! its gradient and the two moments of Adam are four arrays of the same
//! shape (and one buffer each on a GPU):
//!
//! | Index | Meaning |
//! |---|---|
//! | 0..3 | center, world space, meters |
//! | 3..6 | natural log of the standard deviation along each local axis |
//! | 6..10 | rotation quaternion `x, y, z, w`, not normalised (the forward pass normalises) |
//! | 10 | logit of the opacity |
//! | 11..14 | colour, sRGB in `0..=1` (view independent) |
//!
//! The viewer's [`Splat`] is the same Gaussian with 8-bit colour and
//! opacity; [`Gaussians::to_splats`] is what every export goes through.

use gs_core::Splat;

/// Floats per Gaussian.
pub const PARAMS: usize = 14;
/// First index of each parameter group in a Gaussian.
pub const POS: usize = 0;
pub const SCALE: usize = 3;
pub const ROT: usize = 6;
pub const OPACITY: usize = 10;
pub const COLOR: usize = 11;

/// Largest opacity a Gaussian can take. An 8-bit opacity of 255 is fully
/// opaque: nothing behind such a splat receives a gradient any more, and a
/// logit that keeps growing never comes back.
pub const MAX_OPACITY: f32 = 0.995;

/// One Gaussian, see the module documentation for the layout.
pub type Gaussian = [f32; PARAMS];

#[inline]
pub fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

#[inline]
pub fn logit(p: f32) -> f32 {
    (p / (1.0 - p)).ln()
}

/// A cloud of Gaussians in optimiser parameters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Gaussians {
    pub data: Vec<Gaussian>,
}

impl Gaussians {
    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Converts viewer splats. `scale` multiplies every standard deviation
    /// (the fused cloud is sized for the viewer's normalised blending and
    /// leaves gaps when composited as translucent discs); `opacity`
    /// multiplies every opacity (fused surfels are fully opaque, which is
    /// where a sigmoid has no slope left to learn with); `min_thickness`
    /// is the smallest standard deviation kept, in meters, so that no
    /// Gaussian starts out degenerate.
    pub fn from_splats(splats: &[Splat], scale: f32, opacity: f32, min_thickness: f32) -> Self {
        let data = splats
            .iter()
            .filter(|s| s.pos.iter().chain(&s.scale).all(|v| v.is_finite()))
            .map(|s| {
                let mut g = [0.0f32; PARAMS];
                g[POS..POS + 3].copy_from_slice(&s.pos);
                for k in 0..3 {
                    g[SCALE + k] = (s.scale[k].abs() * scale).max(min_thickness).ln();
                }
                let len2: f32 = s.rot.iter().map(|c| c * c).sum();
                let rot = if len2.is_finite() && len2 > 1e-12 { s.rot } else { [0.0, 0.0, 0.0, 1.0] };
                g[ROT..ROT + 4].copy_from_slice(&rot);
                let opacity = (s.rgba[3] as f32 / 255.0 * opacity).clamp(0.02, MAX_OPACITY);
                g[OPACITY] = logit(opacity);
                for k in 0..3 {
                    g[COLOR + k] = s.rgba[k] as f32 / 255.0;
                }
                g
            })
            .collect();
        Self { data }
    }

    /// The viewer's representation: 8-bit colour and opacity, unit
    /// quaternion, standard deviations in meters.
    pub fn to_splats(&self) -> Vec<Splat> {
        self.data.iter().map(to_splat).collect()
    }

    /// Bytes held by the parameters.
    pub fn bytes(&self) -> usize {
        self.data.len() * std::mem::size_of::<Gaussian>()
    }
}

/// One Gaussian as a viewer splat.
pub fn to_splat(g: &Gaussian) -> Splat {
    let q = [g[ROT], g[ROT + 1], g[ROT + 2], g[ROT + 3]];
    let len = q.iter().map(|c| c * c).sum::<f32>().sqrt();
    let rot = if len.is_finite() && len > 1e-12 { q.map(|c| c / len) } else { [0.0, 0.0, 0.0, 1.0] };
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    Splat {
        pos: [g[POS], g[POS + 1], g[POS + 2]],
        scale: [g[SCALE].exp(), g[SCALE + 1].exp(), g[SCALE + 2].exp()],
        rot,
        rgba: [byte(g[COLOR]), byte(g[COLOR + 1]), byte(g[COLOR + 2]), byte(sigmoid(g[OPACITY]))],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splats_survive_the_round_trip() {
        let splat = Splat { pos: [1.0, -2.0, 3.5], scale: [0.02, 0.01, 0.002], rot: [0.1, 0.2, 0.3, 0.927_361_8], rgba: [10, 128, 250, 200] };
        let model = Gaussians::from_splats(&[splat], 1.0, 1.0, 1e-5);
        let back = model.to_splats()[0];
        assert_eq!(back.rgba, splat.rgba);
        for k in 0..3 {
            assert!((back.pos[k] - splat.pos[k]).abs() < 1e-6);
            assert!((back.scale[k] / splat.scale[k] - 1.0).abs() < 1e-5);
        }
        for k in 0..4 {
            assert!((back.rot[k] - splat.rot[k]).abs() < 1e-5);
        }
    }

    #[test]
    fn broken_splats_are_dropped_or_repaired() {
        let good = Splat { pos: [0.0; 3], scale: [0.0, 0.01, 0.01], rot: [0.0; 4], rgba: [255, 255, 255, 255] };
        let bad = Splat { pos: [f32::NAN, 0.0, 0.0], ..good };
        let model = Gaussians::from_splats(&[good, bad], 2.0, 1.0, 1e-4);
        assert_eq!(model.len(), 1);
        let s = model.to_splats()[0];
        assert_eq!(s.rot, [0.0, 0.0, 0.0, 1.0]);
        assert!((s.scale[0] - 1e-4).abs() < 1e-8 && (s.scale[1] - 0.02).abs() < 1e-6);
        // Fully opaque input stays short of 255 so that it can still learn.
        assert!(s.rgba[3] < 255 && s.rgba[3] > 250);
    }
}
