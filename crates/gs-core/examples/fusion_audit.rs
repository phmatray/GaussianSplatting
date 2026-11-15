//! Audit of the multi-view fusion of the stage ([`gs_core::multiview`]): what
//! the fused cloud loses against the same sensors stacked.
//!
//! ```text
//! cargo run -p gs-core --release --features par --example fusion_audit -- <out dir> [holes] [flicker] [strip]
//!       [--set '<JSON patch of the configuration>'] [--times 3.1,9.1] [--save '<part of a view's name>']
//! ```
//!
//! * `holes`: both clouds are drawn from seeded random visitor views and
//!   close-ups, at eight instants of the dance, with and without sensor
//!   noise. A *hole* is a pixel well inside a surface of the true scene
//!   (dancer and balls for the live layer, stage for the static one) that
//!   the stacked cloud closes and the fused one leaves open; the layer is
//!   drawn alone with back faces culled, so that neither the far side of a
//!   limb nor the plate under a shadow hides one; "closed" means inside an
//!   opaque visibility disc (normalised) or half covered (sorted). A
//!   *see-through* is a pixel
//!   of the whole picture that the stacked cloud gets right and the fused one
//!   badly wrong. Both are counted in both directions and under both
//!   blending modes; the worst views are saved as pictures, and so is every
//!   view whose name (`clean t3.1 near2 normalised x1 layer1`) contains the
//!   text given to `--save`.
//! * `flicker`: twelve consecutive ticks at 30 Hz, noisy sensors, subject
//!   moving and frozen, three views. Flicker is the change between two
//!   pictures that the change of the ground truth does not explain, over the
//!   whole picture and over the part the subject occupies. The metric is
//!   checked against clouds that flicker on purpose.
//! * `strip`: the twelve ticks side by side, magnified, as pictures.

#[path = "../tests/common/mod.rs"]
mod common;
#[path = "quality_lab/harness.rs"]
mod harness;

use std::path::{Path, PathBuf};

use common::{save_png, stage_views, Client, BACKGROUND};
use gs_core::capture::{render_reference, render_rgbd, CaptureOptions, NoiseModel};
use gs_core::experimental::sheet::{beside, crop, labelled, magnify, stacked};
use gs_core::experimental::wblend::{render, Blend, Coverage, Layer, Normalized, Silhouette, WblendOptions, LIVE_LAYER_WEIGHT};
use gs_core::pipeline::Pipeline;
use gs_core::protocol::{ControlMsg, PipelineConfig, SORTED_SPLAT_SCALE};
use gs_core::raster::Image;
use gs_core::scene::{make_scene, Scene, SceneKind};
use gs_core::{Camera, Splat};
use harness::stage_random_viewpoints;

/// Instants of the 9.6 s dance loop the holes are counted at.
const HOLE_TIMES: [f32; 8] = [0.7, 1.9, 3.1, 4.3, 5.5, 6.7, 7.9, 9.1];
/// Random views per instant: visitors and close-ups.
const HOLE_VIEWS: (usize, usize) = (6, 3);
const RES: (u32, u32) = (640, 480);
const TICKS: usize = 12;
const DT: f32 = 1.0 / 30.0;
/// Start of the temporal sequences in the dance loop.
const SEQUENCE_START: f32 = 4.1;

/// What a viewer holds of the stage over a few consecutive ticks.
struct Sequence {
    fixed: Vec<Splat>,
    lives: Vec<Vec<Splat>>,
    times: Vec<f32>,
}

/// Runs the pipeline to `start` and records `ticks` consecutive ticks; with
/// `frozen` the scene stands still while the sensors keep exposing.
fn sequence(config: PipelineConfig, start: f32, ticks: usize, frozen: bool) -> Sequence {
    let mut pipeline = Pipeline::new(config);
    let mut client = Client::default();
    client.apply(&pipeline.snapshot());
    client.apply(&pipeline.tick(start));
    if frozen {
        client.apply(&pipeline.handle_control(ControlMsg::SetTimeScale { time_scale: 0.0 }));
    }
    let (mut lives, mut times) = (Vec::new(), Vec::new());
    for k in 0..ticks {
        if k > 0 || frozen {
            client.apply(&pipeline.tick(DT));
        }
        lives.push(client.layers[1].clone());
        times.push(pipeline.scene_time());
    }
    Sequence { fixed: client.layers[0].clone(), lives, times }
}

