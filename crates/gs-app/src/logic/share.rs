//! The shareable viewer: the URL parameters of "4D recording and sharing"
//! (`file`, `embed`, `autoplay`, `loop`, `t`, `view`, `nav`), the
//! `share.json` of an unpacked share archive, and the links the share
//! dialog hands out.
//!
//! Everything a link can carry is validated here, strictly: a link is
//! something people paste from anywhere. A value that does not pass is
//! dropped and reported; it never reaches `fetch`, the viewer or the DOM.

use gs_render::NavMode;
use serde::Deserialize;

/// Longest `file` value accepted, in bytes.
pub const MAX_FILE_URL: usize = 2048;
/// Largest play head position accepted, seconds (about four months).
const MAX_TIME_S: f64 = 1.0e7;
/// Largest camera coordinate accepted, meters.
const MAX_COORDINATE: f32 = 1.0e4;

/// What a `file` parameter points to, by its extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// A `.gsrec` recording, replayed.
    Recording,
    /// A `.ply` or `.splat` scene, shown as an imported file.
    Splats,
}

/// A validated `file` parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRef {
    /// The value as given: a relative path, or an absolute `http(s)` URL.
    /// The browser resolves it against the page; a URL of another origin
    /// is fetched under CORS, without credentials, like any `fetch`.
    pub url: String,
    pub kind: FileKind,
    /// Last path segment, for display and for the format of a splat file.
    pub name: String,
}

/// Why a `file` value was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileUrlError {
    Empty,
    TooLong,
    /// Control characters, tabs, line breaks or backslashes.
    BadCharacters,
    /// Anything but `http:` and `https:` (`javascript:`, `data:`, `blob:`,
    /// `file:` ...).
    Scheme,
    /// User name or password in the URL.
    Credentials,
    /// Not `.gsrec`, `.ply` or `.splat`.
    Extension,
}

impl FileUrlError {
    /// A sentence for the user.
    pub fn text(self) -> &'static str {
        match self {
            Self::Empty => "Le lien ne nomme aucun fichier.",
            Self::TooLong => "L'adresse du fichier est trop longue.",
            Self::BadCharacters => "L'adresse du fichier contient des caractères interdits.",
            Self::Scheme => "Seules les adresses http et https, ou un chemin relatif, sont acceptées.",
            Self::Credentials => "L'adresse du fichier ne doit pas contenir d'identifiants.",
            Self::Extension => "Le fichier doit être un enregistrement .gsrec ou une scène .ply ou .splat.",
        }
    }
}

/// The scheme of `url` (lower case, without the colon), if it starts with one.
fn scheme_of(url: &str) -> Option<String> {
    let (scheme, _) = url.split_once(':')?;
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then(|| scheme.to_ascii_lowercase())
}

/// Validates a `file` value.
///
/// Accepted: a relative reference (`recordings/a.gsrec`, `../x.ply`,
/// `/files/x.splat`, `//host/x.gsrec`) or an absolute `http:` / `https:`
/// URL, without credentials, whose path ends in `.gsrec`, `.ply` or
/// `.splat`. Everything else is refused: no other scheme ever reaches
/// `fetch`.
pub fn check_file_url(raw: &str) -> Result<FileRef, FileUrlError> {
    let url = raw.trim();
    if url.is_empty() {
        return Err(FileUrlError::Empty);
    }
    if url.len() > MAX_FILE_URL {
        return Err(FileUrlError::TooLong);
    }
    // A plain space is harmless (the browser escapes it); tabs and line
    // breaks are what URL parsers silently remove, the classic way to
    // smuggle a scheme past a filter.
    if url.chars().any(|c| c.is_control() || (c.is_whitespace() && c != ' ') || c == '\\') {
        return Err(FileUrlError::BadCharacters);
    }
    // A colon before the first slash, question mark or hash starts a
    // scheme (the same test a URL parser applies to a relative reference).
    let head = url.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = if head.contains(':') {
        match scheme_of(url).as_deref() {
            Some("http" | "https") => {}
            _ => return Err(FileUrlError::Scheme),
        }
        let rest = url.split_once(':').map_or("", |(_, rest)| rest);
        // `http:path` and `http:/path` are legal and confusing: insist on `//`.
        Some(rest.strip_prefix("//").ok_or(FileUrlError::Scheme)?)
    } else {
        url.strip_prefix("//")
    };
    if let Some(rest) = authority {
        let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if host.is_empty() {
            return Err(FileUrlError::Scheme);
        }
        if host.contains('@') {
            return Err(FileUrlError::Credentials);
        }
    }
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let name = path.rsplit('/').next().unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    let kind = if lower.ends_with(".gsrec") {
        FileKind::Recording
    } else if lower.ends_with(".ply") || lower.ends_with(".splat") {
        FileKind::Splats
    } else {
        return Err(FileUrlError::Extension);
    };
    // The name is shown to the user: undo the escapes that are plain text.
    Ok(FileRef { url: url.to_string(), kind, name: percent_decode(name) })
}

