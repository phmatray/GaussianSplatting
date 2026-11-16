//! The walls of a filmed room, and what lies behind them.
//!
//! A LiDAR reads a mirror, or a window at night, as an opening onto a second
//! room: the reflection is fused at the depth of its virtual image, behind
//! the glass. A plane mirror's image is geometrically consistent from every
//! viewpoint, so no amount of cross-checking between frames removes it; the
//! one thing that gives it away is that it stands behind a wall of the room.
//!
//! Walls are found as large vertical planes of the fused cloud that rise
//! from near the floor to near the ceiling, seen from top down as segments.
//! A splat is behind a wall when its projection falls within the wall's
//! extent and it lies on the far side from the cameras that filmed that
//! wall. What a door open onto the next room shows goes too: a room capture
//! is of one room.

use glam::{Quat, Vec2, Vec3};

use crate::types::Splat;

/// Splats whose surface is within this angle of vertical count as wall
/// material (sine of the angle between the normal and the horizontal).
const WALL_NORMAL_MAX_Y: f32 = 0.3;
/// Directions of the top-down normal, over half a turn.
const ANGLE_BINS: usize = 90;
/// Offset bins of a wall plane, meters.
const OFFSET_BIN: f32 = 0.05;
/// Inliers of a wall: this close to its plane...
const INLIER_DISTANCE: f32 = 0.05;
/// ...with a normal this close to its own (cosine).
const INLIER_COS: f32 = 0.94;
/// A wall is at least this long, meters...
const MIN_LENGTH: f32 = 1.2;
/// ...and its splats span this share of the height from floor to ceiling.
const MIN_HEIGHT_SHARE: f32 = 0.75;
/// A splat is behind a wall from this distance on (meters): the wall's own
/// thickness and its noise stay.
const BEHIND_MARGIN: f32 = 0.15;
/// The extent of a wall is widened by this much at each end (meters), so
/// that what is behind a door frame at the end of a wall goes too.
const EXTENT_MARGIN: f32 = 0.1;
/// A wall filmed from both sides, by at least this share of the cameras in
/// front of it on the less filmed side, stands between two rooms of the walk.
const INTERIOR_SHARE: f32 = 0.15;
/// A plane within this angle (cosine of 10 degrees)...
const DUPLICATE_COS: f32 = 0.985;
/// ...and this distance of a wall along its whole length is that wall.
const DUPLICATE_DISTANCE: f32 = 0.5;
/// A splat this close to a wall's plane, within its extent, is the wall.
const ON_WALL: f32 = 0.12;
/// A wall-sized plane with a wall parallel to it less than this far behind
/// is furniture standing against that wall (meters).
const FURNITURE_DEPTH: f32 = 1.0;
/// Height assumed between floor and ceiling when the ceiling is unknown.
const DEFAULT_ROOM_HEIGHT: f32 = 2.4;

/// A wall seen from above: the points `p` with `p · normal = offset`,
/// between `from` and `to` along `tangent`, `normal` pointing into the room.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Wall {
    pub normal: Vec2,
    pub offset: f32,
    pub tangent: Vec2,
    pub from: f32,
    pub to: f32,
    /// False for a wall between two rooms of the walk (filmed from both
    /// sides): it protects its own surface but hides nothing.
    pub cuts: bool,
}

impl Wall {
    /// Signed distance of a top-down point into the room (negative behind
    /// the wall).
    fn inward(&self, p: Vec2) -> f32 {
        p.dot(self.normal) - self.offset
    }

    fn along(&self, p: Vec2) -> f32 {
        p.dot(self.tangent)
    }

    /// True when `p` is behind this wall, within its extent.
    pub fn hides(&self, p: Vec2) -> bool {
        let t = self.along(p);
        self.cuts && t >= self.from - EXTENT_MARGIN && t <= self.to + EXTENT_MARGIN && self.inward(p) < -BEHIND_MARGIN
    }
}

/// The local axis of a splat along which it is thinnest: the normal of the
/// surface it stands for.
fn thin_axis(splat: &Splat) -> Vec3 {
    let [x, y, z, w] = splat.rot;
    let q = Quat::from_xyzw(x, y, z, w);
    let q = if q.is_finite() && q.length_squared() > 1e-12 { q.normalize() } else { Quat::IDENTITY };
    let s = splat.scale;
    let axis = if s[0] <= s[1] && s[0] <= s[2] {
        Vec3::X
    } else if s[1] <= s[2] {
        Vec3::Y
    } else {
        Vec3::Z
    };
    q * axis
}

fn top(p: Vec3) -> Vec2 {
    Vec2::new(p.x, p.z)
}

