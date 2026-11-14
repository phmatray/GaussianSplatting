//! Replaying a recording against a clock.

use std::collections::HashMap;

use miniz_oxide::inflate::core::{decompress, inflate_flags, DecompressorOxide};

use super::reader::Problem;
use super::wire::{classify, peek_class, MessageClass, CLASS_PREFIX, LAYER_COUNT};
use super::{
    fingerprint, parse_file_header, parse_index, payload_of, peek_record, split_keyframe, u64_at, HeaderParse, Peek,
    RecordError, RecordKind, RecordingInfo, END_MAGIC, MAX_RECORD_LEN, RECORD_HEADER_SIZE, TRAILER_SIZE,
};

/// Bytes `[start, end)` of the recording file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// The value of an HTTP `Range` header that asks for these bytes.
    pub fn http_header(&self) -> String {
        format!("bytes={}-{}", self.start, self.end.saturating_sub(1))
    }
}

/// How a [`Player`] that reads through `Range` requests uses the network
/// and its memory. Players that hold the whole file ignore all but
/// `jump_s`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayerOptions {
    /// Bytes of the file kept in memory. What lies farthest from the play
    /// head is dropped first and fetched again when a seek needs it. The
    /// stretch from the keyframe before the play head to the play head is
    /// always kept, whatever its size (a scan, which has one keyframe, is
    /// kept whole up to the play head), and one block more may be in
    /// flight.
    pub max_cache_bytes: u64,
    /// Size of one request.
    pub fetch_block: u64,
    /// While playing, the player asks for data until this many seconds
    /// (of play time at the current speed) lie ahead of the play head.
    pub prefetch_s: f64,
    /// A step forward by more than this many seconds of recording goes
    /// through a keyframe, if there is one on the way, instead of through
    /// every record.
    pub jump_s: f64,
}

impl Default for PlayerOptions {
    /// 96 MiB of cache (15 s of the default stage), requests of 2 MiB,
    /// 3 s read ahead, jumps beyond 2 s.
    fn default() -> Self {
        Self { max_cache_bytes: 96 << 20, fetch_block: 2 << 20, prefetch_s: 3.0, jump_s: 2.0 }
    }
}

/// What a [`Player`] wants a viewer to do.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Update {
    /// Wire messages to apply to the viewer, in order (`Viewer::apply_message`
    /// in `gs-render`). Together they are something the live stream could
    /// have delivered in one batch.
    pub messages: Vec<Vec<u8>>,
    /// The messages start from a keyframe: the play head jumped. The
    /// layers need no clearing (the messages replace what needs
    /// replacing), but whoever shows statistics and thumbnails should
    /// forget the ones on screen first: they belong to another moment, and
    /// the messages bring those of the new one, if it has any.
    pub reset: bool,
    /// The player waits for bytes that have not arrived (a download in
    /// progress, a `Range` request): the picture is older than the play
    /// head, and a playing head does not move.
    pub buffering: bool,
    /// The play head reached the end and stopped (not looping). Reported
    /// once.
    pub ended: bool,
    /// The play head after this update, seconds.
    pub time: f64,
}

/// Counters of a [`Player`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlayerStats {
    /// Bytes of the file held in memory.
    pub cached_bytes: u64,
    /// Records known (their positions, 32 bytes each).
    pub records: u64,
    /// Keyframes known.
    pub keyframes: u64,
    /// Times playback restarted from a keyframe.
    pub resyncs: u64,
    /// Messages handed to the viewer, and their bytes.
    pub messages: u64,
    pub message_bytes: u64,
    /// Recorded messages left out because they did not inflate or were not
    /// valid wire messages.
    pub dropped: u64,
    /// The index at the end of the file was missing or wrong (a `Range`
    /// player then reads forward from the start).
    pub index_missing: bool,
}

#[derive(Clone, Copy, Debug)]
struct Rec {
    /// Position of the record (its length field) in the span's bytes.
    pos: usize,
    payload_len: u32,
    kind: RecordKind,
    deflated: bool,
    time: f64,
}

impl Rec {
    fn total(&self) -> usize {
        RECORD_HEADER_SIZE + self.payload_len as usize
    }
}

/// A contiguous stretch of the file that starts at a record (or, for a
/// span at offset 0, with the file header) and has been parsed up to
/// `scanned`.
struct Span {
    /// File offset of `bytes[0]`.
    base: u64,
    bytes: Vec<u8>,
    recs: Vec<Rec>,
    /// Position in `bytes` behind the last parsed record.
    scanned: usize,
    /// Nothing behind `scanned` will ever parse: the index, the end of the
    /// file or damage.
    closed: bool,
}

impl Span {
    fn new(base: u64, bytes: Vec<u8>, scanned: usize) -> Self {
        Self { base, bytes, recs: Vec::new(), scanned, closed: false }
    }

    fn end(&self) -> u64 {
        self.base + self.bytes.len() as u64
    }

    fn scanned_end(&self) -> u64 {
        self.base + self.scanned as u64
    }

    /// The offset lies in the span or right behind it.
    fn reaches(&self, offset: u64) -> bool {
        offset >= self.base && offset <= self.end()
    }

