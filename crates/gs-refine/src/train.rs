//! The optimiser: Adam over the Gaussians with densification and pruning,
//! a gain per image and channel fitted in closed form, and (optionally) a
//! registration of every image's pose.
//!
//! [`Trainer`] holds every piece of state an optimisation has and advances
//! it one view at a time ([`Trainer::step`]). It knows nothing about files,
//! time budgets or progress reports: [`crate::job`] drives it. All
//! randomness is a pure function of the seed and the iteration number, so a
//! run restored from a checkpoint continues exactly as the uninterrupted
//! one would.

use std::sync::Arc;

use glam::{Mat3, Quat, Vec3};
use serde::{Deserialize, Serialize};

use crate::dataset::{Capture, DatasetError, RegionMask, ViewCache, ViewData, ViewInfo};
use crate::loss::{self, ColorCorrection, LossScratch, LossValue, LossWeights};
use crate::model::{sigmoid, Gaussian, Gaussians, COLOR, MAX_OPACITY, OPACITY, PARAMS, POS, ROT, SCALE};
use crate::raster::{ImageGrads, ParamGrads, Rasterizer, Rendered, View};

/// Step sizes of Adam per parameter group.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LearningRates {
    /// Centers, meters per step; decays exponentially to `position_final`.
    pub position: f32,
    pub position_final: f32,
    /// Log of the standard deviations.
    pub scale: f32,
    /// Quaternion components.
    pub rotation: f32,
    /// Logit of the opacity.
    pub opacity: f32,
    /// Colour, units of full scale.
    pub color: f32,
    /// Per-image pose: length of the first step of a registration, radians
    /// and meters.
    pub pose_rotation: f32,
    pub pose_translation: f32,
}

impl Default for LearningRates {
    fn default() -> Self {
        Self {
            position: 4.0e-4,
            position_final: 2.0e-5,
            scale: 5.0e-3,
            rotation: 2.0e-3,
            opacity: 3.0e-2,
            color: 4.0e-3,
            pose_rotation: 1.0e-3,
            pose_translation: 2.0e-3,
        }
    }
}

/// Densification and pruning.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DensifyParams {
    /// Densify between these fractions of the run, every `every` steps.
    pub from: f32,
    pub until: f32,
    pub every: u32,
    /// Mean screen-space gradient above which a Gaussian is under-fitting.
    pub grad_threshold: f32,
    /// An under-fitting Gaussian wider than this on screen (standard
    /// deviation in pixels of the current level, largest over the views
    /// that saw it) is split in two; a smaller one is cloned.
    pub split_px: f32,
    /// Gaussians more transparent than this are removed.
    pub prune_opacity: f32,
    /// Gaussians with a standard deviation above this are removed, meters.
    pub max_sigma: f32,
}

impl Default for DensifyParams {
    fn default() -> Self {
        Self { from: 0.05, until: 0.8, every: 200, grad_threshold: 2.0e-4, split_px: 1.5, prune_opacity: 0.02, max_sigma: 0.5 }
    }
}

/// One step of the coarse-to-fine schedule.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Level {
    /// Used until this fraction of the run.
    pub until: f32,
    /// Fraction of the final width.
    pub scale: f32,
}

/// Everything that defines an optimisation, apart from the capture.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrainConfig {
    pub iterations: u32,
    pub seed: u64,
    /// Width of the pictures at the last level, pixels (never more than the
    /// photographs have).
    pub width: u32,
    pub levels: Vec<Level>,
    /// Upper bound on the number of Gaussians.
    pub max_gaussians: usize,
    pub rates: LearningRates,
    pub loss: LossWeights,
    pub densify: DensifyParams,
    /// Fit a gain and an offset per image and channel (exposure and white
    /// balance of each photograph), anchored so that they average to the
    /// identity over the capture.
    pub exposure: bool,
    /// Room captures: also fit the photographs where the depth sensor
    /// measured nothing the fusion uses (no return, no confidence, beyond
    /// its range). See [`crate::dataset::decode_view_measured`].
    pub photo_beyond_sensor: bool,
    /// Correct the pose of every view: a registration of each photograph
    /// on the cloud before the first step and at every new level
    /// ([`Trainer::align_pose`]). Off by default: in simulation it gained
    /// 0.6 dB on a capture with jittered poses, cost 2 dB on a clean one and
    /// nearly tripled the time.
    pub pose: bool,
    /// Descent steps of one registration.
    pub pose_align_steps: u32,
}

