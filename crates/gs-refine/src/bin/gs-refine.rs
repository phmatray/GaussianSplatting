//! `gs-refine`: the offline mode of Splat Live on the command line.
//!
//! ```text
//! gs-refine <capture.gscap> -o <sortie.ply> [--preset rapide|standard|fin] [options]
//! gs-refine info <capture.gscap>
//! gs-refine export <capture.gscap> --format colmap|nerfstudio -o <dossier>
//! ```
//!
//! Messages are in French; `gs-refine aide` lists the options.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use gs_refine::dataset::{select_views, Capture};
use gs_refine::export::{export_dataset, DatasetFormat};
use gs_refine::job::{CancelToken, Preset, Progress, RefineConfig, RefineError, RefineJob, Stage};

const HELP: &str = "\
gs-refine : mode hors ligne de Splat Live. Optimise des gaussiennes sur les
photographies d'une capture enregistrée (.gscap), à partir du nuage fusionné
en temps réel.

Usage :
  gs-refine <capture.gscap> -o <sortie.ply> [options]
  gs-refine info <capture.gscap>
  gs-refine export <capture.gscap> --format colmap|nerfstudio -o <dossier>
  gs-refine aide

Options de l'optimisation :
  -o, --sortie <fichier.ply>   fichier 3DGS écrit (lisible par la visionneuse)
  --preset <nom>               rapide, standard (défaut) ou fin
  --iterations <n>             nombre d'itérations (défaut : selon le preset et
                               le nombre de vues)
  --largeur <px>               largeur des images à la dernière étape
  --gaussiennes <n>            nombre maximal de gaussiennes
  --vues <n>                   nombre maximal de vues d'entraînement
  --evaluation <n>             une vue sur n est mise de côté (0 : aucune)
  --threads <n>                nombre de fils d'exécution (défaut : tous)
  --poses                      recale aussi la pose de chaque image (à réserver
                               aux captures dont les poses sont bruitées :
                               sur une capture propre le résultat est moins bon)
  --sans-exposition            ne corrige pas l'exposition par image
  --mesure-seule               n'ajuste les photos que là où le capteur de
                               profondeur a mesuré la surface : ce qui est hors
                               de sa portée (pièce voisine, vue par la fenêtre)
                               reste un trou au lieu d'être peint en halos
  --graine <n>                 graine des tirages aléatoires
  --splat                      écrit aussi un fichier .splat
  --reprise <fichier>          reprend une optimisation interrompue
  --voxel <m>                  taille des cellules de la fusion de départ
  --cache <Mo>                 mémoire pour les photos décodées (défaut : 1024)
  --set <chemin=valeur>        règle un paramètre avancé (voir le rapport JSON)
  --silencieux                 n'affiche pas la progression

À côté de la sortie sont écrits un rapport <sortie>.json et, tant que
l'optimisation n'est pas terminée, un point de reprise <sortie>.reprise.
Ctrl-C arrête proprement et garde le meilleur résultat obtenu.";

/// The arguments after the program name, with simple lookups.
struct Args(Vec<String>);

impl Args {
    fn flag(&self, names: &[&str]) -> bool {
        self.0.iter().any(|a| names.contains(&a.as_str()))
    }

    fn value(&self, names: &[&str]) -> Option<&str> {
        self.0.iter().position(|a| names.contains(&a.as_str())).and_then(|i| self.0.get(i + 1)).map(String::as_str)
    }

    fn number<T: std::str::FromStr>(&self, names: &[&str]) -> Result<Option<T>, String> {
        match self.value(names) {
            None => Ok(None),
            Some(text) => text.parse().map(Some).map_err(|_| format!("valeur incorrecte pour {} : {text}", names[0])),
        }
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.0.windows(2).filter(|w| w[0] == name).map(|w| w[1].as_str()).collect()
    }

    /// The first argument that is neither an option nor an option's value.
    fn positional(&self, skip: usize) -> Option<&str> {
        let mut i = skip;
        while i < self.0.len() {
            let a = self.0[i].as_str();
            if WITH_VALUE.contains(&a) {
                i += 2;
            } else if a.starts_with('-') {
                i += 1;
            } else {
                return Some(a);
            }
        }
        None
    }

