//! Bounding volume hierarchy over primitive world boxes.
//!
//! Built with binned SAH, which is cheap enough to rebuild every tick for the
//! ~50 primitives of the stage (about ten microseconds) and gives good trees
//! for the few hundred boxes of the house.
//!
//! Every node stores the boxes of its two children, so a traversal step
//! touches one node and decides about both subtrees at once.
//!
//! Leaf boxes are padded by a hair (see [`PAD_ABS`]), which makes the
//! hierarchy conservative: a ray that only grazes a primitive, where the
//! primitive test and an exact box test could disagree by a rounding error,
//! still reaches the primitive. Ray casts through the tree therefore agree
//! with testing every primitive. Primitives whose box is not finite (a NaN
//! transform) are left out instead of poisoning the tree.

use glam::{Vec3A, Vec4, Vec4Swizzles};

use crate::types::Aabb;

const LEAF_MAX: usize = 2;
const BINS: usize = 12;
/// Below this depth the builder stops trusting SAH and splits at the median,
/// which bounds the tree depth (and so the traversal stack).
const SAH_MAX_DEPTH: u32 = 24;
const STACK: usize = 48;
/// Subtree references keep a leaf's primitive count above this bit.
const LEAF_SHIFT: u32 = 30;
const LEAF_MASK: u32 = (1 << LEAF_SHIFT) - 1;
const _: () = assert!(LEAF_MAX < 4, "the leaf count must fit above LEAF_SHIFT");
/// Leaf boxes grow by `PAD_ABS + PAD_REL * (largest absolute coordinate)` on
/// every side: a few float steps at any plausible scene scale, far below
/// anything visible (0.08 mm for a box 6 m from the origin).
const PAD_ABS: f32 = 2.0e-5;
const PAD_REL: f32 = 1.0e-5;

/// Two children: their boxes and where each one leads. One cache line.
#[derive(Clone, Copy, Debug)]
struct Node {
    /// Child box slabs per axis, laid out as
    /// `[left.min, right.min, left.max, right.max]` so that one 4-lane
    /// operation per axis intersects the ray with both children at once.
    slabs: [Vec4; 3],
    /// Leaf child: first slot in `order`. Interior child: node index.
    first: [u32; 2],
    /// Number of primitives of a leaf child, 0 for an interior child.
    count: [u32; 2],
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
    const EMPTY: Self = Self {
        min: Vec3A::INFINITY,
        max: Vec3A::NEG_INFINITY,
    };

    #[inline]
    fn grow(&mut self, o: &Bounds) {
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
    }

    /// The box of one primitive, padded (see [`PAD_ABS`]).
    #[inline]
    fn padded(b: &Aabb) -> Self {
        let (min, max) = (Vec3A::from(b.min), Vec3A::from(b.max));
        let pad = PAD_ABS + PAD_REL * min.abs().max(max.abs()).max_element();
        Self {
            min: min - pad,
            max: max + pad,
        }
    }

    #[inline]
    fn is_finite(&self) -> bool {
        self.min.is_finite() && self.max.is_finite()
    }

    #[inline]
    fn half_area(&self) -> f32 {
        let e = (self.max - self.min).max(Vec3A::ZERO);
        e.x * e.y + e.y * e.z + e.z * e.x
    }
}

/// A subtree under construction: a range of `order` and its bounds.
#[derive(Clone, Copy)]
struct Range {
    first: usize,
    count: usize,
    bounds: Bounds,
}

impl Bvh {
    /// Builds the hierarchy over the given boxes (one per primitive).
    pub fn build(boxes: &[Aabb]) -> Self {
        let bounds: Vec<Bounds> = boxes.iter().map(Bounds::padded).collect();
        // Primitives without a finite box can never be hit: leave them out.
        let order: Vec<u32> = (0..boxes.len() as u32)
            .filter(|&i| bounds[i as usize].is_finite())
            .collect();
        let n = order.len();
        if n == 0 {
            return Self::default();
        }
        let mut bvh = Self {
            nodes: Vec::with_capacity(n),
            order,
        };
        let mut all = Bounds::EMPTY;
        for &i in &bvh.order {
            all.grow(&bounds[i as usize]);
        }
        let root = Range {
            first: 0,
            count: n,
            bounds: all,
        };
        if n <= LEAF_MAX {
            // A lone leaf still needs a node: its second child is a box no ray enters.
            let nowhere = Range {
                first: 0,
                count: 0,
                bounds: Bounds {
                    min: Vec3A::INFINITY,
                    max: Vec3A::INFINITY,
                },
            };
            bvh.nodes.push(Node::new(&root, &nowhere));
            bvh.nodes[0].count = [n as u32, 0];
            bvh.nodes[0].first = [0, 0];
        } else {
            bvh.build_node(root, &bounds, 0);
        }
        bvh
    }

