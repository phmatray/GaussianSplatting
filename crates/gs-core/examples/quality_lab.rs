//! Quality lab: measures what buys fidelity (steps 0 to 4 of the experiment
//! plan in `docs/RECHERCHE_QUALITE.md`).
//!
//! ```text
//! cargo run -p gs-core --release --features par --example quality_lab -- <out dir> <experiment> [--quick] [--only <part>]
//!   baseline   today's stage (two instants) and house at 320x240, 640x480, 1280x960
//!   oracle     sensor resolution, colour finer than depth, one camera, exact normals
//!   a1         multi-view fusion for the stage: variants, timings, parameter re-sweep
//!   c1         normalised depth-tested blending: sweeps on the stage and the house
//!   c2         planar splats and the mip filter; then the best combinations
//!   combo      sensors x fusion x renderer on the stage
//!   shadow     weight of the live layer under normalised blending (shadows)
//!   headline   today against the best combinations at the three resolutions
//!   capture    foreground fraction per camera and capture cost estimates
//!   sheets     the comparison pictures
//!   outline    does the renderer fatten or thin the silhouettes of the live layer?
//!   look       diagnostic pictures of the stage (truth, variants, errors)
//!   explore    free-form table: reconstruction variants against renderers
//! ```
//!
//! Tables are printed as Markdown on standard output. Reference renders and
//! observation masks are cached under `<out dir>/cache`.

#[path = "../tests/common/mod.rs"]
mod common;
#[path = "quality_lab/experiments.rs"]
mod experiments;
#[path = "quality_lab/harness.rs"]
mod harness;

use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().unwrap_or_else(|| "quality_lab_out".into()));
    let experiment = args.next().unwrap_or_else(|| "baseline".into());
    let rest: Vec<String> = args.collect();
    let quick = rest.iter().any(|a| a == "--quick");
    let only = rest.iter().position(|a| a == "--only").and_then(|i| rest.get(i + 1).cloned());
    let lab = harness::Lab::new(out, quick, only);
    match experiment.as_str() {
        "baseline" => experiments::baseline(&lab),
        "oracle" => experiments::oracle(&lab),
        "a1" => experiments::a1(&lab),
        "c1" => experiments::c1(&lab),
        "c2" => experiments::c2(&lab),
        "combo" => experiments::combo(&lab),
        "shadow" => experiments::shadow(&lab),
        "headline" => experiments::headline(&lab),
        "capture" => experiments::capture(&lab),
        "sheets" => experiments::sheets(&lab),
        "look" => experiments::look(&lab),
        "explore" => experiments::explore(&lab),
        "outline" => experiments::outline(&lab),
        other => eprintln!("unknown experiment {other:?}"),
    }
}
