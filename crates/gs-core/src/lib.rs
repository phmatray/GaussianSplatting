//! Core of the live Gaussian splatting demo.
//!
//! Pure Rust, no I/O, builds for native and `wasm32-unknown-unknown`.
//! See `docs/ARCHITECTURE.md` for the conventions every module follows.

pub mod capture;
pub mod codec;
pub mod io;
pub mod pipeline;
pub mod protocol;
pub mod raster;
pub mod reconstruct;
pub mod rig;
pub mod scene;
pub mod sort;
pub mod splat;
pub mod types;

pub use glam;
pub use types::{Aabb, Camera, RgbdFrame, Splat};
