//! A furnished single-storey home to walk through.
//!
//! The interior is 11 m x 8 m with a 2.5 m ceiling:
//!
//! ```text
//!        -5.5                 0.5                      5.5   x
//!   -4.0  +--------------------+------------------------+
//!         |                    |  (stub wall, then open)|
//!         |    living room          kitchen             |
//!         |                                             |
//!    0.2  +-------------[ ]-+--+------------+   (open)  |
//!         |                 |  |            |           |
//!         |    bedroom     [ ] |  bathroom  |  dining   |
//!         |                 | [ ]           |   nook    |
//!    4.0  +-----------------+--+------------+-----------+
//!    z                   -1.0  0.4         3.0
//!                          hallway
//! ```
//!
//! `[ ]` are 1 m door openings with a lintel. Every room has its own floor
//! material, wall tint and ceiling light, windows are bright emissive panels
//! on the exterior walls, and the whole interior is sealed by an outer shell,
//! a slab and a ceiling, so no ray cast from inside escapes to the void.

use core::f32::consts::TAU;

use glam::{Affine3A, Quat, Vec3};

use super::material::{srgb, Axis, Material};
use super::primitive::{Primitive, Shape};
use super::{Light, Lighting, Scene, SceneSnapshot};
use crate::types::Aabb;

/// Interior ceiling height in meters (the floor is `y = 0`).
pub const CEILING_HEIGHT: f32 = 2.5;
/// Height of the door openings below their lintel.
pub const DOOR_HEIGHT: f32 = 2.1;
/// Half of the interior extent along X.
pub const HALF_X: f32 = 5.5;
/// Half of the interior extent along Z.
pub const HALF_Z: f32 = 4.0;
/// Eye height of the default walk-through.
pub const EYE_HEIGHT: f32 = 1.4;

/// Thickness of one wall skin; an interior wall is two skins back to back.
const SKIN: f32 = 0.06;
const SHELL: f32 = 0.2;

/// A room of the house: its name and floor rectangle.
///
/// Rectangles reach to the wall center lines, so neighbouring rooms share
/// their boundary and the walls take [`Room::WALL_INSET`] from each side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Room {
    pub name: &'static str,
    /// Minimum `(x, z)` corner.
    pub min: [f32; 2],
    /// Maximum `(x, z)` corner.
    pub max: [f32; 2],
}

impl Room {
    /// Distance from a room rectangle's edge to the wall surface.
    pub const WALL_INSET: f32 = SKIN;

    /// Center of the room at floor level.
    pub fn center(&self) -> Vec3 {
        Vec3::new(
            0.5 * (self.min[0] + self.max[0]),
            0.0,
            0.5 * (self.min[1] + self.max[1]),
        )
    }

    /// The room volume from floor to ceiling.
    pub fn aabb(&self) -> Aabb {
        Aabb::new(
            Vec3::new(self.min[0], 0.0, self.min[1]),
            Vec3::new(self.max[0], CEILING_HEIGHT, self.max[1]),
        )
    }

    /// True when the point lies inside the room rectangle (any height).
    pub fn contains(&self, p: Vec3) -> bool {
        p.x >= self.min[0] && p.x <= self.max[0] && p.z >= self.min[1] && p.z <= self.max[1]
    }
}

const LIVING: Room = Room {
    name: "living",
    min: [-5.5, -4.0],
    max: [0.5, 0.2],
};
const KITCHEN: Room = Room {
    name: "kitchen",
    min: [0.5, -4.0],
    max: [5.5, 0.2],
};
const DINING: Room = Room {
    name: "dining",
    min: [3.0, 0.2],
    max: [5.5, 4.0],
};
const HALL: Room = Room {
    name: "hall",
    min: [-1.0, 0.2],
    max: [0.4, 4.0],
};
const BEDROOM: Room = Room {
    name: "bedroom",
    min: [-5.5, 0.2],
    max: [-1.0, 4.0],
};
const BATH: Room = Room {
    name: "bathroom",
    min: [0.4, 0.2],
    max: [3.0, 4.0],
};

/// All rooms. Together their rectangles tile the interior.
pub const ROOMS: [Room; 6] = [LIVING, KITCHEN, DINING, HALL, BEDROOM, BATH];

/// A passage between two rooms.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Door {
    /// Names of the two rooms it connects.
    pub rooms: [&'static str; 2],
    /// Center of the opening at floor level.
    pub center: Vec3,
    /// Clear width of the opening in meters.
    pub width: f32,
    /// Unit vector across the opening (the direction you walk through it).
    pub through: Vec3,
    /// Height of the opening; [`CEILING_HEIGHT`] when there is no lintel.
    pub height: f32,
}

/// All passages between rooms: three doors and two wide open-plan openings.
pub const DOORS: [Door; 5] = [
    Door {
        rooms: ["hall", "living"],
        center: Vec3::new(-0.3, 0.0, 0.2),
        width: 0.94,
        through: Vec3::Z,
        height: DOOR_HEIGHT,
    },
    Door {
        rooms: ["hall", "bedroom"],
        center: Vec3::new(-1.0, 0.0, 2.0),
        width: 0.94,
        through: Vec3::X,
        height: DOOR_HEIGHT,
    },
    Door {
        rooms: ["hall", "bathroom"],
        center: Vec3::new(0.4, 0.0, 3.0),
        width: 0.94,
        through: Vec3::X,
        height: DOOR_HEIGHT,
    },
    Door {
        rooms: ["living", "kitchen"],
        center: Vec3::new(0.5, 0.0, -1.3),
        width: 2.84,
        through: Vec3::X,
        height: CEILING_HEIGHT,
    },
    Door {
        rooms: ["kitchen", "dining"],
        center: Vec3::new(4.25, 0.0, 0.2),
        width: 2.3,
        through: Vec3::Z,
        height: CEILING_HEIGHT,
    },
];

/// Control points (at [`EYE_HEIGHT`]) of the default walk-through.
///
/// The tour starts at the front door in the hallway, visits the bedroom and
/// the bathroom, walks up the hallway into the living room, circles the
/// coffee table, crosses the kitchen behind the island and ends in the
/// dining nook. Feed them to a smooth spline (see `rig::ScanPath`).
pub fn tour_waypoints() -> Vec<Vec3> {
    const XZ: &[[f32; 2]] = &[
        // Hallway, from the front door.
        [-0.30, 3.45],
        [-0.30, 2.85],
        [-0.52, 2.25],
        // Bedroom: in through the door, a loop between bed and wardrobe, out.
        [-1.00, 2.00],
        [-1.70, 1.78],
        [-2.35, 1.36],
        [-2.92, 1.78],
        [-2.80, 2.38],
        [-2.25, 2.60],
        [-1.68, 2.28],
        [-1.00, 2.00],
        // Across the hallway into the bathroom.
        [-0.42, 2.18],
        [-0.20, 2.72],
        [0.00, 2.98],
        [0.40, 3.00],
        [0.95, 2.92],
        [1.60, 2.60],
        [1.85, 2.00],
        [1.60, 1.42],
        [1.05, 1.38],
        [0.85, 1.95],
        [0.90, 2.50],
        [0.72, 2.94],
        [0.40, 3.00],
        [0.00, 2.92],
        // Up the hallway into the living room.
        [-0.30, 2.30],
        [-0.36, 1.40],
        [-0.30, 0.65],
        [-0.30, 0.20],
        [-0.40, -0.50],
        // Around the coffee table.
        [-1.30, -1.12],
        [-2.40, -1.18],
        [-3.40, -1.18],
        [-4.08, -1.50],
        [-4.08, -2.45],
        [-3.45, -2.92],
        [-2.30, -2.92],
        [-1.15, -2.55],
        // Kitchen: between island and counter, then down the east side.
        [0.30, -2.10],
        [1.60, -2.55],
        [3.00, -2.66],
        [4.10, -2.55],
        [4.38, -1.70],
        [4.38, -0.70],
        // Into the dining nook.
        [4.30, 0.20],
        [4.22, 0.80],
        [4.25, 1.30],
    ];
    XZ.iter().map(|p| Vec3::new(p[0], EYE_HEIGHT, p[1])).collect()
}

/// Build options of the house.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HouseOptions {
    /// Include the ceiling, its beams and light fixtures. Turn off for
    /// floor-plan style renders from above.
    pub ceiling: bool,
}

impl Default for HouseOptions {
    fn default() -> Self {
        Self { ceiling: true }
    }
}

/// The house scene. It is static: built once, every snapshot is the same.
#[derive(Clone, Debug)]
pub struct HouseScene {
    snapshot: SceneSnapshot,
}

impl Default for HouseScene {
    fn default() -> Self {
        Self::new()
    }
}

impl HouseScene {
    /// The complete house.
    pub fn new() -> Self {
        Self::with_options(HouseOptions::default())
    }

    /// The house with build options (for example without its ceiling).
    pub fn with_options(options: HouseOptions) -> Self {
        Self {
            snapshot: SceneSnapshot::new(build(options), lighting()),
        }
    }

    /// The rooms of the house.
    pub fn rooms(&self) -> &'static [Room] {
        &ROOMS
    }

    /// The passages between rooms.
    pub fn doors(&self) -> &'static [Door] {
        &DOORS
    }
}

impl Scene for HouseScene {
    fn name(&self) -> &'static str {
        "house"
    }

    fn snapshot(&self, _t: f32) -> SceneSnapshot {
        self.snapshot.clone()
    }

    fn snapshot_static(&self) -> SceneSnapshot {
        self.snapshot.clone()
    }

    fn bounds(&self) -> Aabb {
        Aabb::new(
            Vec3::new(-HALF_X - SHELL, -0.3, -HALF_Z - SHELL),
            Vec3::new(HALF_X + SHELL, CEILING_HEIGHT + 0.2, HALF_Z + SHELL),
        )
    }

    fn loop_duration(&self) -> Option<f32> {
        None
    }

    fn viewer_start(&self) -> (Vec3, Vec3) {
        (Vec3::new(-0.5, 1.6, -0.45), Vec3::new(-3.4, 0.8, -2.3))
    }
}