/// Decodes `%XX` escapes where the result is valid UTF-8 without control
/// characters; otherwise the text is returned as it is.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (bytes[i], bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(high), Some(low)) => {
                out.push((high * 16 + low) as u8);
                i += 3;
            }
            (byte, ..) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    match String::from_utf8(out) {
        Ok(decoded) if !decoded.chars().any(char::is_control) => decoded,
        _ => text.to_string(),
    }
}

/// A camera placement: eye and target, meters.
pub type View = ([f32; 3], [f32; 3]);

/// Everything a link (or a `share.json`) can ask of the viewer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewerParams {
    pub file: Option<FileRef>,
    /// Canvas only, with small playback controls: for an `<iframe>`.
    pub embed: bool,
    pub autoplay: bool,
    pub looping: bool,
    /// Where playback starts, seconds.
    pub time: Option<f64>,
    pub view: Option<View>,
    pub nav: Option<NavMode>,
}

/// A parameter that was dropped, and why (French, for a notice).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamProblem {
    pub name: &'static str,
    pub text: String,
}

/// A flag: present without value, `1`, `true`, `yes` or `on` is true; `0`,
/// `false`, `no`, `off` is false. Anything else is not a flag.
fn flag(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// `t=<seconds>`: a finite number from 0 up.
pub fn parse_time(value: &str) -> Option<f64> {
    let t: f64 = value.trim().parse().ok()?;
    (t.is_finite() && (0.0..=MAX_TIME_S).contains(&t)).then_some(t)
}

/// `view=<eye x,y,z,target x,y,z>`: six finite numbers, eye and target
/// apart.
pub fn parse_view(value: &str) -> Option<View> {
    let mut numbers = [0.0f32; 6];
    let mut parts = value.split(',');
    for slot in &mut numbers {
        let n: f32 = parts.next()?.trim().parse().ok()?;
        if !n.is_finite() || n.abs() > MAX_COORDINATE {
            return None;
        }
        *slot = n;
    }
    if parts.next().is_some() {
        return None;
    }
    let (eye, target) = ([numbers[0], numbers[1], numbers[2]], [numbers[3], numbers[4], numbers[5]]);
    let apart = eye.iter().zip(&target).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    (apart > 1.0e-4).then_some((eye, target))
}

pub fn parse_nav(value: &str) -> Option<NavMode> {
    match value.trim() {
        "orbit" => Some(NavMode::Orbit),
        "fly" => Some(NavMode::Fly),
        _ => None,
    }
}

impl ViewerParams {
    /// Reads the parameters of a link from decoded `(name, value)` pairs
    /// (what `URLSearchParams` yields). The first occurrence of a name
    /// counts; unknown names are ignored (`source` and `scene` belong to
    /// the live application).
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> (Self, Vec<ParamProblem>) {
        let mut params = Self::default();
        let mut problems = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        for (name, value) in pairs {
            if seen.contains(&name) {
                continue;
            }
            seen.push(name);
            let mut refuse = |name: &'static str, text: &str| problems.push(ParamProblem { name, text: text.to_string() });
            match name {
                "file" => match check_file_url(value) {
                    Ok(file) => params.file = Some(file),
                    Err(error) => refuse("file", error.text()),
                },
                "embed" => match flag(value) {
                    Some(on) => params.embed = on,
                    None => refuse("embed", "Le paramètre embed vaut 0 ou 1."),
                },
                "autoplay" => match flag(value) {
                    Some(on) => params.autoplay = on,
                    None => refuse("autoplay", "Le paramètre autoplay vaut 0 ou 1."),
                },
                "loop" => match flag(value) {
                    Some(on) => params.looping = on,
                    None => refuse("loop", "Le paramètre loop vaut 0 ou 1."),
                },
                "t" => match parse_time(value) {
                    Some(t) => params.time = Some(t),
                    None => refuse("t", "Le paramètre t est un nombre de secondes, positif."),
                },
                "view" => match parse_view(value) {
                    Some(view) => params.view = Some(view),
                    None => refuse("view", "Le paramètre view contient six nombres\u{a0}: la position de l'œil puis le point visé."),
                },
                "nav" => match parse_nav(value) {
                    Some(nav) => params.nav = Some(nav),
                    None => refuse("nav", "Le paramètre nav vaut orbit ou fly."),
                },
                _ => {}
            }
        }
        (params, problems)
    }

    /// Reads a raw query string (`?file=a.gsrec&embed=1`), decoding `+`
    /// and `%XX`. The browser side uses `URLSearchParams` instead; this is
    /// the same reading, to check the links built here.
    #[cfg(test)]
    pub fn from_query(query: &str) -> (Self, Vec<ParamProblem>) {
        let decoded: Vec<(String, String)> = query
            .trim_start_matches('?')
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                (percent_decode(&name.replace('+', " ")), percent_decode(&value.replace('+', " ")))
            })
            .collect();
        Self::from_pairs(decoded.iter().map(|(name, value)| (name.as_str(), value.as_str())))
    }

    /// The parameters of a `share.json` (see [`ShareManifest`]), which the
    /// link's own parameters then override.
    pub fn overridden_by(self, link: Self, link_names: &[&str]) -> Self {
        let has = |name: &str| link_names.contains(&name);
        Self {
            file: link.file.or(self.file),
            embed: if has("embed") { link.embed } else { self.embed },
            autoplay: if has("autoplay") { link.autoplay } else { self.autoplay },
            looping: if has("loop") { link.looping } else { self.looping },
            time: link.time.or(self.time),
            view: link.view.or(self.view),
            nav: link.nav.or(self.nav),
        }
    }
}

