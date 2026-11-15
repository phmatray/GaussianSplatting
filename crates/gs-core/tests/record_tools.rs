//! The two implementations of the recording format, this crate and
//! `tools/gsrec.py`, must agree: on what a valid file is, on what is wrong
//! with a damaged one, and on each other's files.
//!
//! The comparison needs `python3`; without it those tests pass with a note.
//! The fixtures under `tests/record_data` are shared with
//! `tools/test_gsrec.py`. To write them again after a deliberate change of
//! the format: `GSREC_WRITE_FIXTURES=1 cargo test -p gs-core --test record_tools`.

mod record_support;

use std::path::{Path, PathBuf};
use std::process::Command;

use gs_core::record::{verify, Compression, Player, RecordError, RecorderOptions, Report, HEADER_JSON_SIZE, MAGIC};
use record_support::*;
use serde_json::{json, Value};

/// A directory of the repository, found from this file (the package
/// directory when built by the workspace).
fn repo_dir(relative_to_package: &str) -> PathBuf {
    let from_manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_to_package);
    if from_manifest.exists() {
        return from_manifest;
    }
    Path::new(file!()).parent().unwrap().join("..").join(relative_to_package)
}

fn fixture(name: &str) -> PathBuf {
    repo_dir("tests/record_data").join(name)
}

/// What `gsrec.py verify --json` prints for a file, from this crate.
fn report_value(result: &Result<Report, RecordError>) -> Value {
    match result {
        Err(error) => json!({ "error": error.code() }),
        Ok(r) => json!({
            "info": { "name": r.info.name, "created": r.info.created, "scene": r.info.scene, "protocol": r.info.protocol, "duration_s": r.info.duration_s },
            "bytes": r.bytes,
            "complete": r.complete,
            "truncated": r.truncated,
            "records": r.records,
            "states": r.states,
            "layers": r.layers,
            "stats": r.stats,
            "previews": r.previews,
            "keyframes": r.keyframes.iter().map(|(time, offset)| json!([time, offset])).collect::<Vec<_>>(),
            "duration_s": r.duration_s,
            "stored_bytes": r.stored_bytes,
            "raw_bytes": r.raw_bytes,
            "problems": r.problems.iter().map(|p| json!([p.offset, p.error.code()])).collect::<Vec<_>>(),
        }),
    }
}

/// Equal, numbers that are not integers up to one part in 10^14: serde_json
/// reads a decimal number to within a unit in the last place unless built
/// with `float_roundtrip`, so the last digit of a time is not compared.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) if x.is_f64() && y.is_f64() => {
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            (x - y).abs() <= 1.0e-14 * x.abs().max(y.abs())
        }
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(key, x)| y.get(key).is_some_and(|y| same(x, y)))
        }
        _ => a == b,
    }
}

/// `python3`, if it runs.
fn python() -> Option<&'static str> {
    let runs = |name: &str| Command::new(name).arg("--version").output().is_ok_and(|o| o.status.success());
    let found = ["python3", "python"].into_iter().find(|name| runs(name));
    if found.is_none() {
        eprintln!("note: python3 not found, the comparison with tools/gsrec.py is skipped");
    }
    found
}

fn uncompressed() -> RecorderOptions {
    RecorderOptions { compression: Compression::None, ..tiny_options() }
}

#[test]
fn the_fixtures_are_valid_and_play() {
    if std::env::var_os("GSREC_WRITE_FIXTURES").is_some() {
        let file = record(&tiny_stream(), tiny_options());
        std::fs::create_dir_all(fixture("")).unwrap();
        std::fs::write(fixture("rust_tiny.gsrec"), &file).unwrap();
        let expected = serde_json::to_string_pretty(&report_value(&verify(&file))).unwrap();
        std::fs::write(fixture("rust_tiny.json"), expected + "\n").unwrap();
    }
    // Written by this crate: the tiny session of the other tests.
    let file = std::fs::read(fixture("rust_tiny.gsrec")).expect("tests/record_data/rust_tiny.gsrec");
    let report = verify(&file);
    assert!(report.as_ref().unwrap().is_valid());
    let expected: Value = serde_json::from_slice(&std::fs::read(fixture("rust_tiny.json")).unwrap()).unwrap();
    assert!(same(&report_value(&report), &expected), "the checker changed its mind about the fixture");
    let stream = tiny_stream();
    let end = stream.last().unwrap().0;
    let mut player = Player::from_bytes(file).unwrap();
    assert_eq!(player.duration(), Some(end));
    let mut viewer = Viewer::default();
    let mut random = Lcg(5);
    for _ in 0..40 {
        let t = random.next() * end;
        player.seek(t);
        viewer.apply_all(&player.update());
        viewer.assert_shows(&Viewer::live(&stream, t), TINY_TOLERANCE, &format!("fixture at {t}"));
    }
}