fn stacked_config(config: PipelineConfig) -> PipelineConfig {
    let mut config = config;
    config.fusion.enabled = false;
    config
}

/// A blending mode by name.
#[derive(Clone, Copy)]
struct Mode {
    name: &'static str,
    opts: WblendOptions,
    live_weight: f32,
}

fn modes() -> [Mode; 2] {
    [
        Mode { name: "normalised x1", opts: WblendOptions::normalized_soft(), live_weight: LIVE_LAYER_WEIGHT },
        Mode {
            name: "sorted x1.4",
            opts: WblendOptions { kernel_scale: SORTED_SPLAT_SCALE, ..WblendOptions::sorted() },
            live_weight: 1.0,
        },
    ]
}

fn picture(mode: &Mode, fixed: &[Splat], live: &[Splat], cam: &Camera) -> Image {
    let layers = [Layer { splats: fixed, weight: 1.0 }, Layer { splats: live, weight: mode.live_weight }];
    render(&layers, cam, &mode.opts).over(BACKGROUND)
}

/// Coverage of one layer drawn alone with back faces culled: below one half,
/// the surface is open there.
fn coverage(mode: &Mode, splats: &[Splat], cam: &Camera) -> Vec<f32> {
    let opts = match mode.opts.blend {
        // The union of the opaque visibility discs of the hard level (1.6
        // standard deviations) and nothing else: the closure that does not
        // depend on how a renderer fills the pinholes between discs.
        Blend::Normalized(params) => WblendOptions {
            cull_backfaces: true,
            blend: Blend::Normalized(Normalized { vis_cutoff: 1.6, coverage: Coverage::None, silhouette: Silhouette::Hard, ..params }),
            ..mode.opts
        },
        Blend::Over => WblendOptions { cull_backfaces: true, ..mode.opts },
    };
    render(&[Layer { splats, weight: 1.0 }], cam, &opts).alpha
}

/// `mask` without the pixels closer than `radius` to a pixel outside it or
/// to a depth step: what lies well inside a surface.
fn interior(mask: &[bool], depth: &[f32], width: usize, height: usize, radius: usize) -> Vec<bool> {
    let mut out = vec![false; mask.len()];
    for y in radius..height.saturating_sub(radius) {
        for x in radius..width.saturating_sub(radius) {
            let i = y * width + x;
            if !mask[i] {
                continue;
            }
            let z = depth[i];
            out[i] = (y - radius..=y + radius).all(|v| {
                (x - radius..=x + radius).all(|u| {
                    let j = v * width + u;
                    mask[j] && (depth[j] - z).abs() < 0.03 * z
                })
            });
        }
    }
    out
}

/// Counts over the pixels of a mask.
#[derive(Clone, Copy, Default)]
struct Holes {
    pixels: u64,
    /// Open in the stacked cloud / in the fused one.
    open: [u64; 2],
    /// Closed by the stacked cloud, open in the fused one, and the reverse.
    lost: u64,
    gained: u64,
}

impl Holes {
    fn count(mask: &[bool], stacked: &[f32], fused: &[f32], map: Option<&mut [u8]>) -> Self {
        let mut holes = Self::default();
        let mut map = map;
        for i in (0..mask.len()).filter(|&i| mask[i]) {
            let (s, f) = (stacked[i] < 0.5, fused[i] < 0.5);
            holes.pixels += 1;
            holes.open[0] += u64::from(s);
            holes.open[1] += u64::from(f);
            holes.lost += u64::from(f && !s);
            holes.gained += u64::from(s && !f);
            if let Some(map) = map.as_deref_mut() {
                map[i] = if f && !s { 2 } else { 1 };
            }
        }
        holes
    }

