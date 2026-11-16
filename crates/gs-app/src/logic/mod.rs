//! Everything the interface computes that does not need a browser: source
//! selection, configuration patches, scene presentation, depth colouring,
//! number formatting and file checks. Compiled and unit-tested on every
//! target.

pub mod capture;
pub mod config;
pub mod depth;
pub mod files;
pub mod format;
pub mod present;
pub mod qr;
pub mod rate;
pub mod refine;
pub mod render;
pub mod replay;
pub mod rooms;
pub mod share;
pub mod source;
