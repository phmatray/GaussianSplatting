//! Import and export of splat files: format choice, sanity limits and
//! error messages a person can act on.

use gs_core::io::{self, IoError};
use gs_core::Splat;

/// Largest file the import reads, in bytes. The file, the decoded splats
/// and the GPU copy all have to fit into the 32-bit address space of the
/// page.
pub const MAX_IMPORT_BYTES: u64 = 768 * 1024 * 1024;
/// Largest number of splats the import displays.
pub const MAX_IMPORT_SPLATS: usize = 4_000_000;
/// Splats farther than this from the origin are dropped (meters): they are
/// garbage, and they would ruin the framing of the view.
const MAX_COORDINATE: f32 = 1.0e5;

/// A file format of the export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileFormat {
    /// INRIA 3D Gaussian Splatting `.ply`.
    Ply,
    /// antimatter15 `.splat`.
    Splat,
}

impl FileFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Ply => "ply",
            Self::Splat => "splat",
        }
    }

    /// The format a file name announces.
    pub fn from_name(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".ply") {
            Some(Self::Ply)
        } else if lower.ends_with(".splat") {
            Some(Self::Splat)
        } else {
            None
        }
    }

    pub fn encode(self, splats: &[Splat]) -> Vec<u8> {
        match self {
            Self::Ply => io::write_ply(splats),
            Self::Splat => io::write_splat(splats),
        }
    }
}

/// File name of an export: `splat-live-stage-12345.ply`.
pub fn export_name(scene: &str, count: usize, format: FileFormat) -> String {
    let scene: String = scene.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    let scene = if scene.is_empty() { "scene".to_string() } else { scene };
    format!("splat-live-{scene}-{count}.{}", format.extension())
}

fn megabytes(bytes: u64) -> u64 {
    bytes.div_ceil(1024 * 1024)
}

/// Checks the size of a file before it is read into memory.
pub fn check_size(bytes: u64) -> Result<(), String> {
    if bytes == 0 {
        return Err("Le fichier est vide.".to_string());
    }
    if bytes > MAX_IMPORT_BYTES {
        return Err(format!(
            "Le fichier pèse {} Mo, la limite est de {} Mo.",
            megabytes(bytes),
            megabytes(MAX_IMPORT_BYTES)
        ));
    }
    Ok(())
}

fn describe(error: &IoError) -> String {
    match error {
        IoError::Truncated { needed, available } => format!(
            "Le fichier est incomplet: son en-tête annonce {needed} octets, il n'en contient que {available}."
        ),
        IoError::BadLength(len) => {
            format!("Ce n'est pas un fichier .splat valide: sa taille ({len} octets) n'est pas un multiple de 32.")
        }
        IoError::BadHeader(what) => format!("L'en-tête PLY est invalide ({what})."),
        IoError::Unsupported(what) => format!("Ce fichier PLY n'est pas pris en charge ({what})."),
    }
}