    fn add(&mut self, other: &Holes) {
        self.pixels += other.pixels;
        self.open[0] += other.open[0];
        self.open[1] += other.open[1];
        self.lost += other.lost;
        self.gained += other.gained;
    }

    fn row(&self) -> String {
        let percent = |n: u64| 100.0 * n as f64 / self.pixels.max(1) as f64;
        format!(
            "{} | {:.3} | {:.3} | {:.3} | {:.3}",
            self.pixels,
            percent(self.open[0]),
            percent(self.open[1]),
            percent(self.lost),
            percent(self.gained)
        )
    }
}

fn max_channel_error(a: &Image, b: &Image, i: usize) -> u8 {
    (0..3).map(|c| a.rgb[i * 3 + c].abs_diff(b.rgb[i * 3 + c])).max().unwrap_or(0)
}

/// Marks the holes of a view in red on a dimmed copy of the truth.
fn hole_picture(truth: &Image, map: &[u8]) -> Image {
    let mut out = truth.clone();
    for (i, &m) in map.iter().enumerate() {
        let pixel = &mut out.rgb[i * 3..i * 3 + 3];
        match m {
            2 => pixel.copy_from_slice(&[255, 0, 0]),
            1 => {}
            _ => pixel.iter_mut().for_each(|c| *c /= 3),
        }
    }
    out
}

