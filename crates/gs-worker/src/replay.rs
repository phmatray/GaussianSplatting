//! Replay of a `.gsrec` recording: the third mode of the worker, next to
//! the local pipeline and the relay.
//!
//! [`Replay`] wraps a [`gs_core::record::Player`] with what the worker adds:
//! the acknowledged-batch flow control of the other modes (a page that
//! draws slowly skips ahead instead of queueing), a clock that survives a
//! suspended tab, and the [`ReplayStatus`] the page shows on its time line.
//! Nothing here touches the browser; the `splat-worker` binary fetches the
//! bytes (a `Blob`, a download in progress, HTTP `Range` requests).

use gs_core::record::{ByteRange, Player, PlayerOptions, RecordError};
use serde::{Deserialize, Serialize};

use crate::{Batch, FlowControl, MAX_TICK_INTERVAL_MS};

/// Interval between two steps of the play head while playing, milliseconds.
/// Shorter than any pipeline tick, so that every recorded tick gets its own
/// batch at normal speed.
const PLAY_STEP_MS: f64 = 1000.0 / 60.0;
/// How long the player sleeps while it waits for acknowledgements.
const STALL_POLL_MS: f64 = 25.0;
/// While the head moves, a status goes to the page at most this often.
const STATUS_INTERVAL_MS: f64 = 66.0;

/// What the page shows of a replay. Sent as the `replay` worker event
/// whenever something other than the time changes, and about 15 times per
/// second while the play head moves.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplayStatus {
    /// Play head, seconds.
    pub time: f64,
    /// Length of the recording, seconds; `None` while it is not known (the
    /// header of a recording cut short says 0). Grows while a download is
    /// in progress.
    pub duration: Option<f64>,
    pub playing: bool,
    pub speed: f64,
    pub looping: bool,
    /// The player waits for bytes.
    pub buffering: bool,
    /// The play head is at the end and stopped there.
    pub ended: bool,
    /// Time up to which playback will not wait for bytes, seconds.
    pub buffered: f64,
    /// A first picture has been handed to the page.
    pub ready: bool,
    /// Identifier of the last `replay_seek` whose picture has been handed
    /// to the page (0 before any).
    pub seek: u32,
    /// Name, creation time (RFC 3339) and scene from the file header.
    pub name: String,
    pub created: String,
    pub scene: String,
    /// `RecordError::code()` when nothing of the file can be played.
    pub error: Option<String>,
    /// The recording is damaged and ends early.
    pub damaged: bool,
    /// Recorded messages left out because they were not valid.
    pub dropped: u64,
}

/// Longest text of a file header repeated in a status, in characters. The
/// header of a recording may hold a megabyte of JSON, and a status goes to
/// the page many times per second.
const MAX_STATUS_TEXT: usize = 200;

/// `text` cut to [`MAX_STATUS_TEXT`] characters.
fn clip(text: &str) -> String {
    text.chars().take(MAX_STATUS_TEXT).collect()
}

/// What [`Replay::poll`] decided.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplayPoll {
    /// Messages for the viewer, to be posted to the page.
    pub batch: Option<Batch>,
    /// The batch starts from a keyframe: the page forgets the statistics
    /// and thumbnails it shows before applying it.
    pub reset: bool,
    /// Call [`Replay::poll`] again after this many milliseconds, or earlier
    /// when something happens. `None`: only when something happens.
    pub wait_ms: Option<f64>,
}

/// A recording being replayed.
pub struct Replay {
    player: Player,
    flow: FlowControl,
    /// Clock value of the last step of the play head.
    last_ms: Option<f64>,
    /// Something happened that calls for an update without a clock step: a
    /// seek, new bytes, the first picture.
    dirty: bool,
    /// Latest seek asked for, and the latest whose picture left.
    seek_wanted: u32,
    seek_done: u32,
    ready: bool,
    buffering: bool,
    ended: bool,
    /// Why nothing can be played: a `RecordError::code()`, or a code of
    /// the host for a source that failed (`network`, `http_404`, ...).
    fatal: Option<String>,
    sent: Option<(ReplayStatus, f64)>,
}