/// The walls of a room, from its fused splats, the floor and ceiling
/// heights and the positions the phone filmed from.
pub fn find_walls(splats: &[Splat], floor: f32, ceiling: Option<f32>, cameras: &[Vec3]) -> Vec<Wall> {
    let ceiling = ceiling.filter(|c| c.is_finite() && *c > floor + 1.0).unwrap_or(floor + DEFAULT_ROOM_HEIGHT);
    let height = ceiling - floor;
    // Wall material: vertical surfaces between the floor and the ceiling.
    let material: Vec<(Vec2, Vec2, f32)> = splats
        .iter()
        .filter_map(|s| {
            let p = s.position();
            if !p.is_finite() || p.y < floor + 0.05 * height || p.y > ceiling - 0.05 * height {
                return None;
            }
            let n = thin_axis(s);
            if n.y.abs() > WALL_NORMAL_MAX_Y {
                return None;
            }
            let n2 = Vec2::new(n.x, n.z).normalize_or_zero();
            (n2 != Vec2::ZERO).then_some((top(p), n2, p.y))
        })
        .collect();
    if material.len() < 50 {
        return Vec::new();
    }
    // Votes for (direction, offset): the normal folded onto half a turn.
    let fold = |n: Vec2| if n.y < 0.0 || (n.y == 0.0 && n.x < 0.0) { -n } else { n };
    let bin_of = |n: Vec2| ((n.y.atan2(n.x).rem_euclid(std::f32::consts::PI) / std::f32::consts::PI) * ANGLE_BINS as f32) as usize % ANGLE_BINS;
    let mut votes: std::collections::HashMap<(usize, i32), u32> = std::collections::HashMap::new();
    for (p, n, _) in &material {
        let n = fold(*n);
        let key = (bin_of(n), (p.dot(n) / OFFSET_BIN).round() as i32);
        *votes.entry(key).or_default() += 1;
    }
    let mut peaks: Vec<((usize, i32), u32)> = votes.into_iter().collect();
    peaks.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let min_votes = (material.len() as u32 / 1500).max(20);

    let mut walls: Vec<Wall> = Vec::new();
    let mut tried: Vec<(Vec2, f32)> = Vec::new();
    for ((bin, offset_bin), count) in peaks {
        if count < min_votes {
            break;
        }
        let angle = (bin as f32 + 0.5) / ANGLE_BINS as f32 * std::f32::consts::PI;
        let normal = Vec2::new(angle.cos(), angle.sin());
        let offset = offset_bin as f32 * OFFSET_BIN;
        // One wall per plane: a peak next to a wall already found is part of it.
        if tried.iter().any(|(n, d)| n.dot(normal) > 0.995 && (d - offset).abs() < 0.2) {
            continue;
        }
        tried.push((normal, offset));
        let inliers: Vec<(f32, f32)> = material
            .iter()
            .filter(|(p, n, _)| (p.dot(normal) - offset).abs() < INLIER_DISTANCE && n.dot(normal).abs() > INLIER_COS)
            .map(|(p, _, y)| (p.dot(normal.perp()), *y))
            .collect();
        if inliers.len() < min_votes as usize {
            continue;
        }
        let mut along: Vec<f32> = inliers.iter().map(|(t, _)| *t).collect();
        let mut heights: Vec<f32> = inliers.iter().map(|(_, y)| *y).collect();
        along.sort_by(f32::total_cmp);
        heights.sort_by(f32::total_cmp);
        let pick = |v: &[f32], q: f32| v[((v.len() - 1) as f32 * q) as usize];
        let (from, to) = (pick(&along, 0.02), pick(&along, 0.98));
        let span = pick(&heights, 0.98) - pick(&heights, 0.02);
        if to - from < MIN_LENGTH || span < MIN_HEIGHT_SHARE * height {
            continue;
        }
        // Into the room: the side of the cameras that stood in front of it.
        let tangent = normal.perp();
        // A plane a few degrees off a wall already found, and close to it
        // along its length, is the same wall fitted twice: kept, it would
        // cut into the wall's own surface at one end.
        let ends = [tangent * from + normal * offset, tangent * to + normal * offset];
        if walls.iter().any(|w| w.normal.dot(normal).abs() > DUPLICATE_COS && ends.iter().all(|e| w.inward(*e).abs() < DUPLICATE_DISTANCE)) {
            continue;
        }
        let (mut front, mut back) = (0usize, 0usize);
        for c in cameras.iter().map(|c| top(*c)).filter(|c| c.is_finite() && (from..=to).contains(&c.dot(tangent))) {
            if c.dot(normal) > offset {
                front += 1;
            } else {
                back += 1;
            }
        }
        // Filmed from both sides: a wall between two rooms of the walk,
        // which hides nothing.
        let interior = front.min(back) as f32 > INTERIOR_SHARE * (front + back) as f32 || front + back == 0;
        let s = if front >= back { 1.0 } else { -1.0 };
        walls.push(Wall { normal: normal * s, offset: offset * s, tangent, from, to, cuts: !interior });
    }
    // Furniture as tall as the room (a wardrobe, shelves to the ceiling)
    // passes for a wall; the wall it stands against, a short step behind,
    // gives it away. It protects its own surface but hides nothing.
    let furniture: Vec<bool> = walls
        .iter()
        .map(|w| {
            walls.iter().any(|b| {
                let depth = -w.inward(b.normal * b.offset);
                b.cuts && b.normal.dot(w.normal) > DUPLICATE_COS && depth > 0.25 && depth < FURNITURE_DEPTH && b.from < w.to && w.from < b.to
            })
        })
        .collect();
    for (w, is_furniture) in walls.iter_mut().zip(furniture) {
        w.cuts &= !is_furniture;
    }
    walls
}

