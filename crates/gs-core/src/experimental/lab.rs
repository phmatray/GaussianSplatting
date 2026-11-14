//! Capture and reconstruction helpers of the quality lab: the product's
//! stage and scan pipelines rebuilt from their public parts, with the knobs
//! the experiments turn (sensor resolution, colour finer than depth, exact
//! normals, multi-view fusion).
//!
//! With default knobs they produce what [`crate::pipeline::Pipeline`] sends
//! to a viewer; the driver (`examples/quality_lab.rs`) checks its baseline
//! numbers against the novel-view tests.

use glam::Vec3;
#[cfg(feature = "par")]
use rayon::prelude::*;

use super::multiview::{fuse, FusionParams, FusionStats, FusionView};
use super::recon::{frame_to_pixel_splats, PixelSplat, ReconHooks};
use super::upsample::{fuse_color_and_depth, Upsampling};
use crate::capture::{render_rgbd, CaptureOptions};
use crate::codec::{self, LayerMode};
use crate::pipeline::{scan_frame, scan_frame_count, scan_sensor};
use crate::protocol::ScanConfig;
use crate::reconstruct::{foreground_mask, BackgroundPlate, ReconstructParams, VoxelFusion, MASK_SKIP};
use crate::rig::ScanPath;
use crate::scene::SceneSnapshot;
use crate::types::{Camera, RgbdFrame, Splat};

/// `splats` after a round trip through the 16-byte wire format: what a
/// viewer actually draws.
pub fn quantized(splats: &[Splat]) -> Vec<Splat> {
    let mut bytes = Vec::new();
    codec::encode_layer(0, 0, 0, LayerMode::Replace, splats, &mut bytes);
    let mut out = Vec::with_capacity(splats.len());
    codec::decode_layer_into(&bytes, &mut out).expect("the codec cannot read its own layer");
    out
}

/// Runs `f` for every index, in parallel with the `par` feature, and returns
/// the results in order.
pub fn map_indices<T: Send>(count: usize, f: impl Fn(usize) -> T + Sync + Send) -> Vec<T> {
    #[cfg(feature = "par")]
    return (0..count).into_par_iter().map(f).collect();
    #[cfg(not(feature = "par"))]
    (0..count).map(f).collect()
}

/// How the sensors of an experiment differ from one pixel grid for both
/// colour and depth.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sensor {
    /// Colour and depth on the camera's own grid.
    Native,
    /// Colour on the camera's grid, depth `factor` times coarser and brought
    /// to the colour grid by `mode`.
    CoarseDepth { factor: u32, mode: Upsampling },
}

/// One exposure of `cam` through `sensor`.
pub fn expose(snap: &SceneSnapshot, cam: &Camera, sensor: Sensor, opts: &CaptureOptions, edge: f32) -> RgbdFrame {
    match sensor {
        Sensor::Native => render_rgbd(snap, cam, opts),
        Sensor::CoarseDepth { factor, mode } => {
            let color = render_rgbd(snap, cam, opts);
            let coarse = cam.resized(cam.width / factor, cam.height / factor);
            let depth = render_rgbd(snap, &coarse, opts);
            fuse_color_and_depth(&depth, &color, factor, edge, mode)
        }
    }
}

/// World-space surface normal at every pixel center of `cam`, straight from
/// the simulator (zero where the ray hits nothing): an oracle no sensor has.
pub fn exact_normals(snap: &SceneSnapshot, cam: &Camera) -> Vec<Vec3> {
    let width = cam.width as usize;
    let rows = map_indices(cam.height as usize, |y| {
        (0..width)
            .map(|x| {
                let (origin, dir) = cam.ray(x as f32 + 0.5, y as f32 + 0.5);
                snap.raycast(origin, dir, f32::INFINITY).map_or(Vec3::ZERO, |hit| hit.normal)
            })
            .collect::<Vec<Vec3>>()
    });
    rows.into_iter().flatten().collect()
}

/// The ring cameras' view of the empty stage and of one instant.
pub struct RingCapture {
    pub cameras: Vec<Camera>,
    /// The empty stage per camera (the background plates).
    pub plates: Vec<RgbdFrame>,
    /// The live frame per camera.
    pub frames: Vec<RgbdFrame>,
}

impl RingCapture {
    /// Captures the empty stage and the live instant with every camera.
    pub fn new(empty: &SceneSnapshot, now: &SceneSnapshot, cameras: &[Camera], sensor: Sensor, shadows: bool, edge: f32) -> Self {
        let opts = CaptureOptions { shadows, ..CaptureOptions::default() };
        Self {
            cameras: cameras.to_vec(),
            plates: cameras.iter().map(|cam| expose(empty, cam, sensor, &opts, edge)).collect(),
            frames: cameras.iter().map(|cam| expose(now, cam, sensor, &opts, edge)).collect(),
        }
    }