fn lighting() -> Lighting {
    let warm = Vec3::new(0.80, 0.76, 0.68);
    let lamp =
        |room: &Room, x: f32, y: f32, z: f32| Light::point(Vec3::new(x, y, z), 3.0, warm).with_region(room.aabb());
    Lighting {
        ambient_sky: Vec3::new(0.40, 0.40, 0.41),
        // Strong bounce from the floor keeps ceilings and undersides readable.
        ambient_ground: Vec3::new(0.50, 0.49, 0.47),
        lights: vec![
            lamp(&LIVING, -2.6, 2.25, -1.9),
            lamp(&KITCHEN, 3.0, 2.25, -1.5),
            // The dining light is the pendant itself, just under its shade.
            lamp(&DINING, 4.25, 1.56, 2.4),
            lamp(&HALL, -0.3, 2.25, 2.1),
            lamp(&BEDROOM, -2.6, 2.25, 2.0),
            lamp(&BATH, 1.5, 2.25, 2.2),
        ],
        sky_zenith: Vec3::new(0.25, 0.45, 0.85),
        sky_horizon: Vec3::new(0.75, 0.82, 0.9),
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Collects primitives; furniture is authored in a local frame whose origin
/// sits on the floor and whose +Z is the piece's front.
struct Builder {
    prims: Vec<Primitive>,
    frame: Affine3A,
    rng: u32,
}

impl Builder {
    fn new() -> Self {
        Self {
            prims: Vec::with_capacity(640),
            frame: Affine3A::IDENTITY,
            rng: 0x1234_5678,
        }
    }

    /// Places the local frame at floor point `(x, z)`, rotated by `yaw_deg`
    /// about Y (0 faces +Z, 90 faces +X, 180 faces -Z, -90 faces -X).
    fn at(&mut self, x: f32, z: f32, yaw_deg: f32) {
        self.frame =
            Affine3A::from_rotation_translation(Quat::from_rotation_y(yaw_deg.to_radians()), Vec3::new(x, 0.0, z));
    }

    /// Back to world coordinates.
    fn world(&mut self) {
        self.frame = Affine3A::IDENTITY;
    }

    /// Deterministic pseudo-random number in `[0, 1)`.
    fn rand(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng >> 8) as f32 / (1u32 << 24) as f32
    }

    fn push(&mut self, shape: Shape, local: Affine3A, material: Material) {
        self.prims.push(Primitive::new(shape, self.frame * local, material));
    }

    /// Box from two opposite corners.
    fn bx(&mut self, a: [f32; 3], b: [f32; 3], material: Material) {
        let (a, b) = (Vec3::from(a), Vec3::from(b));
        let (min, max) = (a.min(b), a.max(b));
        let local = Affine3A::from_scale_rotation_translation(0.5 * (max - min), Quat::IDENTITY, 0.5 * (min + max));
        self.push(Shape::Box, local, material);
    }

    /// Box rotated about its own center.
    fn bx_rot(&mut self, center: [f32; 3], half: [f32; 3], rot: Quat, material: Material) {
        self.push(
            Shape::Box,
            Affine3A::from_scale_rotation_translation(half.into(), rot, center.into()),
            material,
        );
    }

    /// Upright cylinder standing on `base`.
    fn cyl(&mut self, base: [f32; 3], radius: f32, height: f32, material: Material) {
        let center = Vec3::from(base) + Vec3::Y * (0.5 * height);
        let local =
            Affine3A::from_scale_rotation_translation(Vec3::new(radius, 0.5 * height, radius), Quat::IDENTITY, center);
        self.push(Shape::Cylinder, local, material);
    }

    /// Cylinder between two points.
    fn rod(&mut self, a: [f32; 3], b: [f32; 3], radius: f32, material: Material) {
        let (a, b) = (Vec3::from(a), Vec3::from(b));
        let axis = b - a;
        let len = axis.length();
        let rot = Quat::from_rotation_arc(Vec3::Y, axis / len);
        let local = Affine3A::from_scale_rotation_translation(Vec3::new(radius, 0.5 * len, radius), rot, 0.5 * (a + b));
        self.push(Shape::Cylinder, local, material);
    }

    /// Upright cone frustum standing on `base`.
    fn cone(&mut self, base: [f32; 3], r_bottom: f32, r_top: f32, height: f32, material: Material) {
        let r = r_bottom.max(r_top);
        let center = Vec3::from(base) + Vec3::Y * (0.5 * height);
        let local = Affine3A::from_scale_rotation_translation(Vec3::new(r, 0.5 * height, r), Quat::IDENTITY, center);
        self.push(
            Shape::Cone {
                r0: r_bottom / r,
                r1: r_top / r,
            },
            local,
            material,
        );
    }

    /// Ellipsoid.
    fn ell(&mut self, center: [f32; 3], radii: [f32; 3], material: Material) {
        self.ell_rot(center, radii, Quat::IDENTITY, material);
    }

    fn ell_rot(&mut self, center: [f32; 3], radii: [f32; 3], rot: Quat, material: Material) {
        self.push(
            Shape::Sphere,
            Affine3A::from_scale_rotation_translation(radii.into(), rot, center.into()),
            material,
        );
    }
}

fn solid(hex: u32) -> Material {
    Material::solid(srgb(hex))
}

/// Which wall of a room rectangle.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    /// `z = min`.
    North,
    /// `z = max`.
    South,
    /// `x = min`.
    West,
    /// `x = max`.
    East,
}

/// A gap in a wall, between two coordinates along the wall.
struct Opening {
    side: Side,
    from: f32,
    to: f32,
    /// Underside of the lintel; `CEILING_HEIGHT` for a full-height opening.
    top: f32,
}

const fn door(side: Side, from: f32, to: f32) -> Opening {
    Opening {
        side,
        from,
        to,
        top: DOOR_HEIGHT,
    }
}

const fn open(side: Side, from: f32, to: f32) -> Opening {
    Opening {
        side,
        from,
        to,
        top: CEILING_HEIGHT,
    }
}

/// Floor, wall skins (with openings), lintels and baseboards of one room.
fn room_shell(b: &mut Builder, room: &Room, wall: Material, floor: Material, openings: &[Opening]) {
    let [x0, z0] = room.min;
    let [x1, z1] = room.max;
    b.world();
    b.bx([x0, -0.1, z0], [x1, 0.0, z1], floor);
    let baseboard = solid(0xf4f1ea);
    for side in [Side::North, Side::South, Side::West, Side::East] {
        let (lo, hi) = if matches!(side, Side::North | Side::South) {
            (x0, x1)
        } else {
            (z0, z1)
        };
        // Box along the wall between `s0` and `s1`, from `inner` to `outer`
        // distance off the room edge.
        let slab = |b: &mut Builder, s0: f32, s1: f32, depth: f32, y0: f32, y1: f32, m: Material| match side {
            Side::North => b.bx([s0, y0, z0], [s1, y1, z0 + depth], m),
            Side::South => b.bx([s0, y0, z1 - depth], [s1, y1, z1], m),
            Side::West => b.bx([x0, y0, s0], [x0 + depth, y1, s1], m),
            Side::East => b.bx([x1 - depth, y0, s0], [x1, y1, s1], m),
        };
        let mut gaps: Vec<&Opening> = openings.iter().filter(|o| o.side == side).collect();
        gaps.sort_by(|a, b| a.from.total_cmp(&b.from));
        let mut cursor = lo;
        for gap in gaps.iter().copied().chain(core::iter::once(&Opening {
            side,
            from: hi,
            to: hi,
            top: CEILING_HEIGHT,
        })) {
            if gap.from > cursor + 1e-4 {
                slab(b, cursor, gap.from, SKIN, 0.0, CEILING_HEIGHT, wall);
                slab(b, cursor, gap.from, SKIN + 0.012, 0.0, 0.09, baseboard);
            }
            if gap.top < CEILING_HEIGHT {
                slab(b, gap.from, gap.to, SKIN, gap.top, CEILING_HEIGHT, wall);
            }
            cursor = cursor.max(gap.to);
        }
    }
}

/// Lining of a door opening through a wall centered on `wall` (an x or z
/// coordinate), between `from` and `to` along the wall.
fn door_lining(b: &mut Builder, along_x: bool, wall: f32, from: f32, to: f32) {
    let m = solid(0xf7f5f0);
    let (d, t) = (SKIN + 0.018, 0.03);
    let piece = |b: &mut Builder, s0: f32, s1: f32, y0: f32, y1: f32| {
        if along_x {
            b.bx([s0, y0, wall - d], [s1, y1, wall + d], m);
        } else {
            b.bx([wall - d, y0, s0], [wall + d, y1, s1], m);
        }
    };
    b.world();
    piece(b, from, from + t, 0.0, DOOR_HEIGHT);
    piece(b, to - t, to, 0.0, DOOR_HEIGHT);
    piece(b, from, to, DOOR_HEIGHT - t, DOOR_HEIGHT + 0.05);
}

/// Direction a wall-mounted panel faces.
#[derive(Clone, Copy)]
enum Facing {
    PosX,
    NegX,
    PosZ,
    NegZ,
}

impl Facing {
    /// Yaw that turns a local +Z front toward this direction.
    fn yaw_deg(self) -> f32 {
        match self {
            Facing::PosZ => 0.0,
            Facing::PosX => 90.0,
            Facing::NegZ => 180.0,
            Facing::NegX => -90.0,
        }
    }
}

