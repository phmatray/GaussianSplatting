//! Thumbnails: colour and colourised depth as RGBA pixels for a 2D canvas.

/// Pixels without a depth return.
const INVALID: [u8; 3] = [16, 19, 26];

/// Smallest and largest valid depth of a preview, in meters.
pub fn depth_range(depth_mm: &[u16]) -> Option<(f32, f32)> {
    let mut range: Option<(u16, u16)> = None;
    for &d in depth_mm.iter().filter(|&&d| d != 0) {
        range = Some(range.map_or((d, d), |(lo, hi)| (lo.min(d), hi.max(d))));
    }
    range.map(|(lo, hi)| (lo as f32 * 1e-3, hi as f32 * 1e-3))
}

/// Moves a displayed depth range towards the range of the newest frame, so
/// that the colours of a feed do not flicker from frame to frame.
pub fn smooth_range(previous: Option<(f32, f32)>, current: (f32, f32)) -> (f32, f32) {
    const WEIGHT: f32 = 0.15;
    match previous {
        Some((lo, hi)) if lo.is_finite() && hi.is_finite() => {
            (lo + (current.0 - lo) * WEIGHT, hi + (current.1 - hi) * WEIGHT)
        }
        _ => current,
    }
}

/// Polynomial approximation of Google's Turbo colour map, `t` in `0..=1`
/// (blue to red).
fn turbo(t: f32) -> [u8; 3] {
    // The ends of the map are nearly black; stay inside its colourful part.
    let t = 0.08 + 0.87 * t.clamp(0.0, 1.0);
    let r = 0.1357 + t * (4.6154 + t * (-42.6603 + t * (132.1311 + t * (-152.9424 + t * 59.2864))));
    let g = 0.0914 + t * (2.1942 + t * (4.8430 + t * (-14.1850 + t * (4.2773 + t * 2.8296))));
    let b = 0.1067 + t * (12.6419 + t * (-60.5820 + t * (110.3628 + t * (-89.9031 + t * 27.3482))));
    [r, g, b].map(|c| (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
}

/// Fills `out` with opaque RGBA pixels of the colour image.
pub fn rgb_to_rgba(rgb: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(rgb.len() / 3 * 4);
    for pixel in rgb.chunks_exact(3) {
        out.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
    }
}

/// Fills `out` with opaque RGBA pixels of the depth image: near is warm,
/// far is cold, missing returns are dark. `near` and `far` are in meters.
pub fn colorize_depth(depth_mm: &[u16], near: f32, far: f32, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(depth_mm.len() * 4);
    let span = (far - near).max(1e-3);
    for &d in depth_mm {
        let [r, g, b] = if d == 0 { INVALID } else { turbo(1.0 - (d as f32 * 1e-3 - near) / span) };
        out.extend_from_slice(&[r, g, b, 255]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_ignores_invalid_pixels() {
        assert_eq!(depth_range(&[0, 0]), None);
        assert_eq!(depth_range(&[]), None);
        let (near, far) = depth_range(&[0, 1500, 3000, 0, 2000]).unwrap();
        assert!((near - 1.5).abs() < 1e-5 && (far - 3.0).abs() < 1e-5);
    }

    #[test]
    fn near_is_warm_far_is_cold() {
        let mut out = Vec::new();
        colorize_depth(&[1000, 4000, 0], 1.0, 4.0, &mut out);
        assert_eq!(out.len(), 12);
        let (near, far, invalid) = (&out[0..4], &out[4..8], &out[8..12]);
        assert!(near[0] > near[2], "near should be red-ish: {near:?}");
        assert!(far[2] > far[0], "far should be blue-ish: {far:?}");
        assert_eq!(&invalid[..3], &INVALID);
        assert!(out.chunks(4).all(|p| p[3] == 255));
    }

    #[test]
    fn degenerate_ranges_do_not_divide_by_zero() {
        let mut out = Vec::new();
        colorize_depth(&[2000, 2000], 2.0, 2.0, &mut out);
        assert_eq!(out.len(), 8);
        colorize_depth(&[2000], f32::NAN, f32::NAN, &mut out);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn rgba_conversion() {
        let mut out = vec![9; 3];
        rgb_to_rgba(&[1, 2, 3, 4, 5, 6], &mut out);
        assert_eq!(out, [1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn smoothing_converges() {
        let mut range = smooth_range(None, (1.0, 2.0));
        assert_eq!(range, (1.0, 2.0));
        for _ in 0..100 {
            range = smooth_range(Some(range), (2.0, 5.0));
        }
        assert!((range.0 - 2.0).abs() < 1e-3 && (range.1 - 5.0).abs() < 1e-3);
        assert_eq!(smooth_range(Some((f32::NAN, 1.0)), (1.0, 2.0)), (1.0, 2.0));
    }
}
