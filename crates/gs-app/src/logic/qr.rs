//! QR codes of the links the interface hands out, as inline SVG.

use qrcode::render::svg;
use qrcode::{EcLevel, QrCode};

/// Longest text encoded (a share link is a few hundred bytes at most).
const MAX_TEXT: usize = 1024;

/// The QR code of `text` as an SVG document: dark modules on white, with
/// the quiet zone the standard asks for. `None` when the text is empty or
/// too long.
pub fn svg(text: &str) -> Option<String> {
    if text.is_empty() || text.len() > MAX_TEXT {
        return None;
    }
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M).ok()?;
    Some(code.render::<svg::Color>().min_dimensions(240, 240).quiet_zone(true).dark_color(svg::Color("#000000")).light_color(svg::Color("#ffffff")).build())
}

/// Percent-encodes a document for a `data:` URL (the conservative set:
/// everything but unreserved characters).
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// An SVG document as a `data:` URL for an `<img>`.
pub fn svg_data_url(svg: &str) -> String {
    format!("data:image/svg+xml;charset=utf-8,{}", encode(svg))
}

/// The QR code of `text` as a `data:` URL.
pub fn data_url(text: &str) -> Option<String> {
    svg(text).map(|svg| svg_data_url(&svg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_share_link_becomes_a_square_code() {
        let svg = svg("https://192.168.1.20:7879/?file=recordings/danse.gsrec&embed=1&autoplay=1&loop=1").unwrap();
        assert!(svg.contains("<svg") && svg.contains("#000000") && svg.contains("#ffffff"));
        let url = data_url("https://192.168.1.20:7879/").unwrap();
        assert!(url.starts_with("data:image/svg+xml;charset=utf-8,%3C"));
        assert!(!url.contains('<') && !url.contains('"') && !url.contains(' '));
    }

    #[test]
    fn empty_and_oversized_texts_have_no_code() {
        assert!(svg("").is_none());
        assert!(svg(&"a".repeat(MAX_TEXT + 1)).is_none());
        assert!(svg(&"a".repeat(MAX_TEXT)).is_some());
    }
}
