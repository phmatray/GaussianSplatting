//! Splat Live desktop application.
//!
//! A thin Tauri 2 shell: it starts the capture server in-process and opens
//! the main window on the bundled frontend. The frontend then talks to the
//! server over the loopback HTTP listener and its WebSocket, exactly like a
//! browser tab would. Three commands cover what a web view cannot do by
//! itself:
//!
//! * `server_info`: where the embedded server listens;
//! * `export_recording`: writes a recording, or its share archive, into
//!   the user's download folder (a web view has no download manager);
//! * `reveal`: shows a folder or an exported file in the file manager.

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

/// What `export_recording` writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExportKind {
    /// The `.gsrec` file itself.
    Recording,
    /// The share archive: the web build with the recording inside.
    Share,
}

/// Where exported files go: the user's download folder, or a folder of
/// the application's own when the system has none.
fn export_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = match app.path().download_dir() {
        Ok(dir) => dir,
        Err(_) => app
            .path()
            .app_data_dir()
            .map_err(|err| format!("Aucun dossier où enregistrer le fichier : {err}"))?
            .join("exports"),
    };
    std::fs::create_dir_all(&dir).map_err(|err| format!("Impossible de créer le dossier {} : {err}", dir.display()))?;
    Ok(dir)
}

/// A name of the recordings list, and nothing that could leave a folder.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('.')
        && !name.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
}

