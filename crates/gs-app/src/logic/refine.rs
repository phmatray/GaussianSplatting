//! The captures the native server keeps and their offline refinement, as
//! the interface presents them: the answers of `GET /api/captures`,
//! `GET /api/refine` and `GET /api/refined`, and the French texts that go
//! with them.
//!
//! Everything said here about what the refinement gains was measured on
//! simulated captures only (`crates/gs-refine`); the texts say so.

use serde::Deserialize;

use super::format;

/// A time budget of the refinement (`gs_refine::job::Preset`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    Rapide,
    Standard,
    Fin,
}

impl Preset {
    pub const ALL: [Self; 3] = [Self::Rapide, Self::Standard, Self::Fin];

    /// The name the server knows it by.
    pub fn id(self) -> &'static str {
        match self {
            Self::Rapide => "rapide",
            Self::Standard => "standard",
            Self::Fin => "fin",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Rapide => "Rapide",
            Self::Standard => "Standard",
            Self::Fin => "Fin",
        }
    }

    /// What the preset costs. The durations were measured through the
    /// server on the development machine (a 4-core virtual machine, three
    /// threads for the job) with a simulated bedroom of 220 photographs of
    /// 960x720: 108 s for `rapide` (278 241 Gaussians) on a quiet machine,
    /// 587 s for `standard` (551 093 Gaussians) while it was also
    /// compiling. `fin` was never run to its end. A real capture is longer
    /// and its photographs are larger.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Rapide => {
                "Rapide\u{a0}: images de 480 pixels, 1\u{202f}500 itérations au moins. Environ 2 minutes pour une pièce simulée de 220 photos sur la machine de développement (4 cœurs, dont 3 pour le calcul)."
            }
            Self::Standard => {
                "Standard\u{a0}: images de 640 pixels, 4\u{202f}000 itérations au moins, départ plus fin. Environ 10 minutes pour la même pièce sur la machine de développement."
            }
            Self::Fin => {
                "Fin\u{a0}: images de 960 pixels, 12\u{202f}000 itérations au moins. Jamais mené à son terme sur la machine de développement\u{a0}: comptez plus d'une heure."
            }
        }
    }
}

/// What the interface says before a refinement is started: what it gains,
/// what it does not, and what it runs on.
pub const HONEST_HINT: &str = "L'affinage optimise la reconstruction contre les photos de la capture. Mesuré sur des captures simulées seulement\u{a0}: il précise les contours et ce qui est proche du trajet du téléphone, gagne peu ou rien loin du trajet, là où la reconstruction est déjà fermée, et laisse un léger moutonnement sur les surfaces unies. Le calcul n'utilise que le processeur\u{a0}: une version GPU est prévue et n'existe pas encore.";

/// The one sentence a page without a native server shows instead.
pub const ABSENT_HINT: &str = "La liste des captures et leur affinage demandent l'application de bureau Splat Live (ou le serveur gs-server)\u{a0}: le calcul occupe tous les cœurs du processeur pendant des minutes et ne tourne pas dans une page web.";

/// What a stop costs.
pub const CANCEL_HINT: &str = "Annuler abandonne le calcul\u{a0}: rien n'est conservé, car un affinage interrompu tôt est moins bon que la reconstruction en direct.";

/// File names are used in URLs and as text: anything odd is left out.
fn plain_name(name: &str, ext: &str) -> bool {
    name.len() > ext.len()
        && name.len() <= 255
        && name.ends_with(ext)
        && !name.starts_with('.')
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

fn clip(text: &mut String) {
    const MAX_CHARS: usize = 200;
    if let Some((at, _)) = text.char_indices().nth(MAX_CHARS) {
        text.truncate(at);
    }
}

/// One raw capture of the server's captures directory.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct KeptCapture {
    /// File name (`salon-20261114-101500.gscap`).
    pub file: String,
    /// What the user called it on the phone.
    pub title: String,
    /// `room`, `object`, `person` or `live`.
    pub mode: String,
    pub device: String,
    /// Start of the capture, RFC 3339.
    pub created: String,
    pub size: u64,
    /// The server is still writing it.
    pub capturing: bool,
    /// Frames and the seconds they span (0 while unknown).
    pub frames: u32,
    pub duration_s: f64,
}

