//! Where recordings (`.gsrec`) and raw captures (`.gscap`) live on disk.
//!
//! Two flat directories, each holding files of one kind. Nothing in a
//! request ever becomes a path by itself: a file is reached by its *name*,
//! which must pass [`is_safe_name`] and belong to a regular file directly
//! inside the directory (a symbolic link is not followed). Names the
//! server creates come from [`slug`], which keeps letters and digits only.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use gs_core::ingest::GscapHeader;
use gs_core::record;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::Serialize;

/// Extension of a recording, with its dot.
pub(crate) const RECORDING_EXT: &str = ".gsrec";
/// Extension of a raw capture, with its dot.
pub(crate) const CAPTURE_EXT: &str = ".gscap";
/// Extension of a file that is still being received (never listed or
/// served).
const PART_EXT: &str = ".part";

/// Longest file name accepted, in bytes (file systems stop at 255).
const MAX_NAME_BYTES: usize = 200;
/// Longest stem [`slug`] produces, in characters.
const MAX_SLUG_CHARS: usize = 60;
/// Most files a listing reports: a directory someone filled with a
/// million files must not turn one request into minutes of disk access.
const MAX_LISTED: usize = 2000;
/// How much of a file is read to find its header. The recorder of
/// `gs_core::record` pads its header to 1024 bytes and the capture writer
/// to 1036; a hand-made file with a longer header is listed without its
/// details.
const HEADER_PROBE: usize = 8192;

/// What stays unescaped in a URL path segment (RFC 3986 "unreserved").
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

/// The two kinds of files the store keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Recording,
    Capture,
}

impl Kind {
    pub fn ext(self) -> &'static str {
        match self {
            Self::Recording => RECORDING_EXT,
            Self::Capture => CAPTURE_EXT,
        }
    }

    /// URL prefix the files of this kind are served under.
    pub fn url_prefix(self) -> &'static str {
        match self {
            Self::Recording => "recordings",
            Self::Capture => "captures",
        }
    }
}

/// One recording as `GET /api/recordings` lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RecordingEntry {
    /// File name without `.gsrec`: the `<name>` of
    /// `GET /recordings/<name>.gsrec` and `GET /api/share/<name>.zip`.
    pub name: String,
    /// File name.
    pub file: String,
    /// Name given by whoever recorded it (the `name` of the file header).
    pub title: String,
    /// Scene identifier of the session: `stage`, `house` or `capture`.
    pub scene: String,
    /// Start of the recording, RFC 3339.
    pub created: String,
    /// Length in seconds; 0 for a recording that was never finished.
    pub duration_s: f64,
    /// Size of the file in bytes.
    pub size: u64,
    /// Where the file is served, relative to the root of the server and
    /// already percent-encoded: `recordings/<file>`. Usable as the `file`
    /// URL parameter of the viewer.
    pub url: String,
    /// The server is still writing this file.
    pub recording: bool,
}

/// One raw capture as `GET /api/captures` lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CaptureEntry {
    /// File name without `.gscap`.
    pub name: String,
    /// File name.
    pub file: String,
    /// What the user called the capture on the phone.
    pub title: String,
    /// `room`, `object`, `person` or `live`.
    pub mode: String,
    /// Hardware model of the phone.
    pub device: String,
    /// Start of the capture, RFC 3339.
    pub created: String,
    /// Size of the file in bytes.
    pub size: u64,
    /// `captures/<file>`, percent-encoded (loopback listener only).
    pub url: String,
    /// The server is still writing this file.
    pub capturing: bool,
}

/// The directories and what is being written in them right now.
pub(crate) struct Store {
    recordings: Option<PathBuf>,
    captures: Option<PathBuf>,
    /// Files being written, as `(kind, file name)`: they are not deleted,
    /// zipped or replaced meanwhile.
    busy: Mutex<HashSet<(Kind, String)>>,
}

/// A file of the store that is being written. While this exists the file
/// is not deleted, zipped or replaced, and its name is not given to another
/// writer; dropping it (also when its owner panics) lets go.
pub(crate) struct Hold {
    store: Arc<Store>,
    kind: Kind,
    file: String,
}

impl Hold {
    /// Name of the file.
    pub fn file(&self) -> &str {
        &self.file
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.store.busy().remove(&(self.kind, std::mem::take(&mut self.file)));
    }
}