    /// Refuses what the tool would otherwise drop in silence: an option it
    /// does not know, an option without its value, a second file. A
    /// misspelt `--iterations` must not start the default run instead.
    fn check(&self, skip: usize) -> Result<(), String> {
        let (mut i, mut files) = (skip, 0);
        while i < self.0.len() {
            let a = self.0[i].as_str();
            if WITH_VALUE.contains(&a) {
                if i + 1 >= self.0.len() {
                    return Err(format!("il manque une valeur après {a}"));
                }
                i += 2;
            } else if FLAGS.contains(&a) {
                i += 1;
            } else if a.starts_with('-') {
                return Err(format!("option inconnue : {a}. « gs-refine aide » liste les options."));
            } else {
                files += 1;
                if files > 1 {
                    return Err(format!("argument en trop : {a}"));
                }
                i += 1;
            }
        }
        Ok(())
    }
}

/// Options that take a value, and those that do not.
const WITH_VALUE: [&str; 15] =
    ["-o", "--sortie", "--preset", "--iterations", "--largeur", "--gaussiennes", "--vues", "--evaluation", "--threads", "--graine", "--reprise", "--set", "--format", "--voxel", "--cache"];
const FLAGS: [&str; 5] = ["--poses", "--sans-exposition", "--mesure-seule", "--splat", "--silencieux"];

fn french_number(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

fn decimal(value: f64, places: usize) -> String {
    format!("{value:.places$}").replace('.', ",")
}

fn duration(seconds: f64) -> String {
    let s = seconds.max(0.0).round() as u64;
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min {:02} s", s / 60, s % 60),
        _ => format!("{} h {:02} min", s / 3600, s % 3600 / 60),
    }
}

fn megabytes(bytes: u64) -> String {
    format!("{} Mo", decimal(bytes as f64 / 1.0e6, 1))
}

/// A French sentence for an error.
fn explain(error: &RefineError) -> String {
    match error {
        RefineError::Dataset(e) => format!("capture illisible ou inutilisable : {e}"),
        RefineError::Io(e) => format!("erreur de lecture ou d'écriture : {e}"),
        RefineError::Checkpoint(why) => format!("point de reprise inutilisable : {why}"),
        RefineError::Empty(what) => format!("rien à optimiser : {what}"),
        RefineError::Cancelled => "interrompu avant le début de l'optimisation".to_string(),
    }
}

/// Applies `--set path=value` assignments to the configuration through its
/// JSON form.
fn patch(config: &mut RefineConfig, assignments: &[&str]) -> Result<(), String> {
    let mut value = serde_json::to_value(&*config).map_err(|e| e.to_string())?;
    for assignment in assignments {
        let (path, text) = assignment.split_once('=').ok_or_else(|| format!("--set {assignment} : la forme attendue est chemin=valeur"))?;
        let new: serde_json::Value = serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_string()));
        let mut at = &mut value;
        for key in path.split('.') {
            at = at.get_mut(key).ok_or_else(|| format!("--set {path} : le paramètre {key} n'existe pas"))?;
        }
        *at = new;
    }
    *config = serde_json::from_value(value).map_err(|e| format!("--set : {e}"))?;
    Ok(())
}

fn configuration(args: &Args) -> Result<RefineConfig, String> {
    let preset = match args.value(&["--preset"]) {
        None => Preset::Standard,
        Some(name) => Preset::from_name(name).ok_or_else(|| format!("preset inconnu : {name} (rapide, standard ou fin)"))?,
    };
    let mut config = RefineConfig::preset(preset);
    if let Some(n) = args.number(&["--iterations"])? {
        config.train.iterations = n;
    }
    if let Some(n) = args.number(&["--largeur"])? {
        config.train.width = n;
    }
    if let Some(n) = args.number(&["--gaussiennes"])? {
        config.train.max_gaussians = n;
    }
    if let Some(n) = args.number(&["--vues"])? {
        config.select.max_views = n;
    }
    if let Some(n) = args.number(&["--evaluation"])? {
        config.select.holdout_every = n;
    }
    if let Some(n) = args.number(&["--threads"])? {
        config.threads = n;
    }
    if let Some(n) = args.number(&["--graine"])? {
        config.train.seed = n;
    }
    if let Some(n) = args.number(&["--cache"])? {
        config.cache_mb = n;
    }
    if let Some(v) = args.number(&["--voxel"])? {
        config.init.voxel_size = v;
    }
    config.train.pose |= args.flag(&["--poses"]);
    config.train.exposure &= !args.flag(&["--sans-exposition"]);
    config.train.photo_beyond_sensor &= !args.flag(&["--mesure-seule"]);
    patch(&mut config, &args.all("--set"))?;
    Ok(config)
}

/// One line of progress, rewritten in place.
struct Reporter {
    quiet: bool,
    stage: Option<Stage>,
    last_width: usize,
}

