//! The pairing URI the desktop shows as a QR code:
//! `splatlive://pair?host=<ip>&port=<lan port>&fp=<sha256>&token=<t>`.
//!
//! `fp` is the SHA-256 of the server certificate (DER) as 64 lowercase hex
//! digits, `token` 32 lowercase hex digits. The parsing rules are those of
//! `parse_pairing_uri` in `tools/gscap.py`, shared with the phone.

use std::fmt;

/// Everything before the `?` of a pairing URI.
pub const PAIRING_PREFIX: &str = "splatlive://pair";

/// What can be wrong with a pairing URI; the identifiers are shared with the
/// reference implementation and the fixtures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PairingError {
    BadScheme,
    BadEncoding,
    DuplicateParameter,
    MissingHost,
    MissingPort,
    MissingFp,
    MissingToken,
    BadHost,
    BadPort,
    BadFp,
    BadToken,
}

impl PairingError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadScheme => "bad_scheme",
            Self::BadEncoding => "bad_encoding",
            Self::DuplicateParameter => "duplicate_parameter",
            Self::MissingHost => "missing_host",
            Self::MissingPort => "missing_port",
            Self::MissingFp => "missing_fp",
            Self::MissingToken => "missing_token",
            Self::BadHost => "bad_host",
            Self::BadPort => "bad_port",
            Self::BadFp => "bad_fp",
            Self::BadToken => "bad_token",
        }
    }
}

impl fmt::Display for PairingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for PairingError {}

/// A server the phone can pair with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pairing {
    /// Host name or address, lowercase, an IPv6 literal without brackets.
    pub host: String,
    pub port: u16,
    /// SHA-256 of the server certificate: 64 lowercase hex digits.
    pub fingerprint: String,
    /// 32 lowercase hex digits.
    pub token: String,
}

fn normalise_host(text: &str) -> Result<String, PairingError> {
    let mut host = text;
    if host.len() >= 2 && host.starts_with('[') && host.ends_with(']') {
        host = &host[1..host.len() - 1];
    }
    if host.is_empty() || host.chars().count() > 253 {
        return Err(PairingError::BadHost);
    }
    if host.contains(':') {
        // An IPv6 literal has at least two colons; "1.2.3.4:7879" is a
        // port glued to an address, not a host.
        let ipv6 = host.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.');
        if !ipv6 || host.matches(':').count() < 2 {
            return Err(PairingError::BadHost);
        }
    } else {
        let name = host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
        let edge = |c: Option<char>| matches!(c, Some('.' | '-'));
        if !name || edge(host.chars().next()) || edge(host.chars().last()) {
            return Err(PairingError::BadHost);
        }
    }
    Ok(host.to_ascii_lowercase())
}

/// 64 lowercase hex digits from hex with optional colons (the form
/// `/api/info` shows).
pub fn normalise_fingerprint(text: &str) -> Result<String, PairingError> {
    let fp: String = text.chars().filter(|&c| c != ':').collect();
    if fp.len() != 64 || !fp.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(PairingError::BadFp);
    }
    Ok(fp.to_ascii_lowercase())
}

fn normalise_token(text: &str) -> Result<String, PairingError> {
    if text.len() != 32 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(PairingError::BadToken);
    }
    Ok(text.to_ascii_lowercase())
}

impl Pairing {
    /// Checks and normalises the four parts (the rules of
    /// [`parse_pairing_uri`]). A port of 0 is refused.
    pub fn new(host: &str, port: u16, fingerprint: &str, token: &str) -> Result<Self, PairingError> {
        if port == 0 {
            return Err(PairingError::BadPort);
        }
        Ok(Self {
            host: normalise_host(host)?,
            port,
            fingerprint: normalise_fingerprint(fingerprint)?,
            token: normalise_token(token)?,
        })
    }

    /// `host:port`, an IPv6 literal in brackets.
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Where the phone opens its capture stream.
    pub fn ingest_url(&self) -> String {
        format!("wss://{}/ingest", self.authority())
    }

    /// Where the phone uploads a recorded `.gscap`.
    pub fn captures_url(&self) -> String {
        format!("https://{}/api/captures", self.authority())
    }

    /// The canonical URI, as `GET /api/pair` answers it.
    pub fn uri(&self) -> String {
        format!("{PAIRING_PREFIX}?host={}&port={}&fp={}&token={}", self.host, self.port, self.fingerprint, self.token)
    }
}

/// The pairing URI of a server: what `GET /api/pair` answers and what
/// `GET /api/pair.svg` encodes.
///
/// `fingerprint` is the SHA-256 of the certificate's DER encoding in hex,
/// with or without colons, in any case; `token` is 32 hex digits.
pub fn pairing_uri(host: &str, port: u16, fingerprint: &str, token: &str) -> Result<String, PairingError> {
    Pairing::new(host, port, fingerprint, token).map(|p| p.uri())
}

fn percent_decode(text: &str) -> Result<String, PairingError> {
    let raw = text.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            let hex = |b: Option<&u8>| b.and_then(|&b| (b as char).to_digit(16));
            let (Some(hi), Some(lo)) = (hex(raw.get(i + 1)), hex(raw.get(i + 2))) else {
                return Err(PairingError::BadEncoding);
            };
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| PairingError::BadEncoding)
}