    /// Pixels captured per tick.
    pub fn pixels(&self) -> u64 {
        self.cameras.iter().map(|c| c.width as u64 * c.height as u64).sum()
    }

    /// Fraction of each camera's pixels that the live layer is built from.
    pub fn foreground_fractions(&self, params: &ReconstructParams) -> Vec<f32> {
        (0..self.cameras.len())
            .map(|i| {
                let mask = foreground_mask(&self.frames[i], &BackgroundPlate::from_frame(&self.plates[i]), params);
                mask.iter().filter(|&&m| m != MASK_SKIP).count() as f32 / mask.len().max(1) as f32
            })
            .collect()
    }
}

/// Exact normals for [`ring_candidates`]: per camera, for the plates and for
/// the live frames.
pub struct RingNormals {
    pub plates: Vec<Vec<Vec3>>,
    pub frames: Vec<Vec<Vec3>>,
}

impl RingNormals {
    pub fn new(empty: &SceneSnapshot, now: &SceneSnapshot, cameras: &[Camera]) -> Self {
        Self {
            plates: cameras.iter().map(|c| exact_normals(empty, c)).collect(),
            frames: cameras.iter().map(|c| exact_normals(now, c)).collect(),
        }
    }
}

/// The candidate splats of every ring camera, before any fusion.
pub struct RingCandidates {
    /// From the plates: the static layer.
    pub fixed: Vec<Vec<PixelSplat>>,
    /// From the foreground of the live frames: the live layer.
    pub live: Vec<Vec<PixelSplat>>,
}

/// Reconstructs every camera on its own, exactly like the product's
/// pipeline (plate without mask, live frame with its foreground mask).
pub fn ring_candidates(capture: &RingCapture, params: &ReconstructParams, normals: Option<&RingNormals>) -> RingCandidates {
    let per_camera = map_indices(capture.cameras.len(), |i| {
        let cam = &capture.cameras[i];
        let fixed_hooks = ReconHooks { mask: None, normals: normals.map(|n| n.plates[i].as_slice()) };
        let fixed = frame_to_pixel_splats(&capture.plates[i], cam, params, &fixed_hooks);
        let mask = foreground_mask(&capture.frames[i], &BackgroundPlate::from_frame(&capture.plates[i]), params);
        let live_hooks = ReconHooks { mask: Some(&mask), normals: normals.map(|n| n.frames[i].as_slice()) };
        let live = frame_to_pixel_splats(&capture.frames[i], cam, params, &live_hooks);
        (fixed, live)
    });
    let (fixed, live) = per_camera.into_iter().unzip();
    RingCandidates { fixed, live }
}

/// The two layers of the stage as a viewer holds them.
#[derive(Clone, Debug, Default)]
pub struct StageCloud {
    /// Layer 0: the empty stage.
    pub fixed: Vec<Splat>,
    /// Layer 1: the foreground of this instant.
    pub live: Vec<Splat>,
    /// What the fusion did to each layer (zeros without fusion).
    pub fixed_stats: FusionStats,
    pub live_stats: FusionStats,
}

impl StageCloud {
    /// Both layers in one list, static first (the order the product draws).
    pub fn all(&self) -> Vec<Splat> {
        let mut all = self.fixed.clone();
        all.extend_from_slice(&self.live);
        all
    }
}

/// Turns the candidates into the two wire layers: concatenated as the
/// product does (`fusion = None`) or fused across cameras (A1). `cameras`
/// selects a subset of the ring (all when `None`).
pub fn stage_cloud(
    capture: &RingCapture,
    candidates: &RingCandidates,
    fusion: Option<&FusionParams>,
    cameras: Option<&[usize]>,
) -> StageCloud {
    let all: Vec<usize> = (0..capture.cameras.len()).collect();
    let chosen = cameras.unwrap_or(&all);
    let layer = |lists: &[Vec<PixelSplat>], depths: &[RgbdFrame]| match fusion {
        None => {
            let splats: Vec<Splat> = chosen.iter().flat_map(|&i| lists[i].iter().map(|p| p.splat)).collect();
            (splats, FusionStats::default())
        }
        Some(params) => {
            let views: Vec<FusionView> = chosen
                .iter()
                .map(|&i| FusionView { cam: &capture.cameras[i], depth: &depths[i].depth, splats: &lists[i] })
                .collect();
            let fused = fuse(&views, params);
            (fused.concat(), fused.stats)
        }
    };
    let (fixed, fixed_stats) = layer(&candidates.fixed, &capture.plates);
    let (live, live_stats) = layer(&candidates.live, &capture.frames);
    StageCloud { fixed: quantized(&fixed), live: quantized(&live), fixed_stats, live_stats }
}