impl KeptCapture {
    /// A `live` capture shows a moving scene: there is no cloud to refine.
    pub fn refinable(&self) -> bool {
        self.mode != "live" && !self.capturing
    }
}

/// `GET /api/captures`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct CaptureList {
    pub captures: Vec<KeptCapture>,
    /// Where the files are on the server's machine.
    pub dir: Option<String>,
}

impl CaptureList {
    pub fn parse(json: &str) -> Option<Self> {
        let mut list: Self = serde_json::from_str(json).ok()?;
        list.captures.retain(|capture| plain_name(&capture.file, ".gscap"));
        for capture in &mut list.captures {
            clip(&mut capture.title);
            clip(&mut capture.device);
            if capture.title.trim().is_empty() {
                capture.title = capture.file.trim_end_matches(".gscap").to_string();
            }
        }
        Some(list)
    }
}

/// One refined result of the server (`GET /api/refined`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Refined {
    /// File name (`salon-20261114-101500-rapide.ply`).
    pub file: String,
    /// Title of the capture it was made from.
    pub title: String,
    /// File name of that capture.
    pub capture: String,
    /// Mode of that capture: `room`, `object` or `person`.
    pub mode: String,
    pub preset: String,
    pub created: String,
    pub size: u64,
    pub gaussians: u64,
    pub iterations: u32,
    pub elapsed_s: f64,
    pub beyond_sensor: bool,
    /// Where a refined room is first looked at from: eye, then target.
    pub view: Option<[f32; 6]>,
    /// Held-out score, when the server's description has one.
    pub psnr: Option<f64>,
    pub ssim: Option<f64>,
}

impl Refined {
    /// A refined room: walked through from inside rather than orbited.
    pub fn is_room(&self) -> bool {
        self.mode == "room"
    }

    /// The camera a refined room starts with, when the server proposes
    /// one that makes sense: `(eye, target)`.
    pub fn start_view(&self) -> Option<super::share::View> {
        let view = self.view.filter(|view| self.is_room() && view.iter().all(|c| c.is_finite() && c.abs() < 1.0e5))?;
        Some(([view[0], view[1], view[2]], [view[3], view[4], view[5]]))
    }

    /// File name without `.ply`: the name of its share archive.
    pub fn stem(&self) -> &str {
        self.file.strip_suffix(".ply").unwrap_or(&self.file)
    }
}

/// `GET /api/refined`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct RefinedList {
    pub results: Vec<Refined>,
    pub dir: Option<String>,
    pub datasets_dir: Option<String>,
}

impl RefinedList {
    pub fn parse(json: &str) -> Option<Self> {
        let mut list: Self = serde_json::from_str(json).ok()?;
        list.results.retain(|result| plain_name(&result.file, ".ply"));
        for result in &mut list.results {
            clip(&mut result.title);
        }
        Some(list)
    }
}

/// The offline job in progress on the server.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Job {
    pub id: u64,
    /// `refine` or `export`.
    pub kind: String,
    pub capture: String,
    pub title: String,
    pub preset: Option<String>,
    pub format: Option<String>,
    pub stage: String,
    pub done: u64,
    pub total: u64,
    pub iteration: u32,
    pub iterations: u32,
    pub elapsed_s: f64,
    pub eta_s: Option<f64>,
    pub gaussians: u64,
    pub cancelling: bool,
}

impl Job {
    pub fn is_export(&self) -> bool {
        self.kind == "export"
    }

