//! Runs the recorder, the checker and the three kinds of player where the
//! product runs them: on `wasm32-unknown-unknown`, whose `usize` is 32 bits
//! wide. The unit and integration tests only run natively.
//!
//! ```text
//! cargo build -p gs-core --example gsrec_wasm_smoke --target wasm32-unknown-unknown
//! node -e '
//!   const bytes = require("fs").readFileSync(
//!     "target/wasm32-unknown-unknown/debug/examples/gsrec_wasm_smoke.wasm");
//!   // Nothing here calls into JavaScript; imports the linker kept are stubs.
//!   const stubs = new Proxy({}, { get: () => new Proxy({}, { get: () => () => 0 }) });
//!   WebAssembly.instantiate(bytes, stubs).then(({ instance }) => {
//!     const code = instance.exports.main(0, 0);
//!     console.log(code === 0 ? "ok" : "failed at step " + code);
//!     process.exit(code);
//!   });'
//! ```
//!
//! The exit code is 0, or the number of the first step that failed; a panic
//! is a trap (`RuntimeError: unreachable`). Natively
//! (`cargo run -p gs-core --example gsrec_wasm_smoke`) it prints the same.
//! The dev profile is the one to run: it keeps the overflow checks.

#[path = "../tests/record_support/mod.rs"]
mod record_support;

use gs_core::record::{
    verify, Compression, KeyframePolicy, Player, PlayerOptions, RecordError, Recorder, RecorderOptions, MAX_RECORD_LEN,
};
use record_support::{settle, tiny_stream, Lcg, Stream, Viewer, TINY_TOLERANCE};

/// `RecorderOptions::named` reads the clock, which on wasm32 is JavaScript's.
fn options(compression: Compression) -> RecorderOptions {
    RecorderOptions {
        name: "smoke".into(),
        created: "2026-11-14T10:00:00Z".into(),
        scene: None,
        compression,
        stats: true,
        previews: true,
        keyframes: KeyframePolicy { min_interval_s: 1.0, ratio: 1.0 },
        max_bytes: None,
    }
}

fn record(stream: &Stream, options: RecorderOptions) -> Vec<u8> {
    let mut recorder = Recorder::new(Vec::new(), options);
    for (time, message) in stream {
        recorder.push(*time, message).expect("a memory sink does not fail");
    }
    recorder.finish().expect("a memory sink does not fail").0
}

fn shows(viewer: &Viewer, stream: &Stream, time: f64) -> bool {
    // `assert_shows` panics on a mismatch, which is a trap here; the step
    // number of the caller tells where.
    viewer.assert_shows(&Viewer::live(stream, time), TINY_TOLERANCE, "wasm smoke");
    true
}