impl Default for TrainConfig {
    fn default() -> Self {
        Self {
            iterations: 3000,
            seed: 1,
            width: 640,
            levels: vec![Level { until: 0.3, scale: 0.5 }, Level { until: 1.0, scale: 1.0 }],
            max_gaussians: 1_000_000,
            rates: LearningRates::default(),
            loss: LossWeights::default(),
            densify: DensifyParams::default(),
            exposure: true,
            photo_beyond_sensor: true,
            pose: false,
            pose_align_steps: 6,
        }
    }
}

impl TrainConfig {
    /// Whether `iteration` is the first of a new level.
    pub fn level_starts(&self, iteration: u32, native_width: u32) -> bool {
        iteration > 0 && self.level_width(iteration, native_width) != self.level_width(iteration - 1, native_width)
    }

    /// Width of the pictures at `iteration`, a multiple of 8.
    pub fn level_width(&self, iteration: u32, native_width: u32) -> u32 {
        let progress = iteration as f32 / self.iterations.max(1) as f32;
        let scale = self.levels.iter().find(|l| progress < l.until).or(self.levels.last()).map_or(1.0, |l| l.scale);
        let full = self.width.min(native_width).max(16);
        (((full as f32 * scale.clamp(0.05, 1.0)) as u32).div_ceil(8) * 8).clamp(16, full)
    }
}

/// A split moves its two halves this many standard deviations apart from
/// the old center and shrinks them to this fraction along the split axis:
/// the pair has the variance of the Gaussian it replaces
/// (`0.5² + 0.866² = 1`) and its halves overlap enough to leave no gap.
const SPLIT_OFFSET: f32 = 0.5;
const SPLIT_SHRINK: f32 = 0.866;

/// Each step of a pose registration is this much shorter than the one
/// before.
const ALIGN_DECAY: f32 = 0.75;

/// How far a view's colour correction moves toward its least-squares fit
/// each time the view comes up.
const EXPOSURE_BLEND: f32 = 0.5;

/// SplitMix64 of a few words: the source of every random decision.
pub fn hash(words: &[u64]) -> u64 {
    let mut h = 0x9E37_79B9_7F4A_7C15u64;
    for &w in words {
        h = h.wrapping_add(w).wrapping_add(0x9E37_79B9_7F4A_7C15);
        h = (h ^ (h >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h = (h ^ (h >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        h ^= h >> 31;
    }
    h
}

/// Uniform in `0..1`.
fn unit(words: &[u64]) -> f32 {
    (hash(words) >> 40) as f32 / (1u64 << 24) as f32
}

/// Moments of Adam for one small parameter vector.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Moments<const N: usize> {
    pub m: [f32; N],
    pub v: [f32; N],
}

impl<const N: usize> Default for Moments<N> {
    fn default() -> Self {
        Self { m: [0.0; N], v: [0.0; N] }
    }
}

const BETA1: f32 = 0.9;
const BETA2: f32 = 0.999;
const ADAM_EPS: f32 = 1.0e-15;

impl<const N: usize> Moments<N> {
    /// Updates the moments with `grad` and returns the step direction
    /// `m̂ / (√v̂ + ε)` for step number `t` (from 1).
    #[inline]
    fn advance(&mut self, grad: &[f32; N], t: u32) -> [f32; N] {
        let correction1 = 1.0 / (1.0 - BETA1.powi(t.min(100_000) as i32));
        let correction2 = 1.0 / (1.0 - BETA2.powi(t.min(100_000) as i32));
        let mut out = [0.0f32; N];
        for k in 0..N {
            let g = if grad[k].is_finite() { grad[k] } else { 0.0 };
            self.m[k] = BETA1 * self.m[k] + (1.0 - BETA1) * g;
            self.v[k] = BETA2 * self.v[k] + (1.0 - BETA2) * g * g;
            out[k] = self.m[k] * correction1 / ((self.v[k] * correction2).sqrt() + ADAM_EPS);
        }
        out
    }
}

/// What densification remembers about a Gaussian between two rounds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C)]
pub struct DensifyStat {
    /// Sum of the screen-space gradient norms over the views that saw it.
    pub grad_sum: f32,
    pub views: f32,
    /// Largest standard deviation on screen, pixels of the level.
    pub radius_px: f32,
}

/// Per-view state: colour correction and pose.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewState {
    pub correction: ColorCorrection,
    /// World-to-camera transform, refined when the pose is optimised.
    pub rot: Mat3,
    pub trans: Vec3,
    /// How often the view was used.
    pub steps: u32,
}

impl ViewState {
    pub fn new(info: &ViewInfo) -> Self {
        let cam_from_world = info.cam.world_from_cam.inverse();
        Self {
            correction: ColorCorrection::default(),
            rot: Mat3::from(cam_from_world.matrix3),
            trans: Vec3::from(cam_from_world.translation),
            steps: 0,
        }
    }
}

/// Seconds spent in each part of the steps so far.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StepTiming {
    /// Fetching the view (decoding on a cache miss).
    pub view: f64,
    pub forward: f64,
    pub loss: f64,
    /// The part of `loss` spent in the blurs of the structural term.
    pub loss_blurs: f64,
    pub backward: f64,
    /// Adam on the visible Gaussians.
    pub update: f64,
    pub densify: f64,
}

/// What one step reports.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StepReport {
    pub loss: LossValue,
    pub view: usize,
    pub width: u32,
    pub visible: usize,
    /// Gaussians added and removed by a densification round in this step.
    pub added: usize,
    pub removed: usize,
}

