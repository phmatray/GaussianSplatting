//! Headless Splat Live server.
//!
//! ```text
//! gs-server [--port N] [--lan] [--lan-port N] [--dist DIR]
//!           [--scene stage|house] [--light] [--cert-dir DIR] [--verbose]
//! ```
//!
//! Prints the URLs to open and, with `--lan`, the QR code and the
//! certificate fingerprint for the headset. Ctrl-C stops it cleanly.

use std::path::PathBuf;
use std::process::ExitCode;

use gs_core::protocol::PipelineConfig;
use gs_core::scene::SceneKind;
use gs_server::{ServerConfig, ServerHandle, DEFAULT_LAN_PORT, DEFAULT_PORT};

const USAGE: &str = "\
Splat Live : serveur de capture et de diffusion (sans fenêtre)

Utilisation :
  gs-server [options]

Options :
  --port N           port HTTP local, sur 127.0.0.1 (défaut : 7878 ; le port
                     libre suivant est pris s'il est occupé)
  --lan              ouvre aussi l'accès réseau en HTTPS pour un casque VR
  --lan-port N       port HTTPS de l'accès réseau (défaut : 7879)
  --dist DIR         dossier de l'interface web (crates/gs-app/dist) ; par
                     défaut l'interface intégrée au binaire, sinon
                     crates/gs-app/dist s'il existe dans le dossier courant
  --scene NOM        scène de départ : stage (scène de danse) ou house (maison)
  --light            réglages allégés pour une machine modeste
  --cert-dir DIR     dossier où conserver le certificat auto-signé
                     (défaut : le dossier de données de l'utilisateur)
  --verbose          journal détaillé (diagnostics en anglais)
  -h, --help         affiche cette aide
  -V, --version      affiche la version
";

/// Where the web build lands when it is built in place.
const LOCAL_DIST: &str = "crates/gs-app/dist";

struct Args {
    port: u16,
    lan: bool,
    lan_port: u16,
    dist: Option<PathBuf>,
    scene: SceneKind,
    light: bool,
    cert_dir: Option<PathBuf>,
    verbose: bool,
}

enum Parsed {
    Run(Args),
    Help,
    Version,
}

fn parse_args(mut argv: impl Iterator<Item = String>) -> Result<Parsed, String> {
    let mut args = Args {
        port: DEFAULT_PORT,
        lan: false,
        lan_port: DEFAULT_LAN_PORT,
        dist: None,
        scene: SceneKind::Stage,
        light: false,
        cert_dir: None,
        verbose: false,
    };
    while let Some(arg) = argv.next() {
        // Both `--port 8000` and `--port=8000`.
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag.to_string(), Some(value.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |what: &str| {
            inline.clone().or_else(|| argv.next()).ok_or_else(|| format!("{flag} attend une valeur ({what})"))
        };
        let port = |text: String| text.parse::<u16>().map_err(|_| format!("port invalide : {text}"));
        match flag.as_str() {
            "--port" => args.port = port(value("un port")?)?,
            "--lan-port" => args.lan_port = port(value("un port")?)?,
            "--dist" => args.dist = Some(PathBuf::from(value("un dossier")?)),
            "--cert-dir" => args.cert_dir = Some(PathBuf::from(value("un dossier")?)),
            "--scene" => {
                args.scene = match value("stage ou house")?.as_str() {
                    "stage" => SceneKind::Stage,
                    "house" => SceneKind::House,
                    other => return Err(format!("scène inconnue : {other} (stage ou house)")),
                }
            }
            "--lan" => args.lan = true,
            "--light" => args.light = true,
            "--verbose" => args.verbose = true,
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            other => return Err(format!("option inconnue : {other}")),
        }
    }
    Ok(Parsed::Run(args))
}

/// The per-user data directory of the platform, with `splat-live` appended.
fn default_cert_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) {
        var("APPDATA")
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|home| home.join("Library/Application Support"))
    } else {
        var("XDG_DATA_HOME").or_else(|| var("HOME").map(|home| home.join(".local/share")))
    };
    base.map(|dir| dir.join("splat-live"))
}