    /// Splits `range` in two, emits the node holding both halves and recurses
    /// into halves that are not leaves. Returns the node's index.
    fn build_node(&mut self, range: Range, bounds: &[Bounds], depth: u32) -> u32 {
        let (left, right) = self.split(range, bounds, depth);
        let index = self.nodes.len();
        self.nodes.push(Node::new(&left, &right));
        for (slot, half) in [left, right].into_iter().enumerate() {
            if half.count <= LEAF_MAX {
                self.nodes[index].first[slot] = half.first as u32;
                self.nodes[index].count[slot] = half.count as u32;
            } else {
                let child = self.build_node(half, bounds, depth + 1);
                self.nodes[index].first[slot] = child;
                self.nodes[index].count[slot] = 0;
            }
        }
        index as u32
    }

    /// Partitions the primitives of `range` (more than `LEAF_MAX`) in two
    /// non-empty groups.
    fn split(&mut self, range: Range, bounds: &[Bounds], depth: u32) -> (Range, Range) {
        let Range { first, count, .. } = range;
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
                let mut right_count = [0u32; BINS];
                let mut acc = Bounds::EMPTY;
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

        let half = |range: core::ops::Range<usize>| {
            let mut b = Bounds::EMPTY;
            for &i in &self.order[range.clone()] {
                b.grow(&bounds[i as usize]);
            }
            Range {
                first: range.start,
                count: range.len(),
                bounds: b,
            }
        };
        (half(first..mid), half(mid..first + count))
    }

    /// Walks the tree front to back along the ray `origin + t * dir`,
    /// `t` in `[0, t_max]`, given `inv_dir = safe_recip(dir)`.
    ///
    /// `visit(primitive, &mut t_max)` is called for every primitive whose leaf
    /// box the ray touches. It may shrink `t_max` (closest-hit queries) and
    /// returns `true` to stop the traversal (any-hit queries).
    #[inline]
    pub fn traverse(&self, origin: Vec3A, inv_dir: Vec3A, t_max: f32, mut visit: impl FnMut(u32, &mut f32) -> bool) {
        if self.nodes.is_empty() {
            return;
        }
        // Finite, so that a missed box (entry = +inf) always fails `<= t_max`.
        let mut t_max = t_max.min(f32::MAX);
        let inv = inv_dir;
        let (ox, oy, oz) = (Vec4::splat(origin.x), Vec4::splat(origin.y), Vec4::splat(origin.z));
        let (ix, iy, iz) = (Vec4::splat(inv.x), Vec4::splat(inv.y), Vec4::splat(inv.z));
        // Distances at which the ray enters the two child boxes of a node,
        // +inf for a box it misses.
        let entries = |node: &Node| -> (f32, f32) {
            // Per axis: parameters at the min planes (lanes 0, 1) and at the
            // max planes (lanes 2, 3) of the left and right child.
            let tx = (node.slabs[0] - ox) * ix;
            let ty = (node.slabs[1] - oy) * iy;
            let tz = (node.slabs[2] - oz) * iz;
            // Swapping the halves pairs each min plane with its max plane.
            let (sx, sy, sz) = (tx.zwxy(), ty.zwxy(), tz.zwxy());
            let near = tx.min(sx).max(ty.min(sy)).max(tz.min(sz)).max(Vec4::ZERO);
            let far = tx.max(sx).min(ty.max(sy)).min(tz.max(sz));
            let hit = near.cmple(far).bitmask();
            (
                if hit & 1 != 0 { near.x } else { f32::INFINITY },
                if hit & 2 != 0 { near.y } else { f32::INFINITY },
            )
        };
        // A reference to a subtree: a node index, or a leaf's primitive
        // range with its count in the top bits.
        let reference = |node: &Node, slot: usize| node.first[slot] | (node.count[slot] << LEAF_SHIFT);
        let mut stack = [(0u32, 0.0f32); STACK];
        let mut sp = 0;
        let mut current = 0u32;
        loop {
            let count = current >> LEAF_SHIFT;
            if count > 0 {
                let first = (current & LEAF_MASK) as usize;
                for &prim in &self.order[first..first + count as usize] {
                    if visit(prim, &mut t_max) {
                        return;
                    }
                }
            } else {
                let node = &self.nodes[current as usize];
                let (t0, t1) = entries(node);
                let (near, far, t_near, t_far) = if t0 <= t1 { (0, 1, t0, t1) } else { (1, 0, t1, t0) };
                if t_near <= t_max {
                    if t_far <= t_max && sp < STACK {
                        stack[sp] = (reference(node, far), t_far);
                        sp += 1;
                    }
                    current = reference(node, near);
                    continue;
                }
            }
            // Pop the next subtree that can still contain something closer.
            loop {
                if sp == 0 {
                    return;
                }
                sp -= 1;
                let (reference, t) = stack[sp];
                if t <= t_max {
                    current = reference;
                    break;
                }
            }
        }
    }