/// The state of an optimisation.
pub struct Trainer<R: Rasterizer> {
    pub config: TrainConfig,
    pub model: Gaussians,
    pub moments: Vec<Moments<PARAMS>>,
    pub stats: Vec<DensifyStat>,
    pub views: Vec<ViewInfo>,
    pub view_state: Vec<ViewState>,
    /// Next iteration to run, from 0.
    pub iteration: u32,
    /// Object and person captures: where the scene is.
    pub region: Option<RegionMask>,
    pub raster: R,
    pub timing: StepTiming,
    /// Epoch and view order of that epoch (derived, never stored).
    order: (u64, Vec<u32>),
    rendered: Rendered,
    image_grads: ImageGrads,
    grads: ParamGrads,
    scratch: LossScratch,
}

impl<R: Rasterizer> Trainer<R> {
    pub fn new(config: TrainConfig, model: Gaussians, views: Vec<ViewInfo>, region: Option<RegionMask>, raster: R) -> Self {
        let view_state = views.iter().map(ViewState::new).collect();
        Self {
            moments: vec![Moments::default(); model.len()],
            stats: vec![DensifyStat::default(); model.len()],
            config,
            model,
            views,
            view_state,
            iteration: 0,
            region,
            raster,
            timing: StepTiming::default(),
            order: (u64::MAX, Vec::new()),
            rendered: Rendered::default(),
            image_grads: ImageGrads::default(),
            grads: ParamGrads::default(),
            scratch: LossScratch::default(),
        }
    }

    /// The view used at `iteration`: every epoch walks all views in a fresh
    /// random order (a Fisher-Yates shuffle drawn from the seed and the
    /// number of the epoch).
    pub fn view_at(&mut self, iteration: u32) -> usize {
        let n = self.views.len().max(1);
        let (epoch, slot) = (iteration as usize / n, iteration as usize % n);
        if self.order.0 != epoch as u64 || self.order.1.len() != n {
            let mut order: Vec<u32> = (0..n as u32).collect();
            for i in (1..n).rev() {
                order.swap(i, (hash(&[self.config.seed, epoch as u64, i as u64]) % (i as u64 + 1)) as usize);
            }
            self.order = (epoch as u64, order);
        }
        self.order.1[slot] as usize
    }

    /// The view `index` as the rasterizer sees it at `width`.
    pub fn render_view(&self, index: usize, width: u32, height: u32, background: [f32; 3]) -> View {
        let cam = self.views[index].cam.resized(width, height);
        let state = &self.view_state[index];
        View { width, height, fx: cam.fx, fy: cam.fy, cx: cam.cx, cy: cam.cy, rot: state.rot, trans: state.trans, background }
    }

    fn position_rate(&self) -> f32 {
        let r = &self.config.rates;
        // A rate of zero freezes the centers; so do a negative one and NaN.
        if r.position.is_nan() || r.position <= 0.0 {
            return 0.0;
        }
        let progress = self.iteration as f32 / self.config.iterations.max(1) as f32;
        r.position * (r.position_final / r.position).clamp(1e-6, 1e6).powf(progress.clamp(0.0, 1.0))
    }

