//! Back-to-front sorting of splat indices by view depth.
//!
//! Alpha blending needs the farthest splat drawn first. An exact sort is not
//! required: depths are quantized to 16 bits over the range present in the
//! view and sorted with a counting sort, which is `O(n)` and stable (splats in
//! the same depth bucket keep their input order, so the result is
//! deterministic and does not flicker between frames).
//!
//! Depth is the distance along the camera forward axis (`−z` in camera
//! space), the same quantity as [`crate::splat::Projected::depth`].

use glam::Mat4;

use crate::types::Splat;

/// Number of counting-sort buckets (16-bit keys).
const BUCKETS: usize = 1 << 16;

/// Key of splats whose depth is NaN or infinite; sorts after everything else.
const NON_FINITE_KEY: u16 = u16::MAX;

/// Largest key of a finite depth (the nearest splat).
const MAX_FINITE_KEY: u32 = NON_FINITE_KEY as u32 - 1;

/// Reusable depth sorter. Keep one around per view: after the first call with
/// a given splat count no memory is allocated.
#[derive(Clone, Debug)]
pub struct DepthSorter {
    depths: Vec<f32>,
    keys: Vec<u16>,
    /// Histogram, then running output offset, per key.
    counts: Vec<u32>,
}

impl Default for DepthSorter {
    fn default() -> Self {
        Self::new()
    }
}

impl DepthSorter {
    pub fn new() -> Self {
        Self { depths: Vec::new(), keys: Vec::new(), counts: vec![0; BUCKETS] }
    }

    /// Sorts point indices back to front (farthest first) for the
    /// world-to-camera matrix `view`. `out` is cleared and receives
    /// `positions.len()` indices.
    ///
    /// Points with a non-finite depth are placed at the end, in input order.
    pub fn sort_back_to_front(&mut self, positions: &[[f32; 3]], view: &Mat4, out: &mut Vec<u32>) {
        let depth = forward_depth(view);
        self.sort_by_depth(positions.iter().map(depth), out);
    }

    /// Same as [`Self::sort_back_to_front`], reading the positions straight
    /// out of a splat slice.
    pub fn sort_splats_back_to_front(&mut self, splats: &[Splat], view: &Mat4, out: &mut Vec<u32>) {
        let depth = forward_depth(view);
        self.sort_by_depth(splats.iter().map(|s| depth(&s.pos)), out);
    }

    /// Sorts the indices `0..n` of the `n` depths yielded by `depths` by
    /// decreasing depth.
    ///
    /// This is the generic entry point: the iterator can read any layout (a
    /// GPU staging buffer, already projected splats, …).
    ///
    /// Indices are `u32`; the iterator must not yield more than `u32::MAX`
    /// items.
    pub fn sort_by_depth<I: IntoIterator<Item = f32>>(&mut self, depths: I, out: &mut Vec<u32>) {
        out.clear();
        self.depths.clear();
        self.depths.extend(depths);
        let n = self.depths.len();
        debug_assert!(u32::try_from(n).is_ok(), "splat indices are u32");
        if n == 0 {
            return;
        }

        // Pass 1: range of the finite depths. All-equal, empty or overflowing
        // ranges collapse to a single bucket; so do ranges so small (below
        // about 2e-34) that the scale factor itself would overflow.
        let (near, far) = finite_range(&self.depths);
        let range = far - near;
        let to_key = MAX_FINITE_KEY as f32 / range;
        let to_key = if range > 0.0 && to_key.is_finite() { to_key } else { 0.0 };

        // Pass 2: quantize (farthest → key 0), then build the histogram.
        self.keys.clear();
        self.keys.extend(self.depths.iter().map(|&d| {
            if is_finite(d) {
                // Float → int casts saturate (and map NaN to 0), so this never
                // leaves the finite key range.
                (((far - d) * to_key) as u32).min(MAX_FINITE_KEY) as u16
            } else {
                NON_FINITE_KEY
            }
        }));
        self.counts.fill(0);
        for &key in &self.keys {
            self.counts[key as usize] += 1;
        }

        // Histogram → start offset of each bucket.
        let mut offset = 0u32;
        for c in self.counts.iter_mut() {
            let count = *c;
            *c = offset;
            offset += count;
        }

        // Pass 3: stable scatter.
        out.resize(n, 0);
        for (i, &key) in self.keys.iter().enumerate() {
            let slot = &mut self.counts[key as usize];
            out[*slot as usize] = i as u32;
            *slot += 1;
        }
    }
}

