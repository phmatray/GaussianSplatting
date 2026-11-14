//! A phone for the integration tests: frames filmed by the simulated
//! phone of the gs-core tests in the procedural scenes, and a client that
//! speaks the `/ingest` protocol over TLS as the companion app does.
//!
//! Included with `#[path = "common/phone.rs"] mod phone;` next to
//! `mod common;` by the tests that need it.

#![allow(dead_code)]

// The simulator belongs to gs-core's tests (`examples/gscap_write.rs`
// includes it the same way).
#[path = "../../../gs-core/tests/ingest_sim/mod.rs"]
pub mod ingest_sim;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use gs_core::glam::Vec3;
use gs_core::ingest::frame::encode_frame;
use gs_core::ingest::{parse_pairing_uri, CaptureRegion, GscapHeader, GscapWriter, Pairing, ServerMessage};
use gs_core::scene::stage::StageScene;
use gs_core::scene::{make_scene, Scene, SceneKind};
use gs_server::ServerConfig;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::common::{self, BoxIo, TestServer, PATIENCE};
use ingest_sim::{orbit_pose, room_sweep, Phone, PhoneSim};

/// The loop of the house tour through the bedroom (as in `gscap_write`).
const BEDROOM_LOOP: [[f32; 2]; 7] =
    [[-1.3, 2.0], [-1.7, 1.78], [-2.35, 1.36], [-2.92, 1.78], [-2.8, 2.38], [-2.25, 2.6], [-1.68, 2.28]];

/// A capture as a phone would send it: its frames, wire-encoded, and what
/// the phone knows besides.
#[derive(Clone)]
pub struct Footage {
    pub mode: &'static str,
    pub frames: Vec<Vec<u8>>,
    /// World height of the floor, as the phone's `floor` message.
    pub floor_y: f32,
    /// Region of interest, for `object` and `person`.
    pub region: Option<CaptureRegion>,
}

/// A small phone: the tests are about the plumbing, not the picture.
fn small_phone() -> PhoneSim {
    PhoneSim { color_width: 256, color_height: 192, depth_width: 128, depth_height: 96, ..PhoneSim::default() }
}

impl Footage {
    /// `count` frames of a sweep through the bedroom of the house, in
    /// `room` mode. The frame indices start at `first_index`.
    pub fn bedroom(count: usize, first_index: u32) -> Self {
        let sim = small_phone();
        let house = make_scene(SceneKind::House).snapshot(0.0);
        let camera = |i: usize| sim.camera(room_sweep(&BEDROOM_LOOP, i, count, 55.0));
        let mut phone = Phone::new(sim, &camera(0));
        let frames = (0..count)
            .map(|i| {
                let mut shot = phone.shoot(&house, None, &camera(i), i as f64 / 10.0);
                shot.frame.index += first_index;
                encode_frame(&shot.frame).expect("frame encoding failed")
            })
            .collect();
        Self { mode: "room", frames, floor_y: phone.world.floor_y(), region: None }
    }

    /// `count` frames of the dancer on the stage: an orbit around her
    /// holding still (`object`), or the phone swaying in front of her
    /// while she dances (`live`).
    pub fn dancer(mode: &'static str, count: usize) -> Self {
        let sim = small_phone();
        let stage = StageScene::new();
        let dancer = Vec3::new(0.0, 1.0, 0.0);
        let live = mode == "live";
        let shot = |i: usize| {
            if live {
                let sway = 0.125 * (std::f32::consts::TAU * i as f32 / count as f32).sin();
                let (eye, target) = orbit_pose(dancer, 2.6, 1.4, 1.4, sway, 1.0);
                (sim.look_at(eye, target), stage.snapshot(i as f32 / 15.0))
            } else {
                let (eye, target) = orbit_pose(dancer, 2.3, 1.0, 1.75, i as f32 / count as f32, 1.0);
                (sim.look_at(eye, target), stage.snapshot(1.0))
            }
        };
        let mut phone = Phone::new(sim, &shot(0).0);
        let fps = if live { 15.0 } else { 10.0 };
        let frames = (0..count)
            .map(|i| {
                let (camera, snapshot) = shot(i);
                let taken = phone.shoot(&snapshot, None, &camera, i as f64 / fps);
                encode_frame(&taken.frame).expect("frame encoding failed")
            })
            .collect();
        let region =
            (!live).then(|| CaptureRegion { center: phone.world.to_capture(dancer).to_array(), radius: 1.1 });
        Self { mode, frames, floor_y: phone.world.floor_y(), region }
    }