    /// One optimisation step on one view.
    pub fn step(&mut self, capture: &Capture, cache: &mut ViewCache) -> Result<StepReport, DatasetError> {
        let it = self.iteration;
        let mut clock = std::time::Instant::now();
        let mut lap = |slot: &mut f64| {
            *slot += clock.elapsed().as_secs_f64();
            clock = std::time::Instant::now();
        };
        let index = self.view_at(it);
        let native = self.views[index].cam.width;
        let width = self.config.level_width(it, native);
        let data: Arc<ViewData> = cache.get(capture, &self.views[index], width, self.region.as_ref())?;
        lap(&mut self.timing.view);
        // A background the scene cannot rely on: holes cost something
        // whatever colour the photograph has there.
        let background = [unit(&[self.config.seed, it as u64, 3]), unit(&[self.config.seed, it as u64, 4]), unit(&[self.config.seed, it as u64, 5])];
        let view = self.render_view(index, data.width, data.height, background);
        self.raster.forward(&self.model, &view, &mut self.rendered);
        lap(&mut self.timing.forward);
        if self.config.exposure {
            self.fit_exposure(index, &data, EXPOSURE_BLEND);
            self.anchor_exposure();
        }
        let correction = self.view_state[index].correction;
        let (value, _) = loss::evaluate(&self.rendered, &correction, &data.target(), &self.config.loss, &mut self.image_grads, &mut self.scratch);
        lap(&mut self.timing.loss);
        self.timing.loss_blurs = self.scratch.blur_seconds;
        self.raster.backward(&self.model, &view, &self.image_grads, false, &mut self.grads);
        lap(&mut self.timing.backward);

        // Gaussians: Adam on the visible ones.
        let t = it + 1;
        let rates = self.config.rates;
        let position_rate = self.position_rate();
        let mut rate = [0.0f32; PARAMS];
        rate[POS..POS + 3].fill(position_rate);
        rate[SCALE..SCALE + 3].fill(rates.scale);
        rate[ROT..ROT + 4].fill(rates.rotation);
        rate[OPACITY] = rates.opacity;
        rate[COLOR..COLOR + 3].fill(rates.color);
        let max_logit = crate::model::logit(MAX_OPACITY);
        for (slot, &i) in self.grads.visible.iter().enumerate() {
            let i = i as usize;
            let direction = self.moments[i].advance(&self.grads.grads[i], t);
            let stat = &mut self.stats[i];
            stat.grad_sum += self.grads.screen_grad[slot];
            stat.views += 1.0;
            stat.radius_px = stat.radius_px.max(self.grads.radius_px[slot]);

            let g = &mut self.model.data[i];
            for k in 0..PARAMS {
                g[k] -= rate[k] * direction[k];
            }
            g[OPACITY] = g[OPACITY].min(max_logit);
            for c in &mut g[COLOR..COLOR + 3] {
                *c = c.clamp(0.0, 1.0);
            }
        }

        self.view_state[index].steps += 1;
        let mut report = StepReport { loss: value, view: index, width: data.width, visible: self.grads.visible.len(), ..StepReport::default() };
        lap(&mut self.timing.update);
        self.iteration += 1;
        let d = self.config.densify;
        let progress = self.iteration as f32 / self.config.iterations.max(1) as f32;
        if d.every > 0 && self.iteration.is_multiple_of(d.every) && progress >= d.from && progress <= d.until {
            (report.added, report.removed) = self.densify();
            lap(&mut self.timing.densify);
        }
        Ok(report)
    }

    /// Moves the colour correction of view `index` toward the gain per
    /// channel that fits the current rendering to its photograph best
    /// (`blend` of 1: all the way). A view comes up a handful of times in a
    /// whole run: far too rarely for gradient steps, but its exposure is a
    /// least-squares problem with a closed solution. Gains only: with an
    /// offset as well, the two trade against each other on views of few
    /// colours and the result was measurably worse.
    fn fit_exposure(&mut self, index: usize, data: &ViewData, blend: f32) {
        let target = data.target();
        // Only where the cloud is closed: a hole shows the background of
        // the rendering, which has nothing to do with the photograph.
        let scene = loss::scene_mask(&target);
        let mask: Vec<bool> = self.rendered.alpha.iter().enumerate().map(|(i, &a)| a > 0.9 && scene.as_ref().is_none_or(|m| m[i])).collect();
        let fit = loss::fit_gain(&self.rendered.color, target.rgb, Some(&mask));
        let correction = &mut self.view_state[index].correction;
        for (gain, fitted) in correction.gain.iter_mut().zip(fit) {
            *gain += blend * (fitted - *gain);
        }
    }