/// `f32::is_finite` written as one comparison (false for NaN and ±∞).
#[inline(always)]
fn is_finite(d: f32) -> bool {
    d.abs() < f32::INFINITY
}

/// Smallest and largest finite value of `depths`; `(+∞, −∞)` when there is
/// none.
fn finite_range(depths: &[f32]) -> (f32, f32) {
    // Eight independent accumulators: no loop-carried dependency between
    // neighbours, so the compiler can vectorize the reduction.
    const LANES: usize = 8;
    let mut lo = [f32::INFINITY; LANES];
    let mut hi = [f32::NEG_INFINITY; LANES];
    let mut fold = |lane: usize, d: f32| {
        let finite = is_finite(d);
        let (l, h) = if finite { (d, d) } else { (f32::INFINITY, f32::NEG_INFINITY) };
        lo[lane] = if l < lo[lane] { l } else { lo[lane] };
        hi[lane] = if h > hi[lane] { h } else { hi[lane] };
    };
    let chunks = depths.chunks_exact(LANES);
    let tail = chunks.remainder();
    for chunk in chunks {
        for (lane, &d) in chunk.iter().enumerate() {
            fold(lane, d);
        }
    }
    for (lane, &d) in tail.iter().enumerate() {
        fold(lane, d);
    }
    (lo.iter().copied().fold(f32::INFINITY, f32::min), hi.iter().copied().fold(f32::NEG_INFINITY, f32::max))
}

/// Returns a closure computing the forward depth of a world position for the
/// world-to-camera matrix `view`: `−(view · (p, 1)).z`.
#[inline]
fn forward_depth(view: &Mat4) -> impl Fn(&[f32; 3]) -> f32 {
    let (fx, fy, fz, fw) = (-view.x_axis.z, -view.y_axis.z, -view.z_axis.z, -view.w_axis.z);
    move |p: &[f32; 3]| fx * p[0] + fy * p[1] + fz * p[2] + fw
}

#[cfg(test)]
mod tests {
    use glam::Vec3;

    use super::*;
    use crate::splat::test_util::Rng;
    use crate::types::Camera;

    fn view() -> Mat4 {
        Camera::look_at(Vec3::new(3.0, 2.0, 5.0), Vec3::new(0.0, 1.0, 0.0), Vec3::Y, 1.0, 64, 64).view_matrix()
    }

    fn random_positions(n: usize, seed: u64) -> Vec<[f32; 3]> {
        let mut rng = Rng::new(seed);
        (0..n).map(|_| rng.vec3(-4.0, 4.0).to_array()).collect()
    }

    /// Reference: the documented quantization followed by a stable comparison
    /// sort.
    fn reference_order(depths: &[f32]) -> Vec<u32> {
        let finite = depths.iter().copied().filter(|d| d.is_finite());
        let near = finite.clone().fold(f32::INFINITY, f32::min);
        let far = finite.fold(f32::NEG_INFINITY, f32::max);
        let range = far - near;
        let to_key = if range > 0.0 && (65534.0 / range).is_finite() { 65534.0 / range } else { 0.0 };
        let key = |d: f32| if d.is_finite() { (((far - d) * to_key) as u32).min(65534) } else { 65535 };
        let mut order: Vec<u32> = (0..depths.len() as u32).collect();
        order.sort_by_key(|&i| key(depths[i as usize]));
        order
    }

    fn is_permutation(order: &[u32], n: usize) -> bool {
        let mut seen = vec![false; n];
        order.len() == n && order.iter().all(|&i| (i as usize) < n && !std::mem::replace(&mut seen[i as usize], true))
    }