/// `<dir>/<stem>.<ext>`, or `<stem> (2).<ext>`, `<stem> (3).<ext>`... when
/// the name is taken: an export never overwrites a file of the user's.
fn free_path(dir: &std::path::Path, stem: &str, ext: &str) -> PathBuf {
    let mut path = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while path.exists() && n < 1000 {
        path = dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    path
}

/// Writes the recording `name` (as `GET /api/recordings` lists it) or its
/// share archive into the download folder and returns the path of the new
/// file. Errors are sentences for the user.
#[tauri::command]
async fn export_recording(
    app: tauri::AppHandle,
    server: State<'_, ServerHandle>,
    name: String,
    kind: ExportKind,
) -> Result<String, String> {
    if !is_plain_name(&name) {
        return Err("Nom d'enregistrement invalide.".to_string());
    }
    let dir = export_dir(&app)?;
    let server = server.inner().clone();
    // Gigabytes may be copied: not on the thread that draws the window.
    let written = tauri::async_runtime::spawn_blocking(move || {
        let path = free_path(&dir, &name, if kind == ExportKind::Share { "zip" } else { "gsrec" });
        let result = match kind {
            ExportKind::Recording => server.export_recording(&name, &path),
            ExportKind::Share => server.export_share(&name, &path),
        };
        match result {
            Ok(bytes) => {
                log::info!("exported {kind:?} of {name} to {} ({bytes} bytes)", path.display());
                Ok(path)
            }
            Err(err) => {
                // Nothing half written is left in the user's folder.
                let _ = std::fs::remove_file(&path);
                Err(match err.kind() {
                    std::io::ErrorKind::NotFound => "Cet enregistrement n'existe plus.".to_string(),
                    std::io::ErrorKind::ResourceBusy => "Cet enregistrement est encore en cours d'écriture.".to_string(),
                    _ => format!("L'export a échoué : {err}"),
                })
            }
        }
    })
    .await
    .map_err(|err| format!("L'export a échoué : {err}"))??;
    Ok(written.display().to_string())
}

/// Shows something in the file manager: `recordings`, `captures` or
/// `exports` (the folders), or the path of a file `export_recording`
/// returned. Nothing else is opened, whatever the window asks.
#[tauri::command]
fn reveal(app: tauri::AppHandle, server: State<'_, ServerHandle>, target: String) -> Result<(), String> {
    let missing = || "Ce dossier n'existe pas encore.".to_string();
    let (path, select) = match target.as_str() {
        "recordings" => (server.recordings_dir().ok_or_else(missing)?, false),
        "captures" => (server.captures_dir().ok_or_else(missing)?, false),
        "exports" => (export_dir(&app)?, false),
        file => {
            // Only a file that sits directly in the export folder.
            let dir = export_dir(&app)?.canonicalize().map_err(|_| missing())?;
            let path = std::path::Path::new(file).canonicalize().map_err(|_| "Ce fichier n'existe plus.".to_string())?;
            if path.parent() != Some(dir.as_path()) || !path.is_file() {
                return Err("Ce fichier ne peut pas être affiché.".to_string());
            }
            (path, true)
        }
    };
    if !path.exists() {
        return Err(missing());
    }
    open_in_file_manager(&path, select).map_err(|err| format!("Impossible d'ouvrir le gestionnaire de fichiers : {err}"))
}

/// `canonicalize` answers with a verbatim path on Windows (`\\?\C:\…`),
/// which Explorer does not take (it opens the user's documents instead):
/// the plain form of the same path. Network paths (`\\?\UNC\…`) and
/// everything else are left as they are.
fn without_verbatim_prefix(path: &std::path::Path) -> PathBuf {
    match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

/// Opens the system's file manager on `path`; with `select`, on its folder
/// with the file selected where the platform can do that.
fn open_in_file_manager(path: &std::path::Path, select: bool) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut command;
    if cfg!(target_os = "macos") {
        command = Command::new("open");
        if select {
            command.arg("-R");
        }
        command.arg(path);
    } else if cfg!(windows) {
        let path = without_verbatim_prefix(path);
        command = Command::new("explorer");
        if select {
            let mut argument = std::ffi::OsString::from("/select,");
            argument.push(&path);
            command.arg(argument);
        } else {
            command.arg(&path);
        }
    } else {
        // No portable way to select a file: its folder is shown.
        command = Command::new("xdg-open");
        command.arg(if select { path.parent().unwrap_or(path) } else { path });
    }
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(|_| ())
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
    let data = app.path().app_data_dir().ok();
    let mut config = ServerConfig {
        // Recordings (and, next to them, the raw captures of a phone) are
        // kept with the application data.
        recordings_dir: data.as_ref().map(|dir| dir.join("recordings")),
        // So does the certificate, so that the headset and the phone keep
        // trusting it from one run to the next.
        cert_dir: data,
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
    let builder = builder.invoke_handler(tauri::generate_handler![server_info, export_recording, reveal]);
    #[cfg(feature = "probe")]
    let builder =
        builder.invoke_handler(tauri::generate_handler![server_info, export_recording, reveal, probe::probe_report]);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_names_are_exported() {
        for good in ["Salon", "Danse-du-salon-2", "été 1", "v1.2"] {
            assert!(is_plain_name(good), "{good}");
        }
        for bad in ["", ".caché", "../x", "a/b", "a\\b", "c:x", "a\0b", "a\nb", "x?", "<x>", &"a".repeat(201)] {
            assert!(!is_plain_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn an_export_never_takes_the_name_of_an_existing_file() {
        let dir = std::env::temp_dir().join(format!("splat-live-export-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(free_path(&dir, "Salon", "zip"), dir.join("Salon.zip"));
        std::fs::write(dir.join("Salon.zip"), b"x").unwrap();
        assert_eq!(free_path(&dir, "Salon", "zip"), dir.join("Salon (2).zip"));
        std::fs::write(dir.join("Salon (2).zip"), b"x").unwrap();
        assert_eq!(free_path(&dir, "Salon", "zip"), dir.join("Salon (3).zip"));
        assert_eq!(free_path(&dir, "Salon", "gsrec"), dir.join("Salon.gsrec"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn explorer_gets_a_plain_path() {
        let plain = |text: &str| without_verbatim_prefix(std::path::Path::new(text));
        assert_eq!(plain(r"\\?\C:\Users\a\Downloads\x.zip"), PathBuf::from(r"C:\Users\a\Downloads\x.zip"));
        assert_eq!(plain(r"C:\Users\a\x.zip"), PathBuf::from(r"C:\Users\a\x.zip"));
        assert_eq!(plain(r"\\?\UNC\server\share\x.zip"), PathBuf::from(r"\\?\UNC\server\share\x.zip"));
        assert_eq!(plain("/home/a/x.zip"), PathBuf::from("/home/a/x.zip"));
    }
}