impl Replay {
    fn with_player(player: Player) -> Self {
        Self {
            player,
            flow: FlowControl::default(),
            last_ms: None,
            dirty: true,
            seek_wanted: 0,
            seek_done: 0,
            ready: false,
            buffering: false,
            ended: false,
            fatal: None,
            sent: None,
        }
    }

    /// A recording held in memory as a whole.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, RecordError> {
        Player::from_bytes(bytes).map(Self::with_player)
    }

    /// A recording that arrives front to back: [`Replay::push_bytes`] per
    /// chunk, [`Replay::finish_stream`] at the end.
    pub fn streaming() -> Self {
        Self::with_player(Player::streaming())
    }

    /// A recording of `total_len` bytes fetched piece by piece: ask
    /// [`Replay::wanted`] whenever no request is in flight and answer with
    /// [`Replay::supply`].
    pub fn ranged(total_len: u64) -> Self {
        Self::with_player(Player::ranged(total_len))
    }

    /// Sets the size of one request of a [`Replay::ranged`] recording.
    pub fn with_fetch_block(mut self, bytes: u64) -> Self {
        self.player.set_options(PlayerOptions { fetch_block: bytes.max(64 * 1024), ..PlayerOptions::default() });
        self
    }

    /// Continues the batch numbering of whatever fed the page before, so
    /// that acknowledgements still on their way stay meaningful.
    pub fn with_flow(mut self, flow: FlowControl) -> Self {
        self.flow = flow;
        self
    }

    pub fn flow(&self) -> FlowControl {
        self.flow
    }

    /// A replay that only reports why its source could not be opened.
    pub fn failed(code: &str) -> Self {
        let mut replay = Self::streaming();
        replay.fail(code);
        replay
    }

    /// Records a failure of the source (the network, the file) under a
    /// short code: the replay keeps the picture it has and reports the
    /// error in its status.
    pub fn fail(&mut self, code: &str) {
        self.fatal.get_or_insert_with(|| code.to_string());
        self.player.pause();
    }

    // ------------------------------------------------------------ source

    /// Next bytes of a [`Replay::streaming`] recording.
    pub fn push_bytes(&mut self, chunk: &[u8]) {
        if let Err(error) = self.player.push_bytes(chunk) {
            self.fail(error.code());
        }
        self.dirty = true;
    }

    pub fn finish_stream(&mut self) {
        self.player.finish_stream();
        self.dirty = true;
    }

    /// The bytes a [`Replay::ranged`] recording needs next.
    pub fn wanted(&self) -> Option<ByteRange> {
        if self.fatal.is_some() {
            return None;
        }
        self.player.wanted()
    }

    /// The answer to [`Replay::wanted`]. A stale answer is ignored.
    pub fn supply(&mut self, start: u64, bytes: &[u8]) {
        match self.player.supply(start, bytes) {
            Ok(_) => self.dirty = true,
            Err(error) => self.fail(error.code()),
        }
    }

    // --------------------------------------------------------- transport

    pub fn play(&mut self) {
        if !self.player.is_playing() {
            self.player.play();
            self.last_ms = None;
            self.ended = false;
        }
    }

    pub fn pause(&mut self) {
        self.player.pause();
    }

    /// Moves the play head; `id` comes back in [`ReplayStatus::seek`] once
    /// the picture of that moment has been handed to the page. Seeks made
    /// while the page is behind collapse into the last one.
    pub fn seek(&mut self, time_s: f64, id: u32) {
        if time_s.is_finite() {
            self.player.seek(time_s);
            self.ended = false;
        }
        self.seek_wanted = id;
        self.dirty = true;
    }

    pub fn set_speed(&mut self, speed: f64) {
        if speed.is_finite() && speed > 0.0 {
            self.player.set_speed(speed);
        }
    }

    pub fn set_looping(&mut self, looping: bool) {
        self.player.set_looping(looping);
    }

    /// The viewer was written to by something else: the next batch must
    /// not assume it still shows what the player sent.
    pub fn invalidate_viewer(&mut self) {
        self.player.invalidate_viewer();
        self.dirty = true;
    }

    /// Records the acknowledgement of every batch up to `batch`.
    pub fn ack(&mut self, batch: u32) {
        self.flow.ack(batch);
    }

    // ------------------------------------------------------------ driving

    /// Moves the play head to clock value `now_ms` (milliseconds of a
    /// monotonic clock) and hands out what the viewer needs, if the page
    /// has kept up.
    pub fn poll(&mut self, now_ms: f64) -> ReplayPoll {
        let playing = self.player.is_playing();
        if !playing && !self.dirty {
            return ReplayPoll::default();
        }
        if !self.flow.can_tick() {
            // The step that was due is dropped; the next one covers its time.
            return ReplayPoll { wait_ms: Some(STALL_POLL_MS), ..ReplayPoll::default() };
        }
        let update = if playing {
            let elapsed_ms = self.last_ms.map_or(0.0, |last| now_ms - last);
            self.last_ms = Some(now_ms);
            // After a stall (a suspended tab) playback resumes instead of leaping.
            self.player.advance(elapsed_ms.clamp(0.0, MAX_TICK_INTERVAL_MS) / 1000.0)
        } else {
            self.player.update()
        };
        self.dirty = false;
        self.buffering = update.buffering;
        if update.ended {
            self.ended = true;
        }
        // The picture of the latest seek is in this update, unless the
        // player still waits for its bytes.
        if !update.buffering {
            self.seek_done = self.seek_wanted;
        }
        let batch = if update.messages.is_empty() {
            None
        } else {
            self.ready = true;
            Some(Batch { id: self.flow.next_id(), messages: update.messages })
        };
        let wait_ms = self.player.is_playing().then_some(PLAY_STEP_MS);
        ReplayPoll { reset: update.reset && batch.is_some(), batch, wait_ms }
    }

    /// The status as it is now.
    pub fn status(&self) -> ReplayStatus {
        let error = self.fatal.clone().or_else(|| self.player.error().map(|e| e.code().to_string()));
        let info = self.player.info();
        ReplayStatus {
            time: self.player.time(),
            duration: self.player.duration().filter(|d| d.is_finite() && *d > 0.0),
            playing: self.player.is_playing(),
            speed: self.player.speed(),
            looping: self.player.is_looping(),
            buffering: self.buffering,
            ended: self.ended && !self.player.is_playing(),
            buffered: self.player.buffered_until(),
            ready: self.ready,
            seek: self.seek_done,
            name: info.map(|i| clip(&i.name)).unwrap_or_default(),
            created: info.map(|i| clip(&i.created)).unwrap_or_default(),
            scene: info.map(|i| clip(&i.scene)).unwrap_or_default(),
            error,
            damaged: self.player.damage().is_some(),
            dropped: self.player.stats().dropped,
        }
    }

    /// The status, if the page should hear about it now: something other
    /// than the time changed, or the head moved and the last report is
    /// [`STATUS_INTERVAL_MS`] old.
    pub fn take_status(&mut self, now_ms: f64) -> Option<ReplayStatus> {
        let status = self.status();
        let due = match &self.sent {
            None => true,
            Some((last, at)) => {
                let moved = last.time != status.time || last.buffered != status.buffered;
                let same_otherwise = ReplayStatus { time: status.time, buffered: status.buffered, ..last.clone() } == status;
                !same_otherwise || (moved && now_ms - at >= STATUS_INTERVAL_MS)
            }
        };
        due.then(|| {
            self.sent = Some((status.clone(), now_ms));
            status
        })
    }
}