    fn stored(&self, rec: &Rec) -> &[u8] {
        &self.bytes[rec.pos + RECORD_HEADER_SIZE..rec.pos + rec.total()]
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Key {
    time: f64,
    offset: u64,
}

/// The viewer holds the state after every record before `offset`.
#[derive(Clone, Copy, Debug)]
struct Cursor {
    offset: u64,
    /// Time of the last record applied.
    time: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Boot {
    /// The file header is wanted; it needs this many bytes.
    Head(u64),
    /// The end of the file is wanted, to find the index.
    Tail,
    /// The index record at this offset is wanted.
    Index(u64),
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Source {
    /// The whole file is in memory.
    Whole,
    /// The file arrives front to back.
    Stream { finished: bool },
    /// The file is fetched in pieces.
    Ranged { total: u64, boot: Boot },
}

enum Loaded {
    /// The messages of the keyframe and the offset of the record after it.
    Messages(Vec<Vec<u8>>, u64),
    /// Its bytes are not there yet.
    Missing,
    /// There is no usable keyframe at that offset.
    Unusable,
}

enum Body {
    Owned(Vec<u8>),
    /// Record `recs[.1]` of span `.0`.
    Stored(usize, usize),
}

struct Item {
    class: MessageClass,
    body: Body,
}

/// The messages of one update, coalesced by the delivery contract of
/// `docs/ARCHITECTURE.md` exactly as the relay of `gs-worker` coalesces a
/// backlog: `State` and layer appends stay, in order; a layer `Replace`
/// drops the earlier messages of its layer; of `Stats` and of each
/// camera's `Preview` only the newest stays.
#[derive(Default)]
struct Coalescer {
    items: Vec<Option<Item>>,
    latest: HashMap<MessageClass, usize>,
    layers: [Vec<usize>; LAYER_COUNT],
}

impl Coalescer {
    fn push(&mut self, class: MessageClass, body: Body) {
        let index = self.items.len();
        match class {
            MessageClass::State => {}
            MessageClass::Layer { layer, replace } => {
                let Some(members) = self.layers.get_mut(layer as usize) else { return };
                if replace {
                    for old in members.drain(..) {
                        self.items[old] = None;
                    }
                }
                members.push(index);
            }
            MessageClass::Stats | MessageClass::Preview(_) => {
                if let Some(old) = self.latest.insert(class, index) {
                    self.items[old] = None;
                }
            }
        }
        self.items.push(Some(Item { class, body }));
    }
}

/// The class of a stored message payload, inflating only its first bytes.
fn peek_stored(inflater: &mut DecompressorOxide, stored: &[u8], deflated: bool) -> Option<MessageClass> {
    if !deflated {
        return peek_class(&stored[..stored.len().min(CLASS_PREFIX)]);
    }
    let mut prefix = [0u8; 32];
    inflater.init();
    // Stops as soon as the small buffer is full; a stream that is damaged
    // further on is found out when the whole message is inflated.
    let (_, _, written) =
        decompress(inflater, stored, &mut prefix, 0, inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF);
    peek_class(&prefix[..written.min(CLASS_PREFIX)])
}

/// Replays a `.gsrec` recording: owns the recording (or the parts of it
/// that have arrived) and a play head, and tells a viewer which messages
/// take it from what it shows to what the play head points at.
///
/// ```
/// # use gs_core::record::{Player, Recorder, RecorderOptions};
/// # use gs_core::codec::{encode, Body, Message};
/// # let mut recorder = Recorder::new(Vec::new(), RecorderOptions::named("demo"));
/// # let state = encode(&Message { seq: 0, time_ms: 0, body: Body::State("{}".into()) });
/// # recorder.push(0.0, &state).unwrap();
/// # let (file, _) = recorder.finish().unwrap();
/// let mut player = Player::from_bytes(file)?;
/// player.set_looping(true);
/// player.play();
/// // Every animation frame, with the seconds since the previous one:
/// let update = player.advance(1.0 / 60.0);
/// for message in &update.messages {
///     // viewer.apply_message(message)
/// #   let _ = message;
/// }
/// // A click on the time line:
/// player.seek(12.5);
/// let update = player.update();
/// # let _ = update;
/// # Ok::<(), gs_core::record::RecordError>(())
/// ```
///
/// Forward play hands over the recorded messages between the old and the
/// new time (keyframes are skipped: they repeat what the viewer has). A
/// step backwards, or far forwards, restarts from the last keyframe before
/// the wanted time. Either way the messages of one update are coalesced
/// like a backlog of the live stream, so the viewer never gets anything
/// the live stream could not have sent, and a keyframe whose `State` or
/// layer 0 is byte for byte what the viewer already holds is not sent
/// again (in a stage recording a seek costs the live layer only).
///
/// The player assumes that nothing else writes to the viewer. After
/// showing something else, call [`Player::invalidate_viewer`].
pub struct Player {
    options: PlayerOptions,
    source: Source,
    info: Option<RecordingInfo>,
    /// Offset of the first record.
    first_record: u64,
    /// Sorted by `base`, never overlapping.
    spans: Vec<Span>,
    /// Sorted by offset (and so by time).
    keys: Vec<Key>,
    /// Where the records stop, once known: the index, damage, or the end
    /// of a file cut short.
    records_end: Option<u64>,
    /// Time of the last record, once known.
    end_time: Option<f64>,
    damage: Option<Problem>,
    error: Option<RecordError>,

    head: f64,
    playing: bool,
    speed: f64,
    looping: bool,
    cursor: Option<Cursor>,
    starved: bool,
    /// Fingerprints of the `State` and of the layer 0 `Replace` the viewer
    /// holds, the latter only while nothing was appended to it.
    viewer_state: Option<u64>,
    viewer_static: Option<u64>,
    stats: PlayerStats,
    inflater: Box<DecompressorOxide>,
}

/// First request of a `Range` player: the file header of this module's
/// writer is 1024 bytes.
const HEAD_FETCH: u64 = 4096;
/// Second request: the end of the file, a 64th of it within these bounds
/// (64 KiB hold the index of about 4000 keyframes).
const TAIL_FETCH_MIN: u64 = 4 << 10;
const TAIL_FETCH_MAX: u64 = 64 << 10;

impl Player {
    fn new(source: Source) -> Self {
        Self {
            options: PlayerOptions::default(),
            source,
            info: None,
            first_record: 0,
            spans: Vec::new(),
            keys: Vec::new(),
            records_end: None,
            end_time: None,
            damage: None,
            error: None,
            head: 0.0,
            playing: false,
            speed: 1.0,
            looping: false,
            cursor: None,
            starved: false,
            viewer_state: None,
            viewer_static: None,
            stats: PlayerStats::default(),
            inflater: Box::default(),
        }
    }

    // ------------------------------------------------------------ sources

    /// A player of a recording that is entirely in memory: a file that was
    /// read or downloaded as a whole, finished or cut short.
    ///
    /// `Err` when nothing in it can be shown: a bad file header, or no
    /// keyframe (`empty`, or the damage that hides them).
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, RecordError> {
        let mut player = Self::new(Source::Whole);
        let (info, first_record) = match parse_file_header(&bytes)? {
            HeaderParse::Done { info, first_record } => (info, first_record),
            HeaderParse::NeedMore(_) => return Err(RecordError::TooShort),
        };
        player.info = Some(info);
        player.first_record = first_record as u64;
        player.spans.push(Span::new(0, bytes, first_record));
        player.scan(0);
        player.close_at_end(0);
        match player.error {
            Some(error) => Err(error),
            None => Ok(player),
        }
    }

    /// A player of a recording that arrives front to back, for example the
    /// body of an HTTP response: hand it every chunk with
    /// [`Player::push_bytes`] and call [`Player::finish_stream`] when the
    /// body ends. Playback can start as soon as the first keyframe is
    /// there and waits ([`Update::buffering`]) whenever the play head
    /// catches up with the download. The whole file ends up in memory.
    pub fn streaming() -> Self {
        let mut player = Self::new(Source::Stream { finished: false });
        player.spans.push(Span::new(0, Vec::new(), 0));
        player
    }

    /// Appends the next bytes of a [`Player::streaming`] recording.
    ///
    /// `Err` when the file header is bad; the player is useless then.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Result<(), RecordError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if self.source != (Source::Stream { finished: false }) {
            return Ok(());
        }
        self.spans[0].bytes.extend_from_slice(chunk);
        if self.info.is_none() {
            match parse_file_header(&self.spans[0].bytes) {
                Ok(HeaderParse::NeedMore(_)) => return Ok(()),
                Ok(HeaderParse::Done { info, first_record }) => {
                    self.info = Some(info);
                    self.first_record = first_record as u64;
                    self.spans[0].scanned = first_record;
                }
                Err(error) => {
                    self.error = Some(error);
                    return Err(error);
                }
            }
        }
        self.scan(0);
        Ok(())
    }

    /// Tells a [`Player::streaming`] player that no more bytes will come.
    /// What has arrived plays to its last complete record.
    pub fn finish_stream(&mut self) {
        if self.source != (Source::Stream { finished: false }) {
            return;
        }
        self.source = Source::Stream { finished: true };
        if self.info.is_none() {
            self.error.get_or_insert(RecordError::TooShort);
            return;
        }
        self.close_at_end(0);
    }

    /// A player of a recording of `total_len` bytes that is fetched piece
    /// by piece, for example with HTTP `Range` requests, so that a long
    /// recording can be opened and searched without downloading all of it.
    ///
    /// The player says which bytes it needs next with [`Player::wanted`];
    /// the host fetches them and hands them to [`Player::supply`]:
    ///
    /// ```
    /// # use gs_core::record::Player;
    /// # fn fetch(_: u64, _: u64) -> Vec<u8> { Vec::new() }
    /// # let total_len = 0;
    /// let mut player = Player::ranged(total_len);
    /// // Whenever no request is in flight:
    /// if let Some(range) = player.wanted() {
    ///     // `Range: bytes=<start>-<end - 1>`, then, when the body is there:
    ///     let body = fetch(range.start, range.end);
    ///     player.supply(range.start, &body)?;
    /// }
    /// # Ok::<(), gs_core::record::RecordError>(())
    /// ```
    ///
    /// It first asks for the start of the file (the header) and for its
    /// end (the index), then for the keyframe before the play head and
    /// what follows it, a block at a time, a few seconds ahead of the play
    /// head. A file without index (cut short) is read forward from its
    /// start; keyframes passed on the way can be sought to later.
    pub fn ranged(total_len: u64) -> Self {
        let mut player = Self::new(Source::Ranged { total: total_len, boot: Boot::Head(HEAD_FETCH) });
        if total_len < 12 {
            player.error = Some(RecordError::TooShort);
        }
        player
    }

    /// Replaces the options (see [`PlayerOptions`]).
    pub fn set_options(&mut self, options: PlayerOptions) {
        self.options = options;
    }

    /// The bytes a [`Player::ranged`] player needs next, if any. The
    /// answer stays the same until [`Player::supply`] delivers them or the
    /// play head moves elsewhere; ask again after every
    /// [`Player::supply`], [`Player::seek`] and [`Player::advance`].
    pub fn wanted(&self) -> Option<ByteRange> {
        let Source::Ranged { total, boot } = self.source else { return None };
        if self.error.is_some() {
            return None;
        }
        let range = match boot {
            Boot::Head(needed) => ByteRange { start: 0, end: needed.min(total) },
            Boot::Tail => {
                // The index of all but very long recordings, without
                // fetching a small file whole for it.
                let tail = (total / 64).clamp(TAIL_FETCH_MIN, TAIL_FETCH_MAX);
                ByteRange { start: total.saturating_sub(tail).max(self.first_record), end: total }
            }
            Boot::Index(offset) => ByteRange { start: offset, end: total },
            Boot::Ready => self.wanted_records(total)?,
        };
        (!range.is_empty()).then_some(range)
    }

    /// Hands over bytes of the file that start at offset `start`: the
    /// answer to [`Player::wanted`], whole or in part (a short read is
    /// asked for again from where it ended).
    ///
    /// `Ok(false)` when the player has no use for them: the answer to a
    /// request that a seek has made pointless. `Err` when the file header
    /// is bad; the player is useless then.
    pub fn supply(&mut self, start: u64, bytes: &[u8]) -> Result<bool, RecordError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let Source::Ranged { total, boot } = self.source else { return Ok(false) };
        let set_boot = |player: &mut Self, boot| player.source = Source::Ranged { total, boot };
        match boot {
            Boot::Head(_) => {
                if start != 0 {
                    return Ok(false);
                }
                match parse_file_header(bytes) {
                    Ok(HeaderParse::Done { info, first_record }) => {
                        self.info = Some(info);
                        self.first_record = first_record as u64;
                        // Too small to hold an index record and a trailer.
                        let tiny = total < self.first_record + (RECORD_HEADER_SIZE + TRAILER_SIZE) as u64;
                        self.stats.index_missing = tiny;
                        set_boot(self, if tiny { Boot::Ready } else { Boot::Tail });
                    }
                    Ok(HeaderParse::NeedMore(needed)) if (needed as u64) <= total => {
                        set_boot(self, Boot::Head(needed as u64))
                    }
                    Ok(HeaderParse::NeedMore(_)) => self.error = Some(RecordError::TooShort),
                    Err(error) => self.error = Some(error),
                }
                self.error.map_or(Ok(true), Err)
            }
            Boot::Tail | Boot::Index(_) => {
                let expected = self.wanted().map_or(u64::MAX, |r| r.start);
                if start != expected || start + bytes.len() as u64 != total {
                    return Ok(false);
                }
                set_boot(self, Boot::Ready);
                match self.read_tail(start, bytes, total) {
                    Some(Boot::Index(offset)) if boot == Boot::Tail => set_boot(self, Boot::Index(offset)),
                    Some(Boot::Ready) => {}
                    _ => self.stats.index_missing = true,
                }
                Ok(true)
            }
            Boot::Ready => Ok(self.supply_records(start, bytes, total)),
        }
    }

    // ---------------------------------------------------------- transport

    /// The file header, once it has arrived.
    pub fn info(&self) -> Option<&RecordingInfo> {
        self.info.as_ref()
    }

    /// Length of the recording in seconds, once known: the time of its
    /// last record, or what the header says while the end of the file has
    /// not been seen.
    pub fn duration(&self) -> Option<f64> {
        self.end_time.or(self.info.as_ref().map(|i| i.duration_s).filter(|d| *d > 0.0))
    }

    /// The play head in seconds.
    pub fn time(&self) -> f64 {
        self.head
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Lets [`Player::advance`] move the play head. At the end of the
    /// recording it starts over.
    pub fn play(&mut self) {
        if self.end_time.is_some_and(|end| self.head >= end) {
            self.head = 0.0;
        }
        self.playing = true;
    }

    pub fn pause(&mut self) {
        self.playing = false;
    }

    /// Recording seconds per second of the clock, between 1/64 and 64.
    pub fn speed(&self) -> f64 {
        self.speed
    }

    pub fn set_speed(&mut self, speed: f64) {
        self.speed = if speed.is_finite() { speed.clamp(1.0 / 64.0, 64.0) } else { 1.0 };
    }

    pub fn is_looping(&self) -> bool {
        self.looping
    }

    /// At the end, start over instead of stopping.
    pub fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
    }

    /// Moves the play head to `time_s` (clamped to the recording). The
    /// next [`Player::update`] or [`Player::advance`] brings the viewer
    /// there; call it right away to scrub while paused.
    pub fn seek(&mut self, time_s: f64) {
        let time = if time_s.is_finite() { time_s.max(0.0) } else { 0.0 };
        self.head = self.duration().map_or(time, |end| time.min(end));
    }

    /// Forgets what the viewer holds: the next update starts from a
    /// keyframe and sends everything. Call it after anything else wrote to
    /// the viewer (the live stream, a file, a cleared scene).
    pub fn invalidate_viewer(&mut self) {
        self.cursor = None;
        self.viewer_state = None;
        self.viewer_static = None;
    }

    /// Moves the clock on by `dt_s` seconds and brings the viewer to the
    /// play head: while playing the head moves by `dt_s` times the speed,
    /// wraps at the end when looping and stops there otherwise. A head
    /// that waits for data does not move.
    pub fn advance(&mut self, dt_s: f64) -> Update {
        self.step(Some(dt_s))
    }

    /// Brings the viewer to the play head without moving it: after a
    /// [`Player::seek`] while paused, or when new bytes have arrived.
    pub fn update(&mut self) -> Update {
        self.step(None)
    }

    /// Times of the keyframes known so far, in order: where a seek is
    /// cheapest.
    pub fn keyframe_times(&self) -> impl Iterator<Item = f64> + '_ {
        self.keys.iter().map(|k| k.time)
    }

