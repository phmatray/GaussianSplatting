//! The gallery's rooms: every filmed capture with what was made from it,
//! its refinements and its 4D recordings.

use super::format;
use super::refine::{KeptCapture, Preset, Refined};

/// A 4D recording as the gallery needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct RoomTake {
    pub id: u32,
    pub title: String,
    /// File name, which ends with the moment it was made (`-20261115-224249`).
    pub file: String,
    pub bytes: u64,
    pub duration_s: f64,
}

impl RoomTake {
    /// `Image fixe` for a still scene, `Enregistrement 4D` otherwise.
    pub fn label(&self) -> &'static str {
        if self.duration_s > 0.0 { "Enregistrement 4D" } else { "Image fixe" }
    }

    /// `15/11 22:42 · 7,0 Mo`, the moment read from the file name.
    pub fn facts(&self) -> String {
        let stem = self.file.rsplit_once('.').map_or(self.file.as_str(), |(stem, _)| stem);
        let mut parts = stem.rsplit('-');
        let moment = match (parts.next(), parts.next()) {
            (Some(time), Some(date)) if time.len() == 6 && date.len() == 8 && time.bytes().chain(date.bytes()).all(|b| b.is_ascii_digit()) => {
                Some(format!("{}/{} {}:{}", &date[6..8], &date[4..6], &time[0..2], &time[2..4]))
            }
            _ => None,
        };
        let size = super::replay::size(self.bytes);
        moment.map_or(size.clone(), |m| format!("{m} · {size}"))
    }
}

/// One filmed room.
#[derive(Clone, Debug, PartialEq)]
pub struct Room {
    /// Stable key: the capture's file name, or `titre:<title>` for results
    /// whose capture is not kept on this machine.
    pub key: String,
    pub title: String,
    pub capture: Option<KeptCapture>,
    /// Newest first, as the server lists them.
    pub refined: Vec<Refined>,
    pub takes: Vec<RoomTake>,
}

impl Room {
    /// The refinement with the best held-out score, or the newest one when
    /// none has a score.
    pub fn best(&self) -> Option<&Refined> {
        self.refined
            .iter()
            .filter(|r| r.psnr.is_some())
            .max_by(|a, b| a.psnr.partial_cmp(&b.psnr).unwrap_or(std::cmp::Ordering::Equal))
            .or_else(|| self.refined.first())
    }

    /// Everything the room holds: the capture, its refinements, its recordings.
    pub fn versions(&self) -> usize {
        usize::from(self.capture.is_some()) + self.refined.len() + self.takes.len()
    }

    /// `Pièce · 135 Mo · 3 versions`.
    pub fn meta(&self) -> String {
        let mut parts = Vec::new();
        let mode = self.capture.as_ref().map(|c| c.mode.as_str()).or_else(|| self.refined.first().map(|r| r.mode.as_str()));
        if let Some(mode) = mode.and_then(mode_label) {
            parts.push(mode.to_string());
        }
        if let Some(capture) = &self.capture {
            parts.push(super::replay::size(capture.size));
        } else {
            parts.push("capture absente de cet ordinateur".to_string());
        }
        let n = self.versions();
        parts.push(if n == 1 { "1 version".to_string() } else { format!("{n} versions") });
        parts.join(" · ")
    }
}

pub fn mode_label(mode: &str) -> Option<&'static str> {
    match mode {
        "room" => Some("Pièce"),
        "object" => Some("Objet"),
        "person" => Some("Personne"),
        "live" => Some("Direct"),
        _ => None,
    }
}

/// `Pièce · 164 Mo · iPhone16,2`: the title already says when.
pub fn capture_facts(capture: &KeptCapture) -> String {
    let mut parts = vec![mode_label(&capture.mode).unwrap_or("Capture").to_string(), super::replay::size(capture.size)];
    if let Some(name) = device_name(&capture.device) {
        parts.push(name.to_string());
    }
    parts.join(" · ")
}

/// The product name of an iPhone with a LiDAR sensor, from Apple's model
/// code (`iPhone16,2`). Unknown codes are not shown.
pub fn device_name(code: &str) -> Option<&'static str> {
    Some(match code {
        "iPhone13,3" => "iPhone 12 Pro",
        "iPhone13,4" => "iPhone 12 Pro Max",
        "iPhone14,2" => "iPhone 13 Pro",
        "iPhone14,3" => "iPhone 13 Pro Max",
        "iPhone15,2" => "iPhone 14 Pro",
        "iPhone15,3" => "iPhone 14 Pro Max",
        "iPhone16,1" => "iPhone 15 Pro",
        "iPhone16,2" => "iPhone 15 Pro Max",
        "iPhone17,1" => "iPhone 16 Pro",
        "iPhone17,2" => "iPhone 16 Pro Max",
        _ => return None,
    })
}

