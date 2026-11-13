//! Bounding volume hierarchy over primitive world boxes.
//!
//! Built with binned SAH, which is cheap enough to rebuild every tick for the
//! ~50 primitives of the stage (tens of microseconds) and gives good trees for
//! the few hundred boxes of the house.

use glam::Vec3A;

use super::primitive::safe_recip;
use crate::types::Aabb;

const LEAF_MAX: usize = 2;
const BINS: usize = 8;
/// Below this depth the builder stops trusting SAH and splits at the median,
/// which bounds the tree depth (and so the traversal stack).
const SAH_MAX_DEPTH: u32 = 24;
const STACK: usize = 64;

#[derive(Clone, Copy, Debug)]
struct Node {
    min: Vec3A,
    max: Vec3A,
    /// Leaf: first slot in `order`. Interior: index of the left child; the
    /// right child is the next node.
    first: u32,
    /// Number of primitives for a leaf, 0 for an interior node.
    count: u32,
}

/// Flattened BVH. Leaves reference primitive indices through `order`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Bvh {
    nodes: Vec<Node>,
    order: Vec<u32>,
}

#[derive(Clone, Copy)]
struct Bounds {
    min: Vec3A,
    max: Vec3A,
}

impl Bounds {
    const EMPTY: Self = Self { min: Vec3A::INFINITY, max: Vec3A::NEG_INFINITY };

    #[inline]
    fn grow(&mut self, o: &Bounds) {
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
    }

    #[inline]
    fn half_area(&self) -> f32 {
        let e = (self.max - self.min).max(Vec3A::ZERO);
        e.x * e.y + e.y * e.z + e.z * e.x
    }
}

impl Bvh {
    /// Builds the hierarchy over the given boxes (one per primitive).
    pub fn build(boxes: &[Aabb]) -> Self {
        let n = boxes.len();
        if n == 0 {
            return Self::default();
        }
        let bounds: Vec<Bounds> = boxes.iter().map(|b| Bounds { min: b.min.into(), max: b.max.into() }).collect();
        let mut bvh = Self { nodes: Vec::with_capacity(2 * n), order: (0..n as u32).collect() };
        let mut root = Bounds::EMPTY;
        for b in &bounds {
            root.grow(b);
        }
        bvh.nodes.push(Node { min: root.min, max: root.max, first: 0, count: n as u32 });
        bvh.subdivide(0, &bounds, 0);
        bvh
    }

    fn subdivide(&mut self, node: usize, bounds: &[Bounds], depth: u32) {
        let Node { first, count, .. } = self.nodes[node];
        let (first, count) = (first as usize, count as usize);
        if count <= LEAF_MAX {
            return;
        }
        let centroid = |i: u32| 0.5 * (bounds[i as usize].min + bounds[i as usize].max);
        let slots = first..first + count;

        let mut cmin = Vec3A::INFINITY;
        let mut cmax = Vec3A::NEG_INFINITY;
        for &i in &self.order[slots.clone()] {
            let c = centroid(i);
            cmin = cmin.min(c);
            cmax = cmax.max(c);
        }
        let extent = cmax - cmin;

        // Binned SAH over the three axes.
        let mut best: Option<(f32, usize, usize)> = None; // (cost, axis, split bin)
        if depth < SAH_MAX_DEPTH {
            for axis in 0..3 {
                if extent[axis] < 1e-6 {
                    continue;
                }
                let scale = BINS as f32 / extent[axis];
                let mut bin_bounds = [Bounds::EMPTY; BINS];
                let mut bin_count = [0u32; BINS];
                for &i in &self.order[slots.clone()] {
                    let b = (((centroid(i)[axis] - cmin[axis]) * scale) as usize).min(BINS - 1);
                    bin_bounds[b].grow(&bounds[i as usize]);
                    bin_count[b] += 1;
                }
                // Sweep from the right to get suffix areas, then from the left.
                let mut right_area = [0.0f32; BINS];
                let mut acc = Bounds::EMPTY;
                let mut right_count = [0u32; BINS];
                let mut cnt = 0;
                for b in (1..BINS).rev() {
                    acc.grow(&bin_bounds[b]);
                    cnt += bin_count[b];
                    right_area[b] = acc.half_area();
                    right_count[b] = cnt;
                }
                let mut acc = Bounds::EMPTY;
                let mut cnt = 0;
                for b in 1..BINS {
                    acc.grow(&bin_bounds[b - 1]);
                    cnt += bin_count[b - 1];
                    if cnt == 0 || right_count[b] == 0 {
                        continue;
                    }
                    let cost = acc.half_area() * cnt as f32 + right_area[b] * right_count[b] as f32;
                    if best.is_none_or(|(c, _, _)| cost < c) {
                        best = Some((cost, axis, b));
                    }
                }
            }
        }

        let mid = match best {
            Some((_, axis, split)) => {
                let scale = BINS as f32 / extent[axis];
                let bin = |i: u32| (((centroid(i)[axis] - cmin[axis]) * scale) as usize).min(BINS - 1);
                // In-place partition: left part has bin < split.
                let slice = &mut self.order[slots.clone()];
                let mut l = 0;
                let mut r = slice.len();
                while l < r {
                    if bin(slice[l]) < split {
                        l += 1;
                    } else {
                        r -= 1;
                        slice.swap(l, r);
                    }
                }
                first + l
            }
            None => {
                // Coincident centroids or depth limit: median split on the widest axis.
                let axis = if extent.x >= extent.y && extent.x >= extent.z {
                    0
                } else if extent.y >= extent.z {
                    1
                } else {
                    2
                };
                let slice = &mut self.order[slots.clone()];
                slice.sort_unstable_by(|&a, &b| centroid(a)[axis].total_cmp(&centroid(b)[axis]));
                first + count / 2
            }
        };

        let left = self.nodes.len();
        for range in [first..mid, mid..first + count] {
            let mut b = Bounds::EMPTY;
            for &i in &self.order[range.clone()] {
                b.grow(&bounds[i as usize]);
            }
            self.nodes.push(Node { min: b.min, max: b.max, first: range.start as u32, count: range.len() as u32 });
        }
        self.nodes[node].first = left as u32;
        self.nodes[node].count = 0;
        self.subdivide(left, bounds, depth + 1);
        self.subdivide(left + 1, bounds, depth + 1);
    }