/// Parses `splatlive://pair?host=..&port=..&fp=..&token=..`.
///
/// 1. Leading and trailing spaces, tabs, CR and LF are ignored.
/// 2. The text is split at the first `?`; the part before it must be
///    `splatlive://pair`, letters A-Z compared without case.
/// 3. The query is split on `&`, empty items are skipped, each item is split
///    at its first `=` and both halves are percent-decoded. `+` is not a
///    space.
/// 4. `host`, `port`, `fp` and `token` may appear only once and are all
///    required (missing ones are reported in that order). Other keys are
///    ignored.
/// 5. Host: optional surrounding brackets are removed; 1 to 253 characters;
///    letters, digits, `.` and `-` (not at either end), or an IPv6 literal
///    (hex digits, `:` and `.`, at least two `:`, no zone). Lowercased.
/// 6. Port: 1 to 5 ASCII digits, value 1 to 65535.
/// 7. Fingerprint: colons removed, then exactly 64 hex digits, lowercased.
/// 8. Token: exactly 32 hex digits, lowercased.
pub fn parse_pairing_uri(text: &str) -> Result<Pairing, PairingError> {
    let text = text.trim_matches([' ', '\t', '\r', '\n']);
    let Some((prefix, query)) = text.split_once('?') else { return Err(PairingError::BadScheme) };
    if !prefix.eq_ignore_ascii_case(PAIRING_PREFIX) {
        return Err(PairingError::BadScheme);
    }
    const KEYS: [&str; 4] = ["host", "port", "fp", "token"];
    let mut params: [Option<String>; 4] = [None, None, None, None];
    for item in query.split('&').filter(|item| !item.is_empty()) {
        let (key, value) = item.split_once('=').unwrap_or((item, ""));
        let key = percent_decode(key)?;
        let value = percent_decode(value)?;
        if let Some(slot) = KEYS.iter().position(|k| *k == key) {
            if params[slot].is_some() {
                return Err(PairingError::DuplicateParameter);
            }
            params[slot] = Some(value);
        }
    }
    let [host, port, fp, token] = params;
    let host = host.ok_or(PairingError::MissingHost)?;
    let port = port.ok_or(PairingError::MissingPort)?;
    let fp = fp.ok_or(PairingError::MissingFp)?;
    let token = token.ok_or(PairingError::MissingToken)?;

    let host = normalise_host(&host)?;
    if !(1..=5).contains(&port.len()) || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(PairingError::BadPort);
    }
    let port = port.parse::<u32>().ok().and_then(|p| u16::try_from(p).ok()).filter(|&p| p != 0);
    let port = port.ok_or(PairingError::BadPort)?;
    Ok(Pairing { host, port, fingerprint: normalise_fingerprint(&fp)?, token: normalise_token(&token)? })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "3f5a9c0d7e2b41886a1f0c9d8e7b6a5f4e3d2c1b0a99887766554433221100ff";
    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn the_uri_a_server_builds_parses_back() {
        let colons: String = FP.to_uppercase().as_bytes().chunks(2).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join(":");
        let uri = pairing_uri("192.168.1.20", 7879, &colons, &TOKEN.to_uppercase()).unwrap();
        assert_eq!(uri, format!("splatlive://pair?host=192.168.1.20&port=7879&fp={FP}&token={TOKEN}"));
        let pairing = parse_pairing_uri(&uri).unwrap();
        assert_eq!(pairing, Pairing::new("192.168.1.20", 7879, FP, TOKEN).unwrap());
        assert_eq!(pairing.ingest_url(), "wss://192.168.1.20:7879/ingest");
        assert_eq!(pairing.captures_url(), "https://192.168.1.20:7879/api/captures");

        let v6 = Pairing::new("[FE80::1]", 7879, FP, TOKEN).unwrap();
        assert_eq!((v6.host.as_str(), v6.authority().as_str()), ("fe80::1", "[fe80::1]:7879"));
        assert_eq!(parse_pairing_uri(&v6.uri()).unwrap(), v6);
        let named = Pairing::new("MacBook-Pro.local", 443, FP, TOKEN).unwrap();
        assert_eq!(named.host, "macbook-pro.local");
    }

    #[test]
    fn a_server_cannot_build_a_uri_the_phone_would_refuse() {
        use PairingError::*;
        assert_eq!(pairing_uri("", 7879, FP, TOKEN), Err(BadHost));
        assert_eq!(pairing_uri("192.168.1.20:7879", 7879, FP, TOKEN), Err(BadHost));
        assert_eq!(pairing_uri("my mac", 7879, FP, TOKEN), Err(BadHost));
        assert_eq!(pairing_uri("fe80::1%en0", 7879, FP, TOKEN), Err(BadHost));
        assert_eq!(pairing_uri("10.0.0.2", 0, FP, TOKEN), Err(BadPort));
        assert_eq!(pairing_uri("10.0.0.2", 7879, &FP[1..], TOKEN), Err(BadFp));
        assert_eq!(pairing_uri("10.0.0.2", 7879, FP, "token"), Err(BadToken));
        // Multi-byte text is counted and refused without slicing inside it.
        assert_eq!(pairing_uri("é", 1, FP, TOKEN), Err(BadHost));
        assert_eq!(pairing_uri("10.0.0.2", 1, &"é".repeat(32), TOKEN), Err(BadFp));
        assert_eq!(pairing_uri("10.0.0.2", 1, FP, &"é".repeat(16)), Err(BadToken));
    }
}