/// `25,9 dB`.
pub fn score(psnr: f64) -> String {
    format!("{} dB", format::decimal(psnr as f32, 1))
}

/// `Standard`, or the file name when the preset is unknown (a result of
/// the command line with another name).
pub fn refined_label(result: &Refined) -> String {
    match Preset::from_id(&result.preset) {
        Some(preset) => preset.label().to_string(),
        None => result.file.trim_end_matches(".ply").to_string(),
    }
}

/// `2,0 M gaussiennes · 7 min 49`.
pub fn refined_facts(result: &Refined) -> String {
    let mut parts = Vec::new();
    if result.gaussians > 0 {
        parts.push(format!("{} M gaussiennes", format::decimal(result.gaussians as f32 / 1.0e6, 1)));
    }
    if result.elapsed_s >= 1.0 {
        let s = result.elapsed_s.round() as u64;
        parts.push(if s >= 60 { format!("{} min {:02}", s / 60, s % 60) } else { format!("{s} s") });
    }
    if parts.is_empty() {
        parts.push(super::replay::size(result.size));
    }
    parts.join(" · ")
}

/// Groups what the server keeps into rooms, newest capture first. A
/// result belongs to the capture whose file it names, or failing that
/// whose title it carries (the command line names the capture by the
/// path it read, often a copy).
pub fn rooms(captures: &[KeptCapture], refined: &[Refined], takes: &[RoomTake]) -> Vec<Room> {
    let mut rooms: Vec<Room> = captures
        .iter()
        .map(|c| Room { key: c.file.clone(), title: c.title.clone(), capture: Some(c.clone()), refined: Vec::new(), takes: Vec::new() })
        .collect();
    for result in refined {
        let by_file = rooms.iter().position(|r| !result.capture.is_empty() && r.capture.as_ref().is_some_and(|c| c.file == result.capture));
        let by_title = || rooms.iter().position(|r| !result.title.is_empty() && r.title == result.title);
        match by_file.or_else(by_title) {
            Some(i) => rooms[i].refined.push(result.clone()),
            None => {
                let title = if result.title.is_empty() { refined_label(result) } else { result.title.clone() };
                rooms.push(Room { key: format!("titre:{title}"), title, capture: None, refined: vec![result.clone()], takes: Vec::new() });
            }
        }
    }
    for take in takes {
        if let Some(room) = rooms.iter_mut().find(|r| r.title == take.title) {
            room.takes.push(take.clone());
        }
    }
    rooms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(file: &str, title: &str) -> KeptCapture {
        KeptCapture { file: file.into(), title: title.into(), mode: "room".into(), size: 135_000_000, ..KeptCapture::default() }
    }

    fn result(file: &str, capture: &str, title: &str, psnr: Option<f64>) -> Refined {
        Refined { file: file.into(), capture: capture.into(), title: title.into(), mode: "room".into(), psnr, ..Refined::default() }
    }

    #[test]
    fn results_join_their_capture_by_file_then_by_title() {
        let captures = [capture("a.gscap", "Salon"), capture("b.gscap", "Chambre")];
        let refined = [
            result("a-standard.ply", "a.gscap", "Salon", Some(25.0)),
            // The command line read a copy: only the title matches.
            result("copie.ply", "copie.gscap", "Chambre", Some(24.0)),
            result("seul.ply", "ailleurs.gscap", "Cuisine", None),
        ];
        let takes = [RoomTake { id: 1, title: "Salon".into(), file: "salon-20261115-224249.gsrec".into(), bytes: 7_000_000, duration_s: 0.0 }];
        let rooms = rooms(&captures, &refined, &takes);
        assert_eq!(rooms.len(), 3);
        assert_eq!(rooms[0].refined.len(), 1);
        assert_eq!(rooms[0].takes.len(), 1);
        assert_eq!(rooms[1].refined[0].file, "copie.ply");
        assert_eq!(rooms[2].key, "titre:Cuisine");
        assert!(rooms[2].capture.is_none());
        assert_eq!(rooms[0].versions(), 3);
        assert_eq!((rooms[0].takes[0].label(), rooms[0].takes[0].facts().as_str()), ("Image fixe", "15/11 22:42 · 7,0 Mo"));
    }

    #[test]
    fn the_best_version_is_the_best_scored() {
        let room = Room {
            key: "a".into(),
            title: "a".into(),
            capture: None,
            refined: vec![result("new.ply", "", "", None), result("good.ply", "", "", Some(26.1)), result("ok.ply", "", "", Some(25.9))],
            takes: Vec::new(),
        };
        assert_eq!(room.best().map(|r| r.file.as_str()), Some("good.ply"));
        assert_eq!(score(25.92), "25,9 dB");
        assert_eq!((device_name("iPhone16,2"), device_name("iPad13,8")), (Some("iPhone 15 Pro Max"), None));
    }
}
