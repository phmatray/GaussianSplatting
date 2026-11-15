//! A ZIP archive laid out in advance and streamed.
//!
//! The share archive holds the web build (a few megabytes) and one
//! recording (up to gigabytes). [`ZipBuilder`] decides every byte of the
//! archive except the contents of the large file before anything is sent:
//! the result is a [`ZipPlan`], a list of byte blocks and file spans whose
//! total length is known, so the download has a `Content-Length` and the
//! recording goes from the disk to the socket without ever being held in
//! memory.
//!
//! The archive is as plain as ZIP gets, for the unpackers of every
//! platform: sizes and checksums in the local headers (no data
//! descriptors), small files deflated, the large one stored, UTF-8 names,
//! ZIP64 records only when a size or an offset needs them.

use std::path::PathBuf;

use bytes::Bytes;

/// Sizes and offsets from this value on do not fit the classic fields.
const ZIP64_LIMIT: u64 = 0xFFFF_FFFF;
/// Entry counts from this value on do not fit the classic field.
const ZIP64_ENTRIES: usize = 0xFFFF;

const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const END_SIGNATURE: u32 = 0x0605_4b50;
const END64_SIGNATURE: u32 = 0x0606_4b50;
const END64_LOCATOR_SIGNATURE: u32 = 0x0706_4b50;
/// Header id of the ZIP64 extended information field.
const ZIP64_EXTRA: u16 = 0x0001;
/// Bit 11 of the flags: the name is UTF-8.
const FLAG_UTF8: u16 = 0x0800;
const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;
/// "Made by" a Unix system, so that the external attributes are read as a
/// file mode.
const MADE_BY_UNIX: u16 = 3 << 8;
/// `-rw-r--r--`, a regular file.
const FILE_MODE: u32 = 0o100_644 << 16;
/// Deflate level of the small files: the web build is compressed once per
/// download, and a third of its size is worth 50 ms.
const DEFLATE_LEVEL: u8 = 6;

/// One stretch of the archive.
#[derive(Clone, Debug)]
pub(crate) enum Part {
    /// Bytes held in memory: headers, small files, the directory.
    Bytes(Bytes),
    /// The first `len` bytes of a file on disk.
    File { path: PathBuf, len: u64 },
}

/// A complete archive, ready to be streamed part by part.
#[derive(Clone, Debug)]
pub(crate) struct ZipPlan {
    pub parts: Vec<Part>,
    /// Size of the archive in bytes.
    pub len: u64,
}

struct Entry {
    name: String,
    method: u16,
    crc: u32,
    stored: u64,
    size: u64,
    offset: u64,
}

/// Lays out an archive entry by entry.
pub(crate) struct ZipBuilder {
    parts: Vec<Part>,
    /// Bytes pending for the next [`Part::Bytes`].
    pending: Vec<u8>,
    offset: u64,
    entries: Vec<Entry>,
    dos_time: u16,
    dos_date: u16,
    zip64_limit: u64,
}

fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// The MS-DOS date and time of a Unix time (UTC; the format has no zone).
fn dos_date_time(unix_seconds: i64) -> (u16, u16) {
    let Ok(moment) = time::OffsetDateTime::from_unix_timestamp(unix_seconds) else { return (0x21, 0) };
    let year = moment.year().clamp(1980, 2107) as u16;
    let date = ((year - 1980) << 9) | (u16::from(u8::from(moment.month())) << 5) | u16::from(moment.day());
    let time = (u16::from(moment.hour()) << 11) | (u16::from(moment.minute()) << 5) | u16::from(moment.second() / 2);
    (date, time)
}

impl ZipBuilder {
    /// An empty archive whose files are dated `unix_seconds`.
    pub fn new(unix_seconds: i64) -> Self {
        let (dos_date, dos_time) = dos_date_time(unix_seconds);
        Self {
            parts: Vec::new(),
            pending: Vec::new(),
            offset: 0,
            entries: Vec::new(),
            dos_time,
            dos_date,
            zip64_limit: ZIP64_LIMIT,
        }
    }

    /// Uses the ZIP64 records from `limit` bytes on instead of 4 GiB, so
    /// that tests reach them with small files.
    #[cfg(test)]
    fn with_zip64_limit(mut self, limit: u64) -> Self {
        self.zip64_limit = limit;
        self
    }

    fn flush_pending(&mut self) {
        if !self.pending.is_empty() {
            self.parts.push(Part::Bytes(Bytes::from(std::mem::take(&mut self.pending))));
        }
    }