    /// What the job is doing, in a few words.
    pub fn stage_label(&self) -> &'static str {
        if self.cancelling {
            return "Arrêt en cours…";
        }
        match self.stage.as_str() {
            "prepare" => "Préparation",
            "select" => "Choix des vues",
            "fuse" => "Reconstruction de départ",
            "exposure" => "Mesure de l'exposition des photos",
            "poses" => "Recalage des vues",
            "evaluate" => "Évaluation",
            "optimise" => "Optimisation",
            "write" => "Écriture du résultat",
            "export" => "Export du jeu de données",
            _ => "Calcul en cours",
        }
    }

    /// Progress within the current stage, `0..=1`; `None` when the stage
    /// has no measure (the bar is then indeterminate).
    pub fn fraction(&self) -> Option<f64> {
        let ratio = |done: f64, total: f64| (total > 0.0).then(|| (done / total).clamp(0.0, 1.0));
        match self.stage.as_str() {
            "optimise" | "evaluate" if self.iterations > 0 => ratio(f64::from(self.iteration), f64::from(self.iterations)),
            "select" | "fuse" | "exposure" | "poses" | "export" => ratio(self.done as f64, self.total as f64),
            _ => None,
        }
    }

    /// `Itération 320 sur 1 500 · 412 300 gaussiennes` or `Image 12 sur 220`.
    pub fn detail(&self) -> String {
        match self.stage.as_str() {
            "optimise" | "evaluate" | "write" if self.iterations > 0 => {
                let mut text = format!("Itération {} sur {}", format::count(u64::from(self.iteration)), format::count(u64::from(self.iterations)));
                if self.gaussians > 0 {
                    text.push_str(&format!(" · {} gaussiennes", format::count(self.gaussians)));
                }
                text
            }
            "export" if self.total > 0 => format!("Étape {} sur {}", format::count(self.done), format::count(self.total)),
            _ if self.total > 0 => format!("Image {} sur {}", format::count(self.done), format::count(self.total)),
            _ => String::new(),
        }
    }

    /// `Depuis 2 minutes 5 secondes, reste environ 4 minutes`.
    pub fn clock(&self) -> String {
        let mut text = format!("Depuis {}", duration(self.elapsed_s));
        if let Some(eta) = self.eta_s.filter(|eta| eta.is_finite() && *eta >= 1.0 && self.stage == "optimise" && !self.cancelling) {
            text.push_str(&format!(", reste environ {}", duration(eta)));
        }
        text
    }
}

/// A duration in words, rounded as a person would say it.
pub fn duration(seconds: f64) -> String {
    let seconds = if seconds.is_finite() { seconds.max(0.0) } else { 0.0 };
    if seconds < 90.0 {
        return super::replay::spoken(seconds);
    }
    let minutes = (seconds / 60.0).round() as u64;
    if minutes < 90 {
        return format!("{minutes} minutes");
    }
    let (hours, rest) = (minutes / 60, minutes % 60);
    let hour_text = if hours > 1 { format!("{hours} heures") } else { "1 heure".to_string() };
    if rest == 0 {
        hour_text
    } else {
        format!("{hour_text} {rest:02}")
    }
}

/// Why a job failed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct JobError {
    pub code: String,
}

/// A dataset the server wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Dataset {
    pub name: String,
    /// Where it is on the server's machine.
    pub dir: String,
    pub format: String,
    pub images: u64,
    pub points: u64,
}

/// The latest job that ended on the server.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct JobEnd {
    pub id: u64,
    pub kind: String,
    pub capture: String,
    pub title: String,
    pub preset: Option<String>,
    pub format: Option<String>,
    /// `done`, `cancelled` or `failed`.
    pub outcome: String,
    pub error: Option<JobError>,
    pub result: Option<Refined>,
    pub dataset: Option<Dataset>,
    pub iterations_done: u32,
    pub elapsed_s: f64,
}

impl JobEnd {
    pub fn is_done(&self) -> bool {
        self.outcome == "done"
    }