/// `share.json`, next to `index.html` in an unpacked share archive: it
/// names the recording the viewer starts on. Only `file` is required;
/// unknown keys are ignored.
///
/// ```json
/// { "file": "recordings/danse.gsrec", "autoplay": true, "loop": true }
/// ```
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ShareManifest {
    pub file: String,
    pub embed: bool,
    pub autoplay: bool,
    #[serde(rename = "loop")]
    pub looping: bool,
    pub t: Option<f64>,
    pub view: Option<[f32; 6]>,
    pub nav: Option<String>,
}

/// Largest `share.json` read, bytes.
pub const MAX_MANIFEST_BYTES: usize = 16 * 1024;

impl ShareManifest {
    /// Parses and validates a `share.json`. `None` for anything that is not
    /// one (the index page a server answers for unknown paths, say), or
    /// whose file is refused: a host's manifest gets no more trust than a
    /// link.
    pub fn parse(json: &str) -> Option<ViewerParams> {
        if json.len() > MAX_MANIFEST_BYTES {
            return None;
        }
        let manifest: Self = serde_json::from_str(json).ok()?;
        let file = check_file_url(&manifest.file).ok()?;
        Some(ViewerParams {
            file: Some(file),
            embed: manifest.embed,
            autoplay: manifest.autoplay,
            looping: manifest.looping,
            time: manifest.t.filter(|t| t.is_finite() && (0.0..=MAX_TIME_S).contains(t)),
            view: manifest.view.and_then(|v| parse_view(&v.map(|n| n.to_string()).join(","))),
            nav: manifest.nav.as_deref().and_then(parse_nav),
        })
    }
}