    /// The capture as a `.gscap` file, as the phone records it offline.
    pub fn gscap(&self, name: &str) -> Vec<u8> {
        let header = GscapHeader {
            protocol: Some(1),
            device: "simulateur".into(),
            app: "0.1.0".into(),
            mode: self.mode.into(),
            name: name.into(),
            created: "2026-11-14T12:00:00Z".into(),
            region: self.region,
            floor: Some(self.floor_y),
        };
        let mut writer = GscapWriter::new(Vec::new(), &header, gs_core::ingest::file::HEADER_PAD).unwrap();
        for frame in &self.frames {
            writer.append(frame).unwrap();
        }
        writer.into_inner()
    }
}

/// Where a test server keeps its files.
pub struct Dirs {
    pub root: tempfile::TempDir,
}

impl Dirs {
    pub fn recordings(&self) -> PathBuf {
        self.root.path().join("recordings")
    }

    pub fn captures(&self) -> PathBuf {
        self.root.path().join("captures")
    }

    /// Names of the files of a directory with this extension, sorted.
    pub fn files(dir: &std::path::Path, ext: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
            .unwrap_or_default();
        names.retain(|name| name.ends_with(ext));
        names.sort();
        names
    }
}

/// A server with LAN access on and directories of its own for recordings
/// and captures.
pub async fn start_capture_server(adjust: impl FnOnce(&mut ServerConfig)) -> (TestServer, Dirs) {
    let dirs = Dirs { root: tempfile::tempdir().unwrap() };
    let recordings = dirs.recordings();
    let server = common::start_server(common::small_stage(20.0), |config| {
        config.lan = true;
        config.recordings_dir = Some(recordings);
        adjust(config);
    })
    .await;
    (server, dirs)
}

/// The pairing of a server, as the phone gets it from the QR code.
pub async fn pairing(server: &TestServer) -> Pairing {
    let answer = common::get(server.port(), "/api/pair").await;
    if answer.status == 404 && answer.json()["code"] == "no_address" {
        // A machine without any network address (a build sandbox): the
        // listener is still reachable through the loopback interface.
        let info = server.handle.info();
        return Pairing::new("127.0.0.1", server.lan_port(), &info.lan.fingerprint, &server.handle.pairing_token())
            .expect("no pairing");
    }
    assert_eq!(answer.status, 200, "{}", answer.text());
    parse_pairing_uri(answer.json()["uri"].as_str().expect("no uri")).expect("the pairing URI does not parse")
}

/// The LAN listener, reached through the loopback interface.
pub fn lan(server: &TestServer) -> SocketAddr {
    common::loopback(server.lan_port())
}

/// What the server said.
#[derive(Debug, PartialEq)]
pub enum Said {
    Message(ServerMessage),
    /// The connection ended, with this close frame if there was one.
    Closed(Option<(u16, String)>),
}

/// The phone's side of one `/ingest` connection.
pub struct PhoneClient {
    pub ws: WebSocketStream<BoxIo>,
    pub max_inflight: u32,
    /// `(frame, used)` of every ack received.
    pub acks: Vec<(u32, bool)>,
    /// `(frames, splats)` of every progress report received.
    pub progress: Vec<(u64, u64)>,
    /// Frames sent and not acknowledged yet.
    pub unacked: usize,
}

pub fn hello_json(token: &str, mode: &str, name: &str) -> String {
    serde_json::json!({
        "type": "hello", "protocol": 1, "token": token, "device": "iPhone17,2", "app": "0.1.0", "mode": mode, "name": name,
    })
    .to_string()
}

impl PhoneClient {
    /// Opens the WebSocket (TLS with the server's own certificate) without
    /// saying anything yet.
    pub async fn connect(server: &TestServer) -> Self {
        let addr = lan(server);
        let io: BoxIo = Box::new(common::tls_stream(addr, &server.tls(), &[b"http/1.1"]).await.expect("TLS failed"));
        let ws = common::handshake(io, &format!("wss://{addr}/ingest"), &[]).await.expect("WebSocket refused");
        Self { ws, max_inflight: 1, acks: Vec::new(), progress: Vec::new(), unacked: 0 }
    }