    /// The sentence that sums the job up, and whether it is bad news.
    pub fn text(&self) -> (bool, String) {
        let export = self.kind == "export";
        match self.outcome.as_str() {
            "done" if export => {
                let dataset = self.dataset.clone().unwrap_or_default();
                let format = if dataset.format == "nerfstudio" { "nerfstudio" } else { "COLMAP" };
                (false, format!("Jeu de données {format} de «\u{a0}{}\u{a0}» écrit\u{a0}: {} images, {} points de départ.", self.title, format::count(dataset.images), format::count(dataset.points)))
            }
            "done" => {
                let gaussians = self.result.as_ref().map_or(0, |result| result.gaussians);
                (false, format!("«\u{a0}{}\u{a0}» est affinée\u{a0}: {} gaussiennes en {}.", self.title, format::count(gaussians), duration(self.elapsed_s)))
            }
            "cancelled" if export => (false, format!("L'export de «\u{a0}{}\u{a0}» a été annulé.", self.title)),
            "cancelled" => (false, format!("L'affinage de «\u{a0}{}\u{a0}» a été annulé\u{a0}: rien n'a été conservé.", self.title)),
            _ => {
                let code = self.error.as_ref().map_or("internal", |error| error.code.as_str());
                let what = if export { "L'export" } else { "L'affinage" };
                (true, format!("{what} de «\u{a0}{}\u{a0}» a échoué\u{a0}: {}", self.title, failure_text(code)))
            }
        }
    }
}

/// Why a job failed, by the server's code.
pub fn failure_text(code: &str) -> &'static str {
    match code {
        "bad_capture" => "le fichier de capture est illisible ou abîmé.",
        "no_views" => "la capture ne contient aucune image utilisable (suivi de position perdu, images floues ou trop peu nombreuses).",
        "disk_full" => "le disque est plein.",
        "disk_error" => "le résultat n'a pas pu être écrit sur le disque.",
        _ => "erreur interne du calcul.",
    }
}

/// `GET /api/refine`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct RefineState {
    pub job: Option<Job>,
    pub last: Option<JobEnd>,
}

impl RefineState {
    pub fn parse(json: &str) -> Option<Self> {
        let mut state: Self = serde_json::from_str(json).ok()?;
        if let Some(job) = &mut state.job {
            clip(&mut job.title);
        }
        if let Some(last) = &mut state.last {
            clip(&mut last.title);
            last.result = last.result.take().filter(|result| plain_name(&result.file, ".ply"));
        }
        Some(state)
    }
}

/// The answer of `POST /api/refine`: the job, or the code of a refusal.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct RefineAnswer {
    pub job: Option<Job>,
    pub code: String,
}

impl RefineAnswer {
    pub fn parse(json: &str) -> Self {
        serde_json::from_str(json).unwrap_or_default()
    }
}

/// What to tell the user when the server refused to start (or stop) a job.
pub fn refusal_text(status: u16, code: &str) -> String {
    match (status, code) {
        (_, "job_running") => "Un calcul est déjà en cours sur le serveur\u{a0}: un seul à la fois.".to_string(),
        (_, "no_job") => "Aucun calcul n'est en cours.".to_string(),
        (_, "not_found") => "Cette capture n'existe plus sur le serveur.".to_string(),
        (_, "in_use") => "Cette capture est encore en cours d'enregistrement.".to_string(),
        (_, "unsupported_mode") => "Une capture en mode direct montre une scène qui bouge\u{a0}: elle ne peut pas être affinée.".to_string(),
        (_, "disk_full") | (507, _) => "Il n'y a plus assez de place sur le disque pour ce calcul.".to_string(),
        (_, "disk_error") => "Le dossier des résultats n'a pas pu être créé sur le disque.".to_string(),
        (_, "no_refined_dir") => "Ce serveur n'a pas de dossier où écrire ses résultats.".to_string(),
        (403, _) => "L'affinage se commande depuis l'ordinateur qui exécute Splat Live.".to_string(),
        (404, _) => "Ce serveur ne propose pas l'affinage (version plus ancienne).".to_string(),
        (status, _) => format!("Le serveur a refusé la demande (HTTP {status})."),
    }
}

/// The state of the server's disk (`storage` of `GET /api/record`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Storage {
    pub free_bytes: Option<u64>,
    /// Little room is left for a capture.
    pub low: bool,
    pub problem: Option<StorageProblem>,
}

/// Something the disk refused.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct StorageProblem {
    pub serial: u64,
    pub code: String,
    pub file: String,
}

impl Storage {
    /// The `storage` member of the recorder status, when the server sends one.
    pub fn parse(json: &str) -> Option<Self> {
        #[derive(Deserialize)]
        struct Status {
            storage: Option<Storage>,
        }
        serde_json::from_str::<Status>(json).ok()?.storage
    }