    /// Time up to which the recording can play without waiting for more
    /// bytes, from the play head on.
    pub fn buffered_until(&self) -> f64 {
        let offset = self.cursor.map_or(self.first_record, |c| c.offset);
        let loaded = self.spans.iter().find(|s| s.reaches(offset)).and_then(|s| s.recs.last()).map(|r| r.time);
        loaded.unwrap_or(0.0).max(self.cursor.map_or(0.0, |c| c.time))
    }

    /// Why nothing can be played at all, if so: a bad file header, or a
    /// recording without a keyframe.
    pub fn error(&self) -> Option<RecordError> {
        self.error
    }

    /// Where and why the playable part of the recording ends early, if a
    /// record is damaged.
    pub fn damage(&self) -> Option<Problem> {
        self.damage
    }

    /// Counters, for diagnostics and tests.
    pub fn stats(&self) -> PlayerStats {
        PlayerStats {
            cached_bytes: self.cached_bytes(),
            records: self.spans.iter().map(|s| s.recs.len() as u64).sum(),
            keyframes: self.keys.len() as u64,
            ..self.stats
        }
    }

    // ------------------------------------------------------------ playing

    fn step(&mut self, dt: Option<f64>) -> Update {
        let before = self.head;
        let clock = dt.filter(|_| self.playing).map(|dt| if dt.is_finite() && dt > 0.0 { dt } else { 0.0 });
        if let Some(dt) = clock {
            self.head += dt * self.speed;
        }
        let mut stopping = false;
        if let Some(end) = self.end_time.filter(|end| self.head >= *end) {
            if clock.is_some() && self.looping && end > 0.0 {
                self.head %= end;
            } else {
                self.head = end;
                stopping = clock.is_some();
            }
        }
        let mut batch = Coalescer::default();
        let reset = self.catch_up(&mut batch);
        // The head stops once the viewer shows the end, not before.
        let ended = stopping && self.at_end();
        if ended {
            self.playing = false;
        }
        if self.starved && clock.is_some() {
            // Wait for the data: the head goes no further than what the
            // viewer shows (or than where a seek put it).
            let shown = self.cursor.map_or(0.0, |c| c.time);
            self.head = self.head.min(before.max(shown));
        }
        let messages = self.materialize(batch);
        Update { messages, reset, buffering: self.starved, ended, time: self.head }
    }