/// The frames of a home scan: camera and depth map of every exposure, kept
/// to tell which surface points the scan has seen at all.
pub struct ScanExposures {
    pub frames: Vec<(Camera, Vec<f32>)>,
    /// Depth returns beyond this distance were not fused.
    pub max_range: f32,
}

/// A finished home scan.
pub struct Scan {
    /// The fused map after the wire round trip.
    pub splats: Vec<Splat>,
    /// Sensor pixels captured over the whole scan.
    pub pixels: u64,
    /// Number of sensor frames.
    pub frame_count: usize,
    /// Present when asked for.
    pub exposures: Option<ScanExposures>,
}

/// Walks the default scan path and fuses every frame, exactly like the
/// product's pipeline does (same frames, same poses, same fusion), with the
/// sensor of the experiment.
pub fn run_scan(
    snap: &SceneSnapshot,
    scan: &ScanConfig,
    recon: &ReconstructParams,
    sensor: Sensor,
    shadows: bool,
    max_voxels: usize,
    keep_exposures: bool,
) -> Scan {
    let path = ScanPath::house_default().with_speed(scan.speed);
    let mut fusion = VoxelFusion::new(scan.voxel_size);
    fusion.max_range = scan.max_range;
    fusion.max_voxels = max_voxels;
    let opts = CaptureOptions { shadows, ..CaptureOptions::default() };
    let count = scan_frame_count(&path, scan);
    let mut exposures = keep_exposures.then(|| ScanExposures { frames: Vec::new(), max_range: scan.max_range });
    let mut pixels = 0u64;
    for index in 0..count {
        let shot = scan_frame(&path, scan, index);
        let cam = scan_sensor(&path, shot.time, scan, shot.sensor);
        let frame = expose(snap, &cam, sensor, &opts, recon.edge_threshold);
        pixels += cam.width as u64 * cam.height as u64;
        fusion.integrate(&frame, &cam, recon);
        if let Some(e) = &mut exposures {
            e.frames.push((cam, frame.depth));
        }
    }
    Scan { splats: quantized(fusion.splats()), pixels, frame_count: count as usize, exposures }
}

