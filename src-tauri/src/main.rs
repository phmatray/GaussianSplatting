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
    server.info()
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

    let app = tauri::Builder::default()
        .setup(|app| {
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
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![server_info])
        .build(tauri::generate_context!())
        .expect("cannot start the Splat Live window");

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