impl Reporter {
    fn line(&mut self, text: &str) {
        if self.quiet {
            return;
        }
        let padding = self.last_width.saturating_sub(text.chars().count());
        eprint!("\r{text}{}", " ".repeat(padding));
        let _ = std::io::stderr().flush();
        self.last_width = text.chars().count();
    }

    fn end_line(&mut self) {
        if !self.quiet && self.last_width > 0 {
            eprintln!();
        }
        self.last_width = 0;
    }

    fn report(&mut self, p: &Progress) {
        if self.stage.is_some_and(|s| s != p.stage) {
            self.end_line();
        }
        self.stage = Some(p.stage);
        match p.stage {
            Stage::Select => self.line(&format!("Lecture de la capture et choix des vues ({} images)", french_number(p.total as usize))),
            Stage::Fuse => self.line(&format!("Fusion du nuage de départ : image {} sur {}", french_number(p.done as usize), french_number(p.total as usize))),
            Stage::Exposure => self.line(&format!("Mesure de l'exposition des vues : {} sur {}", french_number(p.done as usize), french_number(p.total as usize))),
            Stage::Poses => self.line(&format!("Recalage des poses : vue {} sur {}", french_number(p.done as usize), french_number(p.total as usize))),
            Stage::Evaluate => self.line("Évaluation sur les vues mises de côté"),
            Stage::Optimise => {
                let percent = 100.0 * p.iteration as f64 / p.iterations.max(1) as f64;
                let mut text = format!(
                    "Optimisation : {} / {} ({} %), {} px, {} gaussiennes, perte {}",
                    french_number(p.iteration as usize),
                    french_number(p.iterations as usize),
                    decimal(percent, 0),
                    p.width,
                    french_number(p.gaussians),
                    decimal(p.loss as f64, 4)
                );
                if let Some(psnr) = p.psnr {
                    text.push_str(&format!(", PSNR {} dB", decimal(psnr as f64, 1)));
                }
                if let Some(eta) = p.eta_s {
                    text.push_str(&format!(", reste {}", duration(eta)));
                }
                self.line(&text);
            }
        }
    }
}

/// Writes a file through a temporary one beside it, so that a second
/// Ctrl-C (which quits at once) never leaves half a result under its name.
fn write_whole(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)).map_err(|e| format!("impossible d'écrire {} : {e}", path.display()))
}