/// Plays a recording written by the Python tool and compares every stop
/// with the stream read straight from the file.
fn check_python_recording(file: &[u8]) {
    let report = verify(file).unwrap();
    assert!(report.is_valid(), "{:?}", report.problems);
    assert_eq!((report.info.scene.as_str(), report.info.protocol), ("synth", 1));
    assert!(report.keyframes.len() >= 4 && (report.info.duration_s - report.duration_s).abs() < 1e-9);
    let stream = stream_of(file);
    assert_eq!(stream.len() as u64, report.messages() + 4);
    let end = stream.last().unwrap().0;
    // The boxes of the synthetic session are 4 m wide: a merged keyframe
    // moves positions by less than half a step of their union.
    let tolerance = 12.0 / 65535.0;

    let mut player = Player::from_bytes(file.to_vec()).unwrap();
    let mut viewer = Viewer::default();
    player.play();
    loop {
        let update = player.advance(0.04);
        viewer.apply_all(&update);
        viewer.assert_shows(
            &Viewer::live(&stream, update.time),
            0.0,
            &format!("python file, forward at {}", update.time),
        );
        if update.ended {
            break;
        }
    }
    let mut random = Lcg(21);
    for _ in 0..60 {
        let t = random.next() * end;
        player.seek(t);
        viewer.apply_all(&player.update());
        viewer.assert_shows(&Viewer::live(&stream, t), tolerance, &format!("python file, seek to {t}"));
    }
    // Through range requests as well.
    let mut ranged = Player::ranged(file.len() as u64);
    let mut viewer = Viewer::default();
    for _ in 0..20 {
        let t = random.next() * end;
        ranged.seek(t);
        settle(&mut ranged, file, &mut viewer, &mut Vec::new());
        viewer.assert_shows(&Viewer::live(&stream, t), tolerance, &format!("python file, ranged seek to {t}"));
    }
    assert!(!ranged.stats().index_missing);
}