/// Decodes an imported file. The format follows the file name; the content
/// must agree with it. Errors are sentences for the user.
pub fn parse_import(name: &str, bytes: &[u8]) -> Result<Vec<Splat>, String> {
    check_size(bytes.len() as u64)?;
    let format = FileFormat::from_name(name)
        .ok_or_else(|| "Format non pris en charge: choisissez un fichier .ply ou .splat.".to_string())?;
    let mut splats = match format {
        FileFormat::Ply => io::read_ply(bytes),
        FileFormat::Splat => io::read_splat(bytes),
    }
    .map_err(|e| describe(&e))?;
    splats.retain(|s| s.pos.iter().all(|c| c.abs() <= MAX_COORDINATE) && s.scale.iter().all(|c| *c <= MAX_COORDINATE));
    if splats.is_empty() {
        return Err("Le fichier ne contient aucun splat affichable.".to_string());
    }
    if splats.len() > MAX_IMPORT_SPLATS {
        return Err(format!(
            "Le fichier contient {} splats, la limite est de {}.",
            super::format::count(splats.len() as u64),
            super::format::count(MAX_IMPORT_SPLATS as u64)
        ));
    }
    Ok(splats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cloud(n: usize) -> Vec<Splat> {
        (0..n)
            .map(|i| {
                let t = i as f32 * 0.37;
                Splat {
                    pos: [t.sin() * 2.0, 1.0 + (t * 0.7).cos(), t.cos() * 2.0],
                    scale: [0.02, 0.03, 0.004],
                    rot: [0.0, 0.0, 0.0, 1.0],
                    rgba: [(i * 7 % 256) as u8, 120, 200, 230],
                }
            })
            .collect()
    }

    #[test]
    fn both_formats_roundtrip_with_the_same_count() {
        let splats = cloud(500);
        for format in [FileFormat::Ply, FileFormat::Splat] {
            let bytes = format.encode(&splats);
            let name = export_name("stage", splats.len(), format);
            assert_eq!(FileFormat::from_name(&name), Some(format));
            let back = parse_import(&name, &bytes).unwrap();
            assert_eq!(back.len(), splats.len(), "{name}");
            for (a, b) in splats.iter().zip(&back) {
                assert!(a.pos.iter().zip(&b.pos).all(|(x, y)| (x - y).abs() < 1e-5));
            }
        }
    }

    #[test]
    fn names() {
        assert_eq!(export_name("house", 42, FileFormat::Splat), "splat-live-house-42.splat");
        assert_eq!(export_name("../etc", 1, FileFormat::Ply), "splat-live-etc-1.ply");
        assert_eq!(export_name("", 1, FileFormat::Ply), "splat-live-scene-1.ply");
        assert_eq!(FileFormat::from_name("Scan.PLY"), Some(FileFormat::Ply));
        assert_eq!(FileFormat::from_name("photo.png"), None);
    }

    #[test]
    fn broken_files_give_readable_errors() {
        let good = FileFormat::Ply.encode(&cloud(100));
        // Cut in the middle of the vertex data.
        let cut = parse_import("cut.ply", &good[..good.len() - 100]).unwrap_err();
        assert!(cut.contains("incomplet"), "{cut}");
        // Not a PLY at all.
        let fake = parse_import("fake.ply", b"this is not a ply file at all, sorry").unwrap_err();
        assert!(fake.contains("PLY"), "{fake}");
        // A .splat whose size is off.
        let odd = parse_import("odd.splat", &[0u8; 33]).unwrap_err();
        assert!(odd.contains("multiple de 32"), "{odd}");
        assert!(parse_import("empty.splat", &[]).unwrap_err().contains("vide"));
        assert!(parse_import("notes.txt", b"hello").unwrap_err().contains("Format non pris en charge"));
        // A PLY header without vertices.
        let none = parse_import("none.ply", b"ply\nformat binary_little_endian 1.0\nelement vertex 0\nproperty float x\nproperty float y\nproperty float z\nend_header\n");
        assert!(none.unwrap_err().contains("aucun splat"));
        for message in [cut, fake, odd] {
            assert!(!message.contains('\u{2014}'));
        }
    }

    #[test]
    fn garbage_positions_are_dropped() {
        let mut splats = cloud(10);
        splats[3].pos[0] = 1.0e9;
        splats[4].scale[1] = 1.0e9;
        let bytes = FileFormat::Splat.encode(&splats);
        assert_eq!(parse_import("x.splat", &bytes).unwrap().len(), 8);
        // Nothing but garbage: an error, not an empty scene.
        let junk: Vec<Splat> = splats.iter().map(|s| Splat { pos: [1.0e9; 3], ..*s }).collect();
        assert!(parse_import("x.splat", &FileFormat::Splat.encode(&junk)).is_err());
    }

    #[test]
    fn size_limits() {
        assert!(check_size(1).is_ok());
        assert!(check_size(MAX_IMPORT_BYTES).is_ok());
        let too_big = check_size(MAX_IMPORT_BYTES + 1).unwrap_err();
        assert!(too_big.contains("limite"), "{too_big}");
        assert!(check_size(0).is_err());
    }
}