fn refine(args: &Args) -> Result<ExitCode, String> {
    let quiet = args.flag(&["--silencieux"]);
    let resume = args.value(&["--reprise"]).map(PathBuf::from);
    let capture_path = args.positional(0).map(PathBuf::from);
    let cancel = CancelToken::default();
    {
        let cancel = cancel.clone();
        let _ = ctrlc::set_handler(move || {
            if cancel.is_cancelled() {
                // A second Ctrl-C: the user does not want to wait.
                std::process::exit(130);
            }
            cancel.cancel();
            eprintln!("\nArrêt demandé : fin de l'itération en cours, puis écriture du meilleur résultat. Ctrl-C à nouveau pour quitter tout de suite.");
        });
    }
    let mut reporter = Reporter { quiet, stage: None, last_width: 0 };

    let (mut job, output) = if let Some(checkpoint) = &resume {
        let job = RefineJob::resume(checkpoint, capture_path.as_deref()).map_err(|e| explain(&e))?;
        let output = args.value(&["-o", "--sortie"]).map(PathBuf::from).unwrap_or_else(|| checkpoint.with_extension("ply"));
        if !quiet {
            eprintln!("Reprise à l'itération {} sur {}.", french_number(job.trainer.iteration as usize), french_number(job.config.train.iterations as usize));
        }
        (job, output)
    } else {
        let capture_path = capture_path.ok_or("il manque le fichier de capture (.gscap). « gs-refine aide » décrit l'usage.")?;
        let output = PathBuf::from(args.value(&["-o", "--sortie"]).ok_or("il manque la sortie : -o <fichier.ply>")?);
        let config = configuration(args)?;
        let job = RefineJob::prepare(&capture_path, config, &cancel, &mut |p| reporter.report(p)).map_err(|e| explain(&e))?;
        reporter.end_line();
        (job, output)
    };
    if !quiet {
        let s = &job.info.selection;
        eprintln!(
            "Capture « {} » ({}) : {} images, {} vues d'entraînement, {} mises de côté. Départ : {} gaussiennes, cellules de {} mm. {} fils d'exécution.",
            job.info.name,
            job.info.mode,
            french_number(job.info.frames),
            french_number(s.train.len()),
            french_number(s.holdout.len()),
            french_number(job.info.fused_splats),
            decimal(job.info.voxel_size as f64 * 1000.0, 1),
            job.threads()
        );
        if job.info.truncated {
            eprintln!("Attention : l'enregistrement est tronqué, seules les images complètes sont utilisées.");
        }
    }
    // Before the long part: a result that cannot be written is found out
    // now, and the checkpoint needs the directory as well.
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("impossible de créer {} : {e}", parent.display()))?;
    }
    let checkpoint = output.with_extension("reprise");
    job.checkpoint_to(Some(checkpoint.clone()));

    let started = Instant::now();
    let outcome = job.run(&cancel, &mut |p| reporter.report(p)).map_err(|e| explain(&e))?;
    reporter.end_line();

    let splats = job.splats();
    write_whole(&output, &gs_core::io::write_ply(&splats))?;
    if args.flag(&["--splat"]) {
        write_whole(&output.with_extension("splat"), &gs_core::io::write_splat(&splats))?;
    }
    let report = job.report();
    let report_path = output.with_extension("json");
    let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    write_whole(&report_path, json.as_bytes())?;
    if outcome.completed {
        let _ = std::fs::remove_file(&checkpoint);
    }

    if !quiet {
        if outcome.completed {
            eprintln!("Optimisation terminée en {}.", duration(started.elapsed().as_secs_f64()));
        } else {
            eprintln!(
                "Optimisation interrompue à l'itération {} sur {}. Pour la reprendre : gs-refine --reprise {}",
                french_number(outcome.iterations_done as usize),
                french_number(job.config.train.iterations as usize),
                checkpoint.display()
            );
        }
        let score = |s: &gs_refine::job::EvalScore| format!("{} dB (SSIM {})", decimal(s.psnr as f64, 2), decimal(s.ssim as f64, 3));
        if let (Some(fusion), Some(refined)) = (&report.fusion, &report.refined) {
            eprintln!("Vues mises de côté : fusion temps réel {}, résultat {} (itération {}).", score(fusion), score(refined), french_number(report.result_iteration as usize));
            // Measured in simulation: a gain of 9 dB here went with 1 to 4 dB
            // from viewpoints a meter away from the path.
            eprintln!("Ces vues sont voisines des vues d'entraînement : loin du trajet de l'appareil, le gain est nettement plus faible.");
        }
        eprintln!("{} : {} gaussiennes. Rapport : {}", output.display(), french_number(splats.len()), report_path.display());
    }
    Ok(if outcome.completed { ExitCode::SUCCESS } else { ExitCode::from(130) })
}

fn info(args: &Args) -> Result<ExitCode, String> {
    let path = PathBuf::from(args.positional(1).ok_or("il manque le fichier de capture (.gscap)")?);
    let capture = Capture::open(&path).map_err(|e| explain(&e.into()))?;
    let config = configuration(args)?;
    println!("Capture : {}", path.display());
    println!("  nom : {}, mode : {}, appareil : {}, application : {}", capture.header.name, capture.mode.name(), capture.header.device, capture.header.app);
    println!("  enregistrée le {}", if capture.header.created.is_empty() { "(date inconnue)" } else { &capture.header.created });
    println!(
        "  {} images en {}, {}{}",
        french_number(capture.frames.len()),
        duration(capture.duration()),
        megabytes(capture.bytes),
        if capture.truncated { " (enregistrement tronqué)" } else { "" }
    );
    if let Some(first) = capture.frames.first() {
        let h = &first.header;
        let with = |present: bool, name: &str| if present { format!(", {name}") } else { String::new() };
        println!(
            "  couleur {}x{}, profondeur {}x{}{}{}",
            h.color_width,
            h.color_height,
            h.depth_width,
            h.depth_height,
            with(h.has_confidence(), "confiance"),
            with(h.has_mask(), "masque de personne")
        );
        let fov = 2.0 * (0.5 * h.color_width as f32 / h.intrinsics[0]).atan().to_degrees();
        println!("  champ horizontal {} degrés", decimal(fov as f64, 1));
    }
    match capture.header.floor {
        Some(y) => println!("  sol détecté à y = {} m", decimal(y as f64, 2)),
        None => println!("  sol non renseigné (il sera estimé)"),
    }
    if let Some(region) = &capture.header.region {
        println!("  région d'intérêt : rayon {} m", decimal(region.radius as f64, 2));
    }
    let cams: Vec<_> = capture.frames.iter().filter_map(|f| gs_refine::dataset::color_camera(&f.header)).collect();
    let path_length: f32 = cams.windows(2).map(|w| w[0].position().distance(w[1].position())).sum();
    println!("  trajet de l'appareil : {} m", decimal(path_length as f64, 1));

    let selection = select_views(&capture, &config.select);
    println!("Sélection des vues (preset {}) :", args.value(&["--preset"]).unwrap_or("standard"));
    println!("  {} vues d'entraînement, {} mises de côté pour l'évaluation", french_number(selection.train.len()), french_number(selection.holdout.len()));
    for (count, why) in [
        (selection.dropped_tracking, "suivi limité ou absent"),
        (selection.dropped_pose, "pose ou optique incohérente"),
        (selection.dropped_color, "photo illisible"),
        (selection.dropped_blurred, "floues"),
        (selection.dropped_duplicate, "quasi identiques à la précédente"),
        (selection.dropped_thinned, "retirées pour respecter le nombre maximal de vues"),
    ] {
        if count > 0 {
            println!("  {} écartées : {why}", french_number(count));
        }
    }
    if let Some(first) = capture.frames.first() {
        let width = config.train.width.min(first.header.color_width as u32) as u64;
        let per_view = width * (width * first.header.color_height as u64 / first.header.color_width.max(1) as u64) * 3;
        println!(
            "Mémoire : les photos décodées à {} px occupent {} par vue, soit {} pour toutes ; le cache est limité à {} Mo.",
            width,
            megabytes(per_view),
            megabytes(per_view * selection.train.len() as u64),
            french_number(config.cache_mb)
        );
    }
    if !capture.mode.fuses() {
        println!("Ce mode de capture (direct) montre une scène en mouvement : il ne peut pas être optimisé hors ligne.");
    }
    Ok(ExitCode::SUCCESS)
}