    #[test]
    fn empty_input_gives_empty_output() {
        let mut sorter = DepthSorter::new();
        let mut out = vec![7, 8, 9];
        sorter.sort_back_to_front(&[], &view(), &mut out);
        assert!(out.is_empty());
        sorter.sort_splats_back_to_front(&[], &view(), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn single_and_equal_depths_keep_input_order() {
        let mut sorter = DepthSorter::new();
        let mut out = Vec::new();
        sorter.sort_back_to_front(&[[1.0, 2.0, 3.0]], &view(), &mut out);
        assert_eq!(out, [0]);
        // Same position ⇒ same depth ⇒ stable order.
        sorter.sort_back_to_front(&[[0.5, 0.5, 0.5]; 100], &view(), &mut out);
        assert_eq!(out, (0..100).collect::<Vec<u32>>());
    }

    #[test]
    fn depth_is_distance_along_camera_forward() {
        let cam = Camera::look_at(Vec3::new(3.0, 2.0, 5.0), Vec3::new(0.0, 1.0, 0.0), Vec3::Y, 1.0, 64, 64);
        let depth = forward_depth(&cam.view_matrix());
        for p in random_positions(50, 1) {
            let expected = (Vec3::from(p) - cam.position()).dot(cam.forward());
            assert!((depth(&p) - expected).abs() < 1e-4);
        }
    }

    #[test]
    fn matches_stable_comparison_sort_on_quantized_keys() {
        let view = view();
        let depth = forward_depth(&view);
        let mut sorter = DepthSorter::new();
        let mut out = Vec::new();
        for (n, seed) in [(1usize, 1u64), (2, 2), (17, 3), (1000, 4), (70_000, 5), (300_000, 6)] {
            let positions = random_positions(n, seed);
            let depths: Vec<f32> = positions.iter().map(&depth).collect();
            sorter.sort_back_to_front(&positions, &view, &mut out);
            assert!(is_permutation(&out, n));
            assert_eq!(out, reference_order(&depths), "n = {n}");
        }
    }

    #[test]
    fn output_is_back_to_front_within_one_bucket() {
        let view = view();
        let depth = forward_depth(&view);
        let positions = random_positions(50_000, 9);
        let depths: Vec<f32> = positions.iter().map(&depth).collect();
        let near = depths.iter().copied().fold(f32::INFINITY, f32::min);
        let far = depths.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let bucket = (far - near) / 65534.0;
        let mut out = Vec::new();
        DepthSorter::new().sort_back_to_front(&positions, &view, &mut out);
        assert_eq!(depths[out[0] as usize], far);
        assert_eq!(depths[*out.last().unwrap() as usize], near);
        for pair in out.windows(2) {
            let (a, b) = (depths[pair[0] as usize], depths[pair[1] as usize]);
            assert!(a >= b - bucket * 1.001, "{a} drawn before {b}");
        }
    }

    #[test]
    fn splat_entry_point_matches_position_entry_point() {
        let mut rng = Rng::new(11);
        let splats: Vec<Splat> = (0..5000).map(|_| rng.splat(3.0, 0.01, 0.1)).collect();
        let positions: Vec<[f32; 3]> = splats.iter().map(|s| s.pos).collect();
        let mut sorter = DepthSorter::new();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        sorter.sort_splats_back_to_front(&splats, &view(), &mut a);
        sorter.sort_back_to_front(&positions, &view(), &mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn non_finite_positions_go_last_without_panicking() {
        let mut positions = random_positions(200, 12);
        positions[3] = [f32::NAN, 0.0, 0.0];
        positions[50] = [0.0, f32::INFINITY, 0.0];
        positions[120] = [f32::NEG_INFINITY, f32::NAN, 1.0];
        positions[199] = [f32::MAX, f32::MAX, f32::MAX];
        let view = view();
        let depth = forward_depth(&view);
        let mut out = Vec::new();
        DepthSorter::new().sort_back_to_front(&positions, &view, &mut out);
        assert!(is_permutation(&out, positions.len()));
        let bad: Vec<u32> = (0..200u32).filter(|&i| !depth(&positions[i as usize]).is_finite()).collect();
        assert!(bad.len() >= 3);
        assert_eq!(&out[out.len() - bad.len()..], &bad[..], "non-finite depths last, in input order");

        // Only non-finite input.
        DepthSorter::new().sort_back_to_front(&[[f32::NAN; 3]; 5], &view, &mut out);
        assert_eq!(out, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn finite_range_ignores_non_finite_values() {
        assert_eq!(finite_range(&[]), (f32::INFINITY, f32::NEG_INFINITY));
        assert_eq!(finite_range(&[f32::NAN, f32::INFINITY]), (f32::INFINITY, f32::NEG_INFINITY));
        let mut rng = Rng::new(30);
        for n in [1usize, 7, 8, 9, 100, 1001] {
            let mut values: Vec<f32> = (0..n).map(|_| rng.range(-50.0, 50.0)).collect();
            let expected = (
                values.iter().copied().fold(f32::INFINITY, f32::min),
                values.iter().copied().fold(f32::NEG_INFINITY, f32::max),
            );
            values.extend([f32::NAN, f32::NEG_INFINITY, f32::INFINITY]);
            let rotate = rng.below(values.len());
            values.rotate_left(rotate);
            assert_eq!(finite_range(&values), expected, "n = {n}");
        }
    }

    #[test]
    fn extreme_ranges_do_not_panic() {
        let mut out = Vec::new();
        let mut sorter = DepthSorter::new();
        let depths = [f32::MAX, -f32::MAX, 0.0, 1.0, f32::MIN_POSITIVE, -1.0e30];
        sorter.sort_by_depth(depths.iter().copied(), &mut out);
        assert!(is_permutation(&out, depths.len()));
        let tiny = [1.0, 1.0 + f32::EPSILON, 1.0, 1.0 + 2.0 * f32::EPSILON];
        sorter.sort_by_depth(tiny.iter().copied(), &mut out);
        assert_eq!(out, [3, 1, 0, 2]);
        // Smallest range for which the key scale is still finite: exact order.
        let small = [2.0e-34, 0.0, 6.0e-34, 4.0e-34, -0.0];
        sorter.sort_by_depth(small.iter().copied(), &mut out);
        assert_eq!(out, [2, 3, 0, 1, 4]);
        // Subnormal range: the scale would be infinite, so the depths count
        // as equal and keep their input order (instead of a garbage order).
        let subnormal = [1.0e-45, 0.0, 4.0e-45, 3.0e-45];
        sorter.sort_by_depth(subnormal.iter().copied(), &mut out);
        assert_eq!(out, [0, 1, 2, 3]);
    }

    #[test]
    fn sorter_is_reusable_and_stops_allocating() {
        let view = view();
        let mut sorter = DepthSorter::new();
        let mut out = Vec::new();
        let big = random_positions(10_000, 20);
        sorter.sort_back_to_front(&big, &view, &mut out);
        let (depths_cap, keys_cap, out_cap) = (sorter.depths.capacity(), sorter.keys.capacity(), out.capacity());
        for seed in 21..25 {
            let positions = random_positions(8_000 + seed as usize * 10, seed);
            sorter.sort_back_to_front(&positions, &view, &mut out);
            assert!(is_permutation(&out, positions.len()));
            let depth = forward_depth(&view);
            let depths: Vec<f32> = positions.iter().map(&depth).collect();
            assert_eq!(out, reference_order(&depths));
        }
        assert_eq!(sorter.depths.capacity(), depths_cap);
        assert_eq!(sorter.keys.capacity(), keys_cap);
        assert_eq!(out.capacity(), out_cap);
    }

    /// Timing report. Run with
    /// `cargo test -p gs-core --release sort::tests::timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing report, run in --release"]
    fn timing() {
        let view = view();
        for n in [200_000usize, 1_000_000] {
            let mut rng = Rng::new(n as u64);
            let splats: Vec<Splat> = (0..n).map(|_| rng.splat(4.0, 0.01, 0.05)).collect();
            let mut sorter = DepthSorter::new();
            let mut out = Vec::new();
            sorter.sort_splats_back_to_front(&splats, &view, &mut out); // warm-up
            let mut best = f64::MAX;
            for _ in 0..30 {
                let start = web_time::Instant::now();
                sorter.sort_splats_back_to_front(&splats, &view, &mut out);
                best = best.min(start.elapsed().as_secs_f64() * 1e3);
                std::hint::black_box(&out);
            }
            println!("sort {n:>9} splats: {best:.2} ms (best of 30)");
        }
    }
}