#[cfg(test)]
mod tests {
    use gs_core::codec::{self, Kind};
    use gs_core::pipeline::Pipeline;
    use gs_core::protocol::PipelineConfig;
    use gs_core::record::{Recorder, RecorderOptions};
    use gs_core::scene::SceneKind;

    use super::*;
    use crate::MAX_UNACKED_TICKS;

    /// A short recording of a small stage: `seconds` at 20 ticks per second.
    pub(crate) fn recording(seconds: f64) -> Vec<u8> {
        let mut config = PipelineConfig::light(SceneKind::Stage);
        config.rig.count = 3;
        config.rig.width = 48;
        config.rig.height = 36;
        let mut pipeline = Pipeline::new(config);
        let mut recorder = Recorder::new(Vec::new(), RecorderOptions::named("essai"));
        for message in pipeline.snapshot() {
            recorder.push(0.0, &message.bytes).unwrap();
        }
        let ticks = (seconds * 20.0).round() as u32;
        for tick in 1..=ticks {
            for message in pipeline.tick(0.05) {
                recorder.push(f64::from(tick) * 0.05, &message.bytes).unwrap();
            }
        }
        recorder.finish().unwrap().0
    }

    fn kinds(batch: &Batch) -> Vec<Kind> {
        batch.messages.iter().map(|m| codec::peek_kind(m).unwrap()).collect()
    }