    /// Walks the tree front to back along the ray `origin + t * dir`,
    /// `t` in `[0, t_max]`.
    ///
    /// `visit(primitive, &mut t_max)` is called for every primitive whose leaf
    /// box the ray touches. It may shrink `t_max` (closest-hit queries) and
    /// returns `true` to stop the traversal (any-hit queries).
    #[inline]
    pub fn traverse(&self, origin: Vec3A, dir: Vec3A, mut t_max: f32, mut visit: impl FnMut(u32, &mut f32) -> bool) {
        if self.nodes.is_empty() {
            return;
        }
        let inv = safe_recip(dir);
        let entry = |n: &Node, t_max: f32| -> f32 {
            let t0 = (n.min - origin) * inv;
            let t1 = (n.max - origin) * inv;
            let near = t0.min(t1).max_element().max(0.0);
            let far = t0.max(t1).min_element().min(t_max);
            if near <= far {
                near
            } else {
                f32::INFINITY
            }
        };
        if entry(&self.nodes[0], t_max) == f32::INFINITY {
            return;
        }
        let mut stack = [(0u32, 0.0f32); STACK];
        let mut sp = 0;
        let mut current = 0u32;
        loop {
            let node = &self.nodes[current as usize];
            if node.count > 0 {
                for &prim in &self.order[node.first as usize..(node.first + node.count) as usize] {
                    if visit(prim, &mut t_max) {
                        return;
                    }
                }
            } else {
                let (a, b) = (node.first, node.first + 1);
                let ta = entry(&self.nodes[a as usize], t_max);
                let tb = entry(&self.nodes[b as usize], t_max);
                let (near, tn, far, tf) = if ta <= tb { (a, ta, b, tb) } else { (b, tb, a, ta) };
                if tn < f32::INFINITY {
                    if tf < f32::INFINITY && sp < STACK {
                        stack[sp] = (far, tf);
                        sp += 1;
                    }
                    current = near;
                    continue;
                }
            }
            // Pop the next subtree that can still contain something closer.
            loop {
                if sp == 0 {
                    return;
                }
                sp -= 1;
                let (n, t) = stack[sp];
                if t <= t_max {
                    current = n;
                    break;
                }
            }
        }
    }