    /// True when the viewer holds everything up to the end of the records.
    fn at_end(&self) -> bool {
        self.cursor.is_some_and(|c| self.records_end == Some(c.offset))
    }

    /// No keyframe is known and none can arrive any more.
    fn exhausted(&self) -> bool {
        match self.source {
            Source::Whole => true,
            Source::Stream { finished } => finished,
            Source::Ranged { total, boot } => {
                boot == Boot::Ready && self.wanted_records(total).is_none_or(|r| r.is_empty())
            }
        }
    }

    /// The last keyframe that is not after `time`, or the first one.
    fn key_at(&self, time: f64) -> Option<Key> {
        let after = self.keys.partition_point(|k| k.time <= time);
        self.keys.get(after.saturating_sub(1)).copied()
    }

    /// Span and record index of the record that starts at `offset`.
    fn rec_at(&self, offset: u64) -> Option<(usize, usize)> {
        let si = self.spans.iter().position(|s| offset >= s.base && offset < s.scanned_end())?;
        let pos = (offset - self.spans[si].base) as usize;
        let ri = self.spans[si].recs.binary_search_by_key(&pos, |r| r.pos).ok()?;
        Some((si, ri))
    }

    /// The offset lies inside parsed records (so if no record starts
    /// there, none ever will).
    fn inside_scanned(&self, offset: u64) -> bool {
        self.spans.iter().any(|s| offset >= s.base && offset < s.scanned_end())
    }

