//! Writes demo recordings with the real pipeline, for testing viewers and
//! servers without a live session:
//!
//! ```text
//! cargo run --release -p gs-core --example gsrec_make -- stage out.gsrec --seconds 10
//! cargo run --release -p gs-core --example gsrec_make -- house out.gsrec --light
//! ```
//!
//! `stage` records the dancer for `--seconds` (default 10); `house` records
//! a complete scan and two seconds of the finished map. `--light` uses the
//! light preset (smaller files), `--no-previews` leaves the thumbnails
//! out, `--raw` stores the payloads uncompressed.

use std::fs::File;
use std::io::BufWriter;
use std::process::ExitCode;

use gs_core::pipeline::Pipeline;
use gs_core::protocol::{PipelineConfig, ScanState};
use gs_core::record::{verify, Compression, IoSink, Recorder, RecorderOptions};
use gs_core::scene::SceneKind;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<f64>().ok())
    };
    let (scene, path) = match (args.first().map(String::as_str), args.get(1)) {
        (Some("stage"), Some(path)) => (SceneKind::Stage, path),
        (Some("house"), Some(path)) => (SceneKind::House, path),
        _ => {
            eprintln!("usage: gsrec_make <stage|house> <out.gsrec> [--seconds N] [--light] [--no-previews] [--raw]");
            return ExitCode::FAILURE;
        }
    };
    let config = if flag("--light") { PipelineConfig::light(scene) } else { PipelineConfig::for_scene(scene) };
    let seconds = value("--seconds").unwrap_or(10.0);
    let mut options =
        RecorderOptions::named(format!("{} (demo)", if scene == SceneKind::Stage { "Scène" } else { "Maison" }));
    options.previews = !flag("--no-previews");
    if flag("--raw") {
        options.compression = Compression::None;
    }

    let file = match File::create(path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("{path}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut recorder = Recorder::new(IoSink(BufWriter::new(file)), options);
    let mut pipeline = Pipeline::new(config);
    let dt = 1.0 / config.target_hz;
    // What a viewer gets on connecting, then every tick.
    let mut push = |time: f64, messages: Vec<gs_core::pipeline::Outgoing>| {
        for message in messages {
            if let Err(error) = recorder.push(time, &message.bytes) {
                eprintln!("{path}: {error}");
                std::process::exit(1);
            }
        }
    };
    push(0.0, pipeline.snapshot());
    let mut time = 0.0f64;
    let mut tail = 0.0f64;
    loop {
        time += dt as f64;
        push(time, pipeline.tick(dt));
        let done = match scene {
            SceneKind::Stage => time >= seconds,
            SceneKind::House => {
                tail += if pipeline.scan_state() == Some(ScanState::Done) { dt as f64 } else { 0.0 };
                tail >= 2.0
            }
        };
        if done {
            break;
        }
    }
    let summary = match recorder.finish() {
        Ok((_, summary)) => summary,
        Err(error) => {
            eprintln!("{path}: {error}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "{path}: {:.1} s, {} bytes ({:.2} MB/s, {:.0} % of the stream), {} messages, {} keyframes",
        summary.duration_s,
        summary.bytes,
        summary.bytes as f64 / 1.0e6 / summary.duration_s.max(1.0e-9),
        summary.bytes as f64 * 100.0 / summary.raw_bytes.max(1) as f64,
        summary.messages,
        summary.keyframes
    );
    let valid = std::fs::read(path).ok().and_then(|bytes| verify(&bytes).ok()).is_some_and(|report| report.is_valid());
    if !valid {
        eprintln!("{path}: the file does not verify");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