    /// Connects and says hello with the right token; expects the welcome.
    pub async fn open(server: &TestServer, mode: &str, name: &str) -> Self {
        let token = pairing(server).await.token;
        let mut phone = Self::connect(server).await;
        phone.text(hello_json(&token, mode, name)).await;
        match phone.said().await {
            Said::Message(ServerMessage::Welcome { protocol: 1, max_inflight }) => phone.max_inflight = max_inflight,
            other => panic!("expected a welcome, got {other:?}"),
        }
        phone
    }

    pub async fn text(&mut self, text: impl Into<String>) {
        self.ws.send(Message::Text(text.into().into())).await.expect("cannot send");
    }

    /// Sends one frame without regard for the flow control.
    pub async fn frame(&mut self, bytes: &[u8]) {
        self.ws.send(Message::Binary(bytes.to_vec().into())).await.expect("cannot send");
        self.unacked += 1;
    }

    /// The next message of the server, noted in `acks` and `progress`.
    pub async fn said(&mut self) -> Said {
        loop {
            let frame = tokio::time::timeout(PATIENCE, self.ws.next()).await.expect("nothing from the server");
            match frame {
                Some(Ok(Message::Text(text))) => {
                    let message = ServerMessage::parse(text.as_str())
                        .unwrap_or_else(|| panic!("not a server message: {text}"))
                        .unwrap_or_else(|kind| panic!("unknown server message type {kind}"));
                    match message {
                        ServerMessage::Ack { frame, used } => {
                            self.acks.push((frame, used));
                            self.unacked = self.unacked.saturating_sub(1);
                        }
                        ServerMessage::Progress { frames, splats } => self.progress.push((frames, splats)),
                        _ => {}
                    }
                    return Said::Message(message);
                }
                Some(Ok(Message::Close(frame))) => {
                    return Said::Closed(frame.map(|f: CloseFrame| (u16::from(f.code), f.reason.to_string())))
                }
                Some(Ok(Message::Binary(_))) => panic!("binary frame from the server"),
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return Said::Closed(None),
            }
        }
    }

    /// Reads until every frame sent is acknowledged.
    pub async fn drain_acks(&mut self) {
        while self.unacked > 0 {
            match self.said().await {
                Said::Message(_) => {}
                Said::Closed(frame) => panic!("closed with {} frames unacknowledged: {frame:?}", self.unacked),
            }
        }
    }

    /// Sends `frames` like the app: never more than `max_inflight`
    /// unacknowledged. Returns once all are acknowledged.
    pub async fn stream(&mut self, frames: &[Vec<u8>]) {
        for frame in frames {
            while self.unacked >= self.max_inflight as usize {
                match self.said().await {
                    Said::Message(_) => {}
                    Said::Closed(frame) => panic!("closed while streaming: {frame:?}"),
                }
            }
            self.frame(frame).await;
        }
        self.drain_acks().await;
    }

    /// Sends `finish` and waits for the progress report that answers it.
    pub async fn finish(&mut self) -> (u64, u64) {
        let seen = self.progress.len();
        self.text(r#"{"type":"finish"}"#).await;
        while self.progress.len() == seen {
            if let Said::Closed(frame) = self.said().await {
                panic!("closed instead of the last progress: {frame:?}");
            }
        }
        *self.progress.last().unwrap()
    }

    /// The error the server refuses with, and the close code that follows.
    pub async fn refusal(&mut self) -> (String, String, Option<u16>) {
        let (code, message) = loop {
            match self.said().await {
                Said::Message(ServerMessage::Error { code, message }) => break (code, message),
                Said::Message(_) => {}
                Said::Closed(frame) => panic!("closed without an error message: {frame:?}"),
            }
        };
        let close = loop {
            match self.said().await {
                Said::Closed(frame) => break frame.map(|(code, _)| code),
                Said::Message(_) => {}
            }
        };
        (code, message, close)
    }

    /// Closes like the app does at the end of a capture.
    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), async { while self.ws.next().await.is_some() {} }).await;
    }
}