/// Framed flat panel on a wall (window, picture, mirror, screen).
///
/// `center` lies on the wall surface; `width` runs along the wall.
fn panel(b: &mut Builder, facing: Facing, center: [f32; 3], width: f32, height: f32, frame: Material, inner: Material) {
    b.at(center[0], center[2], facing.yaw_deg());
    let (w, h, y) = (0.5 * width, 0.5 * height, center[1]);
    b.bx([-w, y - h, 0.0], [w, y + h, 0.03], frame);
    let rim = 0.045;
    b.bx([-w + rim, y - h + rim, 0.0], [w - rim, y + h - rim, 0.038], inner);
    b.world();
}

/// Window: bright emissive pane with a white frame and a cross of mullions.
fn window(b: &mut Builder, facing: Facing, center: [f32; 3], width: f32, height: f32) {
    let white = solid(0xfbfaf6);
    // Sky gradient, brighter toward the top.
    let pane =
        Material::gradient(srgb(0xd8ecff), srgb(0x9ccbff), Axis::Y, -0.5 * height, 0.5 * height).with_emissive(1.4);
    panel(b, facing, center, width, height, white, pane);
    b.at(center[0], center[2], facing.yaw_deg());
    let (w, h, y) = (0.5 * width, 0.5 * height, center[1]);
    b.bx([-0.02, y - h, 0.0], [0.02, y + h, 0.046], white);
    b.bx([-w, y - 0.02, 0.0], [w, y + 0.02, 0.046], white);
    // Sill.
    b.bx([-w - 0.04, y - h - 0.04, 0.0], [w + 0.04, y - h, 0.1], white);
    b.world();
}

// ---------------------------------------------------------------------------
// Furniture (local frame: origin on the floor, +Z is the front)
// ---------------------------------------------------------------------------

fn sofa(b: &mut Builder) {
    let fabric = Material::cells(srgb(0x2f6f86), srgb(0x3b819a), Vec3::splat(0.012));
    let cushion = Material::cells(srgb(0x4b93ab), srgb(0x58a2ba), Vec3::splat(0.012));
    let wood = solid(0x4a3524);
    b.bx([-1.1, 0.12, -0.475], [1.1, 0.40, 0.475], fabric);
    b.bx([-1.1, 0.40, -0.475], [1.1, 0.88, -0.24], fabric);
    for s in [-1.0f32, 1.0] {
        b.bx([s * 1.1, 0.40, -0.24], [s * 0.88, 0.64, 0.475], fabric);
        b.bx([s * 0.86, 0.40, -0.24], [s * 0.01, 0.51, 0.46], cushion);
        b.bx([s * 0.86, 0.51, -0.26], [s * 0.01, 0.84, -0.12], cushion);
        for z in [-0.42f32, 0.42] {
            b.cyl([s * 1.02, 0.0, z], 0.03, 0.12, wood);
        }
    }
    // Throw pillows.
    b.ell_rot(
        [-0.62, 0.66, -0.04],
        [0.2, 0.2, 0.075],
        Quat::from_rotation_x(-0.35),
        Material::stripes(srgb(0xe9b44c), srgb(0xf6e7c1), Axis::X, 0.05),
    );
    b.ell_rot(
        [0.5, 0.66, -0.04],
        [0.19, 0.19, 0.075],
        Quat::from_rotation_x(-0.35) * Quat::from_rotation_z(0.5),
        Material::dots(srgb(0xc8553d), srgb(0xf6e7c1), 0.08, 0.025),
    );
}

fn armchair(b: &mut Builder) {
    let fabric = Material::cells(srgb(0xd29a2e), srgb(0xe0aa40), Vec3::splat(0.012));
    let cushion = solid(0xe8bb5a);
    let wood = solid(0x4a3524);
    b.bx([-0.42, 0.14, -0.42], [0.42, 0.40, 0.42], fabric);
    b.bx([-0.42, 0.40, -0.42], [0.42, 0.92, -0.24], fabric);
    for s in [-1.0f32, 1.0] {
        b.bx([s * 0.42, 0.40, -0.24], [s * 0.28, 0.62, 0.42], fabric);
        for z in [-0.36f32, 0.36] {
            b.cyl([s * 0.36, 0.0, z], 0.028, 0.14, wood);
        }
    }
    b.bx([-0.27, 0.40, -0.24], [0.27, 0.50, 0.41], cushion);
    b.ell_rot(
        [0.0, 0.64, -0.17],
        [0.2, 0.17, 0.07],
        Quat::from_rotation_x(-0.3),
        Material::checker(srgb(0x2f6f86), srgb(0xf1ead8), 0.06),
    );
}

fn coffee_table(b: &mut Builder) {
    let wood = Material::cells(srgb(0x8a5a33), srgb(0x9c6a3f), Vec3::new(0.09, f32::INFINITY, 2.0));
    let dark = solid(0x2b2623);
    b.bx([-0.6, 0.38, -0.35], [0.6, 0.42, 0.35], wood);
    b.bx([-0.52, 0.14, -0.27], [0.52, 0.165, 0.27], wood);
    for (x, z) in [(-0.55f32, -0.3f32), (0.55, -0.3), (-0.55, 0.3), (0.55, 0.3)] {
        b.bx([x - 0.022, 0.0, z - 0.022], [x + 0.022, 0.38, z + 0.022], dark);
    }
    // A stack of books and a small vase with a sprig.
    b.bx_rot(
        [-0.3, 0.435, 0.05],
        [0.13, 0.015, 0.09],
        Quat::from_rotation_y(0.3),
        solid(0xb5443a),
    );
    b.bx_rot(
        [-0.3, 0.462, 0.05],
        [0.11, 0.012, 0.08],
        Quat::from_rotation_y(-0.15),
        solid(0x3d6fa5),
    );
    b.cone(
        [0.3, 0.42, -0.05],
        0.045,
        0.07,
        0.16,
        Material::stripes(srgb(0xf1ead8), srgb(0x3e7c6a), Axis::Y, 0.02),
    );
    b.ell([0.3, 0.66, -0.05], [0.07, 0.1, 0.07], solid(0x4f9a4b));
}

fn tv_unit(b: &mut Builder) {
    let walnut = Material::cells(srgb(0x5d3f2a), srgb(0x6b4a32), Vec3::new(0.5, 0.08, f32::INFINITY));
    b.bx([-1.0, 0.08, -0.22], [1.0, 0.5, 0.22], walnut);
    b.bx([-0.96, 0.0, -0.18], [0.96, 0.08, 0.18], solid(0x2a2522));
    for i in 0..3 {
        let x0 = -0.97 + i as f32 * 0.65;
        b.bx([x0 + 0.015, 0.11, 0.22], [x0 + 0.625, 0.47, 0.232], solid(0xe9e2d2));
        b.bx([x0 + 0.27, 0.3, 0.232], [x0 + 0.37, 0.315, 0.25], solid(0x2a2522));
    }
    // Television on its foot.
    let black = solid(0x121214);
    b.bx([-0.25, 0.5, -0.1], [0.25, 0.52, 0.1], black);
    b.bx([-0.04, 0.52, -0.03], [0.04, 0.6, 0.01], black);
    b.bx([-0.78, 0.58, -0.04], [0.78, 1.48, 0.0], black);
    let picture = Material::gradient(srgb(0x24408f), srgb(0xe07a3f), Axis::X, -0.75, 0.75).with_emissive(0.8);
    b.bx([-0.75, 0.61, 0.0], [0.75, 1.45, 0.006], picture);
    b.bx(
        [-0.75, 0.61, 0.0],
        [0.75, 0.9, 0.008],
        Material::rings(srgb(0x1b2f6b), srgb(0x2f8f6b), Axis::Z, 0.11).with_emissive(0.6),
    );
    // Speaker and a decorative box.
    b.bx(
        [0.72, 0.5, -0.12],
        [0.94, 0.86, 0.1],
        Material::checker(srgb(0x33363c), srgb(0x24262b), 0.02),
    );
    b.cyl(
        [-0.84, 0.5, 0.0],
        0.07,
        0.2,
        Material::stripes(srgb(0xd9772b), srgb(0xf1ead8), Axis::Y, 0.04),
    );
}

fn bookshelf(b: &mut Builder) {
    let wood = solid(0xe8e1d1);
    let (w, d, h) = (0.7, 0.16, 2.0);
    b.bx([-w, 0.0, -d], [-w + 0.03, h, d], wood);
    b.bx([w - 0.03, 0.0, -d], [w, h, d], wood);
    b.bx([-w, 0.0, -d], [w, h, -d + 0.015], solid(0xcfc6b2));
    let palette = [
        0xb5443a, 0x3d6fa5, 0xe2b13c, 0x3e7c6a, 0x7b4f8e, 0xd9772b, 0x2f3a4f, 0xf1ead8, 0x8c2f39, 0x4f9a4b,
    ];
    let shelves = 5;
    let pitch = (h - 0.08) / shelves as f32;
    for s in 0..=shelves {
        let y = 0.05 + s as f32 * pitch;
        b.bx([-w + 0.03, y - 0.03, -d + 0.015], [w - 0.03, y, d], wood);
        if s == shelves {
            break;
        }
        // A row of books, then sometimes a gap with an ornament.
        let mut x = -w + 0.045;
        let end = if s % 2 == 0 { w - 0.05 } else { w - 0.36 };
        while x < end - 0.03 {
            let thick = 0.028 + 0.04 * b.rand();
            let tall = pitch * (0.52 + 0.33 * b.rand());
            let deep = 0.19 + 0.06 * b.rand();
            let color = palette[(b.rand() * palette.len() as f32) as usize % palette.len()];
            let x1 = (x + thick).min(end);
            b.bx([x, y, -d + 0.02], [x1, y + tall, -d + 0.02 + deep], solid(color));
            x = x1 + 0.004;
        }
        if s % 2 == 1 {
            let cx = w - 0.19;
            match s {
                1 => b.ell(
                    [cx, y + 0.1, 0.0],
                    [0.09, 0.1, 0.09],
                    Material::stripes(srgb(0x3d6fa5), srgb(0xf1ead8), Axis::Y, 0.03),
                ),
                _ => b.cone([cx, y, 0.0], 0.08, 0.03, 0.2, solid(0xd9772b)),
            }
        }
    }
}