    /// The scene must not drift in brightness: the gains of all views
    /// average to one, per channel.
    fn anchor_exposure(&mut self) {
        let n = self.view_state.len() as f32;
        for c in 0..3 {
            let gain = (self.view_state.iter().map(|s| s.correction.gain[c]).sum::<f32>() / n).max(0.1);
            for s in &mut self.view_state {
                s.correction.gain[c] /= gain;
            }
        }
    }

    /// Registers view `index` on the cloud as it is: `steps` descent steps
    /// on its pose alone, at the size of the pictures at `iteration`, with
    /// steps that shrink from the pose rates of the configuration. Returns
    /// the loss before and after; a pose that does not lower the loss is
    /// not kept.
    ///
    /// A view comes up a handful of times in a run, which is too rarely to
    /// find its pose one optimiser step at a time. Registering each
    /// photograph on the cloud that all of them built (frame to model) is
    /// the classic remedy, and the cloud does not move meanwhile.
    pub fn align_pose(&mut self, index: usize, iteration: u32, capture: &Capture, cache: &mut ViewCache, steps: u32) -> Result<(f64, f64), DatasetError> {
        let width = self.config.level_width(iteration, self.views[index].cam.width);
        let data = cache.get(capture, &self.views[index], width, self.region.as_ref())?;
        let rates = self.config.rates;
        let mut best = (f64::INFINITY, self.view_state[index].rot, self.view_state[index].trans);
        let mut first = f64::INFINITY;
        for k in 0..=steps {
            let view = self.render_view(index, data.width, data.height, [0.5; 3]);
            self.raster.forward(&self.model, &view, &mut self.rendered);
            if self.config.exposure {
                self.fit_exposure(index, &data, 1.0);
            }
            let correction = self.view_state[index].correction;
            let (value, _) = loss::evaluate(&self.rendered, &correction, &data.target(), &self.config.loss, &mut self.image_grads, &mut self.scratch);
            if k == 0 {
                first = value.total;
            }
            if value.total < best.0 {
                best = (value.total, self.view_state[index].rot, self.view_state[index].trans);
            }
            if k == steps {
                break;
            }
            self.raster.backward(&self.model, &view, &self.image_grads, true, &mut self.grads);
            // Normalised steepest descent, turn and shift apart: the two
            // gradients have different units and very different sizes.
            let g = self.grads.pose;
            let (turn, shift) = (Vec3::new(g[0] as f32, g[1] as f32, g[2] as f32), Vec3::new(g[3] as f32, g[4] as f32, g[5] as f32));
            let shrink = ALIGN_DECAY.powi(k as i32);
            let turn = -turn.normalize_or_zero() * (rates.pose_rotation * shrink);
            let shift = -shift.normalize_or_zero() * (rates.pose_translation * shrink);
            let state = &mut self.view_state[index];
            let turn = Mat3::from_quat(Quat::from_scaled_axis(turn));
            state.rot = turn * state.rot;
            state.trans = turn * state.trans + shift;
        }
        let state = &mut self.view_state[index];
        (state.rot, state.trans) = (best.1, best.2);
        Ok((first, best.0))
    }

    /// Before the first step, for every view: fits its exposure against
    /// the starting cloud. Follow with [`Self::settle_exposures`].
    pub fn prefit_exposure(&mut self, index: usize, capture: &Capture, cache: &mut ViewCache) -> Result<(), DatasetError> {
        let width = self.config.level_width(0, self.views[index].cam.width);
        let data = cache.get(capture, &self.views[index], width, self.region.as_ref())?;
        let view = self.render_view(index, data.width, data.height, [0.5; 3]);
        self.raster.forward(&self.model, &view, &mut self.rendered);
        self.fit_exposure(index, &data, 1.0);
        Ok(())
    }

    /// Moves the average of the fitted gains into the cloud itself, so that
    /// the optimisation starts with the brightness and the white balance of
    /// the capture as a whole and not of the frames the fusion happened to
    /// anchor on: colours are multiplied by the mean gain and every gain is
    /// divided by it, which shows the same pictures as before.
    pub fn settle_exposures(&mut self) {
        let n = self.views.len().max(1);
        for c in 0..3 {
            let gain = (self.view_state.iter().map(|s| s.correction.gain[c]).sum::<f32>() / n as f32).clamp(0.5, 2.0);
            for g in &mut self.model.data {
                g[COLOR + c] = (gain * g[COLOR + c]).clamp(0.0, 1.0);
            }
            for s in &mut self.view_state {
                s.correction.gain[c] /= gain;
            }
        }
    }