fn export(args: &Args) -> Result<ExitCode, String> {
    let path = PathBuf::from(args.positional(1).ok_or("il manque le fichier de capture (.gscap)")?);
    let dir = PathBuf::from(args.value(&["-o", "--sortie"]).ok_or("il manque le dossier de sortie : -o <dossier>")?);
    let format = args.value(&["--format"]).ok_or("il manque le format : --format colmap ou --format nerfstudio")?;
    let format = DatasetFormat::from_name(format).ok_or_else(|| format!("format inconnu : {format} (colmap ou nerfstudio)"))?;
    let config = configuration(args)?;
    let capture = Capture::open(&path).map_err(|e| explain(&e.into()))?;
    let cancel = CancelToken::default();
    {
        let cancel = cancel.clone();
        let _ = ctrlc::set_handler(move || cancel.cancel());
    }
    let mut reporter = Reporter { quiet: args.flag(&["--silencieux"]), stage: None, last_width: 0 };
    let summary = export_dataset(&capture, &dir, format, &config.select, &config.init, &cancel, &mut |done, total| {
        reporter.line(&format!("Export : étape {} sur {}", french_number(done), french_number(total)));
    })
    .map_err(|e| explain(&e))?;
    reporter.end_line();
    println!(
        "{} : {} images, {} points de départ{}{}.",
        summary.dir.display(),
        french_number(summary.images),
        french_number(summary.points),
        if summary.depth_maps > 0 { format!(", {} cartes de profondeur", french_number(summary.depth_maps)) } else { String::new() },
        if summary.masks > 0 { format!(", {} masques", french_number(summary.masks)) } else { String::new() }
    );
    match format {
        DatasetFormat::Colmap => println!("Format COLMAP (sparse/0, images) : utilisable tel quel par les entraîneurs 3DGS. Le fichier .ply obtenu s'ouvre dans la visionneuse."),
        DatasetFormat::Nerfstudio => println!(
            "Format nerfstudio (transforms.json). Pour garder le repère et l'échelle de la capture : --orientation-method none --center-method none --auto-scale-poses False"
        ),
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    let args = Args(std::env::args().skip(1).collect());
    let command = args.0.first().map(String::as_str);
    let result = match command {
        None | Some("aide" | "help" | "--help" | "-h" | "--aide") => {
            println!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Some("info") => args.check(1).and_then(|()| info(&args)),
        Some("export") => args.check(1).and_then(|()| export(&args)),
        Some(_) => args.check(0).and_then(|()| refine(&args)),
    };
    match result {
        Ok(code) => code,
        Err(message) => {
            eprintln!("gs-refine : {message}");
            ExitCode::from(1)
        }
    }
}