fn floor_lamp(b: &mut Builder) {
    let metal = solid(0x2b2b2e);
    b.cyl([0.0, 0.0, 0.0], 0.16, 0.025, metal);
    b.cyl([0.0, 0.0, 0.0], 0.015, 1.45, metal);
    b.cone(
        [0.0, 1.42, 0.0],
        0.2,
        0.12,
        0.3,
        Material::solid(srgb(0xffe2a8)).with_emissive(0.9),
    );
}

fn plant(b: &mut Builder, scale: f32) {
    b.cone(
        [0.0, 0.0, 0.0],
        0.13 * scale,
        0.19 * scale,
        0.36 * scale,
        Material::stripes(srgb(0xc2643c), srgb(0xb0552f), Axis::Y, 0.06),
    );
    b.cyl([0.0, 0.33 * scale, 0.0], 0.175 * scale, 0.03 * scale, solid(0x3a2a1e));
    b.cyl([0.0, 0.36 * scale, 0.0], 0.018 * scale, 0.5 * scale, solid(0x5b4630));
    let leaf = Material::stripes(srgb(0x3f8f4a), srgb(0x2f7a3d), Axis::X, 0.03);
    for i in 0..9 {
        let az = i as f32 * TAU / 9.0 + 0.3 * (i % 3) as f32;
        let tilt = 0.45 + 0.28 * (i % 3) as f32;
        let rot = Quat::from_rotation_y(az) * Quat::from_rotation_z(-tilt);
        let base = Vec3::new(0.0, (0.62 + 0.09 * (i % 4) as f32) * scale, 0.0);
        let center = base + rot * Vec3::new(0.0, 0.26 * scale, 0.0);
        b.ell_rot(center.into(), [0.085 * scale, 0.3 * scale, 0.02 * scale], rot, leaf);
    }
}

fn chair(b: &mut Builder, seat: Material) {
    let wood = solid(0x6b4a32);
    b.bx([-0.21, 0.43, -0.21], [0.21, 0.47, 0.21], seat);
    b.bx([-0.21, 0.47, -0.21], [0.21, 0.92, -0.175], wood);
    for (x, z) in [(-0.19f32, -0.19f32), (0.19, -0.19), (-0.19, 0.19), (0.19, 0.19)] {
        b.bx([x - 0.018, 0.0, z - 0.018], [x + 0.018, 0.43, z + 0.018], wood);
    }
}

fn stool(b: &mut Builder) {
    let metal = solid(0x2b2b2e);
    b.cyl([0.0, 0.0, 0.0], 0.17, 0.02, metal);
    b.cyl([0.0, 0.0, 0.0], 0.022, 0.64, metal);
    b.cyl([0.0, 0.25, 0.0], 0.13, 0.015, metal);
    b.cyl([0.0, 0.64, 0.0], 0.17, 0.05, solid(0xc8553d));
}

fn bed(b: &mut Builder) {
    let wood = solid(0x6b4a32);
    b.bx([-0.82, 0.0, -1.05], [0.82, 0.28, 1.05], wood);
    b.bx([-0.78, 0.28, -1.0], [0.78, 0.5, 1.03], solid(0xf4f1ea));
    // Duvet over the lower two thirds, then a folded throw at the foot.
    b.bx(
        [-0.81, 0.3, -0.3],
        [0.81, 0.56, 1.045],
        Material::stripes(srgb(0x4a6fa5), srgb(0xdfe7f2), Axis::Z, 0.12),
    );
    b.bx(
        [-0.815, 0.3, 0.52],
        [0.815, 0.585, 0.95],
        Material::checker(srgb(0xd9772b), srgb(0xe9b44c), 0.1),
    );
    b.ell([-0.38, 0.58, -0.7], [0.33, 0.085, 0.2], solid(0xfbfaf6));
    b.ell([0.38, 0.58, -0.7], [0.33, 0.085, 0.2], solid(0xc9d8ec));
    // Upholstered headboard.
    b.bx(
        [-0.88, 0.0, -1.13],
        [0.88, 1.12, -1.05],
        Material::checker(srgb(0x5b5f73), srgb(0x666b80), 0.22),
    );
}

fn nightstand(b: &mut Builder) {
    b.bx([-0.22, 0.0, -0.2], [0.22, 0.5, 0.2], solid(0xe8e1d1));
    b.bx([-0.19, 0.27, 0.2], [0.19, 0.46, 0.212], solid(0xcfc6b2));
    b.bx([-0.19, 0.05, 0.2], [0.19, 0.24, 0.212], solid(0xcfc6b2));
    b.ell([0.0, 0.365, 0.22], [0.015, 0.015, 0.015], solid(0x2b2b2e));
    b.ell([0.0, 0.145, 0.22], [0.015, 0.015, 0.015], solid(0x2b2b2e));
    // Bedside lamp.
    b.cyl([0.0, 0.5, -0.02], 0.06, 0.02, solid(0x2b2b2e));
    b.cyl([0.0, 0.5, -0.02], 0.012, 0.22, solid(0x2b2b2e));
    b.cone(
        [0.0, 0.69, -0.02],
        0.11,
        0.07,
        0.16,
        Material::solid(srgb(0xffd9a0)).with_emissive(0.9),
    );
}

fn wardrobe(b: &mut Builder) {
    b.bx([-0.95, 0.0, -0.3], [0.95, 2.1, 0.29], solid(0xd9cfba));
    for i in 0..3 {
        let x0 = -0.94 + i as f32 * 0.6267;
        let tone = if i == 1 { 0xa9825a } else { 0xf2ede2 };
        b.bx([x0 + 0.008, 0.06, 0.29], [x0 + 0.6187, 2.07, 0.305], solid(tone));
        let hx = if i == 0 { x0 + 0.56 } else { x0 + 0.06 };
        b.bx([hx - 0.01, 0.95, 0.305], [hx + 0.01, 1.2, 0.33], solid(0x2b2b2e));
    }
}

fn desk(b: &mut Builder) {
    let wood = Material::cells(srgb(0xc9a16b), srgb(0xd4ae7a), Vec3::new(0.11, f32::INFINITY, 2.0));
    b.bx([-0.65, 0.71, -0.3], [0.65, 0.74, 0.3], wood);
    b.bx([-0.65, 0.0, -0.3], [-0.62, 0.71, 0.3], wood);
    b.bx([0.22, 0.0, -0.3], [0.65, 0.71, 0.28], solid(0xf2ede2));
    for i in 0..3 {
        let y = 0.06 + i as f32 * 0.215;
        b.bx([0.24, y, 0.28], [0.63, y + 0.195, 0.292], solid(0xd9cfba));
        b.bx([0.38, y + 0.09, 0.292], [0.49, y + 0.105, 0.31], solid(0x2b2b2e));
    }
    // Laptop: base and a slightly reclined glowing screen.
    b.bx([-0.36, 0.74, -0.12], [-0.02, 0.755, 0.12], solid(0x9a9da3));
    let tilt = Quat::from_rotation_x(-0.25);
    b.bx_rot([-0.19, 0.865, -0.15], [0.17, 0.115, 0.006], tilt, solid(0x9a9da3));
    b.bx_rot(
        [-0.19, 0.866, -0.143],
        [0.155, 0.1, 0.002],
        tilt,
        Material::tiles(srgb(0x8fd0ff), srgb(0xf2f7ff), 0.05).with_emissive(0.7),
    );
    // Desk lamp, mug and books.
    b.cyl([0.48, 0.74, -0.16], 0.07, 0.015, solid(0x2b2b2e));
    b.rod([0.48, 0.75, -0.16], [0.4, 1.12, -0.12], 0.01, solid(0x2b2b2e));
    b.cone(
        [0.34, 1.02, -0.09],
        0.085,
        0.03,
        0.11,
        Material::solid(srgb(0xffd9a0)).with_emissive(0.7),
    );
    b.cyl([0.12, 0.74, 0.1], 0.04, 0.09, solid(0xc8553d));
    b.bx([-0.6, 0.74, -0.26], [-0.44, 0.78, -0.02], solid(0x3e7c6a));
    b.bx([-0.59, 0.78, -0.25], [-0.45, 0.81, -0.04], solid(0xe2b13c));
}

// ---------------------------------------------------------------------------
// The house
// ---------------------------------------------------------------------------