    fn local_header(&mut self, name: &str, method: u16, crc: u32, stored: u64, size: u64) {
        let zip64 = stored >= self.zip64_limit || size >= self.zip64_limit;
        let before = self.pending.len();
        let out = &mut self.pending;
        put32(out, LOCAL_SIGNATURE);
        put16(out, if zip64 { 45 } else { 20 });
        put16(out, FLAG_UTF8);
        put16(out, method);
        put16(out, self.dos_time);
        put16(out, self.dos_date);
        put32(out, crc);
        put32(out, if zip64 { u32::MAX } else { stored as u32 });
        put32(out, if zip64 { u32::MAX } else { size as u32 });
        put16(out, name.len() as u16);
        put16(out, if zip64 { 20 } else { 0 });
        out.extend_from_slice(name.as_bytes());
        if zip64 {
            put16(out, ZIP64_EXTRA);
            put16(out, 16);
            put64(out, size);
            put64(out, stored);
        }
        self.entries.push(Entry { name: name.to_string(), method, crc, stored, size, offset: self.offset });
        self.offset += (self.pending.len() - before) as u64;
    }

    /// Adds a file held in memory, deflated when that makes it smaller.
    /// `name` uses `/` between directories and is at most 65535 bytes.
    pub fn add_bytes(&mut self, name: &str, data: &[u8]) {
        debug_assert!(name.len() <= usize::from(u16::MAX));
        let crc = crc32fast::hash(data);
        let deflated = miniz_oxide::deflate::compress_to_vec(data, DEFLATE_LEVEL);
        let (method, body): (u16, &[u8]) =
            if deflated.len() < data.len() { (METHOD_DEFLATE, &deflated) } else { (METHOD_STORED, data) };
        self.local_header(name, method, crc, body.len() as u64, data.len() as u64);
        self.pending.extend_from_slice(body);
        self.offset += body.len() as u64;
    }

    /// Adds the first `len` bytes of the file at `path`, stored as they
    /// are. `crc` is their CRC-32, computed by the caller.
    pub fn add_stored_file(&mut self, name: &str, path: PathBuf, len: u64, crc: u32) {
        debug_assert!(name.len() <= usize::from(u16::MAX));
        self.local_header(name, METHOD_STORED, crc, len, len);
        self.flush_pending();
        self.parts.push(Part::File { path, len });
        self.offset += len;
    }

