//! The JSON text messages of `/ingest`, both directions.
//!
//! Phone to server: `hello` first, then `region`, `floor`, `pause`,
//! `resume`, `reset`, `finish` at any time. Server to phone: `welcome` or
//! `error` (then a close), an `ack` for every frame, `progress` about once a
//! second. Unknown keys are ignored in both directions; text that is not a
//! JSON object with a string `type` is refused.

use std::fmt;

use serde_json::{Map, Value};

use super::file::json_f32;
use crate::protocol::CaptureMode;

/// Protocol version of the capture stream and of `.gscap` headers.
pub const INGEST_PROTOCOL: u32 = 1;
/// Longest client text message the server should accept, in bytes. The
/// largest real one is a `hello` with a long capture name.
pub const MAX_CLIENT_TEXT_BYTES: usize = 16 * 1024;
/// Names (capture, device, app version) are cut to this many characters:
/// they are shown to viewers and may end up in file names.
pub const MAX_NAME_CHARS: usize = 120;

/// Error codes a server sends in an `error` message before closing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IngestErrorCode {
    /// The first message is not a usable `hello`.
    BadHello,
    /// The phone speaks another protocol version.
    BadProtocol,
    /// Missing or wrong pairing token.
    BadToken,
    /// A text message that is not one of the known JSON messages.
    BadMessage,
    /// A binary message that is not a valid capture frame.
    BadFrame,
    /// More than `max_inflight` frames unacknowledged.
    FlowControl,
}

impl IngestErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadHello => "bad_hello",
            Self::BadProtocol => "bad_protocol",
            Self::BadToken => "bad_token",
            Self::BadMessage => "bad_message",
            Self::BadFrame => "bad_frame",
            Self::FlowControl => "flow_control",
        }
    }

    /// WebSocket close code that goes with the error: 1007 for data that
    /// cannot be read, 1008 for a peer that breaks the rules.
    pub fn close_code(self) -> u16 {
        match self {
            Self::BadFrame => 1007,
            _ => 1008,
        }
    }
}

impl fmt::Display for IngestErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why the session refuses a message. The server sends
/// [`SessionError::to_message`] and closes the connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionError {
    pub code: IngestErrorCode,
    /// One sentence for the person holding the phone (French; the app
    /// shows it as is).
    pub message: String,
    /// The technical cause, for the server log (English).
    pub detail: String,
}

impl SessionError {
    pub(crate) fn new(code: IngestErrorCode, message: &str, detail: impl Into<String>) -> Self {
        Self { code, message: message.to_string(), detail: detail.into() }
    }

    /// The `error` message to send before closing.
    pub fn to_message(&self) -> ServerMessage {
        ServerMessage::Error { code: self.code.as_str().to_string(), message: self.message.clone() }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for SessionError {}

/// The region of interest of the `object` and `person` modes: a sphere in
/// world coordinates (meters).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaptureRegion {
    pub center: [f32; 3],
    pub radius: f32,
}

impl CaptureRegion {
    /// Smallest and largest radius a region is given: a hostile or absurd
    /// value is clamped, not refused.
    pub const MIN_RADIUS: f32 = 0.05;
    pub const MAX_RADIUS: f32 = 10.0;

    /// From `{"center":[x,y,z],"radius":r}` with finite numbers.
    pub(crate) fn from_json(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let center = object.get("center")?.as_array()?;
        let [x, y, z] = center.as_slice() else { return None };
        let center = [json_f32(x)?, json_f32(y)?, json_f32(z)?];
        let radius = json_f32(object.get("radius")?)?;
        // A center light years away has no meaning and would overflow the
        // geometry; the world of a capture is a building at most.
        (center.iter().all(|c| c.abs() <= 1.0e4) && radius > 0.0).then_some(Self { center, radius })
    }

    pub(crate) fn to_json(self) -> Value {
        let mut object = Map::new();
        object.insert("center".into(), Value::from(self.center.map(|c| c as f64).to_vec()));
        object.insert("radius".into(), Value::from(self.radius as f64));
        Value::Object(object)
    }

