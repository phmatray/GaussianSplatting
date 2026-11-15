import Foundation

/// The four capture modes of the `hello` message.
public enum CaptureMode: String, CaseIterable {
    case room
    case object
    case person
    case live

    /// `object` and `person` fuse inside a region of interest.
    public var usesRegion: Bool {
        return self == .object || self == .person
    }
}

public let captureProtocolVersion = 1

/// JSON text frames sent by the phone on /ingest.
public enum ClientMessage {
    public static func hello(token: String, device: String, app: String, mode: CaptureMode, name: String) -> String {
        return json([
            "type": "hello",
            "protocol": captureProtocolVersion,
            "token": token,
            "device": device,
            "app": app,
            "mode": mode.rawValue,
            "name": name,
        ])
    }

    /// Region of interest in world meters (`object` and `person`).
    public static func region(center: [Float], radius: Float) -> String {
        return json([
            "type": "region",
            "center": center.map { number($0) },
            "radius": number(radius),
        ])
    }

    /// World height of the floor ARKit detected.
    public static func floor(y: Float) -> String {
        return json(["type": "floor", "y": number(y)])
    }

    public static let pause = json(["type": "pause"])
    public static let resume = json(["type": "resume"])
    public static let reset = json(["type": "reset"])
    public static let finish = json(["type": "finish"])

    /// JSON has no NaN or infinity (and JSONSerialization raises an
    /// Objective-C exception on them): such a value is sent as 0.
    static func number(_ value: Float) -> Double {
        return value.isFinite ? Double(value) : 0.0
    }

    static func json(_ object: [String: Any]) -> String {
        guard JSONSerialization.isValidJSONObject(object),
              let data = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]),
              let text = String(data: data, encoding: .utf8) else {
            return "{}"
        }
        return text
    }
}

/// JSON text frames sent by the server on /ingest, as the phone understands
/// them (same rules as `parse_server_message` in tools/gscap.py).
public enum ServerMessage: Equatable {
    /// `max_inflight` is at least 1 (1 if the server did not say).
    case welcome(protocolVersion: Int, maxInflight: Int)
    case ack(frame: UInt32, used: Bool)
    case progress(frames: Int, splats: Int)
    case error(code: String, message: String)
    /// A type this version does not know: ignored.
    case unknown(type: String)

    /// nil for anything that is not a JSON object with a string `type`, and
    /// for an `ack` without a usable frame number.
    public static func parse(_ text: String) -> ServerMessage? {
        guard let data = text.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data, options: []),
              let fields = object as? [String: Any],
              let type = fields["type"] as? String else {
            return nil
        }
        switch type {
        case "welcome":
            let version = wholeNumber(fields["protocol"], limit: 4294967295.0) ?? 0
            let inflight = wholeNumber(fields["max_inflight"], limit: 4294967295.0) ?? 1
            return .welcome(protocolVersion: version, maxInflight: max(1, inflight))
        case "ack":
            guard let frame = wholeNumber(fields["frame"], limit: 4294967295.0) else {
                return nil
            }
            let used = (fields["used"] as? Bool) ?? false
            return .ack(frame: UInt32(frame), used: used)
        case "progress":
            let frames = wholeNumber(fields["frames"], limit: 9007199254740992.0) ?? 0
            let splats = wholeNumber(fields["splats"], limit: 9007199254740992.0) ?? 0
            return .progress(frames: frames, splats: splats)
        case "error":
            let code = (fields["code"] as? String) ?? ""
            let message = (fields["message"] as? String) ?? ""
            return .error(code: code, message: message)
        default:
            return .unknown(type: type)
        }
    }

    /// A JSON number that is a whole value in 0...limit, else nil.
    static func wholeNumber(_ value: Any?, limit: Double) -> Int? {
        guard let number = value as? NSNumber else {
            return nil
        }
        let double = number.doubleValue
        guard double.isFinite, double >= 0, double <= limit, double == double.rounded(.down) else {
            return nil
        }
        return Int(double)
    }
}