/// For each splat, whether it stands behind one of `walls`.
pub fn behind_walls(splats: &[Splat], walls: &[Wall]) -> Vec<bool> {
    splats
        .iter()
        .map(|s| {
            let p = top(s.position());
            let on_a_wall = walls.iter().any(|w| w.inward(p).abs() < ON_WALL && (w.from - EXTENT_MARGIN..=w.to + EXTENT_MARGIN).contains(&w.along(p)));
            !on_a_wall && walls.iter().any(|w| w.hides(p))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wall of surfels from `a` to `b` (top down), floor to ceiling,
    /// facing `inward`.
    fn wall(a: Vec2, b: Vec2, inward: Vec2, height: f32, out: &mut Vec<Splat>) {
        let steps = ((b - a).length() / 0.05) as usize;
        for i in 0..=steps {
            let p = a + (b - a) * (i as f32 / steps as f32);
            for j in 0..(height / 0.05) as usize {
                let n = Vec3::new(inward.x, 0.0, inward.y);
                out.push(Splat::surfel(Vec3::new(p.x, 0.05 * j as f32 + 0.02, p.y), n, 0.03, 0.003, [200; 4]));
            }
        }
    }

    /// A 4 x 3 m room, 2.5 m high, filmed from its middle.
    fn room(out: &mut Vec<Splat>) -> Vec<Vec3> {
        let h = 2.5;
        wall(Vec2::new(0.0, 0.0), Vec2::new(4.0, 0.0), Vec2::Y, h, out);
        wall(Vec2::new(4.0, 0.0), Vec2::new(4.0, 3.0), Vec2::NEG_X, h, out);
        // The wall at z = 3 has a 1 m glass door from x = 1.5 to 2.5.
        wall(Vec2::new(0.0, 3.0), Vec2::new(1.5, 3.0), Vec2::NEG_Y, h, out);
        wall(Vec2::new(2.5, 3.0), Vec2::new(4.0, 3.0), Vec2::NEG_Y, h, out);
        wall(Vec2::new(0.0, 0.0), Vec2::new(0.0, 3.0), Vec2::X, h, out);
        (0..20).map(|i| Vec3::new(1.0 + 0.1 * i as f32, 1.4, 1.5)).collect()
    }

    #[test]
    fn the_reflection_behind_a_glass_door_is_behind_a_wall() {
        let mut splats = Vec::new();
        let cameras = room(&mut splats);
        // The room mirrored in the door: a sofa 1 m inside appears 1 m behind it.
        let ghost: Vec<Splat> = (0..200).map(|i| Splat::surfel(Vec3::new(1.6 + 0.004 * i as f32, 0.6, 4.0), Vec3::NEG_Z, 0.03, 0.003, [90; 4])).collect();
        let sofa: Vec<Splat> = (0..200).map(|i| Splat::surfel(Vec3::new(1.6 + 0.004 * i as f32, 0.6, 2.0), Vec3::Z, 0.03, 0.003, [90; 4])).collect();
        let first_extra = splats.len();
        splats.extend(ghost);
        splats.extend(sofa);
        let walls = find_walls(&splats, 0.0, Some(2.5), &cameras);
        assert_eq!(walls.len(), 4, "{walls:?}");
        let behind = behind_walls(&splats, &walls);
        assert!(behind[..first_extra].iter().all(|b| !b), "a wall is not behind itself");
        assert!(behind[first_extra..first_extra + 200].iter().all(|b| *b), "the ghost goes");
        assert!(behind[first_extra + 200..].iter().all(|b| !b), "the sofa stays");
    }

    #[test]
    fn an_l_shaped_room_keeps_both_arms() {
        // An L: a 4 x 3 room with a 2 x 2 arm to the side, open between them.
        let h = 2.5;
        let mut s = Vec::new();
        wall(Vec2::new(0.0, 0.0), Vec2::new(4.0, 0.0), Vec2::Y, h, &mut s);
        wall(Vec2::new(0.0, 0.0), Vec2::new(0.0, 5.0), Vec2::X, h, &mut s);
        wall(Vec2::new(0.0, 5.0), Vec2::new(2.0, 5.0), Vec2::NEG_Y, h, &mut s);
        wall(Vec2::new(2.0, 3.0), Vec2::new(2.0, 5.0), Vec2::NEG_X, h, &mut s);
        wall(Vec2::new(2.0, 3.0), Vec2::new(4.0, 3.0), Vec2::NEG_Y, h, &mut s);
        wall(Vec2::new(4.0, 0.0), Vec2::new(4.0, 3.0), Vec2::NEG_X, h, &mut s);
        let mut cameras: Vec<Vec3> = (0..10).map(|i| Vec3::new(1.0 + 0.2 * i as f32, 1.4, 1.5)).collect();
        cameras.extend((0..10).map(|i| Vec3::new(1.0, 1.4, 3.2 + 0.15 * i as f32)));
        // A chair in each arm.
        let chair = |x: f32, z: f32| Splat::surfel(Vec3::new(x, 0.5, z), Vec3::Y, 0.03, 0.003, [50; 4]);
        let things = [chair(3.0, 1.5), chair(1.0, 4.2)];
        let first = s.len();
        s.extend(things);
        let walls = find_walls(&s, 0.0, Some(2.5), &cameras);
        let behind = behind_walls(&s, &walls);
        assert!(!behind[first] && !behind[first + 1], "{walls:?}");
    }

    #[test]
    fn a_walk_through_two_rooms_keeps_both() {
        // Two 3 x 3 rooms side by side, a door in the wall between them, the
        // phone in both.
        let h = 2.5;
        let mut s = Vec::new();
        wall(Vec2::new(0.0, 0.0), Vec2::new(6.0, 0.0), Vec2::Y, h, &mut s);
        wall(Vec2::new(0.0, 3.0), Vec2::new(6.0, 3.0), Vec2::NEG_Y, h, &mut s);
        wall(Vec2::new(0.0, 0.0), Vec2::new(0.0, 3.0), Vec2::X, h, &mut s);
        wall(Vec2::new(6.0, 0.0), Vec2::new(6.0, 3.0), Vec2::NEG_X, h, &mut s);
        wall(Vec2::new(3.0, 0.0), Vec2::new(3.0, 1.2), Vec2::NEG_X, h, &mut s);
        wall(Vec2::new(3.0, 2.0), Vec2::new(3.0, 3.0), Vec2::NEG_X, h, &mut s);
        let mut cameras: Vec<Vec3> = (0..10).map(|i| Vec3::new(1.5, 1.4, 0.5 + 0.2 * i as f32)).collect();
        cameras.extend((0..10).map(|i| Vec3::new(4.5, 1.4, 0.5 + 0.2 * i as f32)));
        let chair = |x: f32| Splat::surfel(Vec3::new(x, 0.5, 1.5), Vec3::Y, 0.03, 0.003, [50; 4]);
        let first = s.len();
        s.extend([chair(1.0), chair(5.0)]);
        let walls = find_walls(&s, 0.0, Some(2.5), &cameras);
        let behind = behind_walls(&s, &walls);
        assert!(!behind[first] && !behind[first + 1], "{walls:?}");
    }

    #[test]
    fn a_wardrobe_to_the_ceiling_is_furniture() {
        let mut s = Vec::new();
        let cameras = room(&mut s);
        // A wardrobe front 0.6 m from the wall at z = 0, as high as the room,
        // with a box between it and the wall.
        wall(Vec2::new(0.8, 0.6), Vec2::new(3.2, 0.6), Vec2::Y, 2.5, &mut s);
        let boxes = s.len();
        s.extend((0..50).map(|i| Splat::surfel(Vec3::new(1.0 + 0.04 * i as f32, 0.5, 0.3), Vec3::Y, 0.02, 0.003, [30; 4])));
        let walls = find_walls(&s, 0.0, Some(2.5), &cameras);
        assert!(behind_walls(&s, &walls)[boxes..].iter().all(|b| !b), "{walls:?}");
    }

    #[test]
    fn a_bookcase_is_no_wall() {
        let mut s = Vec::new();
        let cameras = room(&mut s);
        // A 1.6 m high bookcase front at z = 0.4, with books behind it.
        wall(Vec2::new(1.0, 0.4), Vec2::new(3.0, 0.4), Vec2::Y, 1.6, &mut s);
        let books = s.len();
        s.extend((0..50).map(|i| Splat::surfel(Vec3::new(1.2 + 0.03 * i as f32, 1.0, 0.2), Vec3::Z, 0.02, 0.003, [30; 4])));
        let walls = find_walls(&s, 0.0, Some(2.5), &cameras);
        assert!(behind_walls(&s, &walls)[books..].iter().all(|b| !b), "{walls:?}");
    }
}
