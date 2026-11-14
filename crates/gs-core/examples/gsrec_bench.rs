//! Size and speed of recordings of the real pipeline:
//!
//! ```text
//! cargo run --release -p gs-core --example gsrec_bench [-- --light] [--seconds N]
//! ```
//!
//! Records the default stage for `--seconds` (default 12) and a complete
//! house scan, then reports what the recorder costs, what the file weighs
//! with each option, and what playing and seeking cost from memory and
//! through range requests. The numbers quoted in the documentation of
//! `gs_core::record` come from here.

use std::time::Instant;

use gs_core::pipeline::Pipeline;
use gs_core::protocol::{PipelineConfig, ScanState};
use gs_core::record::{
    verify, ByteRange, Compression, KeyframePolicy, Player, PlayerOptions, Recorder, RecorderOptions, Summary,
};
use gs_core::scene::SceneKind;

type Stream = Vec<(f64, Vec<u8>)>;

fn stream(config: PipelineConfig, seconds: f64) -> Stream {
    let mut pipeline = Pipeline::new(config);
    let dt = 1.0 / config.target_hz;
    let mut out: Stream = pipeline.snapshot().into_iter().map(|o| (0.0, o.bytes)).collect();
    let (mut time, mut tail) = (0.0f64, 0.0f64);
    loop {
        time += dt as f64;
        out.extend(pipeline.tick(dt).into_iter().map(|o| (time, o.bytes)));
        let done = match config.scene {
            SceneKind::Stage => time >= seconds,
            SceneKind::House => {
                tail += if pipeline.scan_state() == Some(ScanState::Done) { dt as f64 } else { 0.0 };
                tail >= 2.0
            }
        };
        if done {
            return out;
        }
    }
}

/// Records the stream; returns the file, its summary and the seconds the
/// recorder took.
fn record(stream: &Stream, options: RecorderOptions) -> (Vec<u8>, Summary, f64) {
    let start = Instant::now();
    let mut recorder = Recorder::new(Vec::new(), options);
    for (time, message) in stream {
        recorder.push(*time, message).expect("recording to memory");
    }
    let (file, summary) = recorder.finish().expect("recording to memory");
    (file, summary, start.elapsed().as_secs_f64())
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / 1.0e6
}

