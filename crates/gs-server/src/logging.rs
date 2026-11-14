//! A minimal `log` backend for the headless binary and the desktop shell.

use std::io::Write;
use std::time::Instant;

use log::{LevelFilter, Log, Metadata, Record};

struct Stderr {
    start: Instant,
}

impl Log for Stderr {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        // The detailed level is for this project's crates only; the
        // dependencies would drown them.
        let ours = metadata.target().starts_with("gs_") || metadata.target().starts_with("splat_");
        ours || metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            let t = self.start.elapsed().as_secs_f64();
            // A closed stderr is not worth a panic.
            let _ = writeln!(std::io::stderr().lock(), "[{t:9.3}s {:5}] {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

/// Prints log messages on stderr: warnings and errors (in French, they are
/// meant for the user), plus the English diagnostics of the server
/// (listeners, connections, requests) with `verbose`. Does nothing when the
/// application has already installed a logger.
pub fn log_to_stderr(verbose: bool) {
    let logger = Box::new(Stderr { start: Instant::now() });
    if log::set_boxed_logger(logger).is_ok() {
        log::set_max_level(if verbose { LevelFilter::Debug } else { LevelFilter::Warn });
    }
}