fn build(options: HouseOptions) -> Vec<Primitive> {
    let mut b = Builder::new();
    let b = &mut b;

    // --- Envelope: slab, outer shell, ceiling -----------------------------
    let (ox, oz) = (HALF_X + SHELL, HALF_Z + SHELL);
    let concrete = solid(0x9a9a98);
    b.bx([-ox, -0.3, -oz], [ox, -0.1, oz], concrete);
    b.bx([-ox, -0.1, -oz], [-HALF_X, CEILING_HEIGHT, oz], concrete);
    b.bx([HALF_X, -0.1, -oz], [ox, CEILING_HEIGHT, oz], concrete);
    b.bx([-HALF_X, -0.1, -oz], [HALF_X, CEILING_HEIGHT, -HALF_Z], concrete);
    b.bx([-HALF_X, -0.1, HALF_Z], [HALF_X, CEILING_HEIGHT, oz], concrete);
    if options.ceiling {
        b.bx(
            [-ox, CEILING_HEIGHT, -oz],
            [ox, CEILING_HEIGHT + 0.2, oz],
            solid(0xf6f4ee),
        );
        // Exposed beams over the open-plan living room and kitchen.
        let beam = Material::cells(
            srgb(0x8a6a48),
            srgb(0x9a7850),
            Vec3::new(f32::INFINITY, f32::INFINITY, 1.4),
        );
        for i in 0..7 {
            let x = -4.6 + i as f32 * 1.52;
            b.bx(
                [x - 0.06, CEILING_HEIGHT - 0.13, -HALF_Z + SKIN],
                [x + 0.06, CEILING_HEIGHT, 0.2 - SKIN],
                beam,
            );
        }
        // Ceiling light fixtures, one per room.
        let glow = Material::solid(srgb(0xfff3d6)).with_emissive(1.5);
        for (x, z, r) in [
            (-2.6, -1.9, 0.26),
            (3.0, -1.5, 0.2),
            (-0.3, 2.1, 0.18),
            (-2.6, 2.0, 0.24),
            (1.5, 2.2, 0.18),
        ] {
            b.cyl([x, CEILING_HEIGHT - 0.05, z], r, 0.05, glow);
            b.cyl([x, CEILING_HEIGHT - 0.06, z], r + 0.02, 0.02, solid(0xd8d2c4));
        }
    }

    // --- Rooms: floors and walls ------------------------------------------
    let oak = Material::cells(srgb(0xb98a57), srgb(0xd0a26c), Vec3::new(0.14, f32::INFINITY, 1.3));
    let kitchen_tiles = Material::tiles(srgb(0xe6e0d4), srgb(0xb9b2a4), 0.45);
    let hall_wood = Material::cells(srgb(0x7d5a3c), srgb(0x94704c), Vec3::new(1.1, f32::INFINITY, 0.12));
    let carpet = Material::cells(srgb(0x8f9bb3), srgb(0xa3aec4), Vec3::new(0.035, f32::INFINITY, 0.035));
    let bath_tiles = Material::tiles(srgb(0xf2f5f4), srgb(0x3f9c9a), 0.2);

    use Side::{East, North, South, West};
    room_shell(
        b,
        &LIVING,
        solid(0xeee2c8),
        oak,
        &[open(East, -2.8, 0.2), door(South, -0.8, 0.2)],
    );
    room_shell(
        b,
        &KITCHEN,
        solid(0xd3e3cf),
        kitchen_tiles,
        &[open(West, -2.8, 0.2), open(South, 3.0, 5.5)],
    );
    room_shell(b, &DINING, solid(0xecd0b8), kitchen_tiles, &[open(North, 3.0, 5.5)]);
    room_shell(
        b,
        &HALL,
        solid(0xd5dde6),
        hall_wood,
        &[door(North, -0.8, 0.2), door(West, 1.5, 2.5), door(East, 2.5, 3.5)],
    );
    room_shell(b, &BEDROOM, solid(0xc9d0ea), carpet, &[door(East, 1.5, 2.5)]);
    let bath_wall = Material::checker(srgb(0xe9f3f2), srgb(0xcfe6e4), 0.15);
    room_shell(b, &BATH, bath_wall, bath_tiles, &[door(West, 2.5, 3.5)]);

    door_lining(b, true, 0.2, -0.8, 0.2);
    door_lining(b, false, -1.0, 1.5, 2.5);
    door_lining(b, false, 0.4, 2.5, 3.5);
    // Square posts finishing the two free wall ends of the open plan.
    let post = solid(0xf7f5f0);
    b.bx([0.42, 0.0, -2.92], [0.58, CEILING_HEIGHT, -2.76], post);
    b.bx([2.92, 0.0, 0.12], [3.08, CEILING_HEIGHT, 0.28], post);

    // Interior wall surfaces.
    let (wn, ws, ww, we) = (-HALF_Z + SKIN, HALF_Z - SKIN, -HALF_X + SKIN, HALF_X - SKIN);
    let frame_dark = solid(0x2b2623);
    let frame_gold = solid(0xb8924a);
    let frame_white = solid(0xfbfaf6);

    // --- Living room --------------------------------------------------------
    b.world();
    // Rug with concentric rings under the seating group.
    b.bx(
        [-4.4, 0.0, -2.95],
        [-1.8, 0.012, -0.9],
        Material::rings(srgb(0xb5443a), srgb(0xe9d9b8), Axis::Y, 0.22),
    );
    b.at(-3.1, -0.36, 180.0);
    sofa(b);
    b.at(-3.1, -1.9, 0.0);
    coffee_table(b);
    b.at(-3.1, wn + 0.22, 0.0);
    tv_unit(b);
    b.at(-4.85, -1.9, 90.0);
    armchair(b);
    b.at(-0.95, wn + 0.16, 0.0);
    bookshelf(b);
    b.at(-5.05, -0.28, 0.0);
    floor_lamp(b);
    b.at(-5.0, -3.5, 0.0);
    plant(b, 1.0);
    window(b, Facing::PosX, [ww, 1.5, -1.9], 2.2, 1.2);
    window(b, Facing::PosZ, [-4.95, 1.5, wn], 0.7, 1.2);
    // Pictures: two above the sofa, one beside the hallway door, one on the stub wall.
    panel(
        b,
        Facing::NegZ,
        [-3.65, 1.55, 0.2 - SKIN],
        0.9,
        0.65,
        frame_dark,
        Material::stripes(srgb(0xe07a3f), srgb(0x24408f), Axis::X, 0.11),
    );
    panel(
        b,
        Facing::NegZ,
        [-2.55, 1.55, 0.2 - SKIN],
        0.9,
        0.65,
        frame_dark,
        Material::rings(srgb(0xf1ead8), srgb(0x3e7c6a), Axis::Z, 0.07),
    );
    panel(
        b,
        Facing::NegZ,
        [-1.4, 1.5, 0.2 - SKIN],
        0.6,
        0.8,
        frame_gold,
        Material::checker(srgb(0xe2b13c), srgb(0x8c2f39), 0.12),
    );
    panel(
        b,
        Facing::NegX,
        [0.5 - SKIN, 1.5, -3.4],
        0.7,
        0.9,
        frame_white,
        Material::dots(srgb(0x2f3a4f), srgb(0xf2d34b), 0.14, 0.045),
    );

    // --- Kitchen ------------------------------------------------------------
    b.world();
    let cabinet = solid(0xf2ede2);
    let cabinet_wood = Material::cells(
        srgb(0xa9825a),
        srgb(0xb89066),
        Vec3::new(0.6, f32::INFINITY, f32::INFINITY),
    );
    let stone = Material::cells(srgb(0x35383d), srgb(0x44484e), Vec3::splat(0.02));
    let steel = solid(0xb9bcc2);
    let black = solid(0x17181a);
    // North run of base cabinets with a sink under the window.
    b.bx([1.4, 0.1, wn], [we, 0.88, wn + 0.58], cabinet_wood);
    b.bx([1.42, 0.0, wn], [we, 0.1, wn + 0.52], black);
    b.bx([1.38, 0.88, wn], [we, 0.92, wn + 0.62], stone);
    for i in 0..6 {
        let x = 1.5 + i as f32 * 0.6;
        b.bx([x + 0.45, 0.72, wn + 0.58], [x + 0.47, 0.84, wn + 0.6], steel);
    }
    b.bx([2.45, 0.9, wn + 0.1], [3.15, 0.925, wn + 0.5], steel);
    b.bx([2.5, 0.905, wn + 0.14], [3.1, 0.93, wn + 0.46], solid(0x6d7178));
    b.rod([2.8, 0.92, wn + 0.07], [2.8, 1.18, wn + 0.07], 0.014, steel);
    b.rod([2.8, 1.18, wn + 0.07], [2.8, 1.16, wn + 0.24], 0.012, steel);
    // Upper cabinets either side of the window.
    b.bx([3.85, 1.4, wn], [we, 2.15, wn + 0.34], cabinet);
    b.bx([1.4, 1.4, wn], [1.95, 2.15, wn + 0.34], cabinet);
    for x in [4.38f32, 4.92] {
        b.bx(
            [x - 0.008, 1.42, wn + 0.34],
            [x + 0.008, 2.13, wn + 0.345],
            solid(0xb9b2a4),
        );
    }
    window(b, Facing::PosZ, [2.9, 1.52, wn], 1.5, 0.85);
    // Toaster, kettle and a row of jars on the counter.
    b.bx([4.0, 0.92, wn + 0.15], [4.3, 1.1, wn + 0.36], solid(0xc8553d));
    b.cyl([4.7, 0.92, wn + 0.28], 0.085, 0.2, steel);
    for (i, c) in [0x3e7c6a, 0xe2b13c, 0xb5443a].into_iter().enumerate() {
        b.cyl(
            [1.62 + 0.17 * i as f32, 0.92, wn + 0.2],
            0.055,
            0.16 + 0.03 * i as f32,
            solid(c),
        );
    }
    // Fridge against the stub wall.
    b.bx([0.62, 0.0, wn], [1.32, 1.95, wn + 0.68], steel);
    b.bx([0.63, 0.02, wn + 0.68], [1.31, 0.74, wn + 0.7], solid(0xc9ccd2));
    b.bx([0.63, 0.77, wn + 0.68], [1.31, 1.93, wn + 0.7], solid(0xc9ccd2));
    b.bx([0.68, 0.85, wn + 0.7], [0.71, 1.5, wn + 0.735], black);
    b.bx([0.68, 0.35, wn + 0.7], [0.71, 0.68, wn + 0.735], black);
    // East run with the cooktop, oven and hood.
    b.bx([we - 0.58, 0.1, wn + 0.58], [we, 0.88, -1.2], cabinet_wood);
    b.bx([we - 0.52, 0.0, wn + 0.58], [we, 0.1, -1.2], black);
    b.bx([we - 0.62, 0.88, wn + 0.58], [we, 0.92, -1.18], stone);
    b.bx([we - 0.55, 0.92, -2.75], [we - 0.05, 0.93, -1.95], black);
    for (dx, dz) in [(-0.42f32, -2.55f32), (-0.42, -2.15), (-0.18, -2.55), (-0.18, -2.15)] {
        b.cyl(
            [we + dx, 0.93, dz],
            0.085,
            0.006,
            Material::rings(srgb(0x3a3d44), srgb(0x8c2f39), Axis::Y, 0.028),
        );
    }
    b.bx([we - 0.6, 0.2, -2.72], [we - 0.58, 0.8, -1.98], black);
    b.bx([we - 0.62, 0.72, -2.68], [we - 0.6, 0.74, -2.02], steel);
    b.bx([we - 0.5, 1.6, -2.78], [we, 1.72, -1.92], steel);
    b.bx([we - 0.3, 1.72, -2.55], [we, CEILING_HEIGHT, -2.15], steel);
    panel(
        b,
        Facing::NegX,
        [we, 1.55, -0.55],
        0.75,
        0.75,
        frame_dark,
        Material::polar(srgb(0xf1ead8), srgb(0xd9772b), Axis::Z, 0.09, 8),
    );
    // Island with a stone top overhanging toward three stools.
    b.world();
    b.bx([2.1, 0.0, -1.95], [3.9, 0.88, -1.15], solid(0x3e5c63));
    b.bx([2.05, 0.88, -2.0], [3.95, 0.93, -0.88], stone);
    b.cone([3.45, 0.93, -1.5], 0.09, 0.17, 0.08, solid(0xf1ead8));
    for (dx, dz, c) in [
        (-0.05f32, 0.0f32, 0xd9772b),
        (0.06, 0.03, 0xc23b2e),
        (0.0, -0.06, 0xe2b13c),
    ] {
        b.ell([3.45 + dx, 1.03, -1.5 + dz], [0.045, 0.045, 0.045], solid(c));
    }
    b.bx_rot(
        [2.5, 0.945, -1.45],
        [0.2, 0.012, 0.14],
        Quat::from_rotation_y(0.2),
        cabinet_wood,
    );
    for x in [2.4f32, 3.0, 3.6] {
        b.at(x, -0.6, 0.0);
        stool(b);
    }
    // Pendant lights over the island.
    b.world();
    if options.ceiling {
        for x in [2.55f32, 3.45] {
            b.rod([x, 1.95, -1.5], [x, CEILING_HEIGHT, -1.5], 0.006, black);
            b.cone(
                [x, 1.75, -1.5],
                0.14,
                0.035,
                0.2,
                Material::solid(srgb(0xffd9a0)).with_emissive(0.8),
            );
        }
    }

    // --- Dining nook ----------------------------------------------------------
    b.world();
    let table_wood = Material::cells(srgb(0x9a6b3e), srgb(0xb07d4b), Vec3::new(0.12, f32::INFINITY, 2.5));
    b.bx([3.8, 0.7, 1.75], [4.7, 0.745, 3.05], table_wood);
    for (x, z) in [(3.86f32, 1.82f32), (4.64, 1.82), (3.86, 2.98), (4.64, 2.98)] {
        b.bx([x - 0.03, 0.0, z - 0.03], [x + 0.03, 0.7, z + 0.03], solid(0x4a3524));
    }
    b.bx(
        [4.0, 0.745, 2.0],
        [4.5, 0.75, 2.8],
        Material::stripes(srgb(0xf1ead8), srgb(0xb5443a), Axis::Z, 0.09),
    );
    b.cone([4.25, 0.75, 2.4], 0.05, 0.085, 0.2, solid(0x3d6fa5));
    for (dx, dz, c) in [
        (0.0f32, 0.0f32, 0xe85fa0),
        (0.05, 0.03, 0xf2d34b),
        (-0.04, 0.04, 0xf7f4ee),
    ] {
        b.ell([4.25 + dx, 1.03 + 0.6 * dx, 2.4 + dz], [0.05, 0.05, 0.05], solid(c));
    }
    let seat = solid(0x3e7c6a);
    for z in [2.1f32, 2.7] {
        b.at(3.6, z, 90.0);
        chair(b, seat);
        b.at(4.9, z, -90.0);
        chair(b, seat);
    }
    b.world();
    // Sideboard under the south window.
    b.bx([3.45, 0.08, ws - 0.42], [5.05, 0.85, ws], solid(0xe8e1d1));
    for i in 0..3 {
        let x = 3.47 + i as f32 * 0.527;
        b.bx([x, 0.11, ws - 0.432], [x + 0.507, 0.82, ws - 0.42], solid(0xcfc6b2));
        b.ell([x + 0.25, 0.5, ws - 0.44], [0.016, 0.016, 0.016], black);
    }
    b.cyl(
        [3.75, 0.85, ws - 0.2],
        0.06,
        0.26,
        Material::stripes(srgb(0x24408f), srgb(0xf1ead8), Axis::Y, 0.035),
    );
    b.bx([4.3, 0.85, ws - 0.33], [4.8, 0.88, ws - 0.08], solid(0x8c2f39));
    window(b, Facing::NegX, [we, 1.5, 2.2], 1.6, 1.2);
    window(b, Facing::NegZ, [4.25, 1.7, ws], 1.2, 0.8);
    panel(
        b,
        Facing::PosX,
        [3.0 + SKIN, 1.5, 2.2],
        1.1,
        0.8,
        frame_gold,
        Material::checker(srgb(0x3d6fa5), srgb(0xf1ead8), 0.14),
    );
    if options.ceiling {
        b.rod([4.25, 1.85, 2.4], [4.25, CEILING_HEIGHT, 2.4], 0.008, black);
        b.cone(
            [4.25, 1.62, 2.4],
            0.26,
            0.06,
            0.24,
            Material::solid(srgb(0xffe2a8)).with_emissive(0.8),
        );
    }

    // --- Hallway ------------------------------------------------------------
    b.world();
    b.bx(
        [-0.75, 0.0, 0.5],
        [0.05, 0.01, 3.7],
        Material::stripes(srgb(0x8c2f39), srgb(0xd9b88a), Axis::Z, 0.2),
    );
    // Front door in the south wall.
    let door_green = Material::cells(
        srgb(0x3d5a48),
        srgb(0x486a55),
        Vec3::new(0.3, f32::INFINITY, f32::INFINITY),
    );
    b.bx([-0.82, 0.0, ws - 0.035], [0.22, 2.12, ws], frame_white);
    b.bx([-0.75, 0.0, ws - 0.05], [0.15, 2.05, ws], door_green);
    b.bx(
        [-0.5, 1.3, ws - 0.056],
        [-0.1, 1.75, ws - 0.05],
        Material::tiles(srgb(0xd8ecff), srgb(0x9ccbff), 0.1).with_emissive(0.45),
    );
    b.ell([0.05, 1.02, ws - 0.075], [0.03, 0.03, 0.03], frame_gold);
    // Narrow console with a mirror above it.
    let hall_e = 0.4 - SKIN;
    b.bx([hall_e - 0.22, 0.72, 0.62], [hall_e, 0.76, 1.48], table_wood);
    b.bx([hall_e - 0.2, 0.0, 0.66], [hall_e - 0.17, 0.72, 0.69], frame_dark);
    b.bx([hall_e - 0.2, 0.0, 1.41], [hall_e - 0.17, 0.72, 1.44], frame_dark);
    b.cyl([hall_e - 0.11, 0.76, 0.85], 0.05, 0.14, solid(0x3d6fa5));
    b.bx([hall_e - 0.18, 0.76, 1.1], [hall_e - 0.04, 0.8, 1.36], solid(0xe2b13c));
    panel(
        b,
        Facing::NegX,
        [hall_e, 1.45, 1.05],
        0.6,
        0.9,
        frame_gold,
        Material::gradient(srgb(0xc9d6e2), srgb(0xeef3f7), Axis::Y, -0.4, 0.4).with_emissive(0.25),
    );
    // Coat hooks with a coat and a scarf on the west wall.
    let hall_w = -1.0 + SKIN;
    b.bx([hall_w, 1.62, 2.9], [hall_w + 0.02, 1.7, 3.7], table_wood);
    for z in [3.0f32, 3.3, 3.6] {
        b.rod([hall_w + 0.02, 1.66, z], [hall_w + 0.07, 1.68, z], 0.012, frame_dark);
    }
    b.bx([hall_w + 0.02, 0.85, 2.9], [hall_w + 0.1, 1.66, 3.12], solid(0x2f3a4f));
    b.bx(
        [hall_w + 0.02, 1.05, 3.52],
        [hall_w + 0.07, 1.66, 3.66],
        Material::stripes(srgb(0xd9772b), srgb(0xf1ead8), Axis::Y, 0.06),
    );
    panel(
        b,
        Facing::PosX,
        [hall_w, 1.5, 0.9],
        0.55,
        0.7,
        frame_dark,
        Material::stripes(srgb(0x3e7c6a), srgb(0xf2d34b), Axis::Y, 0.08),
    );

    // --- Bedroom --------------------------------------------------------------
    b.at(ww + 1.13, 2.3, 90.0);
    bed(b);
    b.at(ww + 0.22, 1.2, 90.0);
    nightstand(b);
    b.at(ww + 0.22, 3.4, 90.0);
    nightstand(b);
    b.at(-2.6, 0.2 + SKIN + 0.3, 0.0);
    wardrobe(b);
    b.at(-1.95, ws - 0.3, 180.0);
    desk(b);
    b.at(-2.15, ws - 0.82, 15.0);
    chair(b, solid(0xc8553d));
    b.world();
    b.cyl(
        [-2.45, 0.0, 1.95],
        0.85,
        0.012,
        Material::polar(srgb(0xf1ead8), srgb(0x4a6fa5), Axis::Y, 0.21, 10),
    );
    window(b, Facing::NegZ, [-4.3, 1.5, ws], 1.5, 1.2);
    window(b, Facing::PosX, [ww, 1.75, 2.3], 1.1, 0.75);
    panel(
        b,
        Facing::PosZ,
        [-4.5, 1.5, 0.2 + SKIN],
        1.0,
        0.7,
        frame_white,
        Material::rings(srgb(0x4a6fa5), srgb(0xf2d34b), Axis::Z, 0.06),
    );
    panel(
        b,
        Facing::NegX,
        [-1.0 - SKIN, 1.5, 3.2],
        0.7,
        0.9,
        frame_dark,
        Material::dots(srgb(0xe85fa0), srgb(0xf7f4ee), 0.13, 0.04),
    );
    b.at(-1.36, 0.6, 0.0);
    plant(b, 0.75);

    // --- Bathroom -------------------------------------------------------------
    b.world();
    let (bw, be, bn) = (0.4 + SKIN, 3.0 - SKIN, 0.2 + SKIN);
    let porcelain = solid(0xfbfbf8);
    // Bathtub along the east wall: base, raised rim and water.
    b.bx([be - 0.76, 0.0, bn], [be, 0.44, bn + 1.7], porcelain);
    b.bx([be - 0.76, 0.44, bn], [be - 0.69, 0.57, bn + 1.7], porcelain);
    b.bx([be - 0.07, 0.44, bn], [be, 0.57, bn + 1.7], porcelain);
    b.bx([be - 0.76, 0.44, bn], [be, 0.57, bn + 0.07], porcelain);
    b.bx([be - 0.76, 0.44, bn + 1.63], [be, 0.57, bn + 1.7], porcelain);
    b.bx(
        [be - 0.69, 0.44, bn + 0.07],
        [be - 0.07, 0.5, bn + 1.63],
        Material::tiles(srgb(0x8fd3e8), srgb(0xa8e0f0), 0.11),
    );
    b.rod(
        [be - 0.38, 0.57, bn + 0.035],
        [be - 0.38, 0.72, bn + 0.035],
        0.016,
        steel,
    );
    b.rod([be - 0.38, 0.72, bn + 0.035], [be - 0.38, 0.7, bn + 0.2], 0.014, steel);
    // Shower column and head over the tub.
    b.rod([be - 0.03, 0.9, bn + 0.5], [be - 0.03, 2.05, bn + 0.5], 0.014, steel);
    b.rod([be - 0.03, 2.05, bn + 0.5], [be - 0.33, 2.05, bn + 0.5], 0.012, steel);
    b.cyl([be - 0.33, 2.0, bn + 0.5], 0.11, 0.03, steel);
    // Vanity with basin, tap and mirror on the north wall.
    b.bx([0.62, 0.12, bn], [1.58, 0.82, bn + 0.48], solid(0x3e5c63));
    b.bx([0.6, 0.82, bn], [1.6, 0.86, bn + 0.5], porcelain);
    b.cyl([1.1, 0.86, bn + 0.26], 0.2, 0.07, porcelain);
    b.cyl([1.1, 0.9, bn + 0.26], 0.165, 0.035, solid(0xcfd8dc));
    b.rod([1.1, 0.86, bn + 0.05], [1.1, 1.05, bn + 0.05], 0.013, steel);
    b.rod([1.1, 1.05, bn + 0.05], [1.1, 1.03, bn + 0.18], 0.011, steel);
    for x in [0.86f32, 1.34] {
        b.bx([x - 0.008, 0.3, bn + 0.48], [x + 0.008, 0.6, bn + 0.5], steel);
    }
    panel(
        b,
        Facing::PosZ,
        [1.1, 1.5, bn],
        0.9,
        0.8,
        frame_white,
        Material::gradient(srgb(0xc9d6e2), srgb(0xf2f6f9), Axis::Y, -0.35, 0.35).with_emissive(0.3),
    );
    b.cyl([1.45, 0.86, bn + 0.12], 0.035, 0.13, solid(0xe85fa0));
    // Toilet against the south wall.
    b.world();
    b.bx([2.22, 0.38, ws - 0.2], [2.68, 0.82, ws], porcelain);
    b.cyl([2.45, 0.0, ws - 0.42], 0.14, 0.36, porcelain);
    b.ell([2.45, 0.38, ws - 0.44], [0.2, 0.06, 0.27], porcelain);
    b.ell([2.45, 0.43, ws - 0.44], [0.19, 0.025, 0.26], solid(0xe6e9ea));
    // Towel rail with a striped towel, bath mat, laundry basket.
    b.rod([bw + 0.05, 1.15, 0.85], [bw + 0.05, 1.15, 1.75], 0.012, steel);
    b.bx(
        [bw + 0.03, 0.62, 0.95],
        [bw + 0.075, 1.17, 1.65],
        Material::stripes(srgb(0xe9b44c), srgb(0xf7f4ee), Axis::Y, 0.07),
    );
    b.bx(
        [1.25, 0.0, 1.05],
        [2.0, 0.015, 1.6],
        Material::stripes(srgb(0x3f9c9a), srgb(0xf2f5f4), Axis::X, 0.075),
    );
    b.cone(
        [0.78, 0.0, ws - 0.25],
        0.17,
        0.2,
        0.5,
        Material::checker(srgb(0xc9a16b), srgb(0xa9825a), 0.05),
    );
    window(b, Facing::NegZ, [1.5, 1.8, ws], 0.9, 0.55);
    panel(
        b,
        Facing::NegX,
        [be, 1.5, 2.9],
        0.6,
        0.6,
        frame_dark,
        Material::polar(srgb(0x3f9c9a), srgb(0xf1ead8), Axis::Z, 0.06, 6),
    );

    core::mem::take(&mut b.prims)
}

