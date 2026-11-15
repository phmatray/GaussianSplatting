//! The quality lab: instruments and prototypes for the experiment plan of
//! `docs/RECHERCHE_QUALITE.md` (steps 0 to 4), kept apart from the product
//! code until their measured results decide what gets integrated.
//!
//! | Module | Contents |
//! |---|---|
//! | [`metrics`] | PSNR by region of the reference (silhouette, texture, interior, unobserved), SSIM, coverage, high-frequency noise |
//! | [`recon`] | A copy of the per-pixel depth-to-Gaussian conversion with provenance and a normal override |
//! | [`multiview`] | A1: carving and best-view selection across the cameras of the ring |
//! | [`upsample`] | A2: depth upsampling to the colour grid (nearest, bilinear, joint bilateral planes) |
//! | [`wblend`] | C1 and C2: a rasterizer with normalised depth-tested blending, exact planar splats and a normalised mip filter |
//! | [`lab`] | The stage and scan pipelines rebuilt from public parts, with the knobs the experiments turn |
//! | [`sheet`] | Labelled comparison sheets |
//!
//! The driver is `examples/quality_lab.rs`. Nothing here is used by the
//! pipeline, the wire format or the viewer.

pub mod lab;
pub mod metrics;
pub mod multiview;
pub mod recon;
pub mod sheet;
pub mod upsample;
pub mod wblend;
