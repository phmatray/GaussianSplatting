//! Large files over HTTP: recordings and raw captures, with `Range`.
//!
//! A recording can be gigabytes long and the player seeks in it
//! (`gs_core::record::Player::ranged`), so the file is never read into
//! memory: the answer is a stream of blocks read from the disk, for the
//! whole file or for the one range asked for.
//!
//! What is implemented of RFC 9110: `GET` and `HEAD`; a single byte range
//! (`bytes=a-b`, `bytes=a-`, `bytes=-n`) answered with 206, or 416 when it
//! lies beyond the end; a request for several ranges, another unit or a
//! malformed header is answered with the whole file, as the RFC allows;
//! `If-Range` and `If-None-Match` against a strong `ETag` made of the
//! file's length and modification time (a file that is still being
//! recorded changes both with every write).

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use axum::body::Body;
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::http::{Method, Response, StatusCode};
use bytes::Bytes;
use futures_util::Stream;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Size of the blocks read from the disk.
const BLOCK: usize = 64 << 10;

/// What a `Range` header asks of a file of a given length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ranged {
    /// No usable `Range`: the whole file.
    Whole,
    /// Bytes `start..=end`, both inside the file.
    Part { start: u64, end: u64 },
    /// A valid range that does not touch the file.
    Unsatisfiable,
}

/// Interprets a `Range` header value for a file of `len` bytes.
pub(crate) fn parse_range(value: &str, len: u64) -> Ranged {
    let Some(spec) = value.trim().strip_prefix("bytes=") else { return Ranged::Whole };
    let spec = spec.trim();
    // Several ranges would need a multipart answer; the whole file is a
    // correct answer too.
    if spec.contains(',') {
        return Ranged::Whole;
    }
    let Some((first, last)) = spec.split_once('-') else { return Ranged::Whole };
    let number = |text: &str| -> Option<u64> {
        // `u64::from_str` accepts a sign; a range does not.
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())).then(|| text.parse().ok()).flatten()
    };
    match (first.trim(), last.trim()) {
        // The last `n` bytes.
        ("", suffix) => match number(suffix) {
            Some(0) => Ranged::Unsatisfiable,
            Some(_) if len == 0 => Ranged::Unsatisfiable,
            Some(n) => Ranged::Part { start: len.saturating_sub(n), end: len - 1 },
            None => Ranged::Whole,
        },
        (first, last) => {
            let Some(start) = number(first) else { return Ranged::Whole };
            let end = match last {
                "" => u64::MAX,
                last => match number(last) {
                    Some(end) => end,
                    None => return Ranged::Whole,
                },
            };
            if end < start {
                return Ranged::Whole;
            }
            if start >= len {
                return Ranged::Unsatisfiable;
            }
            Ranged::Part { start, end: end.min(len - 1) }
        }
    }
}

/// Whether a list of entity tags (`If-None-Match`, `If-Range`) names
/// `etag`. `weak` allows the weak comparison of `If-None-Match`.
fn etag_listed(value: &HeaderValue, etag: &str, weak: bool) -> bool {
    let Ok(value) = value.to_str() else { return false };
    value.split(',').map(str::trim).any(|candidate| {
        candidate == "*" && weak || candidate == etag || (weak && candidate.strip_prefix("W/") == Some(etag))
    })
}

/// An HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`) for a Unix time.
fn http_date(unix_seconds: u64) -> Option<HeaderValue> {
    let moment = time::OffsetDateTime::from_unix_timestamp(i64::try_from(unix_seconds).ok()?).ok()?;
    let text = format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        &moment.weekday().to_string()[..3],
        moment.day(),
        &moment.month().to_string()[..3],
        moment.year(),
        moment.hour(),
        moment.minute(),
        moment.second()
    );
    HeaderValue::from_str(&text).ok()
}

/// `len` bytes of `file` from its current position, in blocks. A file that
/// ends early (it was replaced meanwhile) ends the stream with an error,
/// so the client sees a broken download and not a short one that looks
/// complete.
pub(crate) fn file_stream(file: tokio::fs::File, len: u64) -> impl Stream<Item = io::Result<Bytes>> + Send {
    futures_util::stream::try_unfold((file, len), |(mut file, remaining)| async move {
        if remaining == 0 {
            return Ok(None);
        }
        let want = remaining.min(BLOCK as u64) as usize;
        let mut block = vec![0u8; want];
        let mut filled = 0;
        while filled < want {
            match file.read(&mut block[filled..]).await? {
                0 => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the file became shorter")),
                read => filled += read,
            }
        }
        Ok(Some((Bytes::from(block), (file, remaining - want as u64))))
    })
}