    /// Maximum leaf depth (diagnostics and tests).
    #[cfg(test)]
    pub fn depth(&self) -> u32 {
        fn walk(b: &Bvh, n: usize) -> u32 {
            let node = &b.nodes[n];
            if node.count > 0 {
                1
            } else {
                1 + walk(b, node.first as usize).max(walk(b, node.first as usize + 1))
            }
        }
        if self.nodes.is_empty() {
            0
        } else {
            walk(self, 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec3;

    fn boxes(n: usize) -> Vec<Aabb> {
        (0..n)
            .map(|i| {
                let f = i as f32;
                let c = Vec3::new((f * 0.731).sin() * 5.0, (f * 0.413).cos() * 2.0, (f * 1.177).sin() * 5.0);
                let h = Vec3::new(0.1 + (f * 0.3).sin().abs() * 0.5, 0.2, 0.1 + (f * 0.7).cos().abs());
                Aabb::new(c - h, c + h)
            })
            .collect()
    }

    fn slab(b: &Aabb, o: Vec3, d: Vec3, t_max: f32) -> bool {
        let inv = safe_recip(d.into());
        let t0 = (Vec3A::from(b.min) - Vec3A::from(o)) * inv;
        let t1 = (Vec3A::from(b.max) - Vec3A::from(o)) * inv;
        t0.min(t1).max_element().max(0.0) <= t0.max(t1).min_element().min(t_max)
    }

    #[test]
    fn empty_and_single() {
        let b = Bvh::build(&[]);
        b.traverse(Vec3A::ZERO, Vec3A::X, 10.0, |_, _| panic!("nothing to visit"));
        let b = Bvh::build(&[Aabb::new(Vec3::splat(-1.0), Vec3::splat(1.0))]);
        let mut seen = 0;
        b.traverse(Vec3A::new(-5.0, 0.0, 0.0), Vec3A::X, 10.0, |i, _| {
            assert_eq!(i, 0);
            seen += 1;
            false
        });
        assert_eq!(seen, 1);
    }

    #[test]
    fn every_primitive_is_in_exactly_one_leaf() {
        for n in [1, 2, 3, 7, 50, 333] {
            let bx = boxes(n);
            let b = Bvh::build(&bx);
            let mut seen = vec![0u32; n];
            for node in b.nodes.iter().filter(|n| n.count > 0) {
                assert!(node.count as usize <= LEAF_MAX);
                for &i in &b.order[node.first as usize..(node.first + node.count) as usize] {
                    seen[i as usize] += 1;
                    let bb = &bx[i as usize];
                    assert!(Vec3A::from(bb.min).cmpge(node.min).all() && Vec3A::from(bb.max).cmple(node.max).all());
                }
            }
            assert!(seen.iter().all(|&c| c == 1), "n = {n}");
            assert!(b.depth() as usize <= STACK, "n = {n}: depth {}", b.depth());
        }
    }

    #[test]
    fn traversal_visits_every_box_the_ray_touches() {
        let bx = boxes(200);
        let b = Bvh::build(&bx);
        for r in 0..300 {
            let f = r as f32;
            let o = Vec3::new((f * 0.37).sin() * 8.0, (f * 0.21).cos() * 3.0, (f * 0.53).cos() * 8.0);
            let d = (Vec3::new((f * 1.3).sin(), (f * 0.9).cos() * 0.4, (f * 0.61).sin()) - o * 0.1).normalize();
            let t_max = 6.0 + (f * 0.11).sin() * 5.0;
            let mut visited = vec![false; bx.len()];
            b.traverse(o.into(), d.into(), t_max, |i, _| {
                visited[i as usize] = true;
                false
            });
            for (i, bb) in bx.iter().enumerate() {
                if slab(bb, o, d, t_max) {
                    assert!(visited[i], "ray {r} missed box {i}");
                }
            }
        }
    }

    #[test]
    fn coincident_boxes_stay_shallow() {
        let bx = vec![Aabb::new(Vec3::ZERO, Vec3::ONE); 500];
        let b = Bvh::build(&bx);
        assert!(b.depth() <= 12, "depth {}", b.depth());
        let mut seen = 0;
        b.traverse(Vec3A::new(0.5, 0.5, -3.0), Vec3A::Z, 10.0, |_, _| {
            seen += 1;
            false
        });
        assert_eq!(seen, 500);
    }

    #[test]
    fn stop_and_shrink() {
        let bx: Vec<Aabb> = (0..20).map(|i| Aabb::new(Vec3::new(i as f32, 0.0, 0.0), Vec3::new(i as f32 + 0.5, 1.0, 1.0))).collect();
        let b = Bvh::build(&bx);
        // Any-hit: stops at the first visit.
        let mut seen = 0;
        b.traverse(Vec3A::new(-1.0, 0.5, 0.5), Vec3A::X, 100.0, |_, _| {
            seen += 1;
            true
        });
        assert_eq!(seen, 1);
        // Closest-hit: shrinking t_max prunes the far boxes.
        let mut seen = 0;
        b.traverse(Vec3A::new(-1.0, 0.5, 0.5), Vec3A::X, 100.0, |i, t| {
            seen += 1;
            *t = t.min(i as f32 + 1.0);
            false
        });
        assert!(seen <= 4, "visited {seen}");
    }
}