fn print_banner(server: &ServerHandle, args: &Args, dist: Option<&PathBuf>) {
    let info = server.info();
    println!("Splat Live {} (protocole {})", info.version, info.protocol);
    println!();
    println!("  Interface locale : {}", info.http_url);
    println!("  Scène de départ  : {}", if args.scene == SceneKind::Stage { "stage" } else { "house" });
    match dist {
        Some(dir) => println!("  Interface web    : dossier {}", dir.display()),
        None if gs_server::has_embedded_frontend() => println!("  Interface web    : intégrée au binaire"),
        None => {
            println!("  Interface web    : absente, une page d'explication est servie à la place");
            println!("                     (cd crates/gs-app && trunk build --release, puis --dist {LOCAL_DIST})");
        }
    }
    if !info.lan.enabled {
        println!("  Accès réseau     : désactivé (relancer avec --lan pour un casque VR)");
    } else {
        println!("  Accès réseau     : HTTPS avec un certificat auto-signé");
        if info.lan.urls.is_empty() {
            println!("                     aucune adresse réseau trouvée sur cette machine");
        }
        for url in &info.lan.urls {
            println!("                     {url}");
        }
        println!();
        println!("  Empreinte SHA-256 du certificat, à comparer avec celle affichée par le casque :");
        // Two lines of 16 bytes read better than one of 95 characters.
        let bytes: Vec<&str> = info.lan.fingerprint.split(':').collect();
        for line in bytes.chunks(16) {
            println!("    {}", line.join(":"));
        }
        if let Some(url) = info.lan.urls.first() {
            if let Some(qr) = gs_server::qr_terminal(url) {
                println!();
                for line in qr.lines() {
                    println!("  {line}");
                }
                println!();
                println!("  Scannez ce code avec le casque (ou ouvrez {url}),");
                println!("  acceptez le certificat une fois, puis choisissez « Entrer en VR ».");
            }
        }
    }
    println!();
    println!("Ctrl-C pour arrêter.");
}

/// Resolves when the process is asked to stop.
async fn stop_requested() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = interrupt => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = interrupt.await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = interrupt.await;
    }
}

async fn run(args: Args) -> Result<(), gs_server::Error> {
    // Without `--dist` and without a compiled-in build, a build made in
    // place is the obvious thing to serve.
    let dist = args.dist.clone().or_else(|| {
        let local = PathBuf::from(LOCAL_DIST);
        (!gs_server::has_embedded_frontend() && local.join("index.html").is_file()).then_some(local)
    });
    let pipeline = if args.light { PipelineConfig::light(args.scene) } else { PipelineConfig::for_scene(args.scene) };
    let config = ServerConfig {
        pipeline,
        port: args.port,
        lan: args.lan,
        lan_port: args.lan_port,
        dist: dist.clone(),
        cert_dir: args.cert_dir.clone().or_else(default_cert_dir),
        ..ServerConfig::default()
    };
    let server = gs_server::start(config).await?;
    print_banner(&server, &args, dist.as_ref());

    stop_requested().await;
    println!();
    println!("Arrêt en cours...");
    server.shutdown().await;
    println!("Serveur arrêté.");
    Ok(())
}

/// The error in the language of the rest of the output.
fn describe(error: &gs_server::Error) -> String {
    match error {
        gs_server::Error::Bind { addr, source } => format!("impossible d'écouter sur {addr} : {source}"),
        gs_server::Error::Certificate(reason) => format!("impossible de préparer le certificat TLS : {reason}"),
        gs_server::Error::Io(source) => format!("erreur d'entrée-sortie : {source}"),
        gs_server::Error::Closed => "le serveur est déjà arrêté".to_string(),
    }
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(Parsed::Run(args)) => args,
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            println!("gs-server {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("gs-server : {message}");
            eprintln!("Essayez « gs-server --help ».");
            return ExitCode::from(2);
        }
    };
    gs_server::log_to_stderr(args.verbose);

    // Two threads are plenty for the sockets; the heavy work happens on the
    // pipeline thread and its rayon pool.
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("gs-server : impossible de démarrer le moteur asynchrone : {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("gs-server : {}", describe(&err));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Parsed, String> {
        parse_args(line.split_whitespace().map(str::to_string))
    }

    #[test]
    fn defaults() {
        let Ok(Parsed::Run(args)) = parse("") else { panic!("expected a run") };
        assert_eq!((args.port, args.lan_port, args.lan, args.light), (7878, 7879, false, false));
        assert_eq!(args.scene, SceneKind::Stage);
        assert!(args.dist.is_none() && args.cert_dir.is_none());
    }

    #[test]
    fn every_flag() {
        let line = "--port 8000 --lan --lan-port=8443 --dist web --scene house --light --cert-dir /tmp/c --verbose";
        let Ok(Parsed::Run(args)) = parse(line) else { panic!("expected a run") };
        assert_eq!((args.port, args.lan_port, args.lan, args.light, args.verbose), (8000, 8443, true, true, true));
        assert_eq!(args.scene, SceneKind::House);
        assert_eq!(args.dist, Some(PathBuf::from("web")));
        assert_eq!(args.cert_dir, Some(PathBuf::from("/tmp/c")));
    }

    #[test]
    fn mistakes_are_reported() {
        assert!(parse("--port").is_err());
        assert!(parse("--port 70000").is_err());
        assert!(parse("--scene moon").is_err());
        assert!(parse("--frobnicate").is_err());
        assert!(matches!(parse("--help"), Ok(Parsed::Help)));
        assert!(matches!(parse("-V"), Ok(Parsed::Version)));
    }

    #[test]
    fn user_facing_text_has_no_em_dash() {
        assert!(!USAGE.contains('\u{2014}') && !USAGE.contains('\u{2013}'));
    }
}