#[cfg(test)]
mod tests {
    use super::super::test_util::Lcg;
    use super::*;

    #[test]
    fn rooms_tile_the_interior() {
        let total: f32 = ROOMS
            .iter()
            .map(|r| (r.max[0] - r.min[0]) * (r.max[1] - r.min[1]))
            .sum();
        assert!((total - 4.0 * HALF_X * HALF_Z).abs() < 1e-3, "rooms cover {total} m2");
        // Roughly 11 m x 8 m with a 2.5 m ceiling.
        assert_eq!((2.0 * HALF_X, 2.0 * HALF_Z, CEILING_HEIGHT), (11.0, 8.0, 2.5));
        let mut rng = Lcg(5);
        for _ in 0..2000 {
            let p = Vec3::new(rng.range(-HALF_X, HALF_X), 1.0, rng.range(-HALF_Z, HALF_Z));
            let n = ROOMS.iter().filter(|r| r.contains(p)).count();
            assert!(n >= 1, "{p:?} is in no room");
        }
    }

    #[test]
    fn static_scene() {
        let house = HouseScene::new();
        assert_eq!(house.loop_duration(), None);
        let (a, b, c) = (house.snapshot(0.0), house.snapshot(12.5), house.snapshot_static());
        assert_eq!(a.len(), b.len());
        assert_eq!(a.len(), c.len());
        assert!(a.primitives().iter().all(|p| !p.dynamic));
        assert!((200..2000).contains(&a.len()), "{} primitives", a.len());
        let bounds = house.bounds();
        let sb = a.bounds();
        assert!(sb.min.cmpge(bounds.min - 1e-4).all() && sb.max.cmple(bounds.max + 1e-4).all());
    }