    /// The same sphere with its radius forced into the supported range.
    pub fn sanitized(self) -> Self {
        Self { center: self.center, radius: self.radius.clamp(Self::MIN_RADIUS, Self::MAX_RADIUS) }
    }

    pub fn contains(&self, point: glam::Vec3) -> bool {
        point.distance_squared(glam::Vec3::from_array(self.center)) <= self.radius * self.radius
    }
}

/// The first message of a capture stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub protocol: u32,
    /// The pairing token as sent. [`Hello::parse`] has already compared it;
    /// sessions do not keep it.
    pub token: String,
    /// Hardware model, for example `iPhone17,2`.
    pub device: String,
    /// Version of the companion app.
    pub app: String,
    pub mode: CaptureMode,
    /// What the user called the capture.
    pub name: String,
}

fn parse_object(text: &str) -> Option<(Map<String, Value>, String)> {
    if text.len() > MAX_CLIENT_TEXT_BYTES {
        return None;
    }
    let Value::Object(object) = serde_json::from_str::<Value>(text).ok()? else { return None };
    let kind = object.get("type")?.as_str()?.to_string();
    Some((object, kind))
}

fn short_text(object: &Map<String, Value>, key: &str) -> String {
    let text = object.get(key).and_then(Value::as_str).unwrap_or_default();
    // Control characters have no place in a name.
    text.chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect()
}

/// Compares two strings in time that depends only on their lengths.
fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Hello {
    /// Reads and checks the first text message of a stream, in the order of
    /// the reference server: it must be a `hello` (`bad_hello`), of protocol
    /// 1 (`bad_protocol`), with the pairing token `expected_token`
    /// (`bad_token`) and a known mode (`bad_hello`).
    pub fn parse(text: &str, expected_token: &str) -> Result<Self, SessionError> {
        use IngestErrorCode::*;
        let object = match parse_object(text) {
            Some((object, kind)) if kind == "hello" => object,
            _ => {
                return Err(SessionError::new(
                    BadHello,
                    "Le premier message doit être un hello.",
                    "the first message is not a hello",
                ))
            }
        };
        let protocol = object.get("protocol").and_then(Value::as_u64);
        if protocol != Some(INGEST_PROTOCOL as u64) {
            return Err(SessionError::new(
                BadProtocol,
                "Version de protocole inconnue : mettez à jour l'application ou le serveur.",
                format!("protocol {:?}, this server speaks {INGEST_PROTOCOL}", object.get("protocol")),
            ));
        }
        let token = object.get("token").and_then(Value::as_str).unwrap_or_default();
        if expected_token.is_empty() || !constant_time_eq(token, expected_token) {
            return Err(SessionError::new(
                BadToken,
                "Jeton de jumelage incorrect : scannez de nouveau le code QR.",
                "missing or wrong pairing token",
            ));
        }
        let mode = object.get("mode").and_then(Value::as_str).and_then(CaptureMode::from_name);
        let Some(mode) = mode else {
            return Err(SessionError::new(
                BadHello,
                "Mode de capture inconnu.",
                format!("unknown mode {:?}", object.get("mode")),
            ));
        };
        Ok(Self {
            protocol: INGEST_PROTOCOL,
            token: token.to_string(),
            device: short_text(&object, "device"),
            app: short_text(&object, "app"),
            mode,
            name: short_text(&object, "name"),
        })
    }

    /// A hello for a capture that does not come from a live connection (a
    /// file, a test): no token.
    pub fn offline(mode: CaptureMode, name: &str, device: &str, app: &str) -> Self {
        let cut = |s: &str| s.chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
        Self { protocol: INGEST_PROTOCOL, token: String::new(), device: cut(device), app: cut(app), mode, name: cut(name) }
    }
}

/// A text message of the phone.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientMessage {
    Hello(Hello),
    /// Region of interest (`object` and `person`).
    Region(CaptureRegion),
    /// World height of the floor ARKit detected.
    Floor { y: f32 },
    Pause,
    Resume,
    /// Throw away what was captured; the phone starts a new world origin.
    Reset,
    Finish,
}