    /// One round of densification and pruning. Returns how many Gaussians
    /// were added and removed.
    pub fn densify(&mut self) -> (usize, usize) {
        let d = self.config.densify;
        let seed = self.config.seed;
        let round = self.iteration as u64;
        let n = self.model.len();

        // Prune first: what goes makes room under the cap.
        let region = self.region;
        let keep: Vec<bool> = self
            .model
            .data
            .iter()
            .map(|g| {
                let sigma_max = g[SCALE].max(g[SCALE + 1]).max(g[SCALE + 2]).exp();
                let inside = region.is_none_or(|r| Vec3::new(g[POS], g[POS + 1], g[POS + 2]).distance(r.center) <= r.radius * 1.05);
                // Negated comparisons: a NaN anywhere removes the Gaussian.
                let healthy = g.iter().all(|v| v.is_finite());
                healthy && inside && sigmoid(g[OPACITY]) >= d.prune_opacity && sigma_max <= d.max_sigma
            })
            .collect();
        let removed = keep.iter().filter(|k| !**k).count();

        // Candidates, most under-fitted first; ties by index.
        let mut candidates: Vec<(f32, u32)> = (0..n)
            .filter(|&i| keep[i] && self.stats[i].views > 0.0)
            .map(|i| (self.stats[i].grad_sum / self.stats[i].views, i as u32))
            .filter(|(g, _)| *g > d.grad_threshold)
            .collect();
        candidates.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        let room = self.config.max_gaussians.saturating_sub(n - removed);
        candidates.truncate(room);

        let mut added: Vec<Gaussian> = Vec::with_capacity(candidates.len());
        for &(_, i) in &candidates {
            let i = i as usize;
            let g = self.model.data[i];
            if self.stats[i].radius_px > d.split_px {
                // Two smaller Gaussians replace the old one, side by side
                // along its longest axis: together they cover what it
                // covered, with twice the resolution in that direction.
                let q = Quat::from_xyzw(g[ROT], g[ROT + 1], g[ROT + 2], g[ROT + 3]);
                let q = if q.length_squared() > 1e-12 { q.normalize() } else { Quat::IDENTITY };
                let long = (0..3).max_by(|&a, &b| g[SCALE + a].total_cmp(&g[SCALE + b])).expect("three axes");
                let mut axis = Vec3::ZERO;
                axis[long] = 1.0;
                let step = q * axis * (SPLIT_OFFSET * g[SCALE + long].exp());
                let child = |sign: f32| {
                    let mut c = g;
                    let p = Vec3::new(g[POS], g[POS + 1], g[POS + 2]) + step * sign;
                    c[POS..POS + 3].copy_from_slice(&p.to_array());
                    c[SCALE + long] = g[SCALE + long] + SPLIT_SHRINK.ln();
                    c
                };
                self.model.data[i] = child(1.0);
                self.moments[i] = Moments::default();
                added.push(child(-1.0));
            } else {
                // A copy half a standard deviation to the side, in a random
                // direction within the surfel: the gradients do the rest.
                let q = Quat::from_xyzw(g[ROT], g[ROT + 1], g[ROT + 2], g[ROT + 3]);
                let q = if q.length_squared() > 1e-12 { q.normalize() } else { Quat::IDENTITY };
                let thin = (0..3).min_by(|&a, &b| g[SCALE + a].total_cmp(&g[SCALE + b])).expect("three axes");
                let (a, b) = ((thin + 1) % 3, (thin + 2) % 3);
                let angle = std::f32::consts::TAU * unit(&[seed, round, i as u64]);
                let mut side = Vec3::ZERO;
                side[a] = angle.cos() * g[SCALE + a].exp();
                side[b] = angle.sin() * g[SCALE + b].exp();
                let mut copy = g;
                let p = Vec3::new(g[POS], g[POS + 1], g[POS + 2]) + q * side * 0.5;
                copy[POS..POS + 3].copy_from_slice(&p.to_array());
                added.push(copy);
            }
        }

        let mut index = 0;
        self.model.data.retain(|_| (keep[index], index += 1).0);
        index = 0;
        self.moments.retain(|_| (keep[index], index += 1).0);
        let count = added.len();
        for g in added {
            self.model.data.push(g);
            self.moments.push(Moments::default());
        }
        // A new round of statistics.
        self.stats.clear();
        self.stats.resize(self.model.len(), DensifyStat::default());
        (count, removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_grow_to_the_final_width_and_respect_the_photographs() {
        let config = TrainConfig { iterations: 100, width: 640, levels: vec![Level { until: 0.25, scale: 0.5 }, Level { until: 1.0, scale: 1.0 }], ..TrainConfig::default() };
        assert_eq!(config.level_width(0, 1920), 320);
        assert_eq!(config.level_width(24, 1920), 320);
        assert_eq!(config.level_width(25, 1920), 640);
        assert_eq!(config.level_width(99, 1920), 640);
        // Past the end, and photographs smaller than the wanted width.
        assert_eq!(config.level_width(500, 1920), 640);
        assert_eq!(config.level_width(99, 400), 400);
        assert_eq!(config.level_width(0, 400), 200);
        let empty = TrainConfig { levels: Vec::new(), ..config };
        assert_eq!(empty.level_width(0, 1920), 640);
    }

    use crate::model::logit;
    use crate::raster::CpuRasterizer;

    /// A flat disc at `x` on the plane z = 0, facing +Z.
    fn disc(x: f32, opacity: f32) -> Gaussian {
        let mut g = [0.0f32; PARAMS];
        g[POS] = x;
        g[SCALE..SCALE + 3].copy_from_slice(&[0.02f32.ln(), 0.01f32.ln(), 0.001f32.ln()]);
        g[ROT + 3] = 1.0;
        g[OPACITY] = logit(opacity);
        g[COLOR..COLOR + 3].copy_from_slice(&[0.5; 3]);
        g
    }

    fn trainer(data: Vec<Gaussian>, config: TrainConfig) -> Trainer<CpuRasterizer> {
        Trainer::new(config, Gaussians { data }, Vec::new(), None, CpuRasterizer::default())
    }

    #[test]
    fn densification_splits_in_the_plane_clones_prunes_and_respects_the_cap() {
        let config = TrainConfig { max_gaussians: 100, ..TrainConfig::default() };
        let threshold = config.densify.grad_threshold;
        let mut t = trainer(vec![disc(0.0, 0.8), disc(1.0, 0.8), disc(2.0, 0.005), disc(3.0, 0.8)], config.clone());
        // Under-fitting and wide on screen; under-fitting and small;
        // transparent; content.
        t.stats[0] = DensifyStat { grad_sum: 10.0 * threshold, views: 2.0, radius_px: 6.0 };
        t.stats[1] = DensifyStat { grad_sum: 4.0 * threshold, views: 1.0, radius_px: 1.0 };
        t.stats[3] = DensifyStat { grad_sum: 0.5 * threshold, views: 1.0, radius_px: 6.0 };
        t.moments[0].m[0] = 1.0;
        assert_eq!(t.densify(), (2, 1));
        assert_eq!((t.model.len(), t.moments.len(), t.stats.len()), (5, 5, 5));
        // The split: two halves along the long axis (x), half a standard
        // deviation either side, shorter along it, with fresh moments.
        let halves: Vec<&Gaussian> = t.model.data.iter().filter(|g| g[POS].abs() < 0.5).collect();
        assert_eq!(halves.len(), 2);
        let mut xs = [halves[0][POS], halves[1][POS]];
        xs.sort_by(f32::total_cmp);
        assert!((xs[0] + 0.01).abs() < 1e-6 && (xs[1] - 0.01).abs() < 1e-6, "{xs:?}");
        for half in halves {
            assert!((half[SCALE].exp() - 0.02 * 0.866).abs() < 1e-6 && (half[SCALE + 1].exp() - 0.01).abs() < 1e-7);
            assert_eq!((half[POS + 1], half[POS + 2]), (0.0, 0.0));
        }
        assert_eq!(t.moments[0], Moments::default());
        // The clone: a second disc within its own footprint, in the plane.
        let copies: Vec<&Gaussian> = t.model.data.iter().filter(|g| (g[POS] - 1.0).abs() < 0.5).collect();
        assert_eq!(copies.len(), 2);
        let moved = copies.iter().map(|g| ((g[POS] - 1.0).powi(2) + g[POS + 1].powi(2)).sqrt()).fold(0.0, f32::max);
        assert!(moved > 0.004 && moved < 0.011 && copies.iter().all(|g| g[POS + 2] == 0.0), "{moved}");
        // The transparent one is gone, the content one untouched.
        assert!(t.model.data.iter().all(|g| (g[POS] - 2.0).abs() > 0.5));
        assert!(t.model.data.contains(&disc(3.0, 0.8)));
        assert!(t.stats.iter().all(|s| *s == DensifyStat::default()));

        // Under the cap only the most under-fitted is densified.
        let mut t = trainer(vec![disc(0.0, 0.8), disc(1.0, 0.8), disc(3.0, 0.8)], TrainConfig { max_gaussians: 4, ..config });
        t.stats[0] = DensifyStat { grad_sum: 4.0 * threshold, views: 1.0, radius_px: 6.0 };
        t.stats[1] = DensifyStat { grad_sum: 9.0 * threshold, views: 1.0, radius_px: 6.0 };
        assert_eq!(t.densify(), (1, 0));
        assert_eq!(t.model.data.iter().filter(|g| (g[POS] - 1.0).abs() < 0.5).count(), 2);
        // Broken numbers are pruned whatever their statistics.
        t.model.data[0][POS + 1] = f32::NAN;
        assert_eq!(t.densify().1, 1);
    }

    #[test]
    fn a_position_rate_of_zero_freezes_the_centers() {
        // A rate of zero is how a parameter group is frozen (the baseline
        // "colours and opacities only"): it must stay zero, not turn into
        // 0 x infinity.
        let rates = LearningRates { position: 0.0, ..LearningRates::default() };
        let mut t = trainer(vec![disc(0.0, 0.8)], TrainConfig { iterations: 100, rates, ..TrainConfig::default() });
        for iteration in [0, 1, 50, 100, 400] {
            t.iteration = iteration;
            assert_eq!(t.position_rate(), 0.0, "iteration {iteration}");
        }
        // The usual schedule: from the first rate to the final one, never
        // outside the two, also past the end and with a final rate of zero.
        let mut t = trainer(vec![disc(0.0, 0.8)], TrainConfig { iterations: 100, ..TrainConfig::default() });
        let r = LearningRates::default();
        t.iteration = 0;
        assert_eq!(t.position_rate(), r.position);
        t.iteration = 50;
        assert!((t.position_rate() - (r.position * r.position_final).sqrt()).abs() < 1e-7);
        t.iteration = 1000;
        assert!((t.position_rate() - r.position_final).abs() < 1e-9);
        t.config.rates.position_final = 0.0;
        assert!(t.position_rate() >= 0.0 && t.position_rate().is_finite());
    }

    #[test]
    fn every_epoch_visits_every_view_once_in_a_new_order() {
        let info = ViewInfo { frame: 0, cam: gs_core::Camera::look_at(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y, 1.0, 64, 48) };
        let mut t = Trainer::new(TrainConfig::default(), Gaussians::default(), vec![info; 7], None, CpuRasterizer::default());
        let epoch = |t: &mut Trainer<CpuRasterizer>, e: u32| (0..7).map(|k| t.view_at(e * 7 + k)).collect::<Vec<_>>();
        let (first, second) = (epoch(&mut t, 0), epoch(&mut t, 1));
        for order in [&first, &second] {
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..7).collect::<Vec<_>>());
        }
        assert_ne!(first, second);
        // Asking again, in any order of the questions, gives the same answer.
        assert_eq!(epoch(&mut t, 0), first);
    }