    #[test]
    fn floor_and_ceiling_everywhere() {
        let snap = HouseScene::new().snapshot(0.0);
        let mut rng = Lcg(9);
        for _ in 0..3000 {
            let p = Vec3::new(
                rng.range(-HALF_X + 0.1, HALF_X - 0.1),
                1.3,
                rng.range(-HALF_Z + 0.1, HALF_Z - 0.1),
            );
            let down = snap.raycast(p, Vec3::NEG_Y, 10.0).expect("something below");
            assert!(down.pos.y > -0.2, "hole in the floor at {p:?}");
            let up = snap.raycast(p, Vec3::Y, 10.0).expect("something above");
            assert!(up.pos.y <= CEILING_HEIGHT + 1e-3, "hole in the ceiling at {p:?}");
        }
    }

    #[test]
    fn interior_is_watertight() {
        // From points along the walk-through no ray may escape the shell.
        let house = HouseScene::new();
        let snap = house.snapshot(0.0);
        let bounds = house.bounds();
        let mut rng = Lcg(21);
        for eye in tour_waypoints() {
            for _ in 0..3000 {
                let dir = rng.unit_vec();
                let hit = snap
                    .raycast(eye, dir, 100.0)
                    .unwrap_or_else(|| panic!("ray from {eye:?} along {dir:?} escaped"));
                let p = hit.pos;
                assert!(p.cmpge(bounds.min - 1e-3).all() && p.cmple(bounds.max + 1e-3).all());
                // Nothing seen from inside lies beyond the interior faces of the shell.
                assert!(
                    p.x.abs() <= HALF_X + 1e-3
                        && p.z.abs() <= HALF_Z + 1e-3
                        && p.y >= -0.1 - 1e-3
                        && p.y <= CEILING_HEIGHT + 1e-3,
                    "saw the void at {p:?}"
                );
            }
        }
    }