fn holes(out: &Path, base: PipelineConfig, times: &[f32], save: Option<&str>) {
    let scene = make_scene(SceneKind::Stage);
    let empty = scene.snapshot_static();
    let clean = CaptureOptions { shadows: base.shadows, ..CaptureOptions::default() };
    println!("\n## Holes: fused against stacked, {} instants x {} random views at {}x{}\n", times.len(), HOLE_VIEWS.0 + HOLE_VIEWS.1, RES.0, RES.1);
    println!("| sensors | layer | blending | interior pixels | open, stacked % | open, fused % | closed by stacked, open in fused % | the reverse % |");
    println!("|---|---|---|---|---|---|---|---|");
    let mut see_through_rows = Vec::new();
    for (noise_name, noise) in [("clean", NoiseModel::NONE), ("noisy", NoiseModel::realistic())] {
        let config = PipelineConfig { noise, ..base };
        // [layer][mode]
        let mut totals = [[Holes::default(); 2]; 2];
        // [mode]: pixels, fused wrong where stacked is right, the reverse.
        let mut see_through = [[0u64; 3]; 2];
        // (lost pixels, picture) of the worst views.
        let mut worst: Vec<(u64, String, Image)> = Vec::new();
        for &t in times {
            // The views of an instant do not depend on which instants run.
            let k = HOLE_TIMES.iter().position(|&known| known == t).unwrap_or(HOLE_TIMES.len());
            let fused = sequence(config, t, 1, false);
            let piled = sequence(stacked_config(config), t, 1, false);
            let snap = scene.snapshot(fused.times[0]);
            for point in stage_random_viewpoints(&snap, t, HOLE_VIEWS.0, HOLE_VIEWS.1, 77 + k as u64) {
                let cam = point.cam.resized(RES.0, RES.1);
                let (w, h) = (cam.width as usize, cam.height as usize);
                let (full, plate) = (render_rgbd(&snap, &cam, &clean), render_rgbd(&empty, &cam, &clean));
                let subject: Vec<bool> =
                    full.depth.iter().zip(&plate.depth).map(|(&z, &zp)| z > 0.0 && (zp == 0.0 || z < zp - 1.0e-3)).collect();
                let stage: Vec<bool> = full.depth.iter().zip(&subject).map(|(&z, &s)| z > 0.0 && !s).collect();
                let masks = [interior(&stage, &full.depth, w, h, 3), interior(&subject, &full.depth, w, h, 3)];
                let truth = Image::from_frame(&render_reference(&snap, &cam, base.shadows, 2, BACKGROUND));
                for (m, mode) in modes().iter().enumerate() {
                    for (layer, mask) in masks.iter().enumerate() {
                        let cloud = |s: &'_ Sequence| if layer == 0 { coverage(mode, &s.fixed, &cam) } else { coverage(mode, &s.lives[0], &cam) };
                        let mut map = vec![0u8; w * h];
                        let found = Holes::count(mask, &cloud(&piled), &cloud(&fused), Some(&mut map));
                        totals[layer][m].add(&found);
                        let name = format!("{noise_name} t{t} {} {} layer{layer}", point.name, mode.name);
                        let wanted = save.is_some_and(|text| name.contains(text));
                        if found.lost > 0 || wanted {
                            let panels = [
                                labelled(&truth, "truth", 2),
                                labelled(&picture(mode, &piled.fixed, &piled.lives[0], &cam), "stacked", 2),
                                labelled(&picture(mode, &fused.fixed, &fused.lives[0], &cam), "fused", 2),
                                labelled(&hole_picture(&truth, &map), &format!("{} holes (red)", found.lost), 2),
                            ];
                            let sheet = beside(&panels, 4, [255, 255, 255]);
                            if wanted {
                                save_png(&out.join(format!("view_{}.png", name.replace([' ', '.'], "_"))), &sheet);
                            }
                            worst.push((found.lost, name, sheet));
                        }
                    }
                    let (a, b) = (picture(mode, &piled.fixed, &piled.lives[0], &cam), picture(mode, &fused.fixed, &fused.lives[0], &cam));
                    for i in (0..w * h).filter(|&i| masks[0][i] || masks[1][i]) {
                        let (ea, eb) = (max_channel_error(&truth, &a, i), max_channel_error(&truth, &b, i));
                        see_through[m][0] += 1;
                        see_through[m][1] += u64::from(eb > 60 && ea < 25);
                        see_through[m][2] += u64::from(ea > 60 && eb < 25);
                    }
                }
            }
            eprintln!("holes: {noise_name} instant {t} done");
        }
        for (layer, name) in ["static", "live"].iter().enumerate() {
            for (m, mode) in modes().iter().enumerate() {
                println!("| {noise_name} | {name} | {} | {} |", mode.name, totals[layer][m].row());
            }
        }
        for (m, mode) in modes().iter().enumerate() {
            let [n, lost, gained] = see_through[m];
            see_through_rows.push(format!(
                "| {noise_name} | {} | {n} | {:.4} | {:.4} |",
                mode.name,
                100.0 * lost as f64 / n.max(1) as f64,
                100.0 * gained as f64 / n.max(1) as f64
            ));
        }
        worst.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        for (rank, (lost, name, image)) in worst.iter().take(4).enumerate() {
            let file = out.join(format!("holes_{noise_name}_{rank}.png"));
            save_png(&file, image);
            eprintln!("holes: {lost} px in {name} -> {}", file.display());
        }
    }
    println!("\n| sensors | blending | interior pixels | fused wrong (> 60 levels) where stacked is right (< 25) % | the reverse % |");
    println!("|---|---|---|---|---|");
    for row in see_through_rows {
        println!("{row}");
    }
}

fn luma(image: &Image, i: usize) -> f32 {
    0.299 * image.rgb[i * 3] as f32 + 0.587 * image.rgb[i * 3 + 1] as f32 + 0.114 * image.rgb[i * 3 + 2] as f32
}