/// Whether `name` may name a file of the store: one path segment ending in
/// `ext`, nothing hidden, nothing a file system treats specially. This is
/// the only gate between a request and the directory.
pub(crate) fn is_safe_name(name: &str, ext: &str) -> bool {
    let Some(stem) = name.strip_suffix(ext) else { return false };
    !stem.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !stem.starts_with(['.', ' '])
        && !stem.ends_with(['.', ' '])
        && name.chars().all(|c| !c.is_control() && !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        && !is_reserved_device(stem)
}

/// Names Windows reserves whatever their extension (`CON`, `NUL`, `COM1`…).
fn is_reserved_device(stem: &str) -> bool {
    let base = stem.split('.').next().unwrap_or(stem).to_ascii_uppercase();
    matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base.as_bytes()[3].is_ascii_digit())
}

/// A file stem made from free text: letters and digits of any script,
/// `-` and `_`; everything else becomes one `-`. `fallback` when nothing
/// is left.
pub(crate) fn slug(text: &str, fallback: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.chars().count() >= MAX_SLUG_CHARS {
            break;
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() || is_reserved_device(out) {
        fallback.to_string()
    } else {
        out.to_string()
    }
}

/// Now, as `YYYYMMDD-HHMMSS` in UTC: sorts by time and is unique enough for
/// a file name (a counter covers the rest).
pub(crate) fn timestamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
    // `2026-11-14T10:32:07Z`
    let text = record::rfc3339_utc(now);
    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
    match digits.len() {
        14 => format!("{}-{}", &digits[..8], &digits[8..]),
        _ => digits,
    }
}

/// `prefix/file` with the file name escaped for a URL.
pub(crate) fn url_of(kind: Kind, file: &str) -> String {
    format!("{}/{}", kind.url_prefix(), utf8_percent_encode(file, SEGMENT))
}

fn modified_ns(meta: &fs::Metadata) -> u128 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos())
}

/// The first bytes of a file, at most [`HEADER_PROBE`].
fn read_prefix(path: &Path) -> io::Result<Vec<u8>> {
    let mut prefix = Vec::with_capacity(HEADER_PROBE);
    File::open(path)?.take(HEADER_PROBE as u64).read_to_end(&mut prefix)?;
    Ok(prefix)
}

/// The JSON header of the `.gscap` at `path`, if its first kilobytes
/// hold it whole.
pub(crate) fn peek_capture_header(path: &Path) -> Option<GscapHeader> {
    capture_header(&read_prefix(path).ok()?)
}

/// A file in the system's temporary directory to receive data into, for a
/// server that keeps no captures. Returns its path and the open file.
pub(crate) fn temporary_part() -> io::Result<(PathBuf, File)> {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("splat-live-{}-{unique}{CAPTURE_EXT}{PART_EXT}", std::process::id()));
    // `create_new`: never through a link somebody planted under that name.
    let file = OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
    Ok((path, file))
}

/// The JSON header of a `.gscap` from its first bytes, if it is whole.
fn capture_header(prefix: &[u8]) -> Option<GscapHeader> {
    use gs_core::ingest::{GscapParser, ParsedItem};
    let mut parser = GscapParser::new();
    parser.push(prefix);
    match parser.next_item() {
        Ok(Some(ParsedItem::Header(header))) => Some(header),
        _ => None,
    }
}

impl Store {
    /// `captures` defaults to a directory `captures` next to `recordings`.
    pub fn new(recordings: Option<PathBuf>, captures: Option<PathBuf>) -> Self {
        let captures = captures
            .or_else(|| recordings.as_ref().map(|dir| dir.parent().unwrap_or(Path::new("")).join("captures")));
        Self { recordings, captures, busy: Mutex::new(HashSet::new()) }
    }

    pub fn dir(&self, kind: Kind) -> Option<&Path> {
        match kind {
            Kind::Recording => self.recordings.as_deref(),
            Kind::Capture => self.captures.as_deref(),
        }
    }

    /// The directory of `kind`, created if need be.
    fn ensure_dir(&self, kind: Kind) -> io::Result<&Path> {
        let dir = self.dir(kind).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no directory configured"))?;
        fs::create_dir_all(dir)?;
        Ok(dir)
    }

