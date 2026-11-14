//! Stamps the build with an identifier used to version the URL of the
//! worker files, which Trunk does not content-hash (see `worker-boot.js`).

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // Anything that can change the worker must refresh the identifier.
    for path in ["build.rs", "src", "worker-boot.js", "../gs-worker/src", "../gs-core/src"] {
        println!("cargo:rerun-if-changed={path}");
    }
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    println!("cargo:rustc-env=GS_APP_BUILD_ID={seconds:x}");
}
