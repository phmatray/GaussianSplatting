//! Everything the interface computes that does not need a browser: source
//! selection, configuration patches, scene presentation, depth colouring,
//! number formatting and file checks. Compiled and unit-tested on every
//! target.

pub mod config;
pub mod depth;
pub mod files;
pub mod format;
pub mod present;
pub mod source;