#[test]
fn a_recording_written_by_python_plays() {
    let path = fixture("python_synth.gsrec");
    let file = std::fs::read(&path).expect("tests/record_data/python_synth.gsrec");
    check_python_recording(&file);

    let Some(python) = python() else { return };
    let tool = repo_dir("../../tools/gsrec.py");
    let dir = std::env::temp_dir().join(format!("gsrec-python-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // The fixture is what the tool writes today.
    let again = dir.join("again.gsrec");
    let status = Command::new(python)
        .arg(&tool)
        .args(["synth", "--splats", "60", "--no-compress"])
        .arg(&again)
        .output()
        .unwrap();
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    assert!(std::fs::read(&again).unwrap() == file, "tools/gsrec.py synth no longer writes the fixture");
    // Another session, longer, with compressed payloads: the stream is
    // read from its uncompressed twin (same seed, same messages).
    for (name, extra) in [("packed.gsrec", &[][..]), ("plain.gsrec", &["--no-compress"][..])] {
        let status = Command::new(python)
            .arg(&tool)
            .args([
                "synth",
                "--seed",
                "9",
                "--seconds",
                "7",
                "--hz",
                "15",
                "--splats",
                "500",
                "--keyframe-every",
                "0.7",
            ])
            .args(extra)
            .arg(dir.join(name))
            .output()
            .unwrap();
        assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    }
    let packed = std::fs::read(dir.join("packed.gsrec")).unwrap();
    let plain = std::fs::read(dir.join("plain.gsrec")).unwrap();
    check_python_recording(&plain);
    let stream = stream_of(&plain);
    let mut player = Player::from_bytes(packed.clone()).unwrap();
    let mut viewer = Viewer::default();
    let mut random = Lcg(4);
    for _ in 0..60 {
        let t = random.next() * 7.0;
        player.seek(t);
        viewer.apply_all(&player.update());
        viewer.assert_shows(&Viewer::live(&stream, t), 12.0 / 65535.0, &format!("packed python file, seek to {t}"));
    }
    assert!(verify(&packed).unwrap().is_valid());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The file with another JSON header in front of the same records.
fn with_header(file: &[u8], json: &[u8]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(json);
    out.extend_from_slice(&file[12 + HEADER_JSON_SIZE..]);
    out
}

#[test]
fn python_and_rust_agree_on_every_file() {
    let stream = tiny_stream();
    let plain = record(&stream, uncompressed());
    let packed = record(&stream, tiny_options());
    let synth = std::fs::read(fixture("python_synth.gsrec")).unwrap();
    let mut cases: Vec<(String, Vec<u8>)> = vec![
        ("plain".into(), plain.clone()),
        ("packed".into(), packed.clone()),
        ("synth".into(), synth.clone()),
        ("nothing".into(), Vec::new()),
    ];
    cases.push(("a capture file".into(), [&b"GSCAP1\0\0"[..], &plain[8..]].concat()));
    cases.push(("version 2".into(), [&b"GSREC2\0\0"[..], &plain[8..]].concat()));
    cases.push(("three bytes of magic".into(), b"GSR".to_vec()));
    cases.push(("two other bytes".into(), b"PK".to_vec()));
    let mut random = Lcg(2026);
    let mut pick = |n: usize| (random.next() * n as f64) as usize;

    // Cut short: anywhere, and at every byte of the index and the trailer.
    for (name, file) in [("plain", &plain), ("packed", &packed), ("synth", &synth)] {
        for _ in 0..50 {
            let cut = pick(file.len());
            cases.push((format!("{name} cut at {cut}"), file[..cut].to_vec()));
        }
        for cut in file.len() - 150..file.len() {
            cases.push((format!("{name} cut at {cut}"), file[..cut].to_vec()));
        }
        cases.push((format!("{name} with a tail"), [file.as_slice(), b"more"].concat()));
    }
    // Bytes changed anywhere in a file without compressed payloads (the
    // two inflaters need not agree on what a damaged deflate stream is).
    for round in 0..400 {
        let mut file = if round % 4 == 0 { synth.clone() } else { plain.clone() };
        let mut name = String::from(if round % 4 == 0 { "synth" } else { "plain" });
        for _ in 0..1 + round % 2 {
            let at = if round % 5 == 0 { pick(1100.min(file.len())) } else { pick(file.len()) };
            let value = if round % 3 == 0 { pick(256) as u8 } else { file[at] ^ (1 << (round % 8)) };
            file[at] = value;
            name += &format!(", byte {at} = {value}");
        }
        cases.push((name, file));
    }
    // The framing of a file with compressed payloads.
    let records = records_of(&packed);
    for round in 0..120 {
        let mut file = packed.clone();
        let start = records[pick(records.len() - 1)].0 as usize;
        let at = start + pick(16);
        file[at] = if round % 2 == 0 { pick(256) as u8 } else { file[at] ^ (1 << (round % 8)) };
        cases.push((format!("packed, framing byte {at} = {}", file[at]), file));
    }
    // A compressed payload that is no deflate stream at all (a reserved
    // block type, in the first keyframe): both must say bad_deflate.
    let payload_at = 12 + HEADER_JSON_SIZE + 16;
    assert_eq!(packed[payload_at - 11], 1, "the first keyframe is compressed");
    let mut garbage = packed.clone();
    garbage[payload_at] = 0x07;
    cases.push(("packed, reserved deflate block".into(), garbage));
    // JSON headers.
    let deep = |n: usize| format!("{{\"a\":{}{}}}", "[".repeat(n), "]".repeat(n)).into_bytes();
    let headers: Vec<Vec<u8>> = [
        r#"{}"#,
        r#"[]"#,
        r#""name""#,
        r#"   {"name":"x","scene":"y","created":"z","protocol":1,"duration_s":6.0}   "#,
        r#"{"duration_s":1e999}"#,
        r#"{"duration_s":-1}"#,
        r#"{"duration_s":-0.0}"#,
        r#"{"duration_s":7}"#,
        r#"{"duration_s":true}"#,
        r#"{"duration_s":"6"}"#,
        r#"{"duration_s":123456789012345678901234567890}"#,
        r#"{"duration_s":NaN}"#,
        r#"{"duration_s":Infinity}"#,
        r#"{"protocol":1.0}"#,
        r#"{"protocol":-3}"#,
        r#"{"protocol":true}"#,
        r#"{"protocol":4294967295}"#,
        r#"{"protocol":4294967296}"#,
        r#"{"protocol":18446744073709551615}"#,
        r#"{"protocol":18446744073709551616}"#,
        r#"{"name":"a","name":"b"}"#,
        r#"{"name":7,"scene":null,"created":[]}"#,
        r#"{"name":"\ud800"}"#,
        r#"{"name":"😀 é"}"#,
        "{\"name\":\"tab\there\"}",
        r#"{"name":"x"} {}"#,
        r#"{"name":"x",}"#,
        r#"{'name':'x'}"#,
        "",
        "   ",
    ]
    .iter()
    .map(|text| text.as_bytes().to_vec())
    .chain([deep(100), deep(126), deep(140), deep(5000), b"{\"name\":\"\xff\"}".to_vec(), b"\xef\xbb\xbf{}".to_vec()])
    .collect();
    for (i, header) in headers.iter().enumerate() {
        cases.push((
            format!("header {i}: {}", String::from_utf8_lossy(&header[..header.len().min(60)])),
            with_header(&plain, header),
        ));
    }
    assert!(cases.len() > 1000);

    // Both checkers say something about every case, never panic; they
    // must say the same thing.
    let mine: Vec<Value> = cases.iter().map(|(_, file)| report_value(&verify(file))).collect();
    let codes = |value: &Value| -> Vec<String> {
        let problems = value.get("problems").and_then(Value::as_array).cloned().unwrap_or_default();
        problems
            .iter()
            .map(|p| p[1].as_str().unwrap().to_owned())
            .chain(value.get("error").and_then(Value::as_str).map(str::to_owned))
            .collect()
    };
    let seen: std::collections::BTreeSet<String> = mine.iter().flat_map(codes).collect();
    for code in [
        "too_short",
        "bad_magic",
        "bad_header",
        "bad_length",
        "bad_kind",
        "bad_flags",
        "bad_time",
        "bad_deflate",
        "bad_message",
        "bad_keyframe",
        "bad_index",
        "trailing_data",
        "empty",
    ] {
        assert!(seen.contains(code), "no case produced {code}: {seen:?}");
    }

    let Some(python) = python() else { return };
    let dir = std::env::temp_dir().join(format!("gsrec-agree-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let paths: Vec<PathBuf> = (0..cases.len()).map(|i| dir.join(format!("{i:04}.gsrec"))).collect();
    for (path, (_, file)) in paths.iter().zip(&cases) {
        std::fs::write(path, file).unwrap();
    }
    let output = Command::new(python)
        .arg(repo_dir("../../tools/gsrec.py"))
        .args(["verify", "--json"])
        .args(&paths)
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    let theirs: Vec<Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(theirs.len(), cases.len(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.status.code(), Some(1), "some of these files are damaged");
    let mut disagreements = 0;
    for ((name, _), (rust, python)) in cases.iter().zip(mine.iter().zip(&theirs)) {
        if !same(rust, python) {
            disagreements += 1;
            eprintln!("{name}:\n  rust   {rust}\n  python {python}");
        }
    }
    assert_eq!(disagreements, 0, "of {} files", cases.len());
}