/// A download body that gives up on a client which has stopped reading.
///
/// The server only reads `stream` as fast as the client takes it, so a
/// client that asks for a large file and then reads nothing would hold its
/// connection and an open file for as long as it likes, over HTTP/1 as
/// over HTTP/2. Here a task of its own feeds the body; when one block
/// waits longer than `stall` to be taken, the task ends and the body with
/// it, as an error: the client sees a broken download, the connection (or
/// the HTTP/2 stream) and the file are let go.
pub(crate) fn guarded<S>(stream: S, stall: Duration) -> Body
where
    S: Stream<Item = io::Result<Bytes>> + Send + 'static,
{
    use futures_util::StreamExt;
    let (tx, rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(1);
    let complete = Arc::new(AtomicBool::new(false));
    let fed = complete.clone();
    tokio::spawn(async move {
        let mut stream = std::pin::pin!(stream);
        while let Some(item) = stream.next().await {
            let last = item.is_err();
            match tokio::time::timeout(stall, tx.send(item)).await {
                Ok(Ok(())) if !last => {}
                // An error went through, or nobody is listening any more.
                Ok(_) => return,
                Err(_) => {
                    log::debug!("download abandoned: the client took nothing for {stall:?}");
                    return;
                }
            }
        }
        fed.store(true, Ordering::Release);
    });
    Body::from_stream(futures_util::stream::unfold((rx, complete, false), |(mut rx, complete, ended)| async move {
        if ended {
            return None;
        }
        match rx.recv().await {
            Some(item) => Some((item, (rx, complete, false))),
            None if complete.load(Ordering::Acquire) => None,
            // The feeding task gave up: not the end of the file.
            None => {
                let error = io::Error::new(io::ErrorKind::TimedOut, "the client stopped reading");
                Some((Err(error), (rx, complete, true)))
            }
        }
    }))
}

fn status_only(status: StatusCode, extra: &[(header::HeaderName, HeaderValue)]) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    for (name, value) in extra {
        response.headers_mut().insert(name.clone(), value.clone());
    }
    response
}

/// `attachment; filename=…` for a file name that passed
/// `store::is_safe_name` (no quotes, no control characters): an ASCII
/// fallback plus the exact UTF-8 name.
pub(crate) fn attachment_header(name: &str) -> Option<HeaderValue> {
    let ascii: String = name.chars().map(|c| if c.is_ascii() && c != '"' && c != '\\' { c } else { '_' }).collect();
    let encoded = percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC);
    HeaderValue::from_str(&format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")).ok()
}

