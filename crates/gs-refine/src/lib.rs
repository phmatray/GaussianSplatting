//! Offline mode of Splat Live: optimises Gaussians against the photographs
//! of a recorded capture.
//!
//! The real-time path turns every depth frame into surfels and fuses them
//! in a voxel grid; it never checks its picture against the photographs,
//! and its detail is bounded by the cell size and the 256x192 depth map.
//! This crate takes the same recording (`.gscap`: JPEG photographs up to
//! 1920x1440, LiDAR depth, confidence, poses, intrinsics), starts from the
//! fused cloud, and refines it by gradient descent on the difference
//! between rendered and photographed views (3D Gaussian splatting with the
//! depth sensor as a prior). The result is an INRIA 3DGS `.ply` that the
//! viewer draws like any imported splat file. Everything runs on the CPU
//! with rayon; all of it has only ever seen simulated captures.
//!
//! # Pipeline
//!
//! 1. [`dataset`]: index the `.gscap` without loading it, choose training
//!    and held-out views (tracking, blur, near-repeats), decode photographs
//!    on demand at the working size within a memory budget.
//! 2. [`init`]: fuse the training frames with `gs_core::ingest` exactly as
//!    an import does; convert the surfels to Gaussians ([`model`]), 1.4
//!    times larger and a little translucent.
//! 3. [`raster`]: a differentiable tile rasterizer whose forward pass is the
//!    picture the viewer draws for an imported file, with the analytic
//!    backward pass.
//! 4. [`loss`]: L1 + D-SSIM against the photograph through a per-image
//!    colour correction, a depth term against the LiDAR, a coverage term
//!    outside the region of interest.
//! 5. [`train`]: Adam, coarse-to-fine sizes, densification by splitting
//!    surfels in their plane, pruning, a prior that keeps every Gaussian on
//!    the sensed surface, optional pose refinement.
//! 6. [`job`]: the whole thing as a job with progress, cancellation,
//!    evaluation on held-out photographs, checkpoints and a JSON report.
//! 7. [`export`]: the capture as a COLMAP or nerfstudio dataset for
//!    external trainers.
//!
//! The command-line tool `gs-refine` (French messages) wraps 6 and 7.
//!
//! # Coordinates
//!
//! Everything is in the world viewers see: the capture's world (ARKit:
//! meters, right-handed, +Y up) moved vertically so that the floor is
//! `y = 0`. Cameras look along -Z with +Y up; pixel `(0, 0)` is the
//! top-left with centers at `+0.5`; depth is the distance along the camera
//! axis (`docs/ARCHITECTURE.md`).
//!
//! # Porting the hot loops to a GPU
//!
//! [`raster::Rasterizer`] is the seam: the optimiser only calls `forward`
//! (model, view) -> colour, depth, coverage images and `backward` (image
//! gradients) -> parameter gradients, screen-space statistics, pose
//! gradient. [`raster::CpuRasterizer`] is the reference implementation;
//! its module documentation lists the five steps and which of them are
//! per-Gaussian, per-tile and per-pixel. [`loss::evaluate`] is a pure image
//! operation (five separable blurs forward, three backward) and the Adam
//! update of [`train::Trainer::step`] is per-Gaussian arithmetic; both move
//! to compute shaders unchanged.
//!
//! A port is validated against this code by mirroring its tests:
//!
//! * `raster::tests::gradients_match_*`: central differences of a random
//!   linear functional of the three images, for every parameter and for the
//!   camera twist, with the cut-offs switched off
//!   ([`raster::RasterConfig`]). The CPU code agrees with 32-bit finite
//!   differences to 0.1 to 1 %; a port should agree with the CPU gradients
//!   to about `1e-4` relative on the same input (same order of summation
//!   aside), since both are the same formulas in the same precision.
//! * `raster::tests::the_forward_pass_draws_what_the_viewer_draws`: the
//!   forward picture against `gs_core::raster` with `classic_dilation`
//!   (above 50 dB; the residue is the reference's 16-bit depth sort).
//!   Forward pictures of a port against [`raster::CpuRasterizer`]: within
//!   `1e-5` per channel where the depth order is unambiguous.
//! * `loss::tests::the_gradient_of_*`: finite differences of the loss
//!   (agreement to `1e-4`).
//! * `tests/refine.rs`: a run is bit-identical for any number of threads
//!   and across a checkpoint. A GPU cannot promise the first (sums over
//!   pixels are atomically ordered there); compare final held-out PSNR
//!   within 0.1 dB instead.

pub mod dataset;
pub mod export;
pub mod init;
pub mod job;
pub mod loss;
pub mod model;
pub mod raster;
pub mod train;