/// Flicker of a sequence of pictures against the ground truth of the same
/// instants over the pixels of `mask`: `(rms change of the truth, rms of the
/// change the truth does not explain, share of the pixels where that
/// unexplained change exceeds 10 levels)`, in 8-bit levels of luma.
fn flicker(truth: &[Image], pictures: &[Image], mask: &[bool]) -> (f32, f32, f32) {
    let (mut truth_sum, mut excess_sum, mut jumps, mut count) = (0.0f64, 0.0f64, 0u64, 0u64);
    for k in 1..pictures.len() {
        for i in (0..mask.len()).filter(|&i| mask[i]) {
            let truth_change = luma(&truth[k], i) - luma(&truth[k - 1], i);
            let excess = luma(&pictures[k], i) - luma(&pictures[k - 1], i) - truth_change;
            truth_sum += (truth_change * truth_change) as f64;
            excess_sum += (excess * excess) as f64;
            jumps += u64::from(excess.abs() > 10.0);
            count += 1;
        }
    }
    let rms = |sum: f64| (sum / count.max(1) as f64).sqrt() as f32;
    (rms(truth_sum), rms(excess_sum), 100.0 * jumps as f32 / count.max(1) as f32)
}

/// The pixels the moving subject (and its shadow) occupies in any picture of
/// a sequence, grown by a few pixels.
fn subject_area(scene: &dyn Scene, times: &[f32], cam: &Camera) -> Vec<bool> {
    let opts = CaptureOptions::default();
    let plate = render_rgbd(&scene.snapshot_static(), cam, &opts);
    let (w, h) = (cam.width as usize, cam.height as usize);
    let mut mask = vec![false; w * h];
    for &t in times {
        let frame = render_rgbd(&scene.snapshot(t), cam, &opts);
        for (i, marked) in mask.iter_mut().enumerate() {
            *marked |= frame.depth[i] != plate.depth[i] || frame.rgb[i * 3..i * 3 + 3] != plate.rgb[i * 3..i * 3 + 3];
        }
    }
    gs_core::experimental::metrics::dilate(&mask, w, h, 6)
}

fn truths(scene: &dyn Scene, times: &[f32], cam: &Camera) -> Vec<Image> {
    times.iter().map(|&t| Image::from_frame(&render_reference(&scene.snapshot(t), cam, true, 2, BACKGROUND))).collect()
}

fn pictures(mode: &Mode, sequence: &Sequence, cam: &Camera) -> Vec<Image> {
    sequence.lives.iter().map(|live| picture(mode, &sequence.fixed, live, cam)).collect()
}