/// Escapes a query value: everything but unreserved characters and the
/// path characters that are legal there (`/`, `:`), so that links stay
/// readable.
pub fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn number(value: f32) -> String {
    // Centimeters are plenty for a camera.
    let rounded = (f64::from(value) * 100.0).round() / 100.0;
    format!("{rounded}")
}

/// The query string (`?file=...`) of a link to `file`.
pub fn share_query(file: &str, embed: bool, autoplay: bool, looping: bool, time: Option<f64>, view: Option<View>) -> String {
    let mut query = format!("?file={}", encode_query_value(file));
    if embed {
        query.push_str("&embed=1");
    }
    if autoplay {
        query.push_str("&autoplay=1");
    }
    if looping {
        query.push_str("&loop=1");
    }
    if let Some(t) = time.filter(|t| t.is_finite() && *t > 0.0) {
        query.push_str(&format!("&t={}", (t * 10.0).round() / 10.0));
    }
    if let Some((eye, target)) = view {
        let numbers: Vec<String> = eye.iter().chain(&target).map(|n| number(*n)).collect();
        query.push_str(&format!("&view={}", numbers.join(",")));
    }
    query
}

/// The link of the share dialog: the recording `file_name` of the server's
/// recordings directory, in the embedded viewer, on the LAN address `base`
/// (`https://192.168.1.20:7879`).
pub fn lan_share_link(base: &str, file_name: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), share_query(&format!("recordings/{file_name}"), true, true, true, None, None))
}