    /// Whether getting to `target` should go through a keyframe.
    fn wants_resync(&self, target: f64) -> bool {
        let Some(cursor) = self.cursor else { return true };
        if target < cursor.time {
            return true;
        }
        match self.key_at(target) {
            // A keyframe further on that is not after the target: take it
            // when the way there is long or has not been loaded.
            Some(key) if key.offset > cursor.offset => {
                let loaded = self.spans.iter().any(|s| s.reaches(cursor.offset) && key.offset < s.scanned_end());
                target - cursor.time > self.options.jump_s || !loaded
            }
            _ => false,
        }
    }

    /// Moves the cursor towards the play head, collecting the messages on
    /// the way. Returns whether it went through a keyframe.
    fn catch_up(&mut self, batch: &mut Coalescer) -> bool {
        self.starved = false;
        let target = self.head;
        let mut reset = false;
        if self.wants_resync(target) {
            loop {
                let Some(key) = self.key_at(target) else {
                    // Nothing to show yet, or ever.
                    if self.exhausted() {
                        self.error.get_or_insert(self.damage.map_or(RecordError::Empty, |d| d.error));
                    }
                    self.starved = self.error.is_none();
                    return false;
                };
                match self.load_keyframe(key) {
                    Loaded::Messages(messages, next) => {
                        // Whatever this update collected so far is history.
                        *batch = Coalescer::default();
                        for message in messages {
                            if let Some(class) = classify(&message) {
                                batch.push(class, Body::Owned(message));
                            }
                        }
                        self.cursor = Some(Cursor { offset: next, time: key.time });
                        self.stats.resyncs += 1;
                        reset = true;
                        break;
                    }
                    Loaded::Missing => {
                        self.starved = true;
                        return false;
                    }
                    Loaded::Unusable => self.keys.retain(|k| k.offset != key.offset),
                }
            }
        }
        let Some(mut cursor) = self.cursor else { return reset };
        'spans: loop {
            let Some((si, mut ri)) = self.rec_at(cursor.offset) else {
                if self.records_end == Some(cursor.offset) {
                    // The end of the recording.
                } else if self.inside_scanned(cursor.offset) {
                    // No record starts here after all (an index that lied
                    // about a keyframe): start over from a real one.
                    self.cursor = None;
                    self.starved = true;
                    return reset;
                } else {
                    self.starved = true;
                }
                break;
            };
            let span = &self.spans[si];
            while let Some(rec) = span.recs.get(ri) {
                if rec.time > target {
                    break 'spans;
                }
                if rec.kind == RecordKind::Message {
                    match peek_stored(&mut self.inflater, span.stored(rec), rec.deflated) {
                        Some(class) => batch.push(class, Body::Stored(si, ri)),
                        None => self.stats.dropped += 1,
                    }
                }
                cursor = Cursor { offset: span.base + (rec.pos + rec.total()) as u64, time: rec.time };
                ri += 1;
            }
        }
        self.cursor = Some(cursor);
        reset
    }

    fn load_keyframe(&self, key: Key) -> Loaded {
        let Some((si, ri)) = self.rec_at(key.offset) else {
            return if self.inside_scanned(key.offset) { Loaded::Unusable } else { Loaded::Missing };
        };
        let span = &self.spans[si];
        let rec = &span.recs[ri];
        if rec.kind != RecordKind::Keyframe {
            return Loaded::Unusable;
        }
        let Ok(payload) = payload_of(span.stored(rec), rec.deflated) else { return Loaded::Unusable };
        match split_keyframe(&payload) {
            Ok(messages) => {
                Loaded::Messages(messages.into_iter().map(<[u8]>::to_vec).collect(), key.offset + rec.total() as u64)
            }
            Err(_) => Loaded::Unusable,
        }
    }

    /// Turns the surviving items of a batch into messages, leaving out a
    /// `State` and a layer 0 `Replace` the viewer already holds.
    fn materialize(&mut self, batch: Coalescer) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for item in batch.items.into_iter().flatten() {
            let bytes = match item.body {
                Body::Owned(bytes) => Some(bytes),
                Body::Stored(si, ri) => {
                    let span = &self.spans[si];
                    let rec = &span.recs[ri];
                    payload_of(span.stored(rec), rec.deflated).ok().map(|p| p.into_owned())
                }
            };
            let Some(bytes) = bytes.filter(|b| classify(b) == Some(item.class)) else {
                self.stats.dropped += 1;
                continue;
            };
            match item.class {
                MessageClass::State => {
                    let print = fingerprint(&bytes);
                    if self.viewer_state.replace(print) == Some(print) {
                        continue;
                    }
                }
                MessageClass::Layer { layer: 0, replace: true } => {
                    let print = fingerprint(&bytes);
                    if self.viewer_static.replace(print) == Some(print) {
                        continue;
                    }
                }
                MessageClass::Layer { layer: 0, replace: false } => self.viewer_static = None,
                _ => {}
            }
            self.stats.messages += 1;
            self.stats.message_bytes += bytes.len() as u64;
            out.push(bytes);
        }
        out
    }

    // ------------------------------------------------------------ parsing

    /// Parses the records of span `i` that have arrived. Returns `false`
    /// when the span turned out not to start at a record and was removed.
    fn scan(&mut self, i: usize) -> bool {
        loop {
            let span = &self.spans[i];
            if span.closed {
                return true;
            }
            let offset = span.scanned_end();
            if self.records_end == Some(offset) {
                self.spans[i].closed = true;
                self.check_playable();
                return true;
            }
            let rest = &span.bytes[span.scanned..];
            let peeked = peek_record(rest);
            // A span fetched at an offset of the index must start with the
            // keyframe the index promised. If it does not, the index lied:
            // real damage is found by whoever reads up to it.
            if span.recs.is_empty() && span.base > self.first_record {
                let promised = self.keys.iter().find(|k| k.offset == span.base);
                let kept = match peeked {
                    Peek::Record(header) => {
                        header.kind == RecordKind::Keyframe
                            && promised.is_some_and(|k| k.time.to_bits() == header.time.to_bits())
                    }
                    Peek::NeedMore => true,
                    Peek::Damaged(_) => false,
                };
                if !kept {
                    let base = span.base;
                    self.keys.retain(|k| k.offset != base);
                    self.spans.remove(i);
                    self.stats.index_missing = true;
                    return false;
                }
            }
            let header = match peeked {
                Peek::Record(header) => header,
                Peek::NeedMore => return true,
                Peek::Damaged(error) => return self.close_damaged(i, offset, error),
            };
            if rest.len() < header.total_len() {
                return true;
            }
            if span.recs.last().is_some_and(|last| header.time < last.time) {
                return self.close_damaged(i, offset, RecordError::BadTime);
            }
            let rec = Rec {
                pos: span.scanned,
                payload_len: header.payload_len,
                kind: header.kind,
                deflated: header.deflated(),
                time: header.time,
            };
            match header.kind {
                RecordKind::Index => {
                    // The last record. What follows is the trailer.
                    self.records_end = Some(offset);
                    self.end_time = Some(header.time);
                    self.spans[i].closed = true;
                    self.check_playable();
                    return true;
                }
                RecordKind::Keyframe => {
                    if let Err(at) = self.keys.binary_search_by_key(&offset, |k| k.offset) {
                        self.keys.insert(at, Key { time: header.time, offset });
                    }
                }
                RecordKind::Message => {}
            }
            let span = &mut self.spans[i];
            span.recs.push(rec);
            span.scanned += rec.total();
        }
    }

    /// The file ends behind span `i` without an index: a recording cut
    /// short. What was parsed is all there is.
    fn close_at_end(&mut self, i: usize) {
        let span = &mut self.spans[i];
        if !span.closed {
            span.closed = true;
            self.records_end = Some(span.scanned_end());
            self.end_time = Some(span.recs.last().map_or(0.0, |r| r.time));
            self.stats.index_missing = true;
        }
        self.check_playable();
    }

    fn close_damaged(&mut self, i: usize, offset: u64, error: RecordError) -> bool {
        self.damage = Some(Problem { offset, error });
        let span = &mut self.spans[i];
        span.closed = true;
        self.records_end = Some(offset);
        self.end_time = Some(span.recs.last().map_or(0.0, |r| r.time));
        // Nothing behind the damage is reachable any more.
        self.keys.retain(|k| k.offset < offset);
        self.spans.retain(|s| s.base <= offset);
        self.check_playable();
        true
    }

    /// Once the records have been read to their end: is there anything to
    /// show?
    fn check_playable(&mut self) {
        let read_from_start = self.spans.first().is_some_and(|s| s.base <= self.first_record && s.closed);
        if self.keys.is_empty() && read_from_start {
            self.error.get_or_insert(self.damage.map_or(RecordError::Empty, |d| d.error));
        }
    }

    // -------------------------------------------------------------- ranges

    fn cached_bytes(&self) -> u64 {
        self.spans.iter().map(|s| s.bytes.len() as u64).sum()
    }

    /// Where the player reads next: the keyframe a seek goes through, or
    /// the record after the last one applied.
    fn anchor(&self) -> u64 {
        if self.wants_resync(self.head) {
            self.key_at(self.head).map_or(self.first_record, |k| k.offset)
        } else {
            self.cursor.map_or(self.first_record, |c| c.offset)
        }
    }

    fn wanted_records(&self, total: u64) -> Option<ByteRange> {
        let limit = self.records_end.unwrap_or(total);
        let anchor = self.anchor();
        let block = self.options.fetch_block.max(1);
        let start = match self.spans.iter().find(|s| s.reaches(anchor)) {
            None => anchor,
            Some(span) => {
                if span.closed {
                    return None;
                }
                // Reading ahead is a comfort the cache must have room for.
                let room = self.cached_bytes() < self.options.max_cache_bytes;
                let ahead = if self.playing && room { self.options.prefetch_s.max(0.0) * self.speed } else { 0.0 };
                if span.recs.last().is_some_and(|r| r.time > self.head + ahead) {
                    return None;
                }
                span.end()
            }
        };
        let mut end = start.saturating_add(block);
        // At least the rest of a record that is cut off at the end.
        if let Some(span) = self.spans.iter().find(|s| s.end() == start) {
            if let Peek::Record(header) = peek_record(&span.bytes[span.scanned..]) {
                end = end.max(span.scanned_end() + header.total_len() as u64);
            }
        }
        if let Some(next) = self.spans.iter().find(|s| s.base > start) {
            end = end.min(next.base);
        }
        Some(ByteRange { start, end: end.min(limit) })
    }

    fn supply_records(&mut self, start: u64, bytes: &[u8], total: u64) -> bool {
        let limit = self.records_end.unwrap_or(total);
        if start >= limit || bytes.is_empty() {
            return false;
        }
        let next_base = self.spans.iter().map(|s| s.base).find(|base| *base > start).unwrap_or(u64::MAX);
        let room = (limit.min(next_base) - start).min(bytes.len() as u64) as usize;
        let bytes = &bytes[..room];
        let i = if let Some(i) = self.spans.iter().position(|s| s.end() == start && !s.closed) {
            self.spans[i].bytes.extend_from_slice(bytes);
            i
        } else {
            let boundary = start == self.first_record || self.keys.iter().any(|k| k.offset == start);
            if !boundary || self.spans.iter().any(|s| start >= s.base && start < s.end()) {
                return false;
            }
            let i = self.spans.partition_point(|s| s.base < start);
            self.spans.insert(i, Span::new(start, bytes.to_vec(), 0));
            i
        };
        if self.scan(i) && self.join_next(i) && !self.spans[i].closed && self.spans[i].end() >= limit {
            // Everything up to the end of the records is here; a record
            // that is still incomplete never will be.
            if self.records_end.is_none() {
                self.close_at_end(i);
            } else {
                let end = self.spans[i].scanned_end();
                self.close_damaged(i, end, RecordError::BadIndex);
            }
        }
        self.evict();
        true
    }

    /// Joins span `i` with the one behind it once they touch. Returns
    /// `false` when span `i` was removed on the way.
    fn join_next(&mut self, i: usize) -> bool {
        while i + 1 < self.spans.len() && self.spans[i].end() == self.spans[i + 1].base {
            let next = self.spans.remove(i + 1);
            let joint = next.base;
            self.spans[i].bytes.extend_from_slice(&next.bytes);
            if !self.scan(i) {
                return false;
            }
            // The records of the first span must end where the second one
            // started; if not, the index named an offset inside a record.
            let span = &self.spans[i];
            if span.scanned_end() > joint && self.rec_at(joint).is_none() {
                let (from, to) = (span.base, span.scanned_end());
                let real: Vec<u64> = span
                    .recs
                    .iter()
                    .filter(|r| r.kind == RecordKind::Keyframe)
                    .map(|r| span.base + r.pos as u64)
                    .collect();
                self.keys.retain(|k| k.offset < from || k.offset >= to || real.binary_search(&k.offset).is_ok());
                self.stats.index_missing = true;
            }
        }
        true
    }

    /// Drops cached bytes beyond [`PlayerOptions::max_cache_bytes`]: whole
    /// spans away from where the player reads, then the part of the
    /// current span before the keyframe the play head depends on.
    fn evict(&mut self) {
        let anchor = self.anchor();
        let keep_from = self.cursor.map_or(anchor, |c| c.offset.min(anchor));
        while self.cached_bytes() > self.options.max_cache_bytes {
            let distance = |s: &Span| if s.base > anchor { s.base - anchor } else { anchor.saturating_sub(s.end()) };
            let far = (0..self.spans.len())
                .filter(|&i| !self.spans[i].reaches(anchor))
                .max_by_key(|&i| distance(&self.spans[i]));
            if let Some(i) = far {
                self.spans.remove(i);
                continue;
            }
            let Some(i) = self.spans.iter().position(|s| s.reaches(anchor)) else { break };
            // The keyframe the viewer's state hangs on must stay.
            let key = self.keys.iter().rev().find(|k| k.offset <= keep_from).map_or(0, |k| k.offset);
            let span = &mut self.spans[i];
            if key <= span.base || span.base == 0 {
                break;
            }
            let cut = (key - span.base) as usize;
            span.bytes.drain(..cut);
            span.base = key;
            span.scanned -= cut;
            span.recs.retain(|r| r.pos >= cut);
            for rec in &mut span.recs {
                rec.pos -= cut;
            }
        }
    }

    /// Looks for the trailer and the index in the bytes `[start, total)`.
    /// `Some(Boot::Ready)`: the index was read. `Some(Boot::Index(o))`: it
    /// starts at `o`, before these bytes. `None`: there is none.
    fn read_tail(&mut self, start: u64, bytes: &[u8], total: u64) -> Option<Boot> {
        let trailer = bytes.len().checked_sub(TRAILER_SIZE)?;
        if bytes[trailer + 8..] != END_MAGIC {
            return None;
        }
        let index_at = u64_at(bytes, trailer);
        let index_len = (total - TRAILER_SIZE as u64).checked_sub(index_at)?;
        if index_at < self.first_record
            || index_len < RECORD_HEADER_SIZE as u64
            || index_len > MAX_RECORD_LEN as u64 + 4
        {
            return None;
        }
        if index_at < start {
            return Some(Boot::Index(index_at));
        }
        let record = &bytes[(index_at - start) as usize..trailer];
        let Peek::Record(header) = peek_record(record) else { return None };
        if header.kind != RecordKind::Index || header.total_len() != record.len() {
            return None;
        }
        let payload = payload_of(&record[RECORD_HEADER_SIZE..], header.deflated()).ok()?;
        let entries = parse_index(&payload).ok()?;
        let inside = entries
            .iter()
            .all(|&(time, offset)| offset >= self.first_record && offset < index_at && time <= header.time);
        if !inside {
            return None;
        }
        self.keys = entries.into_iter().map(|(time, offset)| Key { time, offset }).collect();
        self.records_end = Some(index_at);
        self.end_time = Some(header.time);
        Some(Boot::Ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{encode, encode_layer, Body as WireBody, LayerMode, Message};
    use crate::Splat;

    fn layer(id: u8, mode: LayerMode, n: usize) -> Vec<u8> {
        let splats: Vec<Splat> = (0..n)
            .map(|i| Splat { pos: [i as f32, 0.0, 0.0], scale: [0.01; 3], rot: [0.0, 0.0, 0.0, 1.0], rgba: [9; 4] })
            .collect();
        let mut out = Vec::new();
        encode_layer(0, 0, id, mode, &splats, &mut out);
        out
    }

    fn stats(n: u32) -> Vec<u8> {
        encode(&Message { seq: n, time_ms: 0, body: WireBody::Stats(format!("{{\"tick\":{n}}}")) })
    }

    #[test]
    fn a_batch_is_coalesced_like_a_backlog_of_the_live_stream() {
        let mut batch = Coalescer::default();
        let mut push = |bytes: Vec<u8>| batch.push(classify(&bytes).unwrap(), Body::Owned(bytes));
        let state = encode(&Message { seq: 0, time_ms: 0, body: WireBody::State("{}".into()) });
        push(state.clone());
        push(layer(0, LayerMode::Replace, 3));
        push(layer(0, LayerMode::Append, 1));
        push(layer(1, LayerMode::Replace, 5));
        push(stats(1));
        push(layer(1, LayerMode::Replace, 6));
        push(layer(0, LayerMode::Replace, 4));
        push(layer(0, LayerMode::Append, 2));
        push(stats(2));
        push(state.clone());
        let left: Vec<Vec<u8>> = batch
            .items
            .into_iter()
            .flatten()
            .map(|item| match item.body {
                Body::Owned(bytes) => bytes,
                Body::Stored(..) => unreachable!(),
            })
            .collect();
        let expected = [
            state.clone(),
            layer(1, LayerMode::Replace, 6),
            layer(0, LayerMode::Replace, 4),
            layer(0, LayerMode::Append, 2),
            stats(2),
            state,
        ];
        assert_eq!(left, expected);
    }

    #[test]
    fn the_class_of_a_deflated_message_is_read_from_its_first_bytes() {
        let mut inflater = Box::<DecompressorOxide>::default();
        for message in [layer(1, LayerMode::Replace, 5000), layer(0, LayerMode::Append, 0), stats(7)] {
            let packed = miniz_oxide::deflate::compress_to_vec(&message, 1);
            assert_eq!(peek_stored(&mut inflater, &packed, true), classify(&message));
            assert_eq!(peek_stored(&mut inflater, &message, false), classify(&message));
        }
        // Damage further on does not matter here; garbage has no class.
        let message = layer(1, LayerMode::Replace, 5000);
        let packed = miniz_oxide::deflate::compress_to_vec(&message, 1);
        assert_eq!(peek_stored(&mut inflater, &packed[..packed.len() / 2], true), classify(&message));
        assert_eq!(peek_stored(&mut inflater, &[0xff; 64], true), None);
        assert_eq!(peek_stored(&mut inflater, &[], true), None);
        assert_eq!(peek_stored(&mut inflater, &[], false), None);
    }
}