impl ClientMessage {
    /// Reads a text message that follows the hello. Anything that is not
    /// one of the known messages with usable numbers is `bad_message`.
    pub fn parse(text: &str) -> Result<Self, SessionError> {
        let bad = |detail: String| {
            SessionError::new(IngestErrorCode::BadMessage, "Message incompris par le serveur.", detail)
        };
        let (object, kind) = parse_object(text).ok_or_else(|| bad("not a JSON message with a type".into()))?;
        match kind.as_str() {
            "region" => CaptureRegion::from_json(&Value::Object(object))
                .map(|region| Self::Region(region.sanitized()))
                .ok_or_else(|| bad("region without a usable center and radius".into())),
            "floor" => object
                .get("y")
                .and_then(json_f32)
                .filter(|y| y.abs() <= 1.0e4)
                .map(|y| Self::Floor { y })
                .ok_or_else(|| bad("floor without a usable y".into())),
            "pause" => Ok(Self::Pause),
            "resume" => Ok(Self::Resume),
            "reset" => Ok(Self::Reset),
            "finish" => Ok(Self::Finish),
            // A second hello has no meaning either.
            other => Err(bad(format!("unknown message type {other:?}"))),
        }
    }

    /// The message as the phone writes it (for tests and simulated phones).
    pub fn to_json(&self) -> String {
        let mut object = Map::new();
        let kind = match self {
            Self::Hello(hello) => {
                object.insert("protocol".into(), Value::from(hello.protocol));
                object.insert("token".into(), Value::from(hello.token.as_str()));
                object.insert("device".into(), Value::from(hello.device.as_str()));
                object.insert("app".into(), Value::from(hello.app.as_str()));
                object.insert("mode".into(), Value::from(hello.mode.name()));
                object.insert("name".into(), Value::from(hello.name.as_str()));
                "hello"
            }
            Self::Region(region) => {
                if let Value::Object(fields) = region.to_json() {
                    object.extend(fields);
                }
                "region"
            }
            Self::Floor { y } => {
                object.insert("y".into(), Value::from(*y as f64));
                "floor"
            }
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Reset => "reset",
            Self::Finish => "finish",
        };
        object.insert("type".into(), Value::from(kind));
        Value::Object(object).to_string()
    }
}

/// A text message of the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerMessage {
    /// The hello was accepted. The phone never has more than `max_inflight`
    /// frames unacknowledged.
    Welcome { protocol: u32, max_inflight: u32 },
    /// Followed by a close.
    Error { code: String, message: String },
    /// Frame `frame` (its index) was received; `used` tells whether it went
    /// into the reconstruction.
    Ack { frame: u32, used: bool },
    /// Frames received since the hello or the last reset, and splats so far.
    Progress { frames: u64, splats: u64 },
}

impl ServerMessage {
    /// The welcome of this protocol version.
    pub fn welcome(max_inflight: u32) -> Self {
        Self::Welcome { protocol: INGEST_PROTOCOL, max_inflight: max_inflight.max(1) }
    }