fn flicker_audit(base: PipelineConfig) {
    let scene = make_scene(SceneKind::Stage);
    let noisy = PipelineConfig { noise: NoiseModel::realistic(), ..base };
    let views: Vec<_> = stage_views().into_iter().filter(|v| ["ring0", "closeup", "above"].contains(&v.name.as_str())).collect();

    println!("\n## Is the temporal metric honest?\n");
    println!("(frozen subject, view `closeup`, normalised blending, soft level; flicker rms over the whole picture / over the subject's area)\n");
    println!("| sequence | flicker rms, picture | flicker rms, subject | jumping pixels of the subject % |");
    println!("|---|---|---|---|");
    {
        let cam = views.iter().find(|v| v.name == "closeup").expect("no closeup view").cam;
        let mode = &modes()[0];
        let whole = vec![true; (cam.width * cam.height) as usize];
        let clean = sequence(base, SEQUENCE_START, TICKS, true);
        let area = subject_area(scene.as_ref(), &clean.times, &cam);
        let truth = truths(scene.as_ref(), &clean.times, &cam);
        let report = |name: &str, pictures: &[Image]| {
            let (_, a, _) = flicker(&truth, pictures, &whole);
            let (_, b, jumps) = flicker(&truth, pictures, &area);
            println!("| {name} | {a:.2} | {b:.2} | {jumps:.2} |");
        };
        report("the ground truth itself", &truth);
        report("clean sensors, frozen: the same cloud every tick", &pictures(mode, &clean, &cam));
        let steady = sequence(noisy, SEQUENCE_START, TICKS, true);
        report("noisy sensors, frozen (fused)", &pictures(mode, &steady, &cam));
        for drop_every in [20usize, 5] {
            // Every `drop_every`-th live splat vanishes on odd ticks: what a
            // fusion whose selection flips between ticks would look like.
            let mut flipping = Sequence { fixed: steady.fixed.clone(), lives: steady.lives.clone(), times: steady.times.clone() };
            for live in flipping.lives.iter_mut().skip(1).step_by(2) {
                let mut index = 0usize;
                live.retain(|_| {
                    index += 1;
                    !index.is_multiple_of(drop_every)
                });
            }
            report(&format!("the same with every {drop_every}th live splat missing on odd ticks"), &pictures(mode, &flipping, &cam));
        }
        // The clean cloud, alternately as it is and with its live splats
        // moved by 5 mm: pure geometric jitter.
        let mut jitter = Sequence { fixed: clean.fixed.clone(), lives: clean.lives.clone(), times: clean.times.clone() };
        for live in jitter.lives.iter_mut().skip(1).step_by(2) {
            live.iter_mut().for_each(|s| s.pos[1] += 0.005);
        }
        report("clean cloud whose live splats jump 5 mm on odd ticks", &pictures(mode, &jitter, &cam));
    }

    println!("\n## Flicker: fused against stacked, noisy sensors, {TICKS} ticks at 30 Hz\n");
    println!("| subject | view | blending | truth change rms (subject) | stacked: picture / subject / jumps % | fused: picture / subject / jumps % | fused / stacked (subject) |");
    println!("|---|---|---|---|---|---|---|");
    for (subject, frozen) in [("moving", false), ("frozen", true)] {
        let fused = sequence(noisy, SEQUENCE_START, TICKS, frozen);
        let piled = sequence(stacked_config(noisy), SEQUENCE_START, TICKS, frozen);
        let mut ratios: Vec<(String, f32)> = Vec::new();
        for view in &views {
            let cam = view.cam;
            let whole = vec![true; (cam.width * cam.height) as usize];
            let area = subject_area(scene.as_ref(), &fused.times, &cam);
            let truth = truths(scene.as_ref(), &fused.times, &cam);
            for mode in &modes() {
                let cell = |s: &Sequence| {
                    let p = pictures(mode, s, &cam);
                    let (_, a, _) = flicker(&truth, &p, &whole);
                    let (truth_rms, b, jumps) = flicker(&truth, &p, &area);
                    (truth_rms, a, b, jumps)
                };
                let (truth_rms, sa, sb, sj) = cell(&piled);
                let (_, fa, fb, fj) = cell(&fused);
                println!(
                    "| {subject} | {} | {} | {truth_rms:.2} | {sa:.2} / {sb:.2} / {sj:.2} | {fa:.2} / {fb:.2} / {fj:.2} | {:.3} |",
                    view.name,
                    mode.name,
                    fb / sb.max(1.0e-6)
                );
                ratios.push((mode.name.to_string(), fb / sb.max(1.0e-6)));
            }
        }
        for mode in &modes() {
            let of_mode: Vec<f32> = ratios.iter().filter(|(name, _)| name == mode.name).map(|(_, r)| *r).collect();
            println!("| {subject} | mean of the views | {} | | | | {:.3} |", mode.name, common::mean(&of_mode));
        }
    }
}