/// A record with the given kind, flags, time and payload.
fn raw_record(kind: u8, flags: u8, time: f64, payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32 + 12).to_le_bytes().to_vec();
    out.extend_from_slice(&[kind, flags, 0, 0]);
    out.extend_from_slice(&time.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

fn run() -> Result<(), u8> {
    let check = |step: u8, ok: bool| if ok { Ok(()) } else { Err(step) };
    let stream = tiny_stream();
    let end = stream.last().map_or(0.0, |(time, _)| *time);

    for (base, compression) in [(10, Compression::Deflate(1)), (40, Compression::None)] {
        let file = record(&stream, options(compression));
        let report = verify(&file).map_err(|_| base)?;
        check(base + 1, report.is_valid() && report.keyframes.len() >= 4 && report.duration_s == end)?;

        // From memory: scrubbing and clocked play.
        let mut player = Player::from_bytes(file.clone()).map_err(|_| base + 2)?;
        let mut viewer = Viewer::default();
        let mut random = Lcg(5);
        for _ in 0..60 {
            let t = random.next() * end;
            player.seek(t);
            let update = player.update();
            viewer.apply_all(&update);
            check(base + 3, update.time == t && !update.buffering && shows(&viewer, &stream, t))?;
        }
        player.seek(0.0);
        player.set_speed(2.0);
        player.play();
        let mut ended = false;
        for _ in 0..1000 {
            let update = player.advance(1.0 / 60.0);
            viewer.apply_all(&update);
            ended = update.ended;
            if ended {
                break;
            }
        }
        check(base + 4, ended && shows(&viewer, &stream, end))?;

        // From a download.
        let mut player = Player::streaming();
        let mut viewer = Viewer::default();
        player.play();
        for piece in file.chunks(333) {
            player.push_bytes(piece).map_err(|_| base + 5)?;
            let update = player.advance(0.05);
            viewer.apply_all(&update);
            check(base + 6, update.buffering || shows(&viewer, &stream, update.time))?;
        }
        player.finish_stream();
        player.seek(end * 0.4);
        viewer.apply_all(&player.update());
        check(base + 7, shows(&viewer, &stream, end * 0.4))?;

        // From Range requests, with a cache much smaller than the file.
        let mut player = Player::ranged(file.len() as u64);
        player.set_options(PlayerOptions { fetch_block: 700, max_cache_bytes: 6000, prefetch_s: 0.5, jump_s: 2.0 });
        let mut viewer = Viewer::default();
        let mut requests = Vec::new();
        for _ in 0..40 {
            let t = random.next() * end;
            player.seek(t);
            let update = settle(&mut player, &file, &mut viewer, &mut requests);
            check(base + 8, update.time == t && shows(&viewer, &stream, t))?;
        }
        check(base + 9, !player.stats().index_missing && player.stats().cached_bytes < 20_000)?;

        // Cut anywhere, and damaged anywhere: no trap, and what comes out
        // decodes (`Viewer::apply` insists).
        for cut in (0..file.len()).step_by(3) {
            let _ = verify(&file[..cut]);
            if let Ok(mut player) = Player::from_bytes(file[..cut].to_vec()) {
                player.seek(f64::MAX);
                Viewer::default().apply_all(&player.update());
            }
        }
        for round in 0..1500usize {
            let mut bytes = file.clone();
            for _ in 0..1 + round % 3 {
                let lo = if round % 10 == 0 { 0 } else { 1024 };
                let at = lo + (random.next() * (bytes.len() - lo) as f64) as usize;
                bytes[at] = if round % 4 == 0 { (random.next() * 256.0) as u8 } else { bytes[at] ^ (1 << (round % 8)) };
            }
            let _ = verify(&bytes);
            let mut players = Vec::new();
            players.extend(Player::from_bytes(bytes.clone()).ok());
            let mut ranged = Player::ranged(bytes.len() as u64);
            ranged.set_options(PlayerOptions { fetch_block: 900, max_cache_bytes: 8000, ..PlayerOptions::default() });
            players.push(ranged);
            for player in &mut players {
                let mut viewer = Viewer::default();
                for step in 0..4 {
                    player.seek(random.next() * end * 1.2);
                    for _ in 0..300 {
                        while let Some(range) = player.wanted() {
                            let stop = (range.end as usize).min(bytes.len());
                            let start = (range.start as usize).min(stop);
                            if player.supply(range.start, &bytes[start..stop]) != Ok(true) {
                                break;
                            }
                        }
                        let update = if step % 2 == 0 { player.update() } else { player.advance(0.1) };
                        viewer.apply_all(&update);
                        if !update.buffering {
                            break;
                        }
                    }
                }
            }
        }
        check(base + 10, true)?;
    }

    // Lengths chosen to wrap 32-bit arithmetic.
    let good = record(&stream, options(Compression::None));
    let head = &good[..1024];
    for (step, inner) in [(70u8, u32::MAX), (71, u32::MAX - 3), (72, u32::MAX - 4), (73, 1 << 31), (74, 20)] {
        // A keyframe whose first message announces `inner` bytes.
        let mut payload = inner.to_le_bytes().to_vec();
        payload.extend_from_slice(b"GSL1\x01\0\0\0\0\0\0\0\0\0\0\0{}");
        let mut file = head.to_vec();
        file.extend_from_slice(&raw_record(1, 0, 0.0, &payload));
        let report = verify(&file).map_err(|_| step)?;
        check(step, report.problems.iter().any(|p| p.error == RecordError::BadKeyframe))?;
        // The framing is whole; the keyframe turns out unusable when asked for.
        let mut player = Player::from_bytes(file).map_err(|_| step)?;
        check(step, player.update().messages.is_empty() && player.error() == Some(RecordError::Empty))?;
    }
    for (step, length) in [(80u8, u32::MAX), (81, MAX_RECORD_LEN + 1), (82, 11), (83, 0)] {
        let mut file = head.to_vec();
        file.extend_from_slice(&length.to_le_bytes());
        file.extend_from_slice(&[1, 0, 0, 0]);
        file.extend_from_slice(&0f64.to_le_bytes());
        let report = verify(&file).map_err(|_| step)?;
        check(step, report.problems.first().map(|p| p.error) == Some(RecordError::BadLength))?;
        check(step, Player::from_bytes(file.clone()).err() == Some(RecordError::BadLength))?;
        let mut player = Player::streaming();
        check(step, player.push_bytes(&file).is_ok())?;
        check(step, player.error() == Some(RecordError::BadLength))?;
    }
    // A file header that announces more than fits a 32-bit address space,
    // and a file larger than one, reached through ranges.
    let mut huge = good[..8].to_vec();
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    check(90, verify(&huge) == Err(RecordError::BadHeader))?;
    let mut player = Player::ranged(u64::MAX);
    let wanted = player.wanted().ok_or(91)?;
    check(92, wanted.start == 0 && player.supply(0, &good[..wanted.end.min(good.len() as u64) as usize]) == Ok(true))?;
    let tail = player.wanted().ok_or(93)?;
    check(94, tail.end == u64::MAX && tail.len() == 64 << 10)?;
    // The end of such a file is not what the trailer of this one says.
    check(95, player.supply(tail.start, &vec![0u8; tail.len() as usize]) == Ok(true))?;
    check(96, player.stats().index_missing && player.wanted().is_some_and(|r| r.start == 1024))?;
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => {
            #[cfg(not(target_arch = "wasm32"))]
            println!("ok");
            std::process::ExitCode::SUCCESS
        }
        Err(step) => {
            #[cfg(not(target_arch = "wasm32"))]
            println!("failed at step {step}");
            std::process::ExitCode::from(step)
        }
    }
}