    fn busy(&self) -> std::sync::MutexGuard<'_, HashSet<(Kind, String)>> {
        self.busy.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn is_busy(&self, kind: Kind, file: &str) -> bool {
        self.busy().contains(&(kind, file.to_string()))
    }

    /// The path of an existing file of the store, or `None` when `file` is
    /// not a safe name or does not name a regular file of the directory.
    pub fn existing(&self, kind: Kind, file: &str) -> Option<PathBuf> {
        if !is_safe_name(file, kind.ext()) {
            return None;
        }
        let path = self.dir(kind)?.join(file);
        // Not followed: a link planted in the directory leads nowhere.
        fs::symlink_metadata(&path).ok().filter(fs::Metadata::is_file).map(|_| path)
    }

    /// Creates a new file `<stem><ext>` in the directory of `kind`; when
    /// the name is taken, `<stem>-2`, `<stem>-3`… Returns the hold on its
    /// name, its path and the open file.
    pub fn create(self: &Arc<Self>, kind: Kind, stem: &str) -> io::Result<(Hold, PathBuf, File)> {
        let dir = self.ensure_dir(kind)?;
        for attempt in 1..=999u32 {
            let file = match attempt {
                1 => format!("{stem}{}", kind.ext()),
                n => format!("{stem}-{n}{}", kind.ext()),
            };
            if !is_safe_name(&file, kind.ext()) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a usable file name"));
            }
            let path = dir.join(&file);
            // Reserved under the lock, so that two writers never agree on
            // one name, and never the name of a file being replaced.
            {
                let mut busy = self.busy();
                if busy.contains(&(kind, file.clone())) {
                    continue;
                }
                match OpenOptions::new().write(true).create_new(true).open(&path) {
                    Ok(handle) => {
                        busy.insert((kind, file.clone()));
                        return Ok((Hold { store: self.clone(), kind, file }, path, handle));
                    }
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(err) => return Err(err),
                }
            }
        }
        Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free file name"))
    }

    /// A file to receive data into before it gets its final name. Its name
    /// starts with a dot and ends in `.part`: it is never listed or served.
    pub fn create_part(&self, kind: Kind) -> io::Result<(PathBuf, File)> {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = self.ensure_dir(kind)?;
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!(".{}-{unique}{}{PART_EXT}", std::process::id(), kind.ext()));
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path)?;
        Ok((path, file))
    }

    /// Gives a received file ([`Store::create_part`]) its final name,
    /// `<stem><ext>` or the next free variant. Returns the name.
    pub fn adopt(self: &Arc<Self>, kind: Kind, part: &Path, stem: &str) -> io::Result<String> {
        let (hold, path, handle) = self.create(kind, stem)?;
        drop(handle);
        match fs::rename(part, &path) {
            Ok(()) => Ok(hold.file().to_string()),
            Err(err) => {
                let _ = fs::remove_file(&path);
                Err(err)
            }
        }
    }

    /// Removes the leftovers of uploads a previous run did not finish.
    pub fn sweep_parts(&self) {
        for kind in [Kind::Recording, Kind::Capture] {
            let Some(dir) = self.dir(kind) else { continue };
            let Ok(entries) = fs::read_dir(dir) else { continue };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') && name.ends_with(PART_EXT) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }

    /// Deletes a file of the store. `Ok(false)`: no such file.
    pub fn delete(&self, kind: Kind, file: &str) -> io::Result<bool> {
        let Some(path) = self.existing(kind, file) else { return Ok(false) };
        if self.is_busy(kind, file) {
            return Err(io::Error::new(io::ErrorKind::ResourceBusy, "the file is being written"));
        }
        match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Safe file names of `kind` with their metadata, newest first.
    fn files(&self, kind: Kind) -> Vec<(String, PathBuf, fs::Metadata)> {
        let Some(dir) = self.dir(kind) else { return Vec::new() };
        let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else { continue };
            if !is_safe_name(&name, kind.ext()) {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else { continue };
            if meta.is_file() {
                files.push((name, path, meta));
            }
            if files.len() >= MAX_LISTED {
                break;
            }
        }
        files.sort_by(|a, b| modified_ns(&b.2).cmp(&modified_ns(&a.2)).then_with(|| a.0.cmp(&b.0)));
        files
    }

    /// Every recording of the directory, newest first. Reads the first
    /// kilobytes of each file: call it off the async threads.
    pub fn list_recordings(&self) -> Vec<RecordingEntry> {
        self.files(Kind::Recording)
            .into_iter()
            .map(|(file, path, meta)| self.recording_entry(&file, &path, &meta))
            .collect()
    }

    fn recording_entry(&self, file: &str, path: &Path, meta: &fs::Metadata) -> RecordingEntry {
        let info = read_prefix(path).ok().and_then(|prefix| record::read_info(&prefix).ok()).unwrap_or_default();
        let name = file.strip_suffix(RECORDING_EXT).unwrap_or(file).to_string();
        RecordingEntry {
            title: if info.name.is_empty() { name.clone() } else { info.name },
            name,
            file: file.to_string(),
            scene: info.scene,
            created: info.created,
            duration_s: if info.duration_s.is_finite() { info.duration_s.max(0.0) } else { 0.0 },
            size: meta.len(),
            url: url_of(Kind::Recording, file),
            recording: self.is_busy(Kind::Recording, file),
        }
    }

    /// The listing entry of one recording.
    pub fn recording(&self, file: &str) -> Option<RecordingEntry> {
        let path = self.existing(Kind::Recording, file)?;
        let meta = fs::symlink_metadata(&path).ok()?;
        Some(self.recording_entry(file, &path, &meta))
    }

    /// Every raw capture of the directory, newest first.
    pub fn list_captures(&self) -> Vec<CaptureEntry> {
        self.files(Kind::Capture)
            .into_iter()
            .map(|(file, path, meta)| {
                let header = read_prefix(&path).ok().and_then(|prefix| capture_header(&prefix)).unwrap_or_default();
                let name = file.strip_suffix(CAPTURE_EXT).unwrap_or(&file).to_string();
                CaptureEntry {
                    title: if header.name.is_empty() { name.clone() } else { header.name },
                    name,
                    mode: header.mode,
                    device: header.device,
                    created: header.created,
                    size: meta.len(),
                    url: url_of(Kind::Capture, &file),
                    capturing: self.is_busy(Kind::Capture, &file),
                    file,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_names_pass() {
        for good in ["a.gsrec", "Salon-20261114-101500.gsrec", "été au salon.gsrec", "v1.2.gsrec", "日本.gsrec"] {
            assert!(is_safe_name(good, RECORDING_EXT), "{good}");
        }
        for bad in [
            ".gsrec",
            "a.gscap",
            "a.gsrec.txt",
            "a",
            "",
            "../a.gsrec",
            "..gsrec",
            ".hidden.gsrec",
            "a/b.gsrec",
            "a\\b.gsrec",
            "c:a.gsrec",
            "a\0.gsrec",
            "a\n.gsrec",
            "a?.gsrec",
            "a*.gsrec",
            "a|b.gsrec",
            "\"a\".gsrec",
            "<a>.gsrec",
            " a.gsrec",
            "a .gsrec",
            "a..gsrec",
            "CON.gsrec",
            "nul.gsrec",
            "com1.gsrec",
            "LPT9.x.gsrec",
            "a.GSREC",
        ] {
            assert!(!is_safe_name(bad, RECORDING_EXT), "{bad:?}");
        }
        assert!(!is_safe_name(&format!("{}.gsrec", "a".repeat(MAX_NAME_BYTES)), RECORDING_EXT));
    }

    #[test]
    fn slugs_are_safe_names() {
        assert_eq!(slug("Salon", "x"), "Salon");
        assert_eq!(slug("  Danse du salon !  ", "x"), "Danse-du-salon");
        assert_eq!(slug("été / hiver", "x"), "été-hiver");
        assert_eq!(slug("../../etc/passwd", "x"), "etc-passwd");
        assert_eq!(slug("a\\..\\b", "x"), "a-b");
        assert_eq!(slug("...", "capture"), "capture");
        assert_eq!(slug("", "capture"), "capture");
        assert_eq!(slug("CON", "capture"), "capture");
        assert_eq!(slug("\u{0}\u{7}x\n", "c"), "x");
        assert_eq!(slug(&"é".repeat(500), "x").chars().count(), MAX_SLUG_CHARS);
        for text in ["Salon", "a/b", "..", "été", "x y z", "名前", "a.b", "-_-"] {
            let file = format!("{}{RECORDING_EXT}", slug(text, "f"));
            assert!(is_safe_name(&file, RECORDING_EXT), "{text:?} -> {file:?}");
        }
    }

    #[test]
    fn timestamps_have_the_expected_shape() {
        let stamp = timestamp();
        assert_eq!(stamp.len(), 15, "{stamp}");
        assert_eq!(stamp.as_bytes()[8], b'-');
        assert!(stamp.bytes().enumerate().all(|(i, b)| i == 8 || b.is_ascii_digit()));
    }

    #[test]
    fn urls_are_escaped() {
        assert_eq!(url_of(Kind::Recording, "a b.gsrec"), "recordings/a%20b.gsrec");
        assert_eq!(url_of(Kind::Recording, "été.gsrec"), "recordings/%C3%A9t%C3%A9.gsrec");
        assert_eq!(url_of(Kind::Capture, "x-1_2.gscap"), "captures/x-1_2.gscap");
    }

    #[test]
    fn names_are_unique_and_files_stay_inside() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::new(Some(root.path().join("rec")), None));
        assert_eq!(store.dir(Kind::Capture).unwrap(), root.path().join("captures"));

        let (first, path, _) = store.create(Kind::Recording, "salon").unwrap();
        let (second, ..) = store.create(Kind::Recording, "salon").unwrap();
        assert_eq!((first.file(), second.file()), ("salon.gsrec", "salon-2.gsrec"));
        assert!(path.starts_with(root.path().join("rec")));
        assert!(store.is_busy(Kind::Recording, "salon.gsrec"));
        // Being written: not deleted, and listed as such.
        assert!(store.delete(Kind::Recording, "salon.gsrec").is_err());
        assert!(store.list_recordings().iter().all(|entry| entry.recording));
        drop(first);
        assert!(!store.is_busy(Kind::Recording, "salon.gsrec"));
        assert!(store.delete(Kind::Recording, "salon.gsrec").unwrap());
        assert!(!store.delete(Kind::Recording, "salon.gsrec").unwrap());
        // A name that is held is not handed out again, even once its file
        // is gone.
        std::fs::remove_file(root.path().join("rec/salon-2.gsrec")).unwrap();
        let (again, ..) = store.create(Kind::Recording, "salon").unwrap();
        let (third, ..) = store.create(Kind::Recording, "salon").unwrap();
        assert_eq!((again.file(), third.file()), ("salon.gsrec", "salon-3.gsrec"));
        drop(second);

        // What is not a plain file of the directory does not exist.
        fs::write(root.path().join("secret.gsrec"), b"x").unwrap();
        assert!(store.existing(Kind::Recording, "../secret.gsrec").is_none());
        fs::create_dir(root.path().join("rec/dir.gsrec")).unwrap();
        assert!(store.existing(Kind::Recording, "dir.gsrec").is_none());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.path().join("secret.gsrec"), root.path().join("rec/link.gsrec")).unwrap();
            assert!(store.existing(Kind::Recording, "link.gsrec").is_none());
            assert!(store.list_recordings().iter().all(|entry| entry.file != "link.gsrec"));
        }
        assert!(store.create(Kind::Recording, "../x").is_err());
    }

    #[test]
    fn parts_are_invisible_until_adopted() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::new(Some(root.path().join("recordings")), Some(root.path().join("caps"))));
        let (part, mut file) = store.create_part(Kind::Capture).unwrap();
        use std::io::Write;
        file.write_all(b"not a capture").unwrap();
        drop(file);
        assert!(store.list_captures().is_empty());
        let name = store.adopt(Kind::Capture, &part, "Salon").unwrap();
        assert_eq!(name, "Salon.gscap");
        assert!(!part.exists());
        let listed = store.list_captures();
        assert_eq!(listed.len(), 1);
        // No readable header: listed by its file name.
        assert_eq!((listed[0].title.as_str(), listed[0].size, listed[0].capturing), ("Salon", 13, false));

        let (stale, _) = store.create_part(Kind::Capture).unwrap();
        store.sweep_parts();
        assert!(!stale.exists());
        assert_eq!(store.list_captures().len(), 1);
    }
}
