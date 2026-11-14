//! Splat Live desktop application.
//!
//! A thin Tauri 2 shell: it starts the capture server in-process, opens the
//! main window on the bundled frontend and exposes one command,
//! `server_info`. The frontend then talks to the server over the loopback
//! HTTP listener and its WebSocket, exactly like a browser tab would.

// No console window next to the application on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use gs_server::{ServerConfig, ServerHandle, ServerInfo};
use tauri::{Manager, RunEvent, State};

/// The same JSON as `GET /api/info`. The frontend calls it first and uses
/// `http_url` as the base of everything else.
#[tauri::command]
fn server_info(server: State<'_, ServerHandle>) -> ServerInfo {
    log::debug!("server_info command called by the window");
    server.info()
}

/// Self-report of the window for the end-to-end check
/// (`e2e/server/tauri-real.sh`): nothing else can look inside a WebKitGTK
/// webview that runs under Xvfb. Compiled in only with the `probe` feature.
#[cfg(feature = "probe")]
mod probe {
    use std::time::Duration;

    use tauri::Manager;

    /// Summarises what the interface shows (its `window.__splatLive` hook)
    /// and hands it to the `probe_report` command. Plain JavaScript without
    /// any dependency on the page, so that a page that failed to start is
    /// reported as such instead of not at all.
    const SCRIPT: &str = r#"(() => {
  const report = { origin: location.origin, title: document.title };
  try {
    const gl = document.createElement('canvas').getContext('webgl2');
    report.webgl2 = gl ? String(gl.getParameter(gl.VERSION)) : null;
    const badge = document.querySelector('#source-badge');
    report.badge = badge ? badge.innerText.trim() : null;
    const hook = window.__splatLive;
    const s = hook ? hook.state() : null;
    report.hook = !!s;
    if (s) {
      report.source = s.source;
      report.link = s.link;
      report.scene = s.scene;
      report.layers = s.layers;
      report.counts = { messages: s.counts.messages, layers: s.counts.layers, previews: s.counts.previews, stats: s.counts.stats };
      report.viewer = { frames: s.viewer.framesRendered, drawn: s.viewer.splatsDrawn, fps: s.viewer.fps, canvas: [s.viewer.canvasWidth, s.viewer.canvasHeight] };
      report.server = s.server ? { http_url: s.server.http_url, clients: s.server.clients } : null;
      report.errors = s.errors;
    }
  } catch (e) {
    report.exception = String(e);
  }
  window.__TAURI__.core.invoke('probe_report', { report: JSON.stringify(report) });
})();"#;

    #[tauri::command]
    pub fn probe_report(report: String) {
        log::info!("probe: {report}");
    }

    /// Asks the main window for a report every second.
    pub fn start(app: &tauri::AppHandle) {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.eval(SCRIPT);
                }
            }
        });
    }
}

/// Server settings that differ between a development run and a release.
fn server_config(app: &tauri::App) -> ServerConfig {
    let mut config = ServerConfig {
        // The certificate lives with the application data, so the headset
        // keeps trusting it from one run to the next.
        cert_dir: app.path().app_data_dir().ok(),
        ..ServerConfig::default()
    };
    if tauri::is_dev() {
        // The window shows the development server (`devUrl`), a different
        // origin than the embedded server: allow it. Headsets still get a
        // web build from the server if one has been built.
        if let Some(url) = &app.config().build.dev_url {
            config.extra_origins.push(url.origin().ascii_serialization());
        }
        let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../crates/gs-app/dist");
        if dist.join("index.html").is_file() {
            config.dist = Some(dist);
        }
    }
    config
}

/// Resolves when the process is asked to stop from outside the window
/// (Ctrl-C in the terminal it was started from, or a termination signal).
async fn stop_requested() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = interrupt => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    let _ = interrupt.await;
}

fn main() {
    gs_server::log_to_stderr(std::env::var_os("SPLAT_LIVE_VERBOSE").is_some());

    let builder = tauri::Builder::default().setup(|app| {
        let config = server_config(app);
        // The listeners run on Tauri's own Tokio runtime.
        let server = tauri::async_runtime::block_on(gs_server::start(config))?;
        log::info!("server ready on {}", server.info().http_url);
        app.manage(server);
        // A signal ends the application the same way as closing the
        // window does, so that the server is shut down properly.
        let handle = app.handle().clone();
        tauri::async_runtime::spawn(async move {
            stop_requested().await;
            handle.exit(0);
        });
        #[cfg(feature = "probe")]
        probe::start(app.handle());
        Ok(())
    });
    #[cfg(not(feature = "probe"))]
    let builder = builder.invoke_handler(tauri::generate_handler![server_info]);
    #[cfg(feature = "probe")]
    let builder = builder.invoke_handler(tauri::generate_handler![server_info, probe::probe_report]);
    let app = match builder.build(tauri::generate_context!()) {
        Ok(app) => app,
        Err(err) => {
            // No window to show it in: the server could not start (every
            // port taken) or the webview is missing.
            eprintln!("Splat Live : impossible de démarrer l'application : {err}");
            std::process::exit(1);
        }
    };

    app.run(|app, event| {
        if let RunEvent::Exit = event {
            // Viewers get a clean close and the pipeline thread is joined
            // before the process ends.
            if let Some(server) = app.try_state::<ServerHandle>() {
                tauri::async_runtime::block_on(server.shutdown());
                log::info!("server stopped");
            }
        }
    });
}