    /// The JSON text frame, keys in the order of the specification.
    pub fn to_json(&self) -> String {
        match self {
            Self::Welcome { protocol, max_inflight } => {
                format!(r#"{{"type":"welcome","protocol":{protocol},"max_inflight":{max_inflight}}}"#)
            }
            Self::Error { code, message } => {
                format!(r#"{{"type":"error","code":{},"message":{}}}"#, Value::from(code.as_str()), Value::from(message.as_str()))
            }
            Self::Ack { frame, used } => format!(r#"{{"type":"ack","frame":{frame},"used":{used}}}"#),
            Self::Progress { frames, splats } => format!(r#"{{"type":"progress","frames":{frames},"splats":{splats}}}"#),
        }
    }

    /// Interprets a server text frame the way the phone does (the rules of
    /// `parse_server_message` in `tools/gscap.py`): `None` for what it
    /// drops, `Some(Err(type))` for an unknown message type it ignores.
    pub fn parse(text: &str) -> Option<Result<Self, String>> {
        let Value::Object(object) = serde_json::from_str::<Value>(text).ok()? else { return None };
        let kind = object.get("type")?.as_str()?;
        // A whole number in `0..=limit`, also when written as a float.
        let uint = |key: &str, limit: u64| -> Option<u64> {
            let value = object.get(key)?;
            let whole = value.as_u64().or_else(|| {
                value.as_f64().filter(|f| f.fract() == 0.0 && *f >= 0.0 && *f <= limit as f64).map(|f| f as u64)
            })?;
            (whole <= limit).then_some(whole)
        };
        let text_of = |key: &str| object.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        Some(Ok(match kind {
            "welcome" => Self::Welcome {
                protocol: uint("protocol", u32::MAX as u64).unwrap_or(0) as u32,
                max_inflight: uint("max_inflight", u32::MAX as u64).unwrap_or(1).max(1) as u32,
            },
            "ack" => Self::Ack {
                frame: uint("frame", u32::MAX as u64)? as u32,
                used: object.get("used") == Some(&Value::Bool(true)),
            },
            "progress" => Self::Progress {
                frames: uint("frames", 1 << 53).unwrap_or(0),
                splats: uint("splats", 1 << 53).unwrap_or(0),
            },
            "error" => Self::Error { code: text_of("code"), message: text_of("message") },
            other => return Some(Err(other.to_string())),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn hello_json(patch: impl FnOnce(&mut Map<String, Value>)) -> String {
        let hello = Hello { token: TOKEN.into(), ..Hello::offline(CaptureMode::Room, "Salon", "iPhone17,2", "0.1.0") };
        let Value::Object(mut object) = serde_json::from_str(&ClientMessage::Hello(hello).to_json()).unwrap() else { panic!() };
        patch(&mut object);
        Value::Object(object).to_string()
    }

    #[test]
    fn hello_is_checked_in_the_order_of_the_reference() {
        let hello = Hello::parse(&hello_json(|_| {}), TOKEN).unwrap();
        assert_eq!((hello.mode, hello.name.as_str(), hello.device.as_str()), (CaptureMode::Room, "Salon", "iPhone17,2"));
        let code = |text: String, token: &str| Hello::parse(&text, token).unwrap_err().code;
        use IngestErrorCode::*;
        assert_eq!(code("{}".into(), TOKEN), BadHello);
        assert_eq!(code(r#"{"type":"pause"}"#.into(), TOKEN), BadHello);
        assert_eq!(code("not json".into(), TOKEN), BadHello);
        // Protocol before token before mode.
        let all_wrong = hello_json(|o| {
            o.insert("protocol".into(), 2.into());
            o.insert("token".into(), "x".into());
            o.insert("mode".into(), "fisheye".into());
        });
        assert_eq!(code(all_wrong, TOKEN), BadProtocol);
        let two_wrong = hello_json(|o| {
            o.insert("token".into(), "x".into());
            o.insert("mode".into(), "fisheye".into());
        });
        assert_eq!(code(two_wrong, TOKEN), BadToken);
        assert_eq!(code(hello_json(|o| drop(o.insert("mode".into(), "fisheye".into()))), TOKEN), BadHello);
        assert_eq!(code(hello_json(|o| drop(o.remove("token"))), TOKEN), BadToken);
        assert_eq!(code(hello_json(|o| drop(o.insert("token".into(), 7.into()))), TOKEN), BadToken);
        assert_eq!(code(hello_json(|o| drop(o.insert("protocol".into(), "1".into()))), TOKEN), BadProtocol);
        // A server without a token accepts nobody, not everybody.
        assert_eq!(code(hello_json(|o| drop(o.insert("token".into(), "".into()))), ""), BadToken);
        // Optional texts default to nothing and are cut and cleaned.
        let bare = hello_json(|o| {
            o.remove("device");
            o.insert("name".into(), format!("a\u{0}b\n{}", "é".repeat(500)).into());
        });
        let bare = Hello::parse(&bare, TOKEN).unwrap();
        assert_eq!(bare.device, "");
        assert!(bare.name.starts_with("abé") && bare.name.chars().count() == MAX_NAME_CHARS);
        let huge = hello_json(|o| drop(o.insert("name".into(), "x".repeat(MAX_CLIENT_TEXT_BYTES).into())));
        assert_eq!(code(huge, TOKEN), BadHello);
    }

    #[test]
    fn client_messages_parse_and_hostile_ones_are_refused() {
        let ok = |text: &str| ClientMessage::parse(text).unwrap();
        assert_eq!(
            ok(r#"{"type":"region","center":[0.5,-0.25,-1.5],"radius":0.75,"extra":1}"#),
            ClientMessage::Region(CaptureRegion { center: [0.5, -0.25, -1.5], radius: 0.75 })
        );
        assert_eq!(ok(r#"{"y":-1.25,"type":"floor"}"#), ClientMessage::Floor { y: -1.25 });
        assert_eq!(ok(r#"{"type":"pause"}"#), ClientMessage::Pause);
        assert_eq!(ok(r#"{"type":"resume"}"#), ClientMessage::Resume);
        assert_eq!(ok(r#"{"type":"reset"}"#), ClientMessage::Reset);
        assert_eq!(ok(r#"{"type":"finish"}"#), ClientMessage::Finish);
        // An absurd radius is clamped.
        let ClientMessage::Region(big) = ok(r#"{"type":"region","center":[0,0,0],"radius":1e30}"#) else { panic!() };
        assert_eq!(big.radius, CaptureRegion::MAX_RADIUS);
        for bad in [
            "",
            "[]",
            "7",
            r#""pause""#,
            r#"{"type":5}"#,
            r#"{"frame":1}"#,
            r#"{"type":"hello"}"#,
            r#"{"type":"format_disk"}"#,
            r#"{"type":"region"}"#,
            r#"{"type":"region","center":[0,0],"radius":1}"#,
            r#"{"type":"region","center":[0,0,"x"],"radius":1}"#,
            r#"{"type":"region","center":[0,0,0],"radius":0}"#,
            r#"{"type":"region","center":[0,0,0],"radius":-1}"#,
            r#"{"type":"region","center":[1e30,0,0],"radius":1}"#,
            r#"{"type":"region","center":[0,0,1e999],"radius":1}"#,
            r#"{"type":"floor"}"#,
            r#"{"type":"floor","y":"low"}"#,
            r#"{"type":"floor","y":1e30}"#,
            r#"{"type":"floor","y":NaN}"#,
        ] {
            let err = ClientMessage::parse(bad).unwrap_err();
            assert_eq!(err.code, IngestErrorCode::BadMessage, "{bad}");
            assert!(!err.message.is_empty() && !err.message.contains('\u{2014}'));
        }
        let deep = format!(r#"{{"type":"pause","x":{}{}}}"#, "[".repeat(50_000), "]".repeat(50_000));
        assert!(ClientMessage::parse(&deep).is_err());
        // Round trip of what a simulated phone writes.
        for message in [
            ClientMessage::Region(CaptureRegion { center: [1.0, 2.0, -3.5], radius: 0.6 }),
            ClientMessage::Floor { y: -1.375 },
            ClientMessage::Pause,
            ClientMessage::Finish,
        ] {
            assert_eq!(ClientMessage::parse(&message.to_json()).unwrap(), message);
        }
    }

    #[test]
    fn server_messages_are_written_as_specified() {
        assert_eq!(ServerMessage::welcome(4).to_json(), r#"{"type":"welcome","protocol":1,"max_inflight":4}"#);
        assert_eq!(ServerMessage::welcome(0), ServerMessage::Welcome { protocol: 1, max_inflight: 1 });
        assert_eq!(ServerMessage::Ack { frame: 17, used: false }.to_json(), r#"{"type":"ack","frame":17,"used":false}"#);
        assert_eq!(
            ServerMessage::Progress { frames: 3, splats: 5_000_000_000 }.to_json(),
            r#"{"type":"progress","frames":3,"splats":5000000000}"#
        );
        let error = SessionError::new(IngestErrorCode::BadToken, "Jeton \"x\" refusé\n", "d").to_message();
        assert_eq!(error.to_json(), r#"{"type":"error","code":"bad_token","message":"Jeton \"x\" refusé\n"}"#);
        assert_eq!(ServerMessage::parse(&error.to_json()), Some(Ok(error)));
        assert_eq!(IngestErrorCode::BadFrame.close_code(), 1007);
        assert_eq!(IngestErrorCode::FlowControl.close_code(), 1008);
    }
}