/// Escapes text for an HTML attribute value in double quotes.
fn attribute(text: &str) -> String {
    text.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The `<iframe>` snippet of the share dialog for `link`.
pub fn iframe_snippet(link: &str) -> String {
    format!(
        "<iframe src=\"{}\" width=\"960\" height=\"540\" style=\"border:0\" allow=\"fullscreen; xr-spatial-tracking\" allowfullscreen></iframe>",
        attribute(link)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_and_http_files_are_accepted() {
        for (url, kind, name) in [
            ("recordings/danse.gsrec", FileKind::Recording, "danse.gsrec"),
            ("a.gsrec", FileKind::Recording, "a.gsrec"),
            ("./scenes/Salon.PLY", FileKind::Splats, "Salon.PLY"),
            ("../x.splat", FileKind::Splats, "x.splat"),
            ("/files/x.splat?v=3#top", FileKind::Splats, "x.splat"),
            ("https://example.org/a/b/c.gsrec", FileKind::Recording, "c.gsrec"),
            ("HTTP://example.org:8080/c.ply", FileKind::Splats, "c.ply"),
            ("//cdn.example.org/c.gsrec", FileKind::Recording, "c.gsrec"),
            ("recordings/soir%C3%A9e%20du%2014.gsrec", FileKind::Recording, "soirée du 14.gsrec"),
            ("  spaced.gsrec  ", FileKind::Recording, "spaced.gsrec"),
            ("danse du soir.gsrec", FileKind::Recording, "danse du soir.gsrec"),
            // A colon after the first slash is part of the path.
            ("dir/a:b.gsrec", FileKind::Recording, "a:b.gsrec"),
        ] {
            let file = check_file_url(url).unwrap_or_else(|e| panic!("{url}: {e:?}"));
            assert_eq!((file.kind, file.name.as_str()), (kind, name), "{url}");
            assert_eq!(file.url, url.trim());
        }
    }

    #[test]
    fn everything_else_is_refused() {
        use FileUrlError::*;
        for (url, error) in [
            ("", Empty),
            ("   ", Empty),
            ("javascript:alert(1)//x.gsrec", Scheme),
            ("JaVaScRiPt:alert(1)//.gsrec", Scheme),
            ("data:application/octet-stream;base64,AAAA#x.gsrec", Scheme),
            ("data:text/html,<script>1</script>.gsrec", Scheme),
            ("blob:https://example.org/uuid.gsrec", Scheme),
            ("file:///etc/passwd.gsrec", Scheme),
            ("ftp://example.org/a.gsrec", Scheme),
            ("vbscript:x.gsrec", Scheme),
            ("ws://example.org/a.gsrec", Scheme),
            // Legal for a URL parser, never what a link means.
            ("http:a.gsrec", Scheme),
            ("https:/a.gsrec", Scheme),
            ("https:///a.gsrec", Scheme),
            ("a:b.gsrec", Scheme),
            (":x.gsrec", Scheme),
            ("https://user:pass@example.org/a.gsrec", Credentials),
            ("//user@example.org/a.gsrec", Credentials),
            ("java\tscript:x.gsrec", BadCharacters),
            ("a\u{a0}b.gsrec", BadCharacters),
            ("java script:x.gsrec", Scheme),
            ("a\\b.gsrec", BadCharacters),
            ("a\n.gsrec", BadCharacters),
            ("a\u{0}.gsrec", BadCharacters),
            ("index.html", Extension),
            ("a.gsrec.exe", Extension),
            ("a.gscap", Extension),
            ("recordings/", Extension),
            ("x?file=a.gsrec", Extension),
            ("x#a.gsrec", Extension),
            ("https://example.org", Extension),
        ] {
            assert_eq!(check_file_url(url), Err(error), "{url:?}");
            assert!(!error.text().is_empty() && !error.text().contains('\u{2014}'));
        }
        let long = format!("{}.gsrec", "a".repeat(MAX_FILE_URL));
        assert_eq!(check_file_url(&long), Err(TooLong));
    }

    #[test]
    fn a_link_is_read_parameter_by_parameter() {
        let (params, problems) = ViewerParams::from_query("?file=recordings/a.gsrec&embed=1&autoplay&loop=true&t=12.5&view=1,2,3,0,1,0&nav=fly&source=worker");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(params.file.as_ref().map(|f| (f.url.as_str(), f.kind)), Some(("recordings/a.gsrec", FileKind::Recording)));
        assert!(params.embed && params.autoplay && params.looping);
        assert_eq!((params.time, params.nav), (Some(12.5), Some(NavMode::Fly)));
        assert_eq!(params.view, Some(([1.0, 2.0, 3.0], [0.0, 1.0, 0.0])));

        // Nothing given: nothing asked.
        assert_eq!(ViewerParams::from_query(""), (ViewerParams::default(), Vec::new()));
        assert_eq!(ViewerParams::from_query("?scene=house").0, ViewerParams::default());
        // Escapes are decoded before validation.
        let (params, problems) = ViewerParams::from_pairs([("file", "java\tscript:alert(1)//a.gsrec")]);
        assert!(params.file.is_none() && problems.len() == 1);
        let (params, _) = ViewerParams::from_query("?file=https%3A%2F%2Fexample.org%2Fa.gsrec&embed=0");
        assert_eq!(params.file.unwrap().url, "https://example.org/a.gsrec");
        // The first occurrence counts.
        let (params, _) = ViewerParams::from_query("?t=3&t=9&embed=0&embed=1");
        assert_eq!((params.time, params.embed), (Some(3.0), false));
    }

    #[test]
    fn bad_values_are_dropped_one_by_one_and_reported() {
        let (params, problems) =
            ViewerParams::from_query("?file=javascript:alert(1)&embed=maybe&autoplay=2&loop=x&t=-1&view=1,2,3&nav=teleport");
        assert_eq!(params, ViewerParams::default());
        let names: Vec<&str> = problems.iter().map(|p| p.name).collect();
        assert_eq!(names, ["file", "embed", "autoplay", "loop", "t", "view", "nav"]);
        assert!(problems.iter().all(|p| !p.text.is_empty() && !p.text.contains('\u{2014}')));
        for t in ["NaN", "inf", "-0.5", "1e99", "abc", "", "1,5"] {
            assert_eq!(parse_time(t), None, "{t}");
        }
        assert_eq!(parse_time(" 0 "), Some(0.0));
        for view in ["", "1,2,3,4,5", "1,2,3,4,5,6,7", "1,2,3,1,2,3", "1,2,3,4,5,NaN", "1,2,3,4,5,inf", "1e9,0,0,0,0,0", "a,b,c,d,e,f"] {
            assert_eq!(parse_view(view), None, "{view}");
        }
        assert_eq!(parse_view(" 1, 2 ,3,0,0,0"), Some(([1.0, 2.0, 3.0], [0.0; 3])));
        assert_eq!(parse_nav("Orbit"), None);
    }

    #[test]
    fn share_json_names_the_recording_and_gets_no_more_trust_than_a_link() {
        let params = ShareManifest::parse(r#"{"file":"danse.gsrec","autoplay":true,"loop":true,"name":"Danse","t":2,"nav":"orbit","view":[1,2,3,0,1,0]}"#).unwrap();
        assert_eq!(params.file.as_ref().unwrap().url, "danse.gsrec");
        assert!(params.autoplay && params.looping && !params.embed);
        assert_eq!((params.time, params.nav, params.view), (Some(2.0), Some(NavMode::Orbit), Some(([1.0, 2.0, 3.0], [0.0, 1.0, 0.0]))));
        assert!(ShareManifest::parse(r#"{"file":"a.gsrec"}"#).is_some());
        for bad in [
            "<!doctype html><html></html>",
            "",
            "{}",
            "[]",
            r#"{"file":"javascript:alert(1)//a.gsrec"}"#,
            r#"{"file":"data:,x.gsrec"}"#,
            r#"{"file":"a.txt"}"#,
            r#"{"file":7}"#,
        ] {
            assert!(ShareManifest::parse(bad).is_none(), "{bad}");
        }
        assert!(ShareManifest::parse(&format!(r#"{{"file":"a.gsrec","pad":"{}"}}"#, "x".repeat(MAX_MANIFEST_BYTES))).is_none());
        // Bad optional values are dropped, the file still opens.
        let params = ShareManifest::parse(r#"{"file":"a.gsrec","t":-4,"nav":"walk","view":[0,0,0,0,0,0]}"#).unwrap();
        assert_eq!((params.time, params.nav, params.view), (None, None, None));

        // The link overrides the manifest, parameter by parameter.
        let manifest = ShareManifest::parse(r#"{"file":"a.gsrec","autoplay":true,"loop":true}"#).unwrap();
        let (link, _) = ViewerParams::from_query("?loop=0&t=4&embed=1");
        let merged = manifest.overridden_by(link, &["loop", "t", "embed"]);
        assert_eq!(merged.file.as_ref().unwrap().url, "a.gsrec");
        assert!(merged.autoplay && !merged.looping && merged.embed);
        assert_eq!(merged.time, Some(4.0));
    }

    #[test]
    fn share_links_round_trip_through_the_parser() {
        let link = lan_share_link("https://192.168.1.20:7879/", "soirée du 14.gsrec");
        assert_eq!(link, "https://192.168.1.20:7879/?file=recordings/soir%C3%A9e%20du%2014.gsrec&embed=1&autoplay=1&loop=1");
        let (params, problems) = ViewerParams::from_query(link.split_once('?').unwrap().1);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(params.embed && params.autoplay && params.looping);
        assert_eq!(params.file.unwrap().name, "soirée du 14.gsrec");

        let query = share_query("a b&c.gsrec", false, false, false, Some(12.34), Some(([1.234, 2.0, -3.5], [0.0, 1.0, 0.0])));
        assert_eq!(query, "?file=a%20b%26c.gsrec&t=12.3&view=1.23,2,-3.5,0,1,0");
        let (params, problems) = ViewerParams::from_query(&query);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(params.file.as_ref().unwrap().name, "a b&c.gsrec");
        assert_eq!((params.time, params.view), (Some(12.3), Some(([1.23, 2.0, -3.5], [0.0, 1.0, 0.0]))));
    }

    #[test]
    fn the_iframe_snippet_cannot_be_broken_out_of() {
        let snippet = iframe_snippet("https://h/?file=a.gsrec&embed=1");
        assert!(snippet.starts_with("<iframe src=\"https://h/?file=a.gsrec&amp;embed=1\""));
        assert!(snippet.contains("allowfullscreen") && snippet.contains("xr-spatial-tracking"));
        let hostile = iframe_snippet("https://h/\"><script>alert(1)</script>");
        assert!(!hostile.contains("<script>") && !hostile.contains("\"><"));
    }
}