    /// Polls with every batch acknowledged at once; returns the batches.
    fn run(replay: &mut Replay, from_ms: f64, to_ms: f64) -> Vec<Batch> {
        let mut out = Vec::new();
        let mut now = from_ms;
        while now <= to_ms {
            if let Some(batch) = replay.poll(now).batch {
                replay.ack(batch.id);
                out.push(batch);
            }
            now += 10.0;
        }
        out
    }

    #[test]
    fn a_paused_replay_shows_its_first_picture_and_then_waits() {
        let mut replay = Replay::from_bytes(recording(2.0)).unwrap();
        let first = replay.poll(0.0);
        let batch = first.batch.expect("first picture");
        assert_eq!(&kinds(&batch)[..3], [Kind::State, Kind::Layer, Kind::Layer]);
        assert!(first.reset && first.wait_ms.is_none());
        replay.ack(batch.id);
        // Nothing more until something happens.
        assert_eq!(replay.poll(500.0), ReplayPoll::default());
        let status = replay.take_status(500.0).unwrap();
        assert!(status.ready && !status.playing && !status.buffering && status.error.is_none());
        assert_eq!((status.name.as_str(), status.scene.as_str(), status.time), ("essai", "stage", 0.0));
        assert!((status.duration.unwrap() - 2.0).abs() < 1e-6);
        // Unchanged: not reported again.
        assert!(replay.take_status(900.0).is_none());
    }

    #[test]
    fn playing_follows_the_clock_and_stops_at_the_end() {
        let mut replay = Replay::from_bytes(recording(1.0)).unwrap();
        run(&mut replay, 0.0, 0.0);
        replay.play();
        let batches = run(&mut replay, 1000.0, 1600.0);
        assert!(batches.len() >= 8, "{} batches in 0.6 s", batches.len());
        let status = replay.status();
        assert!(status.playing && (status.time - 0.6).abs() < 0.03, "{}", status.time);
        run(&mut replay, 1610.0, 2400.0);
        let status = replay.status();
        assert!(status.ended && !status.playing && (status.time - 1.0).abs() < 1e-6);
        // Play at the end starts over.
        replay.play();
        run(&mut replay, 3000.0, 3200.0);
        let status = replay.status();
        assert!(status.playing && !status.ended && status.time < 0.4, "{}", status.time);
    }

