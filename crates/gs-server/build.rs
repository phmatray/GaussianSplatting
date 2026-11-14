//! Generates the table of frontend files compiled into the crate.
//!
//! With the `embed-dist` feature the web build is read from
//! `crates/gs-app/dist` (or from the directory named by `GS_DIST_DIR`) and
//! every file becomes an `include_bytes!` entry. Without the feature, or
//! when the frontend has not been built, the table is empty and the server
//! serves its placeholder page: the crate always compiles.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn collect(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Hidden files (Trunk's lock file, editor leftovers) are not part
        // of the site; the server would refuse to serve them anyway.
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let key = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        if path.is_dir() {
            collect(&path, &key, out);
        } else if path.is_file() {
            out.push((key, path));
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=GS_DIST_DIR");

    let mut files = Vec::new();
    if env::var_os("CARGO_FEATURE_EMBED_DIST").is_some() {
        let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
        let dist = match env::var_os("GS_DIST_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => manifest_dir.join("../gs-app/dist"),
        };
        // A missing path makes cargo run this script again on every build,
        // so the frontend is picked up as soon as it exists.
        println!("cargo:rerun-if-changed={}", dist.display());
        match dist.canonicalize() {
            Ok(dist) if dist.join("index.html").is_file() => collect(&dist, "", &mut files),
            _ => println!(
                "cargo:warning=embed-dist: no web build in {} (run `trunk build --release` in crates/gs-app); \
                 the server will show its placeholder page",
                dist.display()
            ),
        }
        files.sort();
    }

    let mut table = String::from("&[\n");
    for (key, path) in &files {
        let _ = writeln!(table, "    ({key:?}, include_bytes!({:?}) as &[u8]),", path.display().to_string());
    }
    table.push_str("]\n");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("set by cargo")).join("embedded_dist.rs");
    fs::write(out, table).expect("cannot write the embedded file table");
}