    #[test]
    fn doors_are_open_and_wide_enough() {
        let snap = HouseScene::new().snapshot(0.0);
        for d in DOORS {
            assert!(d.width >= 0.9, "{:?} too narrow", d.rooms);
            let side = d.through.cross(Vec3::Y);
            // Walk straight through at several heights and lateral offsets.
            for y in [0.15, 1.0, d.height - 0.12] {
                for s in [-0.5f32, 0.0, 0.5] {
                    let lateral = side * (s * (d.width - 0.06));
                    let from = d.center + Vec3::Y * y + lateral - d.through * 0.45;
                    assert!(
                        !snap.occluded(from, d.through, 0.9),
                        "door {:?} blocked at y = {y}, offset {s}",
                        d.rooms
                    );
                }
            }
            // Just beyond the clear width there is a jamb or wall.
            let outside = d.center + Vec3::Y + side * (0.5 * d.width + 0.08) - d.through * 0.45;
            let other = d.center + Vec3::Y - side * (0.5 * d.width + 0.08) - d.through * 0.45;
            assert!(
                snap.occluded(outside, d.through, 0.9) || snap.occluded(other, d.through, 0.9),
                "door {:?} wider than declared",
                d.rooms
            );
            // Both sides belong to the rooms the door claims to connect.
            let rooms: Vec<&Room> = d
                .rooms
                .iter()
                .map(|n| ROOMS.iter().find(|r| r.name == *n).unwrap())
                .collect();
            let (a, b) = (d.center - d.through * 0.3, d.center + d.through * 0.3);
            assert!(
                (rooms[0].contains(a) && rooms[1].contains(b)) || (rooms[0].contains(b) && rooms[1].contains(a)),
                "door {:?} does not join its rooms",
                d.rooms
            );
        }
    }

    #[test]
    fn rooms_are_separated_by_walls_away_from_the_doors() {
        let snap = HouseScene::new().snapshot(0.0);
        // Bedroom | hallway wall north of the bedroom door.
        assert!(snap.occluded(Vec3::new(-1.5, 1.2, 0.9), Vec3::X, 1.0));
        // Bathroom | dining wall.
        assert!(snap.occluded(Vec3::new(2.6, 1.2, 2.0), Vec3::X, 1.0));
        // Living | bedroom wall.
        assert!(snap.occluded(Vec3::new(-3.0, 1.2, -0.2), Vec3::Z, 1.0));
        // Above a door there is a lintel.
        assert!(snap.occluded(Vec3::new(-0.5, DOOR_HEIGHT + 0.2, 2.0), Vec3::NEG_X, 1.0));
    }

    #[test]
    fn rooms_are_recognizable() {
        // Each room has its own floor material and wall tint.
        let snap = HouseScene::new().snapshot(0.0);
        let mut floor_ids = Vec::new();
        for (name, x, z) in [
            ("living", -1.2, -1.6),
            ("kitchen", 1.5, -0.6),
            ("hall", -0.85, 2.0),
            ("bedroom", -3.1, 0.95),
            ("bathroom", 1.0, 3.5),
        ] {
            let hit = snap.raycast(Vec3::new(x, 0.5, z), Vec3::NEG_Y, 2.0).unwrap();
            assert!(
                hit.pos.y.abs() < 1e-4,
                "{name}: bare floor expected at ({x}, {z}), got y = {}",
                hit.pos.y
            );
            let m = snap.primitives()[hit.prim as usize].material;
            assert!(!floor_ids.contains(&m), "{name} shares its floor material");
            floor_ids.push(m);
        }
        // Windows glow.
        let hit = snap.raycast(Vec3::new(-3.0, 1.75, -2.4), Vec3::NEG_X, 10.0).unwrap();
        assert!(snap.primitives()[hit.prim as usize].material.emissive > 0.5);
        let c = snap.shade(&hit, true);
        assert!(c.min_element() > 0.9, "window should be bright: {c:?}");
    }

    #[test]
    fn interior_is_bright() {
        let house = HouseScene::new();
        let snap = house.snapshot(0.0);
        let mut rng = Lcg(77);
        let (mut sum, mut n, mut dark) = (0.0f32, 0u32, 0u32);
        for eye in tour_waypoints() {
            for _ in 0..400 {
                let hit = snap.raycast(eye, rng.unit_vec(), 100.0).unwrap();
                let c = snap.shade(&hit, true);
                let luma = c.dot(Vec3::new(0.2126, 0.7152, 0.0722));
                sum += luma.min(1.0);
                n += 1;
                dark += (luma < 0.03) as u32;
            }
        }
        let mean = sum / n as f32;
        assert!(mean > 0.25 && mean < 0.8, "mean luminance {mean}");
        assert!((dark as f32) < 0.05 * n as f32, "{dark} of {n} samples nearly black");
    }

    #[test]
    fn bvh_matches_brute_force() {
        let snap = HouseScene::new().snapshot(0.0);
        let mut rng = Lcg(31);
        let mut hits = 0;
        for i in 0..12000 {
            let origin = rng.in_box(Vec3::new(-5.3, 0.1, -3.8), Vec3::new(5.3, 2.4, 3.8));
            let dir = rng.unit_vec();
            let t_max = if i % 3 == 0 { rng.range(0.2, 4.0) } else { f32::INFINITY };
            let fast = snap.raycast(origin, dir, t_max);
            let slow = snap.raycast_brute_force(origin, dir, t_max);
            match (fast, slow) {
                (None, None) => {}
                (Some(a), Some(b)) => {
                    hits += 1;
                    assert!((a.t - b.t).abs() < 1e-4, "ray {i}: t {} vs {}", a.t, b.t);
                    if a.prim == b.prim {
                        assert_eq!(a, b);
                    }
                }
                (a, b) => panic!("ray {i}: bvh {a:?} vs brute force {b:?}"),
            }
            assert_eq!(snap.occluded(origin, dir, t_max), slow.is_some(), "ray {i}");
        }
        assert!(hits > 9000);
    }

    #[test]
    fn bvh_matches_brute_force_on_grazing_rays() {
        // Origins snapped to a 10 cm grid and axis-parallel or diagonal
        // directions: many of these rays run exactly along faces of the
        // (mostly grid-aligned) furniture or touch a cylinder tangentially,
        // where an unpadded BVH box and the primitive test disagree.
        let snap = HouseScene::new().snapshot(0.0);
        let mut rng = Lcg(999);
        for i in 0..40_000u32 {
            let p = rng.in_box(Vec3::new(-5.4, 0.1, -3.9), Vec3::new(5.4, 2.4, 3.9));
            let origin = (p * 10.0).round() / 10.0;
            let r = rng.unit_vec();
            let dir = match i % 6 {
                0 => Vec3::X * r.x.signum(),
                1 => Vec3::Y * r.y.signum(),
                2 => Vec3::Z * r.z.signum(),
                3 => Vec3::new(r.x.signum(), 0.0, r.z.signum()).normalize(),
                4 => Vec3::new(r.x, 0.0, r.z).normalize(),
                _ => Vec3::new(0.0, r.y, r.z).normalize(),
            };
            let t_max = if i % 4 == 0 { rng.range(0.2, 4.0) } else { f32::INFINITY };
            let fast = snap.raycast(origin, dir, t_max);
            let slow = snap.raycast_brute_force(origin, dir, t_max);
            match (fast, slow) {
                (None, None) => assert!(t_max.is_finite(), "ray {i} from {origin:?} along {dir:?} escaped"),
                (Some(a), Some(b)) => assert!(
                    (a.t - b.t).abs() < 1e-4,
                    "ray {i} from {origin:?} along {dir:?}: t {} vs {}",
                    a.t,
                    b.t
                ),
                (a, b) => panic!("ray {i} from {origin:?} along {dir:?}: bvh {a:?} vs brute force {b:?}"),
            }
            assert_eq!(snap.occluded(origin, dir, t_max), slow.is_some(), "ray {i}");
        }
    }

    #[test]
    fn no_ray_escapes_from_anywhere_inside() {
        // Not only from the tour: from any interior point (free space or
        // inside furniture) every direction ends on something within the shell.
        let snap = HouseScene::new().snapshot(0.0);
        let mut rng = Lcg(4711);
        for i in 0..60_000 {
            let origin = rng.in_box(
                Vec3::new(-HALF_X + 0.07, 0.01, -HALF_Z + 0.07),
                Vec3::new(HALF_X - 0.07, CEILING_HEIGHT - 0.01, HALF_Z - 0.07),
            );
            let dir = rng.unit_vec();
            let hit = snap
                .raycast(origin, dir, f32::INFINITY)
                .unwrap_or_else(|| panic!("ray {i} from {origin:?} along {dir:?} escaped"));
            let p = hit.pos;
            assert!(
                p.x.abs() <= HALF_X + 1e-3
                    && p.z.abs() <= HALF_Z + 1e-3
                    && (-0.101..=CEILING_HEIGHT + 1e-3).contains(&p.y),
                "ray {i} saw the void at {p:?}"
            );
            assert!((hit.normal.length() - 1.0).abs() < 1e-4 && hit.normal.dot(dir) <= 1e-4);
        }
    }

    #[test]
    fn without_ceiling_the_plan_is_visible_from_above() {
        let open = HouseScene::with_options(HouseOptions { ceiling: false }).snapshot(0.0);
        let hit = open.raycast(Vec3::new(-1.2, 10.0, -1.6), Vec3::NEG_Y, 20.0).unwrap();
        assert!(hit.pos.y.abs() < 1e-3, "floor seen from above, got y = {}", hit.pos.y);
        let closed = HouseScene::new().snapshot(0.0);
        let hit = closed.raycast(Vec3::new(-1.2, 10.0, -1.6), Vec3::NEG_Y, 20.0).unwrap();
        assert!(hit.pos.y > CEILING_HEIGHT);
        assert!(open.len() < closed.len());
    }

    #[test]
    fn viewer_start_is_inside_and_sees_furniture() {
        let house = HouseScene::new();
        let (eye, target) = house.viewer_start();
        assert!(ROOMS.iter().any(|r| r.contains(eye)) && eye.y > 1.0 && eye.y < CEILING_HEIGHT);
        let snap = house.snapshot(0.0);
        let hit = snap.raycast(eye, (target - eye).normalize(), 20.0).unwrap();
        assert!(hit.t > 1.0);
    }
}