/// For every pixel of `view`: has any exposure seen the surface point the
/// pixel shows (inside its image, not hidden, within range)? Pixels that
/// show no scene count as observed. The same definition as
/// `tests/common::observed_mask`, computed in parallel.
pub fn observed_mask(snap: &SceneSnapshot, exposures: &ScanExposures, view: &Camera) -> Vec<bool> {
    let width = view.width as usize;
    let inverse: Vec<_> = exposures.frames.iter().map(|(cam, _)| cam.world_from_cam.inverse()).collect();
    let rows = map_indices(view.height as usize, |y| {
        (0..width)
            .map(|x| {
                let (origin, dir) = view.ray(x as f32 + 0.5, y as f32 + 0.5);
                let Some(hit) = snap.raycast(origin, dir, f32::INFINITY) else { return true };
                exposures.frames.iter().zip(&inverse).any(|((cam, depth), cam_from_world)| {
                    let c = cam_from_world.transform_point3(hit.pos);
                    let z = -c.z;
                    if z <= 1e-6 || z > exposures.max_range {
                        return false;
                    }
                    let (px, py) = (cam.cx + cam.fx * c.x / z, cam.cy - cam.fy * c.y / z);
                    if px < 0.0 || py < 0.0 || px >= cam.width as f32 || py >= cam.height as f32 {
                        return false;
                    }
                    let d = depth[py as usize * cam.width as usize + px as usize];
                    // Within a pixel the depth of a slanted surface varies.
                    d > 0.0 && (d - z).abs() <= 0.04 * z + 0.02
                })
            })
            .collect::<Vec<bool>>()
    });
    rows.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Pipeline;
    use crate::protocol::PipelineConfig;
    use crate::scene::{make_scene, SceneKind};

    #[test]
    fn the_lab_stage_is_the_pipelines_stage() {
        // A small rig keeps the test fast; the code path is the default one,
        // with the multi-view fusion switched off: the lab stacks.
        let mut config = PipelineConfig::for_scene(SceneKind::Stage);
        config.rig.width = 64;
        config.rig.height = 48;
        config.rig.count = 4;
        config.fusion.enabled = false;
        let pipeline = Pipeline::new(config);
        let scene = make_scene(SceneKind::Stage);
        let cameras = config.rig.cameras();
        let capture = RingCapture::new(
            &scene.snapshot_static(),
            &scene.snapshot(pipeline.scene_time()),
            &cameras,
            Sensor::Native,
            config.shadows,
            config.recon.edge_threshold,
        );
        assert_eq!(capture.pixels(), 4 * 64 * 48);
        let candidates = ring_candidates(&capture, &config.recon, None);
        let cloud = stage_cloud(&capture, &candidates, None, None);
        assert_eq!(cloud.fixed, quantized(pipeline.static_splats()));
        assert_eq!(cloud.live, quantized(pipeline.dynamic_splats()));
        assert!(cloud.fixed.len() > 3000 && cloud.live.len() > 100);
        assert_eq!(cloud.all().len(), cloud.fixed.len() + cloud.live.len());
        let fractions = capture.foreground_fractions(&config.recon);
        assert!(fractions.iter().all(|&f| f > 0.01 && f < 0.5), "{fractions:?}");

        // Fusion drops a good part of both layers and reports it.
        let fused = stage_cloud(&capture, &candidates, Some(&FusionParams::default()), None);
        assert!(fused.fixed.len() < cloud.fixed.len() && fused.live.len() < cloud.live.len());
        assert_eq!(fused.fixed_stats.kept, fused.fixed.len());
        assert_eq!(fused.live_stats.input, cloud.live.len());
        // The product's fusion (`crate::multiview`) follows the prototype's
        // greedy rule with the same margin. It skips cameras behind a
        // splat's surface, which the prototype asks all the same: the two
        // agree on all but a few splats.
        let product = Pipeline::new(PipelineConfig { fusion: Default::default(), ..config });
        let margin = product.config().fusion.margin;
        let prototype = FusionParams { margin, carve: false, ..FusionParams::default() };
        let fused = stage_cloud(&capture, &candidates, Some(&prototype), None);
        for (ours, theirs) in [(product.static_splats(), &fused.fixed), (product.dynamic_splats(), &fused.live)] {
            let (a, b) = (ours.len() as f32, theirs.len() as f32);
            assert!(a < 0.9 * cloud.fixed.len() as f32 && (a - b).abs() <= 0.02 * b, "{a} against {b}");
        }
        // A subset of the ring.
        let two = stage_cloud(&capture, &candidates, None, Some(&[0, 2]));
        assert_eq!(two.fixed.len(), candidates.fixed[0].len() + candidates.fixed[2].len());
    }

    #[test]
    fn exact_normals_face_the_camera_and_coarse_depth_keeps_the_colour() {
        let scene = make_scene(SceneKind::Stage);
        let snap = scene.snapshot(0.5);
        let cam = Camera::look_at(Vec3::new(0.0, 1.4, 3.2), Vec3::new(0.0, 1.0, 0.0), Vec3::Y, 0.87, 160, 120);
        let normals = exact_normals(&snap, &cam);
        let frame = render_rgbd(&snap, &cam, &CaptureOptions::default());
        let mut hits = 0;
        for (i, n) in normals.iter().enumerate() {
            if frame.depth[i] > 0.0 {
                let (_, dir) = cam.ray((i % 160) as f32 + 0.5, (i / 160) as f32 + 0.5);
                assert!((n.length() - 1.0).abs() < 1e-3 && n.dot(dir) <= 1e-4);
                hits += 1;
            }
        }
        assert!(hits > 6000);
        let sensor = Sensor::CoarseDepth { factor: 2, mode: Upsampling::Guided { sigma_color: 15.0 } };
        let split = expose(&snap, &cam, sensor, &CaptureOptions::default(), 0.03);
        assert_eq!(split.rgb, frame.rgb);
        // The upsampled depth is close to the true one nearly everywhere
        // (measured: 99.1 % within 2 %, against 96.9 % for nearest and
        // 92.9 % for bilinear upsampling).
        let close = split.depth.iter().zip(&frame.depth).filter(|&(&a, &b)| b > 0.0 && (a - b).abs() < 0.02 * b).count();
        assert!(close as f32 > 0.98 * hits as f32, "{close} of {hits}");
    }

    #[test]
    fn a_small_scan_runs_and_knows_what_it_saw() {
        let snap = make_scene(SceneKind::House).snapshot(0.0);
        let scan = ScanConfig { width: 48, height: 36, voxel_size: 0.12, frames_per_second: 1.0, ..ScanConfig::default() };
        let result = run_scan(&snap, &scan, &ReconstructParams::default(), Sensor::Native, true, 1_000_000, true);
        assert!(result.splats.len() > 5_000, "{}", result.splats.len());
        assert_eq!(result.pixels, result.frame_count as u64 * 48 * 36);
        let exposures = result.exposures.as_ref().unwrap();
        assert_eq!(exposures.frames.len(), result.frame_count);
        let view = Camera::look_at(Vec3::new(-1.3, 1.62, -1.7), Vec3::new(-4.5, 0.7, -1.0), Vec3::Y, 1.2, 40, 30);
        let mask = observed_mask(&snap, exposures, &view);
        let seen = mask.iter().filter(|&&m| m).count();
        assert!(seen > 40 * 30 * 6 / 10 && mask.len() == 40 * 30, "{seen}");
    }
}
