//! Runs a `.gscap` through the ingest, as the server does for a live
//! stream: one frame at a time, a progress tick every ten frames, `finish`
//! at the end. Prints what a viewer would be told and how long every step
//! took; optionally writes the cloud and pictures of it.
//!
//! ```text
//! cargo run --release -p gs-core --example gscap_ingest -- capture.gscap
//! cargo run --release -p gs-core --features par --example gscap_ingest -- capture.gscap --png out/view.png --ply out/cloud.ply
//! ```
//!
//! This is also the quickest look at a recording of a real phone without
//! the application: `--png` draws the cloud with the CPU reference renderer
//! from where the phone started and from above.

use std::io::Read;
use std::time::Instant;

use gs_core::experimental::wblend::{self, Layer, WblendOptions, LIVE_LAYER_WEIGHT};
use gs_core::glam::Vec3;
use gs_core::ingest::{CaptureSession, GscapParser, IngestParams, ParsedItem};
use gs_core::raster::Image;
use gs_core::Camera;

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn save_png(path: &str, image: &Image) {
    let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap_or_else(|e| panic!("{path}: {e}")));
    let mut encoder = png::Encoder::new(file, image.width, image.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().and_then(|mut w| w.write_image_data(&image.rgb)).expect("cannot write the PNG");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: gscap_ingest <file.gscap> [--png out.png] [--ply out.ply]");
        std::process::exit(2);
    };
    let option = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1));
    let mut file = std::fs::File::open(path).unwrap_or_else(|e| panic!("{path}: {e}"));

    let mut parser = GscapParser::new();
    let mut session: Option<CaptureSession> = None;
    let mut buffer = vec![0u8; 256 * 1024];
    // Per frame: total, colour and depth preparation, fusion, encoding.
    let mut times: [Vec<f64>; 4] = Default::default();
    let (mut viewer_bytes, mut frame_bytes, mut largest_frame, mut most_buffered) = (0usize, 0usize, 0usize, 0usize);
    let (mut progress_ms, mut skipped) = (Vec::new(), std::collections::BTreeMap::<&'static str, u32>::new());
    let start = Instant::now();
    loop {
        let n = file.read(&mut buffer).expect("read failed");
        if n == 0 {
            break;
        }
        parser.push(&buffer[..n]);
        most_buffered = most_buffered.max(parser.buffered());
        loop {
            match parser.next_item() {
                Ok(Some(ParsedItem::Header(header))) => {
                    println!("header: {}", header.to_json());
                    session = Some(CaptureSession::from_header(&header, IngestParams::default()).unwrap_or_else(|e| panic!("{e}")));
                }
                Ok(Some(ParsedItem::Frame(bytes))) => {
                    let session = session.as_mut().expect("the header comes first");
                    frame_bytes += bytes.len();
                    largest_frame = largest_frame.max(bytes.len());
                    let begin = Instant::now();
                    let reply = session.handle_frame(bytes).unwrap_or_else(|e| panic!("frame {}: {e}", parser_frames(&times)));
                    let total = begin.elapsed().as_secs_f64() * 1e3;
                    let report = reply.frame.expect("a frame reply carries a report");
                    if let Some(reason) = report.skipped {
                        *skipped.entry(reason.as_str()).or_default() += 1;
                    } else {
                        let stats = session.stats();
                        times[0].push(total);
                        times[1].push(stats.capture_ms as f64);
                        times[2].push(stats.reconstruct_ms as f64);
                        times[3].push(stats.encode_ms as f64);
                    }
                    viewer_bytes += reply.to_viewers.iter().map(|m| m.bytes.len()).sum::<usize>();
                    if session.frames_received().is_multiple_of(10) {
                        let begin = Instant::now();
                        let reply = session.progress();
                        progress_ms.push(begin.elapsed().as_secs_f64() * 1e3);
                        viewer_bytes += reply.to_viewers.iter().map(|m| m.bytes.len()).sum::<usize>();
                    }
                }
                Ok(None) => break,
                Err(e) => panic!("{path}: {e}"),
            }
        }
    }
    let end = parser.finish().unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut session = session.expect("no header");
    let begin = Instant::now();
    let reply = session.finish();
    let finish_ms = begin.elapsed().as_secs_f64() * 1e3;
    viewer_bytes += reply.to_viewers.iter().map(|m| m.bytes.len()).sum::<usize>();

    let state = session.state();
    let (floor, floor_known) = session.floor();
    println!(
        "{} frames{} ({:.1} MB, largest {:.0} kB), {} used, skipped {:?}; parser buffered at most {:.0} kB",
        end.frames,
        if end.truncated { ", file cut short" } else { "" },
        frame_bytes as f64 / 1e6,
        largest_frame as f64 / 1e3,
        session.frames_used(),
        skipped,
        most_buffered as f64 / 1e3
    );
    println!(
        "mode {}, layer 0: {} splats, layer 1: {}; floor at {floor:.3} m ({}), ceiling {:?}, region {:?}",
        session.mode().name(),
        session.static_splats().len(),
        session.dynamic_splats().len(),
        if floor_known { "known" } else { "guessed" },
        state.ceiling_height,
        state.capture.as_ref().and_then(|c| c.region)
    );
    println!("bounds {:?} .. {:?}", state.bounds.min, state.bounds.max);
    for (name, values) in ["total", "colour + depth", "fusion", "encoding"].iter().zip(&mut times) {
        values.sort_by(f64::total_cmp);
        let mean = values.iter().sum::<f64>() / values.len().max(1) as f64;
        println!(
            "per used frame, {name:>14}: mean {mean:6.2} ms, median {:6.2}, 95 % {:6.2}, worst {:6.2}",
            percentile(values, 0.5),
            percentile(values, 0.95),
            percentile(values, 1.0)
        );
    }
    progress_ms.sort_by(f64::total_cmp);
    println!(
        "progress tick: median {:.2} ms, worst {:.2} ms; finish {finish_ms:.1} ms; viewers got {:.1} MB; whole run {:.1} s",
        percentile(&progress_ms, 0.5),
        percentile(&progress_ms, 1.0),
        viewer_bytes as f64 / 1e6,
        start.elapsed().as_secs_f64()
    );

    let layers = [session.static_splats(), session.dynamic_splats()];
    if let Some(out) = option("--ply") {
        let mut all = layers[0].clone();
        all.extend_from_slice(&layers[1]);
        std::fs::write(out, gs_core::io::write_ply(&all)).unwrap_or_else(|e| panic!("{out}: {e}"));
        println!("{out}: {} splats", all.len());
    }
    if let Some(out) = option("--png") {
        let draw = |cam: &Camera, clip_above: Option<f32>| {
            let keep = |s: &&gs_core::Splat| clip_above.is_none_or(|y| s.pos[1] < y);
            let shown: [Vec<gs_core::Splat>; 2] = [layers[0].iter().filter(keep).copied().collect(), layers[1].clone()];
            let layers = [Layer { splats: &shown[0], weight: 1.0 }, Layer { splats: &shown[1], weight: LIVE_LAYER_WEIGHT }];
            wblend::render(&layers, cam, &WblendOptions::normalized_soft()).over([18, 20, 28])
        };
        let (eye, target) = (Vec3::from_array(state.viewer_start.eye), Vec3::from_array(state.viewer_start.target));
        let first = draw(&Camera::look_at(eye, target, Vec3::Y, 60f32.to_radians(), 960, 720), None);
        // From above, the ceiling clipped away.
        let (center, size) = (state.bounds.center(), state.bounds.size());
        let height = 1.2 * size.x.max(size.z).max(1.0) / (2.0 * (25f32.to_radians()).tan());
        let above = Camera::look_at(center + Vec3::Y * height, center, Vec3::NEG_Z, 50f32.to_radians(), 960, 720);
        let top = draw(&above, state.ceiling_height.map(|c| c - 0.25));
        let mut sheet = Image::new(1920, 720, [0, 0, 0]);
        for y in 0..720usize {
            sheet.rgb[y * 1920 * 3..][..960 * 3].copy_from_slice(&first.rgb[y * 960 * 3..][..960 * 3]);
            sheet.rgb[(y * 1920 + 960) * 3..][..960 * 3].copy_from_slice(&top.rgb[y * 960 * 3..][..960 * 3]);
        }
        save_png(out, &sheet);
        println!("{out}: from the start view and from above");
    }
}

/// Number of frames timed so far (for error messages).
fn parser_frames(times: &[Vec<f64>; 4]) -> usize {
    times[0].len()
}