    /// Maximum number of nodes on a root-to-leaf path (tests).
    #[cfg(test)]
    pub fn depth(&self) -> u32 {
        fn walk(b: &Bvh, n: usize) -> u32 {
            let node = &b.nodes[n];
            let child = |slot: usize| {
                if node.count[slot] > 0 || !node.child_bounds(slot).0.is_finite() {
                    0
                } else {
                    walk(b, node.first[slot] as usize)
                }
            };
            1 + child(0).max(child(1))
        }
        if self.nodes.is_empty() {
            0
        } else {
            walk(self, 0)
        }
    }
}

impl Node {
    fn new(left: &Range, right: &Range) -> Self {
        let (l, r) = (&left.bounds, &right.bounds);
        let axis = |a: usize| Vec4::new(l.min[a], r.min[a], l.max[a], r.max[a]);
        Self {
            slabs: [axis(0), axis(1), axis(2)],
            first: [0; 2],
            count: [0; 2],
        }
    }

    /// Bounds of one child (tests).
    #[cfg(test)]
    fn child_bounds(&self, slot: usize) -> (glam::Vec3, glam::Vec3) {
        let s = &self.slabs;
        (
            glam::Vec3::new(s[0][slot], s[1][slot], s[2][slot]),
            glam::Vec3::new(s[0][slot + 2], s[1][slot + 2], s[2][slot + 2]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::primitive::safe_recip;
    use super::*;
    use glam::Vec3;

    fn boxes(n: usize) -> Vec<Aabb> {
        (0..n)
            .map(|i| {
                let f = i as f32;
                let c = Vec3::new(
                    (f * 0.731).sin() * 5.0,
                    (f * 0.413).cos() * 2.0,
                    (f * 1.177).sin() * 5.0,
                );
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
        b.traverse(Vec3A::ZERO, safe_recip(Vec3A::X), 10.0, |_, _| {
            panic!("nothing to visit")
        });
        let b = Bvh::build(&[Aabb::new(Vec3::splat(-1.0), Vec3::splat(1.0))]);
        let mut seen = 0;
        b.traverse(Vec3A::new(-5.0, 0.0, 0.0), safe_recip(Vec3A::X), 10.0, |i, _| {
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
            for node in &b.nodes {
                for slot in 0..2 {
                    let (first, count) = (node.first[slot] as usize, node.count[slot] as usize);
                    assert!(count <= LEAF_MAX);
                    for &i in &b.order[first.min(n)..(first + count).min(n)] {
                        seen[i as usize] += 1;
                        let bb = &bx[i as usize];
                        let (min, max) = node.child_bounds(slot);
                        assert!(bb.min.cmpge(min).all() && bb.max.cmple(max).all());
                    }
                }
            }
            assert!(seen.iter().all(|&c| c == 1), "n = {n}: {seen:?}");
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
            b.traverse(o.into(), safe_recip(d.into()), t_max, |i, _| {
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
        b.traverse(Vec3A::new(0.5, 0.5, -3.0), safe_recip(Vec3A::Z), 10.0, |_, _| {
            seen += 1;
            false
        });
        assert_eq!(seen, 500);
    }

    #[test]
    fn boxes_without_finite_bounds_are_left_out() {
        let mut bx = boxes(40);
        bx[3] = Aabb::new(Vec3::splat(f32::NAN), Vec3::splat(f32::NAN));
        bx[17] = Aabb::new(Vec3::NEG_INFINITY, Vec3::INFINITY);
        bx[29] = Aabb::EMPTY;
        let b = Bvh::build(&bx);
        assert_eq!(b.order.len(), 37);
        for r in 0..200 {
            let f = r as f32;
            let o = Vec3::new((f * 0.37).sin() * 8.0, (f * 0.21).cos() * 3.0, (f * 0.53).cos() * 8.0);
            let d = (Vec3::new((f * 1.3).sin(), (f * 0.9).cos() * 0.4, (f * 0.61).sin()) - o * 0.1).normalize();
            let mut visited = vec![false; bx.len()];
            b.traverse(o.into(), safe_recip(d.into()), 50.0, |i, _| {
                visited[i as usize] = true;
                false
            });
            assert!(!visited[3] && !visited[17] && !visited[29]);
            for (i, bb) in bx.iter().enumerate() {
                if ![3, 17, 29].contains(&i) && slab(bb, o, d, 50.0) {
                    assert!(visited[i], "ray {r} missed box {i}");
                }
            }
        }
        // Nothing but broken boxes: an empty tree.
        let b = Bvh::build(&[bx[3], bx[17], bx[29]]);
        b.traverse(Vec3A::ZERO, safe_recip(Vec3A::X), 10.0, |_, _| {
            panic!("nothing to visit")
        });
    }

    #[test]
    fn grazing_rays_reach_the_primitive() {
        // Boxes whose faces are not exactly representable, and axis-parallel
        // rays running exactly in (or one float step outside) a face plane:
        // the padded tree must still offer the primitive to the caller.
        let bx: Vec<Aabb> = (0..30)
            .map(|i| {
                let c = Vec3::new(1.1 + 0.7 * i as f32, 0.3 * (i % 5) as f32, -0.9 * (i % 3) as f32);
                Aabb::new(c - Vec3::splat(0.2), c + Vec3::splat(0.2))
            })
            .collect();
        let b = Bvh::build(&bx);
        for (i, bb) in bx.iter().enumerate() {
            for face in [bb.min, bb.max] {
                for nudge in [-1i32, 0, 1] {
                    let step = |v: f32| f32::from_bits((v.to_bits() as i32 + nudge) as u32);
                    let c = bb.center();
                    // Along Z in the X face plane, along X in the Y face plane.
                    let rays = [
                        (Vec3::new(step(face.x), c.y, c.z - 5.0), Vec3::Z),
                        (Vec3::new(c.x - 5.0, step(face.y), c.z), Vec3::X),
                        (Vec3::new(c.x, c.y + 5.0, step(face.z)), Vec3::NEG_Y),
                    ];
                    for (o, d) in rays {
                        let mut seen = false;
                        b.traverse(o.into(), safe_recip(d.into()), 100.0, |p, _| {
                            seen |= p as usize == i;
                            false
                        });
                        assert!(seen, "box {i}, ray from {o:?} along {d:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn stop_and_shrink() {
        let bx: Vec<Aabb> = (0..20)
            .map(|i| Aabb::new(Vec3::new(i as f32, 0.0, 0.0), Vec3::new(i as f32 + 0.5, 1.0, 1.0)))
            .collect();
        let b = Bvh::build(&bx);
        // Any-hit: stops at the first visit.
        let mut seen = 0;
        b.traverse(Vec3A::new(-1.0, 0.5, 0.5), safe_recip(Vec3A::X), 100.0, |_, _| {
            seen += 1;
            true
        });
        assert_eq!(seen, 1);
        // Closest-hit: shrinking t_max prunes the far boxes.
        let mut seen = 0;
        b.traverse(Vec3A::new(-1.0, 0.5, 0.5), safe_recip(Vec3A::X), 100.0, |i, t| {
            seen += 1;
            *t = t.min(i as f32 + 1.0);
            false
        });
        assert!(seen <= 4, "visited {seen}");
    }
}