    /// Writes the directory and returns the finished layout.
    pub fn finish(mut self) -> ZipPlan {
        let directory_offset = self.offset;
        let limit = self.zip64_limit;
        let entries = std::mem::take(&mut self.entries);
        let mut out = std::mem::take(&mut self.pending);
        let start = out.len();
        for entry in &entries {
            // Only the fields that overflow move to the ZIP64 field, in
            // this order.
            let mut extra = Vec::new();
            let mut field = |value: u64| -> u32 {
                if value >= limit {
                    put64(&mut extra, value);
                    u32::MAX
                } else {
                    value as u32
                }
            };
            let (size, stored, offset) = (field(entry.size), field(entry.stored), field(entry.offset));
            let zip64 = !extra.is_empty();
            put32(&mut out, CENTRAL_SIGNATURE);
            put16(&mut out, MADE_BY_UNIX | 45);
            put16(&mut out, if zip64 { 45 } else { 20 });
            put16(&mut out, FLAG_UTF8);
            put16(&mut out, entry.method);
            put16(&mut out, self.dos_time);
            put16(&mut out, self.dos_date);
            put32(&mut out, entry.crc);
            put32(&mut out, stored);
            put32(&mut out, size);
            put16(&mut out, entry.name.len() as u16);
            put16(&mut out, if zip64 { extra.len() as u16 + 4 } else { 0 });
            put16(&mut out, 0); // comment
            put16(&mut out, 0); // disk
            put16(&mut out, 0); // internal attributes
            put32(&mut out, FILE_MODE);
            put32(&mut out, offset);
            out.extend_from_slice(entry.name.as_bytes());
            if zip64 {
                put16(&mut out, ZIP64_EXTRA);
                put16(&mut out, extra.len() as u16);
                out.extend_from_slice(&extra);
            }
        }
        let directory_len = (out.len() - start) as u64;
        let end_offset = directory_offset + directory_len;
        let zip64 = directory_offset >= limit || directory_len >= limit || entries.len() >= ZIP64_ENTRIES;
        if zip64 {
            put32(&mut out, END64_SIGNATURE);
            put64(&mut out, 44);
            put16(&mut out, MADE_BY_UNIX | 45);
            put16(&mut out, 45);
            put32(&mut out, 0);
            put32(&mut out, 0);
            put64(&mut out, entries.len() as u64);
            put64(&mut out, entries.len() as u64);
            put64(&mut out, directory_len);
            put64(&mut out, directory_offset);
            put32(&mut out, END64_LOCATOR_SIGNATURE);
            put32(&mut out, 0);
            put64(&mut out, end_offset);
            put32(&mut out, 1);
        }
        let count = if zip64 { u16::MAX } else { entries.len() as u16 };
        put32(&mut out, END_SIGNATURE);
        put16(&mut out, 0);
        put16(&mut out, 0);
        put16(&mut out, count);
        put16(&mut out, count);
        put32(&mut out, if zip64 { u32::MAX } else { directory_len as u32 });
        put32(&mut out, if zip64 { u32::MAX } else { directory_offset as u32 });
        put16(&mut out, 0);
        let len = self.offset + (out.len() - start) as u64;
        self.pending = out;
        self.flush_pending();
        ZipPlan { parts: self.parts, len }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// The whole archive in memory, as the stream would deliver it.
    fn materialize(plan: &ZipPlan) -> Vec<u8> {
        let mut out = Vec::new();
        for part in &plan.parts {
            match part {
                Part::Bytes(bytes) => out.extend_from_slice(bytes),
                Part::File { path, len } => {
                    let mut data = Vec::new();
                    std::fs::File::open(path).unwrap().take(*len).read_to_end(&mut data).unwrap();
                    assert_eq!(data.len() as u64, *len);
                    out.extend_from_slice(&data);
                }
            }
        }
        assert_eq!(out.len() as u64, plan.len, "the announced length is the real one");
        out
    }

    fn le16(bytes: &[u8], at: usize) -> usize {
        usize::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]))
    }

    fn le32(bytes: &[u8], at: usize) -> u64 {
        u64::from(u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()))
    }

    fn le64(bytes: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
    }

    /// A reader written from the specification: every file of the archive
    /// by name, through the end record, the directory and the local
    /// headers, with the checksums verified.
    fn read_archive(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
        let end = zip.len() - 22;
        assert_eq!(le32(zip, end), u64::from(END_SIGNATURE));
        let (mut count, mut offset) = (le16(zip, end + 10) as u64, le32(zip, end + 16));
        if offset == u64::from(u32::MAX) {
            let locator = end - 20;
            assert_eq!(le32(zip, locator), u64::from(END64_LOCATOR_SIGNATURE));
            let end64 = le64(zip, locator + 8) as usize;
            assert_eq!(le32(zip, end64), u64::from(END64_SIGNATURE));
            count = le64(zip, end64 + 32);
            offset = le64(zip, end64 + 48);
        }
        let mut at = offset as usize;
        let mut files = Vec::new();
        for _ in 0..count {
            assert_eq!(le32(zip, at), u64::from(CENTRAL_SIGNATURE));
            let method = le16(zip, at + 10) as u16;
            let crc = le32(zip, at + 16) as u32;
            let (mut stored, mut size) = (le32(zip, at + 20), le32(zip, at + 24));
            let (name_len, extra_len) = (le16(zip, at + 28), le16(zip, at + 30));
            let mut local = le32(zip, at + 42);
            let name = String::from_utf8(zip[at + 46..at + 46 + name_len].to_vec()).unwrap();
            let mut extra = at + 46 + name_len;
            if extra_len > 0 {
                assert_eq!(le16(zip, extra), usize::from(ZIP64_EXTRA));
                extra += 4;
                for value in [&mut size, &mut stored, &mut local] {
                    if *value == u64::from(u32::MAX) {
                        *value = le64(zip, extra);
                        extra += 8;
                    }
                }
            }
            at += 46 + name_len + extra_len;

            let local = local as usize;
            assert_eq!(le32(zip, local), u64::from(LOCAL_SIGNATURE));
            assert_eq!(le16(zip, local + 6) as u16, FLAG_UTF8, "no data descriptor, UTF-8 names");
            assert_eq!(le32(zip, local + 14) as u32, crc, "the local header carries the checksum");
            let data = local + 30 + le16(zip, local + 26) + le16(zip, local + 28);
            let body = &zip[data..data + stored as usize];
            let content = match method {
                METHOD_STORED => body.to_vec(),
                METHOD_DEFLATE => {
                    let mut content = Vec::new();
                    flate2::read::DeflateDecoder::new(body).read_to_end(&mut content).unwrap();
                    content
                }
                other => panic!("method {other}"),
            };
            assert_eq!(content.len() as u64, size, "{name}");
            assert_eq!(crc32fast::hash(&content), crc, "{name}");
            files.push((name, content));
        }
        assert_eq!(at as u64, offset + (at as u64 - offset));
        files
    }

    fn sample(dir: &std::path::Path, builder: ZipBuilder) -> (ZipPlan, Vec<(String, Vec<u8>)>) {
        let text = "<!doctype html><title>Splat Live</title>".repeat(40).into_bytes();
        // Random bytes do not deflate: stored.
        let noise: Vec<u8> = (0u32..3000).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let big: Vec<u8> = (0u32..70_000).map(|i| (i % 251) as u8).collect();
        let path = dir.join("big.gsrec");
        std::fs::write(&path, &big).unwrap();

        let mut builder = builder;
        builder.add_bytes("index.html", &text);
        builder.add_bytes("assets/été.bin", &noise);
        builder.add_bytes("empty.txt", b"");
        builder.add_stored_file("recording.gsrec", path, big.len() as u64, crc32fast::hash(&big));
        builder.add_bytes("share.json", b"{\"file\":\"recording.gsrec\"}");
        let expected = vec![
            ("index.html".to_string(), text),
            ("assets/été.bin".to_string(), noise),
            ("empty.txt".to_string(), Vec::new()),
            ("recording.gsrec".to_string(), big),
            ("share.json".to_string(), b"{\"file\":\"recording.gsrec\"}".to_vec()),
        ];
        (builder.finish(), expected)
    }

    /// `python3 -m zipfile`-style check with the standard library of
    /// Python, when there is one: names, contents and `testzip`.
    fn python_agrees(zip: &std::path::Path, expected: &[(String, Vec<u8>)]) {
        let script = "import sys, zipfile, hashlib\n\
                      z = zipfile.ZipFile(sys.argv[1])\n\
                      assert z.testzip() is None\n\
                      for i in z.infolist():\n\
                      \x20   print(i.filename, len(z.read(i)), hashlib.sha256(z.read(i)).hexdigest()[:16], i.compress_type)\n";
        let output = match std::process::Command::new("python3").args(["-I", "-c", script]).arg(zip).output() {
            Ok(output) => output,
            Err(_) => {
                eprintln!("python3 not found: the cross-check of the archive is skipped");
                return;
            }
        };
        assert!(output.status.success(), "python: {}", String::from_utf8_lossy(&output.stderr));
        let listing = String::from_utf8(output.stdout).unwrap();
        let lines: Vec<&str> = listing.lines().collect();
        assert_eq!(lines.len(), expected.len(), "{listing}");
        for (line, (name, content)) in lines.iter().zip(expected) {
            let digest = ring::digest::digest(&ring::digest::SHA256, content);
            let hex: String = digest.as_ref()[..8].iter().map(|b| format!("{b:02x}")).collect();
            assert!(line.starts_with(&format!("{name} {} {hex} ", content.len())), "{line}");
        }
    }

    #[test]
    fn a_plain_archive_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let (plan, expected) = sample(dir.path(), ZipBuilder::new(1_794_650_000));
        let zip = materialize(&plan);
        assert_eq!(read_archive(&zip), expected);
        // The text was deflated, the file on disk is a span of its own.
        assert!(plan.parts.iter().any(|part| matches!(part, Part::File { len: 70_000, .. })));
        assert!((zip.len() as u64) < 70_000 + 3000 + 1600 + 700);
        // Not a single ZIP64 record in an archive that does not need one.
        let needle = END64_SIGNATURE.to_le_bytes();
        assert!(!zip.windows(4).any(|window| window == needle));
        let path = dir.path().join("plain.zip");
        std::fs::write(&path, &zip).unwrap();
        python_agrees(&path, &expected);
    }

    #[test]
    fn large_entries_and_offsets_use_zip64() {
        let dir = tempfile::tempdir().unwrap();
        // With the limit at 2000 bytes the stored file (70 kB), the entry
        // after it and the directory all need the 64-bit records.
        let (plan, expected) = sample(dir.path(), ZipBuilder::new(1_794_650_000).with_zip64_limit(2000));
        let zip = materialize(&plan);
        assert_eq!(read_archive(&zip), expected);
        let needle = END64_LOCATOR_SIGNATURE.to_le_bytes();
        assert!(zip.windows(4).any(|window| window == needle));
        let path = dir.path().join("zip64.zip");
        std::fs::write(&path, &zip).unwrap();
        python_agrees(&path, &expected);
    }

    #[test]
    fn dates_are_dos_dates() {
        // 2026-11-14 10:32:07 UTC.
        let (date, time) = dos_date_time(1_794_652_327);
        assert_eq!((date >> 9) + 1980, 2026);
        assert_eq!(((date >> 5) & 15, date & 31), (11, 14));
        assert_eq!((time >> 11, (time >> 5) & 63, (time & 31) * 2), (10, 32, 6));
        // Before the format's epoch: its first day.
        assert_eq!(dos_date_time(0).0 >> 9, 0);
    }
}