    #[test]
    fn speed_and_loop_reach_the_player() {
        let mut replay = Replay::from_bytes(recording(1.0)).unwrap();
        replay.set_speed(0.5);
        replay.set_looping(true);
        replay.set_speed(f64::NAN);
        replay.set_speed(-2.0);
        replay.play();
        run(&mut replay, 0.0, 1000.0);
        let status = replay.status();
        assert_eq!((status.speed, status.looping), (0.5, true));
        assert!((status.time - 0.5).abs() < 0.03, "{}", status.time);
        // Two more seconds at half speed: through the end and around.
        run(&mut replay, 1010.0, 3000.0);
        let status = replay.status();
        assert!(status.playing && !status.ended && status.time < 1.0);
    }

    #[test]
    fn a_slow_page_never_has_more_than_two_batches_outstanding_and_skips_ahead() {
        let mut replay = Replay::from_bytes(recording(2.0)).unwrap();
        replay.play();
        let mut sent = Vec::new();
        let mut now = 0.0;
        // The page acknowledges nothing for 400 ms (the clock step is capped
        // at 500 ms, like a pipeline tick).
        while now < 400.0 {
            let poll = replay.poll(now);
            if let Some(batch) = poll.batch {
                sent.push(batch.id);
            }
            assert!(replay.flow().outstanding() <= MAX_UNACKED_TICKS);
            assert!(poll.wait_ms.is_some());
            now += 10.0;
        }
        assert_eq!(sent.len(), MAX_UNACKED_TICKS as usize);
        replay.ack(*sent.last().unwrap());
        let batch = replay.poll(now).batch.expect("the step after the stall");
        // One coalesced batch covers the stall: a single live layer, not one per tick.
        let layers = batch.messages.iter().filter(|m| codec::peek_kind(m) == Ok(Kind::Layer)).count();
        assert!(layers <= 2, "{layers} layer messages");
        assert!((replay.status().time - 0.4).abs() < 0.03, "{}", replay.status().time);
    }

    #[test]
    fn seeks_collapse_while_the_page_is_behind_and_report_their_id() {
        let mut replay = Replay::from_bytes(recording(2.0)).unwrap();
        let first = replay.poll(0.0).batch.unwrap();
        replay.seek(0.5, 1);
        let second = replay.poll(1.0).batch.unwrap();
        assert_eq!(replay.status().seek, 1);
        // Two unacknowledged batches: further seeks only move the target.
        replay.seek(1.0, 2);
        replay.seek(1.5, 3);
        replay.seek(f64::NAN, 4);
        assert!(replay.poll(2.0).batch.is_none());
        assert_eq!(replay.status().seek, 1);
        replay.ack(first.id);
        replay.ack(second.id);
        let third = replay.poll(3.0);
        assert!(third.batch.is_some());
        let status = replay.take_status(3.0).unwrap();
        assert_eq!(status.seek, 4);
        assert!((status.time - 1.5).abs() < 1e-9 && !status.playing);
        // A backward seek goes through a keyframe.
        replay.ack(third.batch.unwrap().id);
        replay.seek(0.2, 5);
        assert!(replay.poll(4.0).reset);
    }

    #[test]
    fn a_download_plays_while_it_arrives() {
        let file = recording(2.0);
        let mut replay = Replay::streaming();
        replay.play();
        assert!(replay.poll(0.0).batch.is_none());
        assert!(!replay.status().ready);
        let half = file.len() / 2;
        replay.push_bytes(&file[..half]);
        let batches = run(&mut replay, 10.0, 3000.0);
        assert!(!batches.is_empty());
        let status = replay.status();
        // The head waits at the end of what has arrived.
        assert!(status.ready && status.buffering && status.playing && status.time < 1.6, "{status:?}");
        replay.push_bytes(&file[half..]);
        replay.finish_stream();
        run(&mut replay, 3010.0, 6000.0);
        let status = replay.status();
        assert!(status.ended && (status.time - 2.0).abs() < 1e-6 && !status.buffering, "{status:?}");
    }