/// Answers a `GET` or `HEAD` for the file at `path`. `download` is the
/// name to save it under when the client asked for a download; `stall` is
/// how long the client may leave a block of the file unread ([`guarded`]).
pub(crate) async fn serve(
    path: &Path,
    method: &Method,
    headers: &HeaderMap,
    download: Option<&str>,
    stall: Duration,
) -> Response<Body> {
    let not_found = || status_only(StatusCode::NOT_FOUND, &[]);
    let Ok(mut file) = tokio::fs::File::open(path).await else { return not_found() };
    let Ok(meta) = file.metadata().await else { return not_found() };
    if !meta.is_file() {
        return not_found();
    }
    let len = meta.len();
    let modified = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok());
    let etag = format!("\"{:x}-{:x}\"", len, modified.map_or(0, |d| d.as_nanos()));

    let mut common: Vec<(header::HeaderName, HeaderValue)> = vec![
        (header::ACCEPT_RANGES, HeaderValue::from_static("bytes")),
        // Always revalidated: a recording of that name may be replaced.
        (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        (header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
    ];
    if let Ok(value) = HeaderValue::from_str(&etag) {
        common.push((header::ETAG, value));
    }
    if let Some(value) = modified.and_then(|d| http_date(d.as_secs())) {
        common.push((header::LAST_MODIFIED, value));
    }

    if headers.get(header::IF_NONE_MATCH).is_some_and(|value| etag_listed(value, &etag, true)) {
        let mut response = status_only(StatusCode::NOT_MODIFIED, &common);
        response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        return response;
    }

    // A range of another version of the file is of no use to the client:
    // it gets the current file whole.
    let range_applies = headers.get(header::IF_RANGE).is_none_or(|value| etag_listed(value, &etag, false));
    let mut ranges = headers.get_all(header::RANGE).iter();
    let range = match (ranges.next(), ranges.next()) {
        (Some(value), None) if range_applies => value.to_str().map_or(Ranged::Whole, |value| parse_range(value, len)),
        _ => Ranged::Whole,
    };

    let (status, start, count) = match range {
        Ranged::Whole => (StatusCode::OK, 0, len),
        Ranged::Part { start, end } => (StatusCode::PARTIAL_CONTENT, start, end - start + 1),
        Ranged::Unsatisfiable => {
            let mut extra = common.clone();
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{len}")) {
                extra.push((header::CONTENT_RANGE, value));
            }
            let mut response = status_only(StatusCode::RANGE_NOT_SATISFIABLE, &extra);
            response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            return response;
        }
    };

    let body = if method == Method::HEAD || count == 0 {
        Body::empty()
    } else {
        if start > 0 && file.seek(io::SeekFrom::Start(start)).await.is_err() {
            return not_found();
        }
        guarded(file_stream(file, count), stall)
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let out = response.headers_mut();
    for (name, value) in common {
        out.insert(name, value);
    }
    out.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    out.insert(header::CONTENT_LENGTH, HeaderValue::from(count));
    if status == StatusCode::PARTIAL_CONTENT {
        if let Ok(value) = HeaderValue::from_str(&format!("bytes {start}-{}/{len}", start + count - 1)) {
            out.insert(header::CONTENT_RANGE, value);
        }
    }
    if let Some(value) = download.and_then(attachment_header) {
        out.insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_ranges() {
        use Ranged::*;
        let cases = [
            ("bytes=0-0", Part { start: 0, end: 0 }),
            ("bytes=0-99", Part { start: 0, end: 99 }),
            ("bytes=10-19", Part { start: 10, end: 19 }),
            ("bytes=90-", Part { start: 90, end: 99 }),
            ("bytes=90-5000", Part { start: 90, end: 99 }),
            ("bytes=99-99", Part { start: 99, end: 99 }),
            ("bytes=-1", Part { start: 99, end: 99 }),
            ("bytes=-30", Part { start: 70, end: 99 }),
            ("bytes=-5000", Part { start: 0, end: 99 }),
            (" bytes= 5 - 6 ", Part { start: 5, end: 6 }),
            ("bytes=18446744073709551615-", Unsatisfiable),
            ("bytes=100-", Unsatisfiable),
            ("bytes=100-200", Unsatisfiable),
            ("bytes=-0", Unsatisfiable),
            // Not a byte range this server answers: the whole file.
            ("bytes=0-1,5-6", Whole),
            ("bytes=5-2", Whole),
            ("bytes=", Whole),
            ("bytes=-", Whole),
            ("bytes=a-b", Whole),
            ("bytes=+1-2", Whole),
            ("bytes=1-+2", Whole),
            ("bytes=--5", Whole),
            ("bytes=0x10-", Whole),
            ("bytes=99999999999999999999999-", Whole),
            ("items=0-5", Whole),
            ("0-5", Whole),
            ("", Whole),
        ];
        for (header, expected) in cases {
            assert_eq!(parse_range(header, 100), expected, "{header:?}");
        }
        // Nothing can be asked of an empty file but the file itself.
        assert_eq!(parse_range("bytes=0-", 0), Unsatisfiable);
        assert_eq!(parse_range("bytes=-5", 0), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-0", 1), Part { start: 0, end: 0 });
        assert_eq!(parse_range("bytes=0-", u64::MAX), Part { start: 0, end: u64::MAX - 1 });
    }

    #[test]
    fn entity_tags() {
        let value = |text: &'static str| HeaderValue::from_static(text);
        assert!(etag_listed(&value("\"a-1\""), "\"a-1\"", false));
        assert!(etag_listed(&value("\"x\", \"a-1\""), "\"a-1\"", true));
        assert!(etag_listed(&value("W/\"a-1\""), "\"a-1\"", true));
        assert!(etag_listed(&value("*"), "\"a-1\"", true));
        // `If-Range` needs the strong match, and a date never matches.
        assert!(!etag_listed(&value("W/\"a-1\""), "\"a-1\"", false));
        assert!(!etag_listed(&value("*"), "\"a-1\"", false));
        assert!(!etag_listed(&value("Sat, 14 Nov 2026 10:00:00 GMT"), "\"a-1\"", false));
        assert!(!etag_listed(&value("\"a-2\""), "\"a-1\"", true));
    }

    #[test]
    fn dates_and_names() {
        assert_eq!(http_date(784_111_777).unwrap(), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(http_date(0).unwrap(), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(
            attachment_header("été 1.gsrec").unwrap(),
            "attachment; filename=\"_t_ 1.gsrec\"; filename*=UTF-8''%C3%A9t%C3%A9%201%2Egsrec"
        );
    }
}