fn report(name: &str, stream: &Stream) {
    let duration = stream.last().unwrap().0;
    let raw: u64 = stream.iter().map(|m| m.1.len() as u64).sum();
    println!(
        "\n== {name}: {} messages, {duration:.1} s, {:.1} MB sent to a viewer ({:.2} MB/s)",
        stream.len(),
        mb(raw),
        mb(raw) / duration
    );

    let base = RecorderOptions::named(name);
    let variants: [(&str, RecorderOptions); 6] = [
        ("default (deflate 1, keyframes 8x, all messages)", base.clone()),
        ("uncompressed", RecorderOptions { compression: Compression::None, ..base.clone() }),
        ("deflate level 6", RecorderOptions { compression: Compression::Deflate(6), ..base.clone() }),
        ("without thumbnails", RecorderOptions { previews: false, ..base.clone() }),
        (
            "keyframes 2x",
            RecorderOptions { keyframes: KeyframePolicy { ratio: 2.0, ..KeyframePolicy::default() }, ..base.clone() },
        ),
        (
            "keyframes 32x",
            RecorderOptions { keyframes: KeyframePolicy { ratio: 32.0, ..KeyframePolicy::default() }, ..base.clone() },
        ),
    ];
    println!("  {:<50} {:>9} {:>8} {:>6} {:>10} {:>9}", "recording", "MB", "MB/s", "keys", "recorder", "of a core");
    let mut default_file = Vec::new();
    for (i, (label, options)) in variants.into_iter().enumerate() {
        let (file, summary, seconds) = record(stream, options);
        println!(
            "  {label:<50} {:>9.2} {:>8.2} {:>6} {:>8.2} s {:>8.1} %",
            mb(summary.bytes),
            mb(summary.bytes) / duration,
            summary.keyframes,
            seconds,
            seconds / duration * 100.0
        );
        if i == 0 {
            default_file = file;
        }
    }
    let file = default_file;
    let checked = verify(&file).expect("a recording of this crate");
    assert!(checked.is_valid());
    let gaps: Vec<f64> = checked.keyframes.windows(2).map(|w| w[1].0 - w[0].0).collect();
    if !gaps.is_empty() {
        println!(
            "  keyframes every {:.2} s on average ({:.2} to {:.2} s)",
            gaps.iter().sum::<f64>() / gaps.len() as f64,
            gaps.iter().copied().fold(f64::MAX, f64::min),
            gaps.iter().copied().fold(0.0, f64::max)
        );
    }

    // From memory.
    let copy = file.clone();
    let start = Instant::now();
    let mut player = Player::from_bytes(copy).expect("a recording of this crate");
    let opened = start.elapsed().as_secs_f64();
    let stats = player.stats();
    println!(
        "  player from memory: opened in {:.2} ms; holds the file ({:.1} MB) plus {} records x 32 B = {:.0} KB",
        opened * 1e3,
        mb(file.len() as u64),
        stats.records,
        stats.records as f64 * 32.0 / 1e3
    );
    player.play();
    let start = Instant::now();
    let (mut frames, mut bytes, mut worst) = (0u32, 0u64, 0.0f64);
    loop {
        let frame = Instant::now();
        let update = player.advance(1.0 / 60.0);
        worst = worst.max(frame.elapsed().as_secs_f64());
        bytes += update.messages.iter().map(|m| m.len() as u64).sum::<u64>();
        frames += 1;
        if update.ended {
            break;
        }
    }
    let seconds = start.elapsed().as_secs_f64();
    println!(
        "  forward play at 60 frames/s: {:.1} % of a core, worst frame {:.2} ms, {:.1} MB handed to the viewer",
        seconds / duration * 100.0,
        worst * 1e3,
        mb(bytes)
    );
    let _ = frames;
    let mut random = Lcg(1);
    let (mut total, mut worst, mut bytes, mut worst_bytes) = (0.0f64, 0.0f64, 0u64, 0u64);
    let seeks = 200;
    for _ in 0..seeks {
        player.seek(random.next() * duration);
        let start = Instant::now();
        let update = player.update();
        let took = start.elapsed().as_secs_f64();
        let size: u64 = update.messages.iter().map(|m| m.len() as u64).sum();
        total += took;
        worst = worst.max(took);
        bytes += size;
        worst_bytes = worst_bytes.max(size);
    }
    println!(
        "  seek from memory: {:.2} ms on average, {:.2} ms at worst; {:.2} MB handed to the viewer on average, {:.2} MB at worst",
        total / seeks as f64 * 1e3,
        worst * 1e3,
        mb(bytes) / seeks as f64,
        mb(worst_bytes)
    );

    // Through range requests, with a cache too small to end up holding
    // the whole file.
    let cache = (file.len() as u64 / 4).min(PlayerOptions::default().max_cache_bytes);
    let mut ranged = Player::ranged(file.len() as u64);
    ranged.set_options(PlayerOptions { max_cache_bytes: cache, ..PlayerOptions::default() });
    let fetch = |player: &mut Player, requests: &mut Vec<ByteRange>| loop {
        while let Some(range) = player.wanted() {
            requests.push(range);
            player
                .supply(range.start, &file[range.start as usize..range.end as usize])
                .expect("a recording of this crate");
        }
        if !player.update().buffering {
            return;
        }
    };
    let mut requests = Vec::new();
    fetch(&mut ranged, &mut requests);
    let opening: u64 = requests.iter().map(ByteRange::len).sum();
    println!(
        "  player over ranges (cache of {:.0} MB): {} requests and {:.2} MB to show the first picture",
        mb(cache),
        requests.len(),
        mb(opening)
    );
    let (mut total, mut worst, mut count, mut cached) = (0u64, 0u64, 0usize, 0u64);
    for _ in 0..seeks {
        ranged.seek(random.next() * duration);
        requests.clear();
        fetch(&mut ranged, &mut requests);
        let fetched: u64 = requests.iter().map(ByteRange::len).sum();
        total += fetched;
        worst = worst.max(fetched);
        count += requests.len();
        cached = cached.max(ranged.stats().cached_bytes);
    }
    println!(
        "  seek over ranges: {:.2} MB fetched on average, {:.2} MB at worst, {:.1} requests; cache at most {:.1} MB",
        mb(total) / seeks as f64,
        mb(worst),
        count as f64 / seeks as f64,
        mb(cached)
    );
    ranged.seek(0.0);
    ranged.play();
    requests.clear();
    let (mut stalls, mut cached) = (0u32, 0u64);
    loop {
        while let Some(range) = ranged.wanted() {
            requests.push(range);
            ranged
                .supply(range.start, &file[range.start as usize..range.end as usize])
                .expect("a recording of this crate");
        }
        let update = ranged.advance(1.0 / 60.0);
        stalls += u32::from(update.buffering);
        cached = cached.max(ranged.stats().cached_bytes);
        if update.ended {
            break;
        }
    }
    println!(
        "  play over ranges: {} requests of {:.2} MB on average, {stalls} stalls, cache at most {:.1} MB",
        requests.len(),
        mb(requests.iter().map(ByteRange::len).sum::<u64>()) / requests.len().max(1) as f64,
        mb(cached)
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let light = args.iter().any(|a| a == "--light");
    let seconds = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(12.0);
    let config = |scene| if light { PipelineConfig::light(scene) } else { PipelineConfig::for_scene(scene) };
    report(if light { "stage (light preset)" } else { "stage" }, &stream(config(SceneKind::Stage), seconds));
    report(if light { "house scan (light preset)" } else { "house scan" }, &stream(config(SceneKind::House), 0.0));
}