    #[test]
    fn ranges_are_asked_for_until_the_picture_is_there() {
        let file = recording(2.0);
        let mut replay = Replay::ranged(file.len() as u64);
        replay.seek(1.2, 7);
        let mut requests = 0;
        let mut pictures = 0;
        for step in 0..200 {
            if let Some(batch) = replay.poll(f64::from(step)).batch {
                replay.ack(batch.id);
                pictures += 1;
            }
            match replay.wanted() {
                Some(range) => {
                    let end = (range.end as usize).min(file.len());
                    replay.supply(range.start, &file[range.start as usize..end]);
                    requests += 1;
                }
                None if replay.status().seek == 7 => break,
                None => {}
            }
        }
        let status = replay.status();
        assert!(pictures >= 1 && requests >= 2, "{pictures} pictures, {requests} requests");
        assert_eq!(status.seek, 7);
        assert!(status.ready && !status.buffering && (status.time - 1.2).abs() < 1e-9, "{status:?}");
    }

    #[test]
    fn header_texts_are_clipped_in_the_status() {
        // A header somebody made up: half a megabyte of name.
        let mut recorder = Recorder::new(Vec::new(), RecorderOptions::named("essai"));
        for message in Pipeline::new(PipelineConfig::light(SceneKind::Stage)).snapshot() {
            recorder.push(0.0, &message.bytes).unwrap();
        }
        let file = recorder.finish().unwrap().0;
        let json_len = u32::from_le_bytes(file[8..12].try_into().unwrap()) as usize;
        let long = "é".repeat(250_000);
        let json = String::from_utf8(file[12..12 + json_len].to_vec()).unwrap().replacen("essai", &long, 1);
        let mut forged = file[..8].to_vec();
        forged.extend_from_slice(&(json.len() as u32).to_le_bytes());
        forged.extend_from_slice(json.as_bytes());
        // Without index and trailer (their offsets no longer fit): a
        // recording cut short, which still plays.
        let records_end = file.len() - 16;
        forged.extend_from_slice(&file[12 + json_len..records_end]);
        let mut replay = Replay::from_bytes(forged).unwrap();
        assert!(replay.poll(0.0).batch.is_some());
        let status = replay.status();
        assert_eq!(status.name.chars().count(), MAX_STATUS_TEXT);
        assert!(status.name.chars().all(|c| c == 'é') && status.to_owned().scene == "stage");
        assert!(serde_json::to_string(&status).unwrap().len() < 2000);
    }

    #[test]
    fn bad_files_report_their_code() {
        assert_eq!(Replay::from_bytes(b"not a recording at all".to_vec()).err().map(RecordError::code), Some("bad_magic"));
        let mut replay = Replay::streaming();
        replay.push_bytes(b"GSREC1\0\0\xff\xff\xff\xff");
        assert_eq!(replay.status().error.as_deref(), Some("bad_header"));
        assert!(replay.wanted().is_none());
        // A source failure is kept as the reason.
        let mut replay = Replay::failed("http_404");
        replay.fail("network");
        replay.play();
        assert_eq!(replay.status().error.as_deref(), Some("http_404"));
    }

    #[test]
    fn the_status_is_reported_on_change_and_at_a_bounded_rate() {
        let mut replay = Replay::from_bytes(recording(1.0)).unwrap();
        run(&mut replay, 0.0, 0.0);
        assert!(replay.take_status(0.0).is_some());
        replay.play();
        // A change of state is reported at once.
        assert!(replay.take_status(1.0).unwrap().playing);
        let mut reports = 0;
        let mut now = 2.0;
        while now < 500.0 {
            if let Some(batch) = replay.poll(now).batch {
                replay.ack(batch.id);
            }
            if replay.take_status(now).is_some() {
                reports += 1;
            }
            now += 5.0;
        }
        assert!((5..=9).contains(&reports), "{reports} reports in half a second");
    }
}