/// The twelve ticks side by side: truth, stacked, fused (normalised), fused
/// (sorted), around the subject as the close-up view sees it; and the
/// change from tick to tick of a frozen subject, amplified.
fn strip(out: &Path, base: PipelineConfig) {
    let scene = make_scene(SceneKind::Stage);
    let noisy = PipelineConfig { noise: NoiseModel::realistic(), ..base };
    let cam = stage_views().into_iter().find(|v| v.name == "closeup").expect("no closeup view").cam;
    let rect = (230, 60, 180, 330);
    let tile = |image: &Image| crop(image, rect.0, rect.1, rect.2, rect.3);
    let [soft, sorted] = modes();
    for (subject, frozen) in [("moving", false), ("frozen", true)] {
        let fused = sequence(noisy, SEQUENCE_START, TICKS, frozen);
        let piled = sequence(stacked_config(noisy), SEQUENCE_START, TICKS, frozen);
        let truth = truths(scene.as_ref(), &fused.times, &cam);
        let rows: Vec<(&str, Vec<Image>)> = vec![
            ("truth", truth.clone()),
            ("stacked, normalised soft", pictures(&soft, &piled, &cam)),
            ("fused, normalised soft", pictures(&soft, &fused, &cam)),
            ("fused, sorted x1.4", pictures(&sorted, &fused, &cam)),
        ];
        // Two sheets of six ticks: readable at full size.
        for (half, range) in [(0, 0..TICKS / 2), (1, TICKS / 2..TICKS)] {
            let sheet_rows: Vec<Image> = rows
                .iter()
                .map(|(name, images)| {
                    let tiles: Vec<Image> = images[range.clone()].iter().map(|image| magnify(&tile(image), 2)).collect();
                    labelled(&beside(&tiles, 2, [255, 255, 255]), &format!("{name}, {subject}, ticks {}..{}", range.start, range.end), 2)
                })
                .collect();
            save_png(&out.join(format!("ticks_{subject}_{half}.png")), &stacked(&sheet_rows, 4, [255, 255, 255]));
        }
        if frozen {
            // |picture(k) − picture(k − 1)| times 8: what flickers.
            let change = |images: &[Image]| -> Vec<Image> {
                (1..7)
                    .map(|k| {
                        let (a, b) = (tile(&images[k - 1]), tile(&images[k]));
                        let rgb = a.rgb.iter().zip(&b.rgb).map(|(x, y)| (x.abs_diff(*y) as u32 * 8).min(255) as u8).collect();
                        magnify(&Image { width: a.width, height: a.height, rgb }, 2)
                    })
                    .collect()
            };
            let sheet_rows: Vec<Image> = rows[1..]
                .iter()
                .map(|(name, images)| labelled(&beside(&change(images), 2, [255, 255, 255]), &format!("{name}: change between ticks x8"), 2))
                .collect();
            save_png(&out.join("ticks_frozen_change.png"), &stacked(&sheet_rows, 4, [255, 255, 255]));
        }
    }
    eprintln!("strip: wrote {}/ticks_*.png", out.display());
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(out) = args.first().map(PathBuf::from) else {
        eprintln!("usage: fusion_audit <out dir> [holes] [flicker] [strip] [--set '<JSON patch>']");
        return;
    };
    std::fs::create_dir_all(&out).expect("cannot create the output directory");
    let mut config = PipelineConfig::for_scene(SceneKind::Stage);
    config.preview.enabled = false;
    if let Some(patch) = args.iter().position(|a| a == "--set").and_then(|i| args.get(i + 1)) {
        // Through the pipeline's own `PatchConfig`, sanitised like a viewer's.
        let patch = serde_json::from_str(patch).expect("--set wants a JSON object");
        let mut pipeline = Pipeline::new(PipelineConfig { paused: true, ..config });
        pipeline.handle_control(ControlMsg::PatchConfig { patch });
        config = PipelineConfig { paused: false, ..*pipeline.config() };
    }
    let parts: Vec<&str> = args[1..].iter().map(String::as_str).take_while(|a| !a.starts_with("--")).collect();
    let wants = |name: &str| parts.is_empty() || parts.contains(&name);
    println!("fusion audit: {}x{}x{}, margin {}", config.rig.count, config.rig.width, config.rig.height, config.fusion.margin);
    if wants("holes") {
        let option = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1));
        let times: Vec<f32> = match option("--times") {
            Some(list) => list.split(',').map(|t| t.parse().expect("--times wants seconds, comma separated")).collect(),
            None => HOLE_TIMES.to_vec(),
        };
        holes(&out, config, &times, option("--save").map(String::as_str));
    }
    if wants("flicker") {
        flicker_audit(config);
    }
    if wants("strip") {
        strip(&out, config);
    }
}