    #[test]
    fn hashed_randoms_are_uniform_enough() {
        let n = 20_000;
        let mean: f32 = (0..n).map(|i| unit(&[7, i])).sum::<f32>() / n as f32;
        assert!((mean - 0.5).abs() < 0.01, "{mean}");
        let low = (0..n).filter(|&i| unit(&[7, i]) < 0.25).count() as f32 / n as f32;
        assert!((low - 0.25).abs() < 0.02, "{low}");
        assert_eq!(hash(&[1, 2, 3]), hash(&[1, 2, 3]));
        assert_ne!(hash(&[1, 2, 3]), hash(&[1, 3, 2]));
    }

    #[test]
    fn adam_takes_unit_steps_at_first_and_ignores_broken_gradients() {
        let mut moments = Moments::<3>::default();
        let direction = moments.advance(&[0.5, -2.0, f32::NAN], 1);
        assert!((direction[0] - 1.0).abs() < 1e-4 && (direction[1] + 1.0).abs() < 1e-4 && direction[2] == 0.0);
        // A steady gradient keeps the step near one; a noisy one shrinks it.
        let mut steady = Moments::<1>::default();
        let mut noisy = Moments::<1>::default();
        let (mut a, mut b) = ([0.0], [0.0]);
        for t in 1..=200 {
            a = steady.advance(&[0.3], t);
            b = noisy.advance(&[if t % 2 == 0 { 0.3 } else { -0.3 }], t);
        }
        assert!((a[0] - 1.0).abs() < 1e-3 && b[0].abs() < 0.2, "{a:?} {b:?}");
    }
}