    /// The warning to keep on screen while the disk is nearly full.
    pub fn low_text(&self) -> Option<String> {
        self.low.then(|| match self.free_bytes {
            Some(free) => format!(
                "Le disque de cet ordinateur est presque plein\u{a0}: il reste {}. Une capture d'iPhone écrit jusqu'à 9 Mo par seconde.",
                super::replay::size(free)
            ),
            None => "Le disque de cet ordinateur est presque plein.".to_string(),
        })
    }
}

impl StorageProblem {
    pub fn text(&self) -> String {
        match self.code.as_str() {
            "capture_not_saved" => {
                "Le disque a refusé une écriture (disque plein\u{a0}?)\u{a0}: la capture continue à l'écran mais ses images ne sont plus conservées dans un fichier.".to_string()
            }
            "recording_not_saved" => "Le disque a refusé une écriture (disque plein\u{a0}?)\u{a0}: la capture n'a pas pu être enregistrée.".to_string(),
            other => format!("Le disque a refusé une écriture ({other})."),
        }
    }
}

/// The `file` URL parameter that opens a refined result of the server.
pub fn refined_file_param(file: &str) -> String {
    format!("refined/{file}")
}

/// The link of the share dialog for a refined result: the embedded viewer
/// on the LAN address `base`, opened on the result (a still scene: no
/// playback parameters). A room is opened for a walk (`nav=fly`) from the
/// view the server proposes; without one the viewer starts in the middle
/// of the scene.
pub fn lan_refined_link(base: &str, result: &Refined) -> String {
    let query = super::share::share_query(&refined_file_param(&result.file), true, false, false, None, result.start_view());
    format!("{}/{query}{}", base.trim_end_matches('/'), if result.is_room() { "&nav=fly" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_capture_list_of_the_server_is_read() {
        let json = r#"{"captures":[
            {"name":"Salon-20261114-101500","file":"Salon-20261114-101500.gscap","title":"Salon","mode":"room","device":"iPhone17,2","created":"2026-11-14T10:15:00Z","size":13002342,"url":"captures/Salon-20261114-101500.gscap","capturing":false},
            {"file":"../evil.gscap","title":"x"},
            {"file":"direct.gscap","title":"","mode":"live","size":10},
            {"file":"x.ply"}
        ],"dir":"/data/captures","free_bytes":123}"#;
        let list = CaptureList::parse(json).unwrap();
        assert_eq!(list.captures.len(), 2);
        assert_eq!(list.dir.as_deref(), Some("/data/captures"));
        let salon = &list.captures[0];
        assert_eq!((salon.title.as_str(), salon.mode.as_str(), salon.size), ("Salon", "room", 13_002_342));
        assert!(salon.refinable());
        // A capture without a title is called by its file; a live one is not refined.
        assert_eq!(list.captures[1].title, "direct");
        assert!(!list.captures[1].refinable());
        assert!(!KeptCapture { capturing: true, ..salon.clone() }.refinable());
        assert!(CaptureList::parse("nonsense").is_none());
        assert_eq!(CaptureList::parse("{}").unwrap(), CaptureList::default());
    }

    #[test]
    fn job_progress_reads_as_text() {
        let state = RefineState::parse(
            r#"{"job":{"id":3,"kind":"refine","capture":"a.gscap","title":"Salon","preset":"rapide","format":null,"stage":"optimise",
                "done":0,"total":0,"iteration":320,"iterations":1500,"elapsed_s":125.2,"eta_s":251.0,"loss":0.04,"holdout_psnr":31.2,
                "gaussians":412300,"width":480,"threads":3,"cancelling":false,"started":"2026-11-14T10:15:00Z"},"last":null}"#,
        )
        .unwrap();
        let job = state.job.unwrap();
        assert_eq!(job.stage_label(), "Optimisation");
        assert_eq!(job.detail(), "Itération 320 sur 1\u{202f}500 · 412\u{202f}300 gaussiennes");
        assert!((job.fraction().unwrap() - 320.0 / 1500.0).abs() < 1e-9);
        assert_eq!(job.clock(), "Depuis 2 minutes, reste environ 4 minutes");
        assert!(!job.is_export());

        let fusing = Job { stage: "fuse".into(), done: 12, total: 220, elapsed_s: 4.0, ..Job::default() };
        assert_eq!((fusing.stage_label(), fusing.detail()), ("Reconstruction de départ", "Image 12 sur 220".to_string()));
        assert_eq!(fusing.clock(), "Depuis 4 secondes");
        let preparing = Job { stage: "prepare".into(), ..Job::default() };
        assert_eq!((preparing.fraction(), preparing.detail()), (None, String::new()));
        let stopping = Job { cancelling: true, eta_s: Some(100.0), ..job.clone() };
        assert_eq!(stopping.stage_label(), "Arrêt en cours…");
        assert!(!stopping.clock().contains("reste"));
        let exporting = Job { kind: "export".into(), stage: "export".into(), done: 3, total: 40, ..Job::default() };
        assert!(exporting.is_export() && exporting.detail() == "Étape 3 sur 40");
        // Every stage of the server has words.
        for stage in ["prepare", "select", "fuse", "exposure", "poses", "evaluate", "optimise", "write", "export", "new"] {
            let label = Job { stage: stage.into(), ..Job::default() }.stage_label();
            assert!(!label.is_empty() && !label.contains('\u{2014}') && !label.contains('\u{2013}'));
        }
    }

    #[test]
    fn durations_are_said_as_a_person_would() {
        assert_eq!(duration(0.4), "0 seconde");
        assert_eq!(duration(65.0), "1 minute 5 secondes");
        assert_eq!(duration(125.0), "2 minutes");
        assert_eq!(duration(3600.0), "60 minutes");
        assert_eq!(duration(2.0 * 3600.0), "2 heures");
        assert_eq!(duration(5700.0), "1 heure 35");
        assert_eq!(duration(f64::NAN), "0 seconde");
    }

    #[test]
    fn the_end_of_a_job_is_summed_up_in_one_sentence() {
        let done = RefineState::parse(
            r#"{"job":null,"last":{"id":1,"kind":"refine","capture":"a.gscap","title":"Salon","preset":"rapide","outcome":"done","error":null,
                "result":{"name":"a-rapide","file":"a-rapide.ply","size":28000000,"url":"refined/a-rapide.ply","writing":false,"capture":"a.gscap",
                "title":"Salon","mode":"room","preset":"rapide","created":"2026-11-14T10:20:00Z","gaussians":412300,"iterations":1500,"elapsed_s":170.0,"beyond_sensor":true},
                "dataset":null,"iterations_done":1500,"iterations":1500,"elapsed_s":183.0}}"#,
        )
        .unwrap()
        .last
        .unwrap();
        assert!(done.is_done());
        let (bad, text) = done.text();
        assert!(!bad && text.contains("412\u{202f}300 gaussiennes") && text.contains("3 minutes"), "{text}");
        let result = done.result.clone().unwrap();
        assert_eq!(result.stem(), "a-rapide");

        let cancelled = JobEnd { outcome: "cancelled".into(), result: None, ..done.clone() };
        assert!(cancelled.text().1.contains("rien n'a été conservé") && !cancelled.is_done());
        for code in ["bad_capture", "no_views", "disk_full", "disk_error", "internal", "new_code"] {
            let failed = JobEnd { outcome: "failed".into(), error: Some(JobError { code: code.into() }), ..done.clone() };
            let (bad, text) = failed.text();
            assert!(bad && text.ends_with('.') && !text.contains('\u{2014}'), "{text}");
        }
        let exported = JobEnd {
            kind: "export".into(),
            dataset: Some(Dataset { name: "a-colmap".into(), dir: "/data/datasets/a-colmap".into(), format: "colmap".into(), images: 193, points: 139_000 }),
            result: None,
            ..done.clone()
        };
        assert!(exported.text().1.contains("COLMAP") && exported.text().1.contains("193 images"));
        // A result with a file name that is no file name is dropped.
        let odd = RefineState::parse(r#"{"last":{"outcome":"done","result":{"file":"../x.ply"}}}"#).unwrap();
        assert!(odd.last.unwrap().result.is_none());
    }

    #[test]
    fn refusals_and_storage_have_words() {
        for (status, code) in [(409, "job_running"), (409, "no_job"), (404, "not_found"), (409, "in_use"), (400, "unsupported_mode"), (507, "disk_full"), (500, "disk_error"), (503, "no_refined_dir"), (403, "loopback_only"), (404, ""), (500, "x")] {
            let text = refusal_text(status, code);
            assert!(text.ends_with('.') && !text.contains('\u{2014}') && !text.contains('\u{2013}'), "{text}");
        }
        assert_eq!(RefineAnswer::parse(r#"{"error":"another job is running","code":"job_running","job":{"id":2}}"#).code, "job_running");
        assert_eq!(RefineAnswer::parse("not json"), RefineAnswer::default());

        let storage = Storage::parse(r#"{"recording":null,"last":null,"storage":{"free_bytes":1073741824,"low":true,"problem":{"serial":2,"code":"capture_not_saved","file":"a.gscap","message":"No space left on device"}}}"#).unwrap();
        assert!(storage.low_text().unwrap().contains("1,1 Go"));
        assert!(storage.problem.as_ref().unwrap().text().contains("ne sont plus conservées"));
        assert_eq!(Storage::parse(r#"{"recording":null,"last":null}"#), None);
        assert_eq!(Storage { low: false, ..storage }.low_text(), None);
        for preset in Preset::ALL {
            assert_eq!(Preset::from_id(preset.id()), Some(preset));
            assert!(preset.hint().ends_with('.') && !preset.hint().contains('\u{2014}'));
        }
        for text in [HONEST_HINT, ABSENT_HINT, CANCEL_HINT] {
            assert!(text.ends_with('.') && !text.contains('\u{2014}') && !text.contains('\u{2013}'));
        }
        assert!(HONEST_HINT.contains("simulées") && HONEST_HINT.contains("GPU"));
    }

    #[test]
    fn a_refined_result_is_shared_by_a_plain_link() {
        assert_eq!(refined_file_param("a-rapide.ply"), "refined/a-rapide.ply");
        let object = Refined { file: "Salon été-rapide.ply".into(), mode: "object".into(), view: Some([0.0; 6]), ..Refined::default() };
        let link = lan_refined_link("https://192.168.1.20:7879/", &object);
        assert!(link.starts_with("https://192.168.1.20:7879/?file=refined/"), "{link}");
        assert!(link.contains("embed=1") && !link.contains("autoplay") && !link.contains("loop") && !link.contains("nav") && !link.contains("view"), "{link}");
        // A room is opened for a walk, from where the phone started.
        let room = Refined { file: "a-rapide.ply".into(), mode: "room".into(), view: Some([1.0, 1.4, -2.0, 0.0, 1.1, 0.5]), ..Refined::default() };
        assert_eq!(room.start_view(), Some(([1.0, 1.4, -2.0], [0.0, 1.1, 0.5])));
        let link = lan_refined_link("https://192.168.1.20:7879", &room);
        assert_eq!(link, "https://192.168.1.20:7879/?file=refined/a-rapide.ply&embed=1&view=1,1.4,-2,0,1.1,0.5&nav=fly");
        let (params, problems) = crate::logic::share::ViewerParams::from_query(link.split_once('?').unwrap().1);
        assert!(problems.is_empty() && params.nav == Some(gs_render::NavMode::Fly) && params.view == room.start_view());
        // Without a proposed view, or with a broken one, the viewer chooses.
        let bare = Refined { view: None, ..room.clone() };
        assert_eq!(lan_refined_link("https://h", &bare), "https://h/?file=refined/a-rapide.ply&embed=1&nav=fly");
        assert_eq!(Refined { view: Some([f32::NAN; 6]), ..room }.start_view(), None);
        // The viewer accepts what the link names.
        let (params, problems) = crate::logic::share::ViewerParams::from_query(link.split_once('?').unwrap().1);
        assert!(problems.is_empty(), "{:?}", problems.iter().map(|p| &p.text).collect::<Vec<_>>());
        assert!(params.embed && params.file.is_some_and(|file| file.kind == crate::logic::share::FileKind::Splats));
    }
}
